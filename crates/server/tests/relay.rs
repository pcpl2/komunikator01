use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use protocol::audio::{encode_upstream, parse_downstream};
use protocol::{
    encode_frame, ClientMsg, FrameDecoder, MemberId, MemberInfo, RejectReason, RoomChoice,
    ServerMsg, PROTOCOL_VERSION,
};
use quinn::{Connection, ConnectionError, Endpoint, RecvStream, SendStream, VarInt};
use server::{RunningServer, ServerConfig, UserEntry};
use tokio::time::timeout;

fn hash_x() -> String {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| server::auth::hash_password("x")).clone()
}

async fn start_server(dir: &tempfile::TempDir) -> RunningServer {
    let users = ["ala", "ola", "ewa"]
        .iter()
        .map(|n| UserEntry { name: n.to_string(), password_hash: hash_x(), admin: false })
        .collect();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.path().to_owned(),
        users,
        public_address: None,
    };
    server::start(config).await.unwrap()
}

struct Client {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    dec: FrameDecoder,
    _endpoint: Endpoint,
}

impl Client {
    async fn connect(addr: SocketAddr, fp: [u8; 32]) -> Client {
        let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(client_core::net::client_config(fp).unwrap());
        let conn = endpoint.connect(addr, client_core::net::SERVER_NAME).unwrap().await.unwrap();
        let (send, recv) = conn.open_bi().await.unwrap();
        Client { conn, send, recv, dec: FrameDecoder::new(), _endpoint: endpoint }
    }

    /// Hello as a fresh client run.
    async fn hello(&mut self, name: &str, pw: &str, version: u16) {
        static NEXT: AtomicU64 = AtomicU64::new(1_000);
        self.hello_session(name, pw, version, NEXT.fetch_add(1, Ordering::Relaxed)).await;
    }

    async fn hello_session(&mut self, name: &str, pw: &str, version: u16, session: u64) {
        let msg = ClientMsg::Hello {
            version,
            username: name.into(),
            password: pw.into(),
            session,
            room: RoomChoice::Default,
        };
        self.send.write_all(&encode_frame(&msg)).await.unwrap();
    }

    async fn next_msg(&mut self) -> Option<ServerMsg> {
        let fut = async {
            loop {
                if let Some(m) = self.dec.next::<ServerMsg>().unwrap() {
                    return Some(m);
                }
                let mut buf = [0u8; 4096];
                match self.recv.read(&mut buf).await {
                    Ok(Some(n)) => self.dec.push(&buf[..n]),
                    _ => return None,
                }
            }
        };
        timeout(Duration::from_secs(5), fut).await.ok().flatten()
    }

    async fn login(srv: &RunningServer, name: &str) -> (Client, MemberId, Vec<MemberInfo>) {
        Self::login_session(srv, name, None).await
    }

    async fn login_session(
        srv: &RunningServer,
        name: &str,
        session: Option<u64>,
    ) -> (Client, MemberId, Vec<MemberInfo>) {
        let mut c = Client::connect(srv.local_addr(), srv.fingerprint()).await;
        match session {
            Some(s) => c.hello_session(name, "x", PROTOCOL_VERSION, s).await,
            None => c.hello(name, "x", PROTOCOL_VERSION).await,
        }
        match c.next_msg().await {
            // Everyone lands in the default room; return the others, as before rooms existed.
            Some(ServerMsg::Welcome { you, members, .. }) => {
                (c, you, members.into_iter().filter(|m| m.id != you).collect())
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    async fn send_msg(&mut self, msg: &ClientMsg) {
        self.send.write_all(&encode_frame(msg)).await.unwrap();
    }

    async fn quiet(&mut self, wait: Duration) -> bool {
        timeout(wait, self.next_msg()).await.is_err()
    }

    fn send_audio(&self, seq: u32, payload: &[u8]) {
        let mut d = Vec::new();
        encode_upstream(0, seq, payload, &mut d);
        self.conn.send_datagram(d.into()).unwrap();
    }

    async fn recv_audio(&self, wait: Duration) -> Option<(MemberId, u32, Vec<u8>)> {
        let d = timeout(wait, self.conn.read_datagram()).await.ok()?.ok()?;
        let p = parse_downstream(&d).unwrap();
        Some((p.sender, p.seq, p.payload.to_vec()))
    }
}

fn closed_with(err: &ConnectionError, code: u32) -> bool {
    matches!(err, ConnectionError::ApplicationClosed(c) if c.error_code == VarInt::from_u32(code))
}

#[tokio::test]
async fn welcome_and_member_events() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, ala_id, members) = Client::login(&srv, "ala").await;
    assert!(members.is_empty());
    let (ola, ola_id, members) = Client::login(&srv, "ola").await;
    assert_eq!(members, [MemberInfo { id: ala_id, name: "ala".into(), room: Some(1) }]);
    assert_eq!(
        ala.next_msg().await,
        Some(ServerMsg::MemberJoined(MemberInfo { id: ola_id, name: "ola".into(), room: Some(1) }))
    );
    ola.conn.close(VarInt::from_u32(0), b"bye");
    assert_eq!(ala.next_msg().await, Some(ServerMsg::MemberLeft { id: ola_id }));
}

#[tokio::test]
async fn relays_audio_to_others_only() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (ala, ala_id, _) = Client::login(&srv, "ala").await;
    let (ola, _, _) = Client::login(&srv, "ola").await;
    let (ewa, _, _) = Client::login(&srv, "ewa").await;
    ala.send_audio(5, b"glos");
    let expected = Some((ala_id, 5, b"glos".to_vec()));
    assert_eq!(ola.recv_audio(Duration::from_secs(2)).await, expected);
    assert_eq!(ewa.recv_audio(Duration::from_secs(2)).await, expected);
    assert_eq!(ala.recv_audio(Duration::from_millis(300)).await, None);
}

