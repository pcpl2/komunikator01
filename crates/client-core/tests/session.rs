mod common;

use std::time::Duration;

use client_core::audio::frame::EncodedFrame;
use client_core::net::session::{spawn_session, FatalReason, SessionEvent, SessionParams};
use common::*;
use tokio::time::timeout;

#[tokio::test]
async fn two_clients_exchange_audio() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let (mut a, _dev_a) = session(&srv, "ala", "x", srv.fingerprint());
    let a_id = connected(&mut a).await;
    let (mut b, _dev_b) = session(&srv, "ola", "x", srv.fingerprint());
    connected(&mut b).await;
    assert_eq!(exchange(&a, &mut b, 1).await.map(|f| (f.0, f.2)), Some((a_id, b"hej".to_vec())));
}

#[tokio::test]
async fn reconnects_after_server_restart() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let port = srv.local_addr().port();
    let fp = srv.fingerprint();
    let (mut a, _dev_a) = session(&srv, "ala", "x", fp);
    let (mut b, _dev_b) = session(&srv, "ola", "x", fp);
    connected(&mut a).await;
    connected(&mut b).await;

    srv.shutdown().await;
    let e = wait_for(&mut a, Duration::from_secs(20), |e| matches!(e, SessionEvent::Disconnected { .. })).await;
    assert!(
        matches!(e, Some(SessionEvent::Disconnected { retry_in }) if retry_in == Duration::from_millis(500)),
        "got {e:?}"
    );

    let srv = server::start(config(&dir, port)).await.unwrap();
    assert_eq!(srv.fingerprint(), fp);
    let a_id = connected(&mut a).await;
    connected(&mut b).await;
    // Drain stale frames, then audio flows again.
    while b.audio_in.try_recv().is_ok() {}
    assert_eq!(exchange(&a, &mut b, 7).await.map(|f| f.0), Some(a_id));
}

#[tokio::test]
async fn replaced_session_does_not_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let (mut first, _dev_first) = session(&srv, "ala", "x", srv.fingerprint());
    connected(&mut first).await;
    let (mut second, _dev_second) = session(&srv, "ala", "x", srv.fingerprint());
    connected(&mut second).await;

    let e = wait_for(&mut first, Duration::from_secs(5), |e| matches!(e, SessionEvent::Fatal(_))).await;
    assert!(matches!(e, Some(SessionEvent::Fatal(FatalReason::SessionReplaced))), "got {e:?}");
    let again = wait_for(&mut first, Duration::from_secs(3), |e| matches!(e, SessionEvent::Connecting)).await;
    assert!(again.is_none(), "replaced session tried to reconnect");
    let dropped = wait_for(&mut second, Duration::from_secs(1), |e| {
        matches!(e, SessionEvent::Disconnected { .. } | SessionEvent::Fatal(_))
    })
    .await;
    assert!(dropped.is_none(), "new session was disturbed: {dropped:?}");
}

#[tokio::test]
async fn bad_credentials_is_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let (mut a, _dev_a) = session(&srv, "ala", "zle", srv.fingerprint());
    let e = wait_for(&mut a, Duration::from_secs(5), |e| matches!(e, SessionEvent::Fatal(_))).await;
    assert!(matches!(e, Some(SessionEvent::Fatal(FatalReason::BadCredentials))), "got {e:?}");
    let again = wait_for(&mut a, Duration::from_secs(2), |e| matches!(e, SessionEvent::Connecting)).await;
    assert!(again.is_none());
}

#[tokio::test]
async fn wrong_fingerprint_never_connects() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let (mut a, _dev_a) = session(&srv, "ala", "x", [0; 32]);
    let e = wait_for(&mut a, Duration::from_secs(2), |e| matches!(e, SessionEvent::Connected { .. })).await;
    assert!(e.is_none());
}

#[tokio::test]
async fn stuck_server_login_times_out_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let identity = server::tls::load_or_create_identity(dir.path()).unwrap();
    let endpoint =
        quinn::Endpoint::server(server::quic_config(&identity).unwrap(), "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = endpoint.local_addr().unwrap();
    // Accepts connections and streams but never answers the Hello.
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Some(incoming) = endpoint.accept().await {
            if let Ok(conn) = incoming.await {
                held.push(conn);
            }
        }
    });
    let mut a = spawn_session(
        &tokio::runtime::Handle::current(),
        params(addr.to_string(), identity.fingerprint, "ala", "x", &Device::new()),
    );
    let e = wait_for(&mut a, Duration::from_secs(15), |e| matches!(e, SessionEvent::Disconnected { .. })).await;
    assert!(e.is_some(), "session hung in login");
}

