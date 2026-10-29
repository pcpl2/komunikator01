mod common;

use std::time::Duration;

use client_core::audio::frame::EncodedFrame;
use client_core::e2e::Trust;
use client_core::net::session::{SessionCommand, SessionEvent};
use client_core::net::{client_config, SERVER_NAME};
use common::*;
use protocol::audio::parse_downstream;
use protocol::{encode_frame, ClientMsg, FrameDecoder, ServerMsg, PROTOCOL_VERSION};
use tokio::time::timeout;

#[tokio::test]
async fn two_clients_exchange_encrypted_audio() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let (mut a, _dev_a) = session(&srv, "ala", "x", srv.fingerprint());
    let a_id = connected(&mut a).await;
    let (mut b, _dev_b) = session(&srv, "ola", "x", srv.fingerprint());
    let b_id = connected(&mut b).await;
    e2e_member(&mut a, Some(b_id), None).await;
    e2e_member(&mut b, Some(a_id), None).await;
    let got = exchange_payload(&a, &mut b, 1, b"glos").await;
    assert_eq!(got.map(|f| (f.0, f.2)), Some((a_id, b"glos".to_vec())));
}

#[tokio::test]
async fn server_side_sees_only_ciphertext() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();

    // A plain QUIC client in the room, without E2E: sees what the server relays.
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(client_config(srv.fingerprint()).unwrap());
    let conn = endpoint.connect(srv.local_addr(), SERVER_NAME).unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        username: "ewa".into(),
        auth: protocol::Auth::Password("x".into()),
        session: 7,
        room: protocol::RoomChoice::Default,
    };
    send.write_all(&encode_frame(&hello)).await.unwrap();
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(m) = dec.next::<ServerMsg>().unwrap() {
            assert!(matches!(m, ServerMsg::Welcome { .. }), "got {m:?}");
            break;
        }
        let n = recv.read(&mut buf).await.unwrap().unwrap();
        dec.push(&buf[..n]);
    }

    let (mut a, _dev_a) = session(&srv, "ala", "x", srv.fingerprint());
    let a_id = connected(&mut a).await;
    let mut seq = 1;
    let d = loop {
        a.audio_out.try_send(EncodedFrame::new(seq, b"glos").unwrap()).unwrap();
        seq += 1;
        if let Ok(d) = timeout(Duration::from_millis(50), conn.read_datagram()).await {
            break d.unwrap();
        }
        assert!(seq < 100, "no audio relayed");
    };
    let p = parse_downstream(&d).unwrap();
    assert_eq!(p.sender, a_id);
    assert_ne!(p.payload, b"glos");
    assert!(!p.payload.windows(4).any(|w| w == b"glos"), "plaintext visible to the server");
    drop(send);
}

#[tokio::test]
async fn first_contact_reports_new_user() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let fp = srv.fingerprint();
    let dev_a = Device::new();
    let mut a = session_with(&srv, "ala", "x", fp, &dev_a);
    let a_id = connected(&mut a).await;
    let (mut b, _dev_b) = session(&srv, "ola", "x", fp);
    let b_id = connected(&mut b).await;
    assert_eq!(e2e_member(&mut a, Some(b_id), None).await, Trust::NewUser);
    assert_eq!(e2e_member(&mut b, Some(a_id), None).await, Trust::NewUser);

    // Same device (identity and trust store), new session run.
    a.shutdown();
    let mut a = session_with(&srv, "ala", "x", fp, &dev_a);
    let a_id = connected(&mut a).await;
    assert_eq!(e2e_member(&mut a, Some(b_id), None).await, Trust::Known);
    assert_eq!(e2e_member(&mut b, Some(a_id), None).await, Trust::Known);
}

#[tokio::test]
async fn trust_changed_command_reports_new_trust() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let fp = srv.fingerprint();
    let dev_a = Device::new();
    let dev_b = Device::new();
    let mut a = session_with(&srv, "ala", "x", fp, &dev_a);
    let a_id = connected(&mut a).await;
    let mut b = session_with(&srv, "ola", "x", fp, &dev_b);
    connected(&mut b).await;
    e2e_member(&mut b, Some(a_id), Some(Trust::NewUser)).await;

    dev_b.trust.lock().unwrap().verify("ala", &dev_a.identity.public());
    b.commands.send(SessionCommand::TrustChanged(a_id)).unwrap();
    e2e_member(&mut b, Some(a_id), Some(Trust::Verified)).await;
}

#[tokio::test]
async fn audio_flows_after_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let port = srv.local_addr().port();
    let fp = srv.fingerprint();
    let (mut a, _dev_a) = session(&srv, "ala", "x", fp);
    let (mut b, _dev_b) = session(&srv, "ola", "x", fp);
    connected(&mut a).await;
    connected(&mut b).await;
    // High capture seqs before, low after: a WireSeq reset would show up as got.1 < before.1.
    let before = exchange_payload(&a, &mut b, 1000, b"glos").await.expect("audio before restart");

    srv.shutdown().await;
    let e = wait_for(&mut a, Duration::from_secs(20), |e| matches!(e, SessionEvent::Disconnected { .. })).await;
    assert!(e.is_some());

    let srv = server::start(config(&dir, port)).await.unwrap();
    assert_eq!(srv.fingerprint(), fp);
    let a_id = connected(&mut a).await;
    connected(&mut b).await;
    while b.audio_in.try_recv().is_ok() {}
    // Sequence numbers restart low on purpose: the session must keep its nonces fresh anyway.
    let got = exchange_payload(&a, &mut b, 1, b"glos").await.expect("audio after reconnect");
    assert_eq!((got.0, got.2), (a_id, b"glos".to_vec()));
    // The wire sequence (the nonce) is not reset by the reconnect.
    assert!(got.1 > before.1, "seq after reconnect {} <= before {}", got.1, before.1);
}
