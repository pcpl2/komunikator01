mod common;

use std::time::Duration;

use client_core::audio::frame::EncodedFrame;
use client_core::net::session::{ChatLine, ChatSendError, SessionCommand, SessionEvent, SessionHandle};
use common::*;
use tokio::time::{timeout, Instant};

/// Sends frames from `from` for `dur` and returns whether any reached `to`.
async fn any_audio(from: &SessionHandle, to: &mut SessionHandle, mut seq: u32, dur: Duration) -> bool {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        let _ = from.audio_out.try_send(EncodedFrame::new(seq, b"hej").unwrap());
        seq += 1;
        if let Ok(Some(_)) = timeout(Duration::from_millis(50), to.audio_in.recv()).await {
            return true;
        }
    }
    false
}

async fn chat_line(h: &mut SessionHandle) -> ChatLine {
    match wait_for(h, Duration::from_secs(5), |e| matches!(e, SessionEvent::Chat(_))).await {
        Some(SessionEvent::Chat(line)) => line,
        other => panic!("expected Chat, got {other:?}"),
    }
}

async fn not_sent(h: &mut SessionHandle) -> ChatSendError {
    match wait_for(h, Duration::from_secs(5), |e| matches!(e, SessionEvent::ChatNotSent(_))).await {
        Some(SessionEvent::ChatNotSent(e)) => e,
        other => panic!("expected ChatNotSent, got {other:?}"),
    }
}

#[tokio::test]
async fn chat_reaches_room_member_encrypted() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let (mut a, _dev_a) = session(&srv, "ala", "x", srv.fingerprint());
    let a_id = connected(&mut a).await;
    let (mut b, _dev_b) = session(&srv, "ola", "x", srv.fingerprint());
    connected(&mut b).await;
    // Audio getting through means ala's sender key reached ola; chat uses the same key.
    assert!(exchange(&a, &mut b, 1).await.is_some());

    a.commands.send(SessionCommand::SendChat("192.168.1.10:27015".into())).unwrap();
    let own = chat_line(&mut a).await;
    assert!(own.own && own.from == a_id && own.text == "192.168.1.10:27015", "{own:?}");
    let got = chat_line(&mut b).await;
    assert!(!got.own, "{got:?}");
    assert_eq!((got.from, got.name.as_str(), got.text.as_str()), (a_id, "ala", "192.168.1.10:27015"));
}

#[tokio::test]
async fn audio_follows_room_moves() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let (mut a, _dev_a) = session(&srv, "ala", "x", srv.fingerprint());
    let a_id = connected(&mut a).await;
    let (mut b, _dev_b) = session(&srv, "ola", "x", srv.fingerprint());
    let b_id = connected(&mut b).await;
    assert!(exchange(&a, &mut b, 1).await.is_some());

    let gra = create_room(&mut a, "Gra").await;
    b.commands.send(SessionCommand::JoinRoom(gra)).unwrap();
    moved(&mut b, b_id, Some(gra)).await;
    moved(&mut a, b_id, Some(gra)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    while b.audio_in.try_recv().is_ok() {}
    assert!(!any_audio(&a, &mut b, 100, Duration::from_secs(1)).await, "audio crossed rooms");

    a.commands.send(SessionCommand::JoinRoom(gra)).unwrap();
    moved(&mut a, a_id, Some(gra)).await;
    let got = exchange_within(&a, &mut b, 200, b"hej", Duration::from_secs(3)).await;
    assert_eq!(got.map(|f| f.0), Some(a_id), "audio did not follow into the new room");
}

#[tokio::test]
async fn reconnect_returns_to_last_room() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let port = srv.local_addr().port();
    let fp = srv.fingerprint();
    let (mut a, _dev_a) = session(&srv, "ala", "x", fp);
    let a_id = connected(&mut a).await;
    let gra = create_room(&mut a, "Gra").await;
    a.commands.send(SessionCommand::JoinRoom(gra)).unwrap();
    moved(&mut a, a_id, Some(gra)).await;

    srv.shutdown().await;
    let e = wait_for(&mut a, Duration::from_secs(20), |e| matches!(e, SessionEvent::Disconnected { .. })).await;
    assert!(e.is_some());
    let _srv = server::start(config(&dir, port)).await.unwrap();
    assert_eq!(connected_in(&mut a).await.1, Some(gra));
}