#[tokio::test]
async fn bad_password_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let mut c = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    c.hello("ala", "zle", PROTOCOL_VERSION).await;
    assert_eq!(c.next_msg().await, Some(ServerMsg::Rejected { reason: RejectReason::BadCredentials }));
}

#[tokio::test]
async fn unknown_user_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let mut c = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    c.hello("obcy", "x", PROTOCOL_VERSION).await;
    assert_eq!(c.next_msg().await, Some(ServerMsg::Rejected { reason: RejectReason::BadCredentials }));
}

#[tokio::test]
async fn version_mismatch_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let mut c = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    c.hello("ala", "x", 99).await;
    assert_eq!(
        c.next_msg().await,
        Some(ServerMsg::Rejected { reason: RejectReason::VersionMismatch { server: PROTOCOL_VERSION } })
    );
}

#[tokio::test]
async fn v2_hello_gets_version_mismatch() {
    /// The v2 `ClientMsg::Hello`, without `room`.
    #[derive(serde::Serialize)]
    enum V2ClientMsg {
        Hello { version: u16, username: String, password: String, session: u64 },
    }
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let mut c = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    let hello = V2ClientMsg::Hello { version: 2, username: "ala".into(), password: "x".into(), session: 1 };
    c.send.write_all(&encode_frame(&hello)).await.unwrap();
    assert_eq!(
        c.next_msg().await,
        Some(ServerMsg::Rejected { reason: RejectReason::VersionMismatch { server: 3 } })
    );
}

#[tokio::test]
async fn sixth_attempt_is_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    for _ in 0..5 {
        let mut c = Client::connect(srv.local_addr(), srv.fingerprint()).await;
        c.hello("ala", "zle", PROTOCOL_VERSION).await;
        assert_eq!(c.next_msg().await, Some(ServerMsg::Rejected { reason: RejectReason::BadCredentials }));
    }
    let mut c = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    c.hello("ala", "x", PROTOCOL_VERSION).await;
    assert_eq!(c.next_msg().await, Some(ServerMsg::Rejected { reason: RejectReason::RateLimited }));
}

#[tokio::test]
async fn parallel_guesses_are_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let mut clients = Vec::new();
    for _ in 0..20 {
        clients.push(Client::connect(srv.local_addr(), srv.fingerprint()).await);
    }
    for c in clients.iter_mut() {
        c.hello("ala", "zle", PROTOCOL_VERSION).await;
    }
    let mut bad_credentials = 0;
    for c in clients.iter_mut() {
        if c.next_msg().await == Some(ServerMsg::Rejected { reason: RejectReason::BadCredentials }) {
            bad_credentials += 1;
        }
    }
    assert!(bad_credentials <= 5, "{bad_credentials} guesses were checked");
}