#[test]
fn debug_output_hides_password() {
    let dev = Device::new();
    let p: SessionParams = params("a:1".into(), [0; 32], "ala", "tajne-haslo", &dev);
    let s = format!("{p:?}");
    assert!(!s.contains("tajne-haslo"));
    assert!(!s.contains("Identity") && !s.contains("KnownDevices"), "{s}");
}

#[tokio::test]
async fn audio_queued_before_login_is_not_sent() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let (mut b, _dev_b) = session(&srv, "ola", "x", srv.fingerprint());
    connected(&mut b).await;
    let (mut a, _dev_a) = session(&srv, "ala", "x", srv.fingerprint());
    // Captured while still connecting; by the time we are in, it is stale.
    for seq in 100..110 {
        a.audio_out.try_send(EncodedFrame::new(seq, b"stare").unwrap()).unwrap();
    }
    connected(&mut a).await;
    // Stale frames carried seq 100..110; only ours (from 1 up) may arrive.
    assert!(exchange(&a, &mut b, 1).await.is_some_and(|f| f.1 < 100));
}

/// Fake server: accepts the login with a tiny stream window, then never reads the control
/// stream while sending `MemberJoined`s (each makes the client re-announce).
async fn deaf_server() -> (std::net::SocketAddr, [u8; 32], tempfile::TempDir) {
    use protocol::{encode_frame, MemberInfo, ServerMsg};
    let dir = tempfile::tempdir().unwrap();
    let identity = server::tls::load_or_create_identity(dir.path()).unwrap();
    let mut cfg = server::quic_config(&identity).unwrap();
    let mut transport = quinn::TransportConfig::default();
    transport.stream_receive_window(quinn::VarInt::from_u32(512));
    transport.keep_alive_interval(Some(Duration::from_secs(1)));
    cfg.transport_config(std::sync::Arc::new(transport));
    let endpoint = quinn::Endpoint::server(cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Some(incoming) = endpoint.accept().await {
            let Ok(conn) = incoming.await else { continue };
            let Ok((mut send, mut recv)) = conn.accept_bi().await else { continue };
            let mut buf = [0u8; 256];
            let _ = recv.read(&mut buf).await; // the Hello; nothing is read after it
            let welcome = ServerMsg::Welcome { you: 1, members: vec![] };
            let _ = send.write_all(&encode_frame(&welcome)).await;
            for id in 2..200u16 {
                let m = ServerMsg::MemberJoined(MemberInfo { id, name: format!("u{id}") });
                let _ = send.write_all(&encode_frame(&m)).await;
            }
            held.push((conn, send, recv));
        }
    });
    (addr, identity.fingerprint, dir)
}

#[tokio::test]
async fn server_not_reading_control_stream_forces_retry() {
    let (addr, fp, _dir) = deaf_server().await;
    let dev = Device::new();
    let mut a = spawn_session(&tokio::runtime::Handle::current(), params(addr.to_string(), fp, "ala", "x", &dev));
    connected(&mut a).await;
    let e = wait_for(&mut a, Duration::from_secs(15), |e| matches!(e, SessionEvent::Disconnected { .. })).await;
    assert!(e.is_some(), "session stuck on a control write");
}

#[tokio::test]
async fn shutdown_wins_over_stuck_control_write() {
    let (addr, fp, _dir) = deaf_server().await;
    let dev = Device::new();
    let mut a = spawn_session(&tokio::runtime::Handle::current(), params(addr.to_string(), fp, "ala", "x", &dev));
    connected(&mut a).await;
    // Let the client run into the full stream window.
    wait_for(&mut a, Duration::from_secs(1), |e| matches!(e, SessionEvent::Fatal(_))).await;
    a.shutdown();
    let ended = timeout(Duration::from_secs(1), async { while a.events.recv().await.is_some() {} }).await;
    assert!(ended.is_ok(), "session ignored shutdown");
}
