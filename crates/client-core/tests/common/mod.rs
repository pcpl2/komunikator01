//! Helpers shared by the session integration tests (a real server via `server::start`).
#![allow(dead_code)]

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use client_core::audio::frame::EncodedFrame;
use client_core::e2e::{E2eEvent, Identity, KnownDevices, Trust};
use client_core::net::session::{spawn_session, SessionCommand, SessionEvent, SessionHandle, SessionParams};
use protocol::{MemberId, RoomId};
use server::{RunningServer, ServerConfig, UserEntry};
use tokio::time::{timeout, Instant};

pub fn hash_x() -> String {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| server::auth::hash_password("x")).clone()
}

pub fn config(dir: &tempfile::TempDir, port: u16) -> ServerConfig {
    ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        data_dir: dir.path().to_owned(),
        users: ["ala", "ola", "ewa"]
            .iter()
            .map(|n| UserEntry { name: n.to_string(), password_hash: hash_x(), admin: false })
            .collect(),
    }
}

/// A device: identity plus its trust store (kept in a temp dir that lives as long as it does).
#[derive(Clone)]
pub struct Device {
    pub identity: Arc<Identity>,
    pub trust: Arc<Mutex<KnownDevices>>,
    _dir: Arc<tempfile::TempDir>,
}

impl Device {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let trust = KnownDevices::load(&dir.path().join("known_devices.toml")).unwrap();
        Self {
            identity: Arc::new(Identity::generate()),
            trust: Arc::new(Mutex::new(trust)),
            _dir: Arc::new(dir),
        }
    }
}

pub fn params(server: String, fingerprint: [u8; 32], user: &str, pw: &str, dev: &Device) -> SessionParams {
    SessionParams {
        server,
        fingerprint,
        username: user.into(),
        password: pw.into(),
        identity: dev.identity.clone(),
        trust: dev.trust.clone(),
    }
}

pub fn session_with(srv: &RunningServer, user: &str, pw: &str, fingerprint: [u8; 32], dev: &Device) -> SessionHandle {
    spawn_session(
        &tokio::runtime::Handle::current(),
        params(srv.local_addr().to_string(), fingerprint, user, pw, dev),
    )
}

/// A session on a fresh device. Keep the returned `Device` alive as long as the session: it
/// owns the temp dir the trust store saves into.
pub fn session(srv: &RunningServer, user: &str, pw: &str, fingerprint: [u8; 32]) -> (SessionHandle, Device) {
    let dev = Device::new();
    (session_with(srv, user, pw, fingerprint, &dev), dev)
}

/// Waits for the first event matching `pred`, failing after `wait`.
pub async fn wait_for(
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

pub async fn connected(h: &mut SessionHandle) -> MemberId {
    connected_in(h).await.0
}

/// Waits for `Connected`; returns my id and the room the server put me in.
pub async fn connected_in(h: &mut SessionHandle) -> (MemberId, Option<RoomId>) {
    match wait_for(h, Duration::from_secs(5), |e| matches!(e, SessionEvent::Connected { .. })).await {
        Some(SessionEvent::Connected { you, room, .. }) => (you, room),
        other => panic!("expected Connected, got {other:?}"),
    }
}

/// Creates a room named `name` and waits for the server to confirm it.
pub async fn create_room(h: &mut SessionHandle, name: &str) -> RoomId {
    h.commands.send(SessionCommand::CreateRoom(name.into())).unwrap();
    let e = wait_for(h, Duration::from_secs(5), |e| matches!(e, SessionEvent::RoomCreated(r) if r.name == name)).await;
    match e {
        Some(SessionEvent::RoomCreated(r)) => r.id,
        other => panic!("expected RoomCreated, got {other:?}"),
    }
}

/// Waits until `h` sees member `id` move to `room`.
pub async fn moved(h: &mut SessionHandle, id: MemberId, room: Option<RoomId>) {
    let e = wait_for(h, Duration::from_secs(5), |e| {
        matches!(e, SessionEvent::MemberMoved { id: m, room: r } if *m == id && *r == room)
    })
    .await;
    assert!(e.is_some(), "member {id} never seen moving to {room:?}");
}

/// Waits for an E2E `Member` event about `id` (any member when `None`) with trust `trust`
/// (any trust when `None`); returns its trust.
pub async fn e2e_member(h: &mut SessionHandle, id: Option<MemberId>, trust: Option<Trust>) -> Trust {
    let e = wait_for(h, Duration::from_secs(5), |e| {
        matches!(e, SessionEvent::E2e(E2eEvent::Member { id: m, trust: t, .. })
            if id.is_none_or(|id| id == *m) && trust.is_none_or(|tr| tr == *t))
    })
    .await;
    match e {
        Some(SessionEvent::E2e(E2eEvent::Member { trust, .. })) => trust,
        other => panic!("expected E2e Member, got {other:?}"),
    }
}

/// Sends `payload` from `from` (one frame every 50 ms, sequence numbers from `seq` up) until
/// `to` receives a frame, so it also waits for E2E keys to be in place. Gives up after 5 s.
pub async fn exchange_payload(
    from: &SessionHandle,
    to: &mut SessionHandle,
    seq: u32,
    payload: &[u8],
) -> Option<(MemberId, u32, Vec<u8>)> {
    exchange_within(from, to, seq, payload, Duration::from_secs(5)).await
}

/// `exchange_payload`, giving up after `wait`.
pub async fn exchange_within(
    from: &SessionHandle,
    to: &mut SessionHandle,
    mut seq: u32,
    payload: &[u8],
    wait: Duration,
) -> Option<(MemberId, u32, Vec<u8>)> {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        let _ = from.audio_out.try_send(EncodedFrame::new(seq, payload).unwrap());
        seq += 1;
        if let Ok(Some(f)) = timeout(Duration::from_millis(50), to.audio_in.recv()).await {
            return Some((f.sender, f.seq, f.payload().to_vec()));
        }
    }
    None
}

pub async fn exchange(from: &SessionHandle, to: &mut SessionHandle, seq: u32) -> Option<(MemberId, u32, Vec<u8>)> {
    exchange_payload(from, to, seq, b"hej").await
}