#[tokio::test]
async fn deleted_room_falls_back_to_default_on_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let port = srv.local_addr().port();
    let fp = srv.fingerprint();
    let (mut a, _dev_a) = session(&srv, "ala", "x", fp);
    let (a_id, default) = connected_in(&mut a).await;
    assert!(default.is_some());
    let gra = create_room(&mut a, "Gra").await;
    a.commands.send(SessionCommand::JoinRoom(gra)).unwrap();
    moved(&mut a, a_id, Some(gra)).await;

    srv.shutdown().await;
    let e = wait_for(&mut a, Duration::from_secs(20), |e| matches!(e, SessionEvent::Disconnected { .. })).await;
    assert!(e.is_some());
    // Deleted while the server was down: the last room no longer exists.
    let store = server::store::Store::open(&dir.path().join("komunikator.db")).unwrap();
    assert!(store.delete_room(gra).unwrap());
    drop(store);
    let _srv = server::start(config(&dir, port)).await.unwrap();
    assert_eq!(connected_in(&mut a).await.1, default);
}

#[tokio::test]
async fn chat_errors() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let (mut a, _dev_a) = session(&srv, "ala", "x", srv.fingerprint());
    let a_id = connected(&mut a).await;

    a.commands.send(SessionCommand::SendChat("   ".into())).unwrap();
    assert_eq!(not_sent(&mut a).await, ChatSendError::Empty);
    a.commands.send(SessionCommand::SendChat("x".repeat(2001))).unwrap();
    assert_eq!(not_sent(&mut a).await, ChatSendError::TooLong);

    a.commands.send(SessionCommand::LeaveRoom).unwrap();
    moved(&mut a, a_id, None).await;
    a.commands.send(SessionCommand::SendChat("x".into())).unwrap();
    assert_eq!(not_sent(&mut a).await, ChatSendError::NotInRoom);
}

#[tokio::test]
async fn chat_while_disconnected_is_not_sent() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let (mut a, _dev_a) = session(&srv, "ala", "x", srv.fingerprint());
    connected(&mut a).await;
    srv.shutdown().await;
    let e = wait_for(&mut a, Duration::from_secs(20), |e| matches!(e, SessionEvent::Disconnected { .. })).await;
    assert!(e.is_some());
    a.commands.send(SessionCommand::SendChat("x".into())).unwrap();
    assert_eq!(not_sent(&mut a).await, ChatSendError::NotConnected);
}

#[tokio::test]
async fn member_rejoining_is_heard_again() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let fp = srv.fingerprint();
    let (mut a, _dev_a) = session(&srv, "ala", "x", fp);
    let a_id = connected(&mut a).await;
    let dev_b = Device::new();
    let mut b = session_with(&srv, "ola", "x", fp, &dev_b);
    let b_id = connected(&mut b).await;
    assert!(exchange(&a, &mut b, 1).await.is_some());
    assert!(exchange(&b, &mut a, 1).await.is_some());

    b.shutdown();
    let mut b = session_with(&srv, "ola", "x", fp, &dev_b);
    let b2_id = connected(&mut b).await;
    assert_ne!(b2_id, b_id);
    while a.audio_in.try_recv().is_ok() {}
    let wait = Duration::from_secs(3);
    let to_b = exchange_within(&a, &mut b, 100, b"hej", wait).await;
    assert_eq!(to_b.map(|f| f.0), Some(a_id), "rejoined member does not hear the other");
    let to_a = exchange_within(&b, &mut a, 100, b"hej", wait).await;
    assert_eq!(to_a.map(|f| f.0), Some(b2_id), "rejoined member is not heard");
}
