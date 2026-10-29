//! Invite registration, session tokens and blocking, against a real server.

mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use client_core::net::session::{spawn_session, Credentials, FatalReason, SessionEvent, SessionHandle};
use common::*;
use server::store::Store;
use server::RunningServer;

fn store(dir: &tempfile::TempDir) -> Store {
    Store::open(&dir.path().join("komunikator.db")).unwrap()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn start(srv: &RunningServer, user: &str, credentials: Credentials, dev: &Device) -> SessionHandle {
    let mut p = params(srv.local_addr().to_string(), srv.fingerprint(), user, "", dev);
    p.credentials = credentials;
    spawn_session(&tokio::runtime::Handle::current(), p)
}

async fn token_issued(h: &mut SessionHandle) -> [u8; 32] {
    match wait_for(h, Duration::from_secs(10), |e| matches!(e, SessionEvent::TokenIssued(_))).await {
        Some(SessionEvent::TokenIssued(t)) => t,
        _ => panic!("no token issued"),
    }
}

async fn fatal(h: &mut SessionHandle, wait: Duration) -> Option<FatalReason> {
    match wait_for(h, wait, |e| matches!(e, SessionEvent::Fatal(_))).await {
        Some(SessionEvent::Fatal(r)) => Some(r),
        _ => None,
    }
}

#[tokio::test]
async fn register_then_token_login() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let invite = [7u8; 16];
    store(&dir).create_invite(&invite, now(), 3600).unwrap();
    let dev = Device::new();

    let mut a = start(&srv, "Zenek", Credentials::Register { invite, password: "długie hasło".into() }, &dev);
    let first = token_issued(&mut a).await;
    connected(&mut a).await;
    a.shutdown();

    let mut b = start(&srv, "Zenek", Credentials::Token(first), &dev);
    let second = token_issued(&mut b).await;
    assert_ne!(second, first, "every login issues a fresh token");
    connected(&mut b).await;
    b.shutdown();

    // Tokens are single use: once a newer one was used, the first one is dead (before that, a
    // retry within a minute would be let in, in case the Welcome was lost).
    let mut c = start(&srv, "Zenek", Credentials::Token(second), &dev);
    token_issued(&mut c).await;
    connected(&mut c).await;
    c.shutdown();
    let mut d = start(&srv, "Zenek", Credentials::Token(first), &dev);
    assert_eq!(fatal(&mut d, Duration::from_secs(10)).await, Some(FatalReason::LoginRequired));
}

#[tokio::test]
async fn used_invite_is_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let invite = [8u8; 16];
    store(&dir).create_invite(&invite, now(), 3600).unwrap();
    let dev = Device::new();
    let mut a = start(&srv, "Zenek", Credentials::Register { invite, password: "długie hasło".into() }, &dev);
    connected(&mut a).await;
    let mut b = start(&srv, "Ziutek", Credentials::Register { invite, password: "długie hasło".into() }, &dev);
    assert_eq!(fatal(&mut b, Duration::from_secs(10)).await, Some(FatalReason::InviteInvalid));
}

#[tokio::test]
async fn blocked_while_connected_is_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, 0)).await.unwrap();
    let (mut a, _dev) = session(&srv, "ala", "x", srv.fingerprint());
    connected(&mut a).await;
    assert!(store(&dir).set_blocked("ala", true).unwrap());
    assert_eq!(fatal(&mut a, Duration::from_secs(15)).await, Some(FatalReason::AccountBlocked));
    let again = wait_for(&mut a, Duration::from_secs(2), |e| matches!(e, SessionEvent::Connecting)).await;
    assert!(again.is_none(), "blocked session tried to reconnect");
}