#[tokio::test]
async fn second_login_replaces_first() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut first, first_id, _) = Client::login(&srv, "ala").await;
    let (mut watcher, ola_id, _) = Client::login(&srv, "ola").await;
    let (_second, second_id, members) = Client::login(&srv, "ala").await;
    assert_eq!(members.len(), 1, "only ola remains besides the new session");
    assert_eq!(
        first.next_msg().await,
        Some(ServerMsg::MemberJoined(MemberInfo { id: ola_id, name: "ola".into(), room: Some(1) }))
    );
    assert_eq!(first.next_msg().await, Some(ServerMsg::SessionReplaced));
    let err = timeout(Duration::from_secs(5), first.conn.closed()).await.unwrap();
    assert!(closed_with(&err, 1), "got {err:?}");
    assert_eq!(watcher.next_msg().await, Some(ServerMsg::MemberLeft { id: first_id }));
    assert_eq!(
        watcher.next_msg().await,
        Some(ServerMsg::MemberJoined(MemberInfo { id: second_id, name: "ala".into(), room: Some(1) }))
    );
}

#[tokio::test]
async fn oversized_frame_disconnects_only_offender() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (ala, _, _) = Client::login(&srv, "ala").await;
    let (ola, _, _) = Client::login(&srv, "ola").await;
    let (mut ewa, _, _) = Client::login(&srv, "ewa").await;
    ewa.send.write_all(&70_000u32.to_le_bytes()).await.unwrap();
    let err = timeout(Duration::from_secs(5), ewa.conn.closed()).await.unwrap();
    assert!(closed_with(&err, 2), "got {err:?}");
    ala.send_audio(1, b"dalej");
    assert!(ola.recv_audio(Duration::from_secs(2)).await.is_some());
}

#[tokio::test]
async fn datagram_flood_is_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (ala, _, _) = Client::login(&srv, "ala").await;
    let (ola, _, _) = Client::login(&srv, "ola").await;
    for seq in 0..500 {
        ala.send_audio(seq, b"x");
    }
    let mut received = 0;
    while ola.recv_audio(Duration::from_millis(500)).await.is_some() {
        received += 1;
    }
    assert!(received > 0 && received <= 120, "received {received}");
    assert!(ala.conn.close_reason().is_none());
}

#[tokio::test]
async fn replaced_session_stops_relaying_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (first, _, _) = Client::login(&srv, "ala").await;
    let (ola, _, _) = Client::login(&srv, "ola").await;
    let (second, _, _) = Client::login(&srv, "ala").await;
    // The old connection lingers up to 2 s to read SessionReplaced; it must not be heard.
    first.send_audio(1, b"stare");
    assert_eq!(ola.recv_audio(Duration::from_millis(300)).await, None);
    assert_eq!(second.recv_audio(Duration::from_millis(100)).await, None);
}

#[tokio::test]
async fn replaced_client_cannot_take_the_account_back() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (_laptop, _, _) = Client::login_session(&srv, "ala", Some(1)).await;
    let (mut pc, _, _) = Client::login_session(&srv, "ala", Some(2)).await;
    // The laptop missed its SessionReplaced (network drop) and now reconnects.
    let mut back = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    back.hello_session("ala", "x", PROTOCOL_VERSION, 1).await;
    assert_eq!(back.next_msg().await, Some(ServerMsg::SessionReplaced));
    assert_eq!(timeout(Duration::from_millis(300), pc.next_msg()).await.ok().flatten(), None);
    assert!(pc.conn.close_reason().is_none(), "the newer session was kicked");
}

#[tokio::test]
async fn reconnecting_client_replaces_its_own_stale_connection() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (_zombie, _, _) = Client::login_session(&srv, "ala", Some(7)).await;
    let (_again, _, _) = Client::login_session(&srv, "ala", Some(7)).await;
    let (_third, _, members) = Client::login_session(&srv, "ala", Some(7)).await;
    assert!(members.is_empty());
}

#[tokio::test]
async fn announce_reaches_others_and_late_joiners() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, ala_id, _) = Client::login(&srv, "ala").await;
    let (mut ola, _, _) = Client::login(&srv, "ola").await;
    assert!(matches!(ala.next_msg().await, Some(ServerMsg::MemberJoined(_))));
    ala.send_msg(&ClientMsg::Announce { blob: b"A".to_vec() }).await;
    assert_eq!(ola.next_msg().await, Some(ServerMsg::Announce { from: ala_id, blob: b"A".to_vec() }));
    let (mut ewa, _, members) = Client::login(&srv, "ewa").await;
    assert_eq!(members.len(), 2);
    assert_eq!(ewa.next_msg().await, Some(ServerMsg::Announce { from: ala_id, blob: b"A".to_vec() }));
    assert!(ewa.quiet(Duration::from_millis(300)).await);
}

