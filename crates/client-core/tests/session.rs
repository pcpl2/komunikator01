use std::sync::OnceLock;
use std::time::Duration;

use client_core::audio::frame::EncodedFrame;
use client_core::net::session::{
    spawn_session, FatalReason, SessionEvent, SessionHandle, SessionParams,
};
use protocol::MemberId;
use server::{RunningServer, ServerConfig, UserEntry};
use tokio::time::{timeout, Instant};

fn hash_x() -> String {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| server::auth::hash_password("x")).clone()
}

fn config(dir: &tempfile::TempDir, port: u16) -> ServerConfig {
    ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        data_dir: dir.path().to_owned(),
        users: ["ala", "ola"]
            .iter()
            .map(|n| UserEntry { name: n.to_string(), password_hash: hash_x() })
            .collect(),
    }
}

fn session(srv: &RunningServer, user: &str, pw: &str, fingerprint: [u8; 32]) -> SessionHandle {
    spawn_session(
        &tokio::runtime::Handle::current(),
        SessionParams {
            server: srv.local_addr().to_string(),
            fingerprint,
            username: user.into(),
            password: pw.into(),
        },
    )
}

/// Waits for the first event matching `pred`, failing after `wait`.
async fn wait_for(
    h: &mut SessionHandle,
    wait: Duration,
    pred: impl Fn(&SessionEvent) -> bool,
) -> Option<SessionEvent> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match timeout(left, h.events.recv()).await {
            Ok(Some(e)) if pred(&e) => return Some(e),
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}

async fn connected(h: &mut SessionHandle) -> MemberId {
    match wait_for(h, Duration::from_secs(5), |e| matches!(e, SessionEvent::Connected { .. })).await {
        Some(SessionEvent::Connected { you, .. }) => you,
        other => panic!("expected Connected, got {other:?}"),
    }
}

async fn exchange(from: &SessionHandle, to: &mut SessionHandle, seq: u32) -> Option<(MemberId, u32, Vec<u8>)> {
    from.audio_out.try_send(EncodedFrame::new(seq, b"hej").unwrap()).unwrap();
    let f = timeout(Duration::from_secs(2), to.audio_in.recv()).await.ok()??;
    Some((f.sender, f.seq, f.payload().to_vec()))
}

#[tokio::test]
async fn two_clients_exchange_audio() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let mut a = session(&srv, "ala", "x", srv.fingerprint());
    let a_id = connected(&mut a).await;
    let mut b = session(&srv, "ola", "x", srv.fingerprint());
    connected(&mut b).await;
    assert_eq!(exchange(&a, &mut b, 1).await, Some((a_id, 1, b"hej".to_vec())));
}

#[tokio::test]
async fn reconnects_after_server_restart() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let port = srv.local_addr().port();
    let fp = srv.fingerprint();
    let mut a = session(&srv, "ala", "x", fp);
    let mut b = session(&srv, "ola", "x", fp);
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
    let mut first = session(&srv, "ala", "x", srv.fingerprint());
    connected(&mut first).await;
    let mut second = session(&srv, "ala", "x", srv.fingerprint());
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
    let mut a = session(&srv, "ala", "zle", srv.fingerprint());
    let e = wait_for(&mut a, Duration::from_secs(5), |e| matches!(e, SessionEvent::Fatal(_))).await;
    assert!(matches!(e, Some(SessionEvent::Fatal(FatalReason::BadCredentials))), "got {e:?}");
    let again = wait_for(&mut a, Duration::from_secs(2), |e| matches!(e, SessionEvent::Connecting)).await;
    assert!(again.is_none());
}

#[tokio::test]
async fn wrong_fingerprint_never_connects() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let mut a = session(&srv, "ala", "x", [0; 32]);
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
        SessionParams {
            server: addr.to_string(),
            fingerprint: identity.fingerprint,
            username: "ala".into(),
            password: "x".into(),
        },
    );
    let e = wait_for(&mut a, Duration::from_secs(15), |e| matches!(e, SessionEvent::Disconnected { .. })).await;
    assert!(e.is_some(), "session hung in login");
}

#[test]
fn debug_output_hides_password() {
    let p = SessionParams {
        server: "a:1".into(),
        fingerprint: [0; 32],
        username: "ala".into(),
        password: "tajne-haslo".into(),
    };
    assert!(!format!("{p:?}").contains("tajne-haslo"));
}

#[tokio::test]
async fn audio_queued_before_login_is_not_sent() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let mut b = session(&srv, "ola", "x", srv.fingerprint());
    connected(&mut b).await;
    let mut a = session(&srv, "ala", "x", srv.fingerprint());
    // Captured while still connecting; by the time we are in, it is stale.
    for seq in 100..110 {
        a.audio_out.try_send(EncodedFrame::new(seq, b"stare").unwrap()).unwrap();
    }
    connected(&mut a).await;
    assert_eq!(exchange(&a, &mut b, 1).await.map(|f| f.1), Some(1));
}