#[tokio::test]
async fn relay_goes_only_to_target() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, ala_id, _) = Client::login(&srv, "ala").await;
    let (mut ola, ola_id, _) = Client::login(&srv, "ola").await;
    let (mut ewa, _, _) = Client::login(&srv, "ewa").await;
    // Drain membership events.
    assert!(matches!(ala.next_msg().await, Some(ServerMsg::MemberJoined(_))));
    assert!(matches!(ala.next_msg().await, Some(ServerMsg::MemberJoined(_))));
    assert!(matches!(ola.next_msg().await, Some(ServerMsg::MemberJoined(_))));
    ala.send_msg(&ClientMsg::Relay { to: ola_id, blob: b"x".to_vec() }).await;
    assert_eq!(ola.next_msg().await, Some(ServerMsg::Relay { from: ala_id, blob: b"x".to_vec() }));
    assert!(ewa.quiet(Duration::from_millis(300)).await);
    assert!(ala.quiet(Duration::from_millis(100)).await);
}

#[tokio::test]
async fn relay_to_absent_member_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, ala_id, _) = Client::login(&srv, "ala").await;
    let (mut ola, _, _) = Client::login(&srv, "ola").await;
    assert!(matches!(ala.next_msg().await, Some(ServerMsg::MemberJoined(_))));
    ala.send_msg(&ClientMsg::Relay { to: 999, blob: b"x".to_vec() }).await;
    ala.send_msg(&ClientMsg::Relay { to: ala_id, blob: b"self".to_vec() }).await;
    assert!(ola.quiet(Duration::from_millis(300)).await);
    assert!(ala.quiet(Duration::from_millis(100)).await);
    assert!(ala.conn.close_reason().is_none());
}

#[tokio::test]
async fn oversized_blob_disconnects() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, _, _) = Client::login(&srv, "ala").await;
    ala.send_msg(&ClientMsg::Announce { blob: vec![0; protocol::MAX_BLOB + 1] }).await;
    let err = timeout(Duration::from_secs(5), ala.conn.closed()).await.unwrap();
    assert!(closed_with(&err, 2), "got {err:?}");
}

#[tokio::test]
async fn hello_after_login_is_a_violation() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, _, _) = Client::login(&srv, "ala").await;
    ala.hello("ala", "x", PROTOCOL_VERSION).await;
    let err = timeout(Duration::from_secs(5), ala.conn.closed()).await.unwrap();
    assert!(closed_with(&err, 2), "got {err:?}");
}

#[tokio::test]
async fn control_flood_is_limited() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, _, _) = Client::login(&srv, "ala").await;
    let (mut ola, ola_id, _) = Client::login(&srv, "ola").await;
    assert!(matches!(ala.next_msg().await, Some(ServerMsg::MemberJoined(_))));
    let mut batch = Vec::new();
    for _ in 0..500 {
        batch.extend(encode_frame(&ClientMsg::Relay { to: ola_id, blob: b"x".to_vec() }));
    }
    ala.send.write_all(&batch).await.unwrap();
    let mut received = 0;
    while let Ok(Some(_)) = timeout(Duration::from_millis(500), ola.next_msg()).await {
        received += 1;
    }
    assert!(received > 0 && received <= 160, "received {received}");
    assert!(ala.conn.close_reason().is_none());
}

#[tokio::test]
async fn replaced_session_cannot_relay() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut first, _, _) = Client::login(&srv, "ala").await;
    let (mut ola, ola_id, _) = Client::login(&srv, "ola").await;
    let (_second, _, _) = Client::login(&srv, "ala").await;
    // Drain events ola saw (MemberLeft / MemberJoined for ala's replacement).
    assert!(matches!(ola.next_msg().await, Some(ServerMsg::MemberLeft { .. })));
    assert!(matches!(ola.next_msg().await, Some(ServerMsg::MemberJoined(_))));
    first.send_msg(&ClientMsg::Relay { to: ola_id, blob: b"stare".to_vec() }).await;
    first.send_msg(&ClientMsg::Announce { blob: b"stare".to_vec() }).await;
    assert!(ola.quiet(Duration::from_millis(300)).await);
}
