use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use protocol::audio::{encode_upstream, parse_downstream};
use protocol::{
    encode_frame, ClientMsg, FrameDecoder, MemberId, MemberInfo, RoomChoice, RoomErrorReason,
    RoomId, RoomInfo, ServerMsg, MAX_CHAT_BLOB, PROTOCOL_VERSION,
};
use quinn::{Connection, ConnectionError, Endpoint, RecvStream, SendStream, VarInt};
use server::{RunningServer, ServerConfig, UserEntry};
use tokio::time::timeout;

fn hash_x() -> String {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| server::auth::hash_password("x")).clone()
}

/// ala, ola and ewa; ewa is an admin.
async fn start_server(dir: &tempfile::TempDir) -> RunningServer {
    let users = ["ala", "ola", "ewa"]
        .iter()
        .map(|n| UserEntry { name: n.to_string(), password_hash: hash_x(), admin: *n == "ewa" })
        .collect();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.path().to_owned(),
        users,
        public_address: None,
    };
    server::start(config).await.unwrap()
}

struct Welcome {
    you: MemberId,
    room: Option<RoomId>,
    rooms: Vec<RoomInfo>,
    members: Vec<MemberInfo>,
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

    async fn login(srv: &RunningServer, name: &str, room: RoomChoice) -> (Client, Welcome) {
        static NEXT: AtomicU64 = AtomicU64::new(1_000);
        let mut c = Client::connect(srv.local_addr(), srv.fingerprint()).await;
        c.send_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            username: name.into(),
            password: "x".into(),
            session: NEXT.fetch_add(1, Ordering::Relaxed),
            room,
        })
        .await;
        match c.next_msg().await {
            Some(ServerMsg::Welcome { you, room, rooms, members }) => {
                (c, Welcome { you, room, rooms, members })
            }
            other => panic!("expected Welcome, got {other:?}"),
        }
    }

    async fn send_msg(&mut self, msg: &ClientMsg) {
        self.send.write_all(&encode_frame(msg)).await.unwrap();
    }

    /// Skips messages until one matches.
    async fn expect(&mut self, want: impl Fn(&ServerMsg) -> bool) -> ServerMsg {
        loop {
            match self.next_msg().await {
                Some(m) if want(&m) => return m,
                Some(_) => {}
                None => panic!("stream ended while waiting"),
            }
        }
    }

    /// Nothing matching arrives within `wait`.
    async fn never(&mut self, wait: Duration, bad: impl Fn(&ServerMsg) -> bool) -> bool {
        let fut = async {
            loop {
                match self.next_msg().await {
                    Some(m) if bad(&m) => return false,
                    Some(_) => {}
                    None => return true,
                }
            }
        };
        timeout(wait, fut).await.unwrap_or(true)
    }

    fn send_audio(&self, seq: u32, payload: &[u8]) {
        let mut d = Vec::new();
        encode_upstream(0, seq, payload, &mut d);
        self.conn.send_datagram(d.into()).unwrap();
    }

    async fn recv_audio(&self, wait: Duration) -> Option<MemberId> {
        let d = timeout(wait, self.conn.read_datagram()).await.ok()?.ok()?;
        Some(parse_downstream(&d).unwrap().sender)
    }
}

fn closed_with(err: &ConnectionError, code: u32) -> bool {
    matches!(err, ConnectionError::ApplicationClosed(c) if c.error_code == VarInt::from_u32(code))
}

/// Has ala create "Gra" and returns its id once ala sees the broadcast.
async fn create_gra(ala: &mut Client) -> RoomId {
    ala.send_msg(&ClientMsg::CreateRoom { name: "Gra".into() }).await;
    match ala.expect(|m| matches!(m, ServerMsg::RoomCreated(_))).await {
        ServerMsg::RoomCreated(info) => {
            assert_eq!((info.name.as_str(), info.creator.as_str()), ("Gra", "ala"));
            info.id
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn welcome_lists_rooms_and_places_me_in_default() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (_ala, w) = Client::login(&srv, "ala", RoomChoice::Default).await;
    assert_eq!(w.rooms.len(), 1);
    assert_eq!(w.rooms[0].name, "Ogólny");
    assert_eq!(w.room, Some(w.rooms[0].id));
    assert_eq!(w.members, [MemberInfo { id: w.you, name: "ala".into(), room: w.room }]);
    let (_ola, w2) = Client::login(&srv, "ola", RoomChoice::Nowhere).await;
    assert_eq!(w2.room, None);
    assert_eq!(w2.members.len(), 2);
    assert!(w2.members.contains(&MemberInfo { id: w2.you, name: "ola".into(), room: None }));
}

#[tokio::test]
async fn audio_and_chat_stay_in_room() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, _) = Client::login(&srv, "ala", RoomChoice::Default).await;
    let gra = create_gra(&mut ala).await;
    let (mut ala, a) = Client::login(&srv, "ala", RoomChoice::Room(gra)).await;
    let (mut ola, o) = Client::login(&srv, "ola", RoomChoice::Room(gra)).await;
    let (mut ewa, e) = Client::login(&srv, "ewa", RoomChoice::Default).await;
    assert_eq!((a.room, o.room), (Some(gra), Some(gra)));
    assert_ne!(e.room, Some(gra));
    ala.send_audio(1, b"glos");
    assert_eq!(ola.recv_audio(Duration::from_secs(2)).await, Some(a.you));
    assert_eq!(ewa.recv_audio(Duration::from_millis(300)).await, None);
    ala.send_msg(&ClientMsg::Chat { blob: b"czesc".to_vec() }).await;
    let chat = ola.expect(|m| matches!(m, ServerMsg::Chat { .. })).await;
    assert_eq!(chat, ServerMsg::Chat { from: a.you, blob: b"czesc".to_vec() });
    assert!(ewa.never(Duration::from_millis(300), |m| matches!(m, ServerMsg::Chat { .. })).await);
    assert!(ala.never(Duration::from_millis(100), |m| matches!(m, ServerMsg::Chat { .. })).await);
}

#[tokio::test]
async fn move_is_broadcast_and_announces_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, a) = Client::login(&srv, "ala", RoomChoice::Default).await;
    let gra = create_gra(&mut ala).await;
    let (mut ola, o) = Client::login(&srv, "ola", RoomChoice::Room(gra)).await;
    let (mut ewa, _) = Client::login(&srv, "ewa", RoomChoice::Default).await;
    ola.send_msg(&ClientMsg::Announce { blob: b"O".to_vec() }).await;
    // ola is alone in "Gra": nobody hears her announce.
    assert!(ala.never(Duration::from_millis(300), |m| matches!(m, ServerMsg::Announce { .. })).await);
    ala.send_msg(&ClientMsg::JoinRoom { room: gra }).await;
    let moved = ServerMsg::MemberMoved { id: a.you, room: Some(gra) };
    for c in [&mut ala, &mut ola, &mut ewa] {
        let m = moved.clone();
        c.expect(move |x| *x == m).await;
    }
    assert_eq!(ala.next_msg().await, Some(ServerMsg::Announce { from: o.you, blob: b"O".to_vec() }));
    // Joining the room I am in sends nothing.
    ala.send_msg(&ClientMsg::JoinRoom { room: gra }).await;
    assert!(ala.never(Duration::from_millis(300), |_| true).await);
}

#[tokio::test]
async fn announce_is_cleared_on_move() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, a) = Client::login(&srv, "ala", RoomChoice::Default).await;
    let gra = create_gra(&mut ala).await;
    let (mut ola, o) = Client::login(&srv, "ola", RoomChoice::Default).await;
    ola.send_msg(&ClientMsg::Announce { blob: b"O".to_vec() }).await;
    ala.expect(|m| matches!(m, ServerMsg::Announce { .. })).await;
    ola.send_msg(&ClientMsg::JoinRoom { room: gra }).await;
    ala.expect(|m| matches!(m, ServerMsg::MemberMoved { .. })).await;
    let (mut ewa, _) = Client::login(&srv, "ewa", RoomChoice::Default).await;
    assert!(ewa.never(Duration::from_millis(300), |m| matches!(m, ServerMsg::Announce { .. })).await);
    // Back in the default room, ola's old announce is not replayed either.
    ola.send_msg(&ClientMsg::JoinRoom { room: a.room.unwrap() }).await;
    ola.expect(|m| matches!(m, ServerMsg::MemberMoved { id, .. } if *id == o.you)).await;
    let (mut late, _) = Client::login(&srv, "ala", RoomChoice::Default).await;
    assert!(late.never(Duration::from_millis(300), |m| matches!(m, ServerMsg::Announce { .. })).await);
}

#[tokio::test]
async fn relay_across_rooms_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, _) = Client::login(&srv, "ala", RoomChoice::Default).await;
    let gra = create_gra(&mut ala).await;
    let (mut ola, o) = Client::login(&srv, "ola", RoomChoice::Room(gra)).await;
    let (_nowhere, _) = Client::login(&srv, "ewa", RoomChoice::Nowhere).await;
    ala.send_msg(&ClientMsg::Relay { to: o.you, blob: b"x".to_vec() }).await;
    assert!(ola.never(Duration::from_millis(300), |m| matches!(m, ServerMsg::Relay { .. })).await);
    // Once in the same room, the relay goes through.
    ala.send_msg(&ClientMsg::JoinRoom { room: gra }).await;
    ala.expect(|m| matches!(m, ServerMsg::MemberMoved { .. })).await;
    ala.send_msg(&ClientMsg::Relay { to: o.you, blob: b"y".to_vec() }).await;
    let r = ola.expect(|m| matches!(m, ServerMsg::Relay { .. })).await;
    assert!(matches!(r, ServerMsg::Relay { blob, .. } if blob == b"y"));
}

#[tokio::test]
async fn create_and_delete_rooms() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, _) = Client::login(&srv, "ala", RoomChoice::Default).await;
    let (mut ola, _) = Client::login(&srv, "ola", RoomChoice::Default).await;
    let (mut ewa, _) = Client::login(&srv, "ewa", RoomChoice::Nowhere).await;
    let gra = create_gra(&mut ala).await;
    for c in [&mut ola, &mut ewa] {
        c.expect(|m| matches!(m, ServerMsg::RoomCreated(r) if r.id == gra)).await;
    }
    let is_err = |m: &ServerMsg| matches!(m, ServerMsg::RoomError { .. });
    let err = |reason| ServerMsg::RoomError { reason };

    ola.send_msg(&ClientMsg::CreateRoom { name: "GRA".into() }).await;
    assert_eq!(ola.expect(is_err).await, err(RoomErrorReason::NameTaken));
    ola.send_msg(&ClientMsg::CreateRoom { name: "\u{7}".into() }).await;
    assert_eq!(ola.expect(is_err).await, err(RoomErrorReason::InvalidName));
    ola.send_msg(&ClientMsg::DeleteRoom { room: gra }).await;
    assert_eq!(ola.expect(is_err).await, err(RoomErrorReason::NotAllowed));
    // Errors go to the sender only.
    assert!(ala.never(Duration::from_millis(300), is_err).await);

    ola.send_msg(&ClientMsg::JoinRoom { room: gra }).await;
    ola.expect(|m| matches!(m, ServerMsg::MemberMoved { .. })).await;
    ala.send_msg(&ClientMsg::DeleteRoom { room: gra }).await;
    assert_eq!(ala.expect(is_err).await, err(RoomErrorReason::NotEmpty));
    ola.send_msg(&ClientMsg::LeaveRoom).await;
    ola.expect(|m| matches!(m, ServerMsg::MemberMoved { room: None, .. })).await;

    // ewa is an admin and may delete ala's room.
    ewa.send_msg(&ClientMsg::DeleteRoom { room: gra }).await;
    for c in [&mut ala, &mut ola, &mut ewa] {
        c.expect(|m| *m == ServerMsg::RoomDeleted { room: gra }).await;
    }
    ala.send_msg(&ClientMsg::DeleteRoom { room: gra }).await;
    assert_eq!(ala.expect(is_err).await, err(RoomErrorReason::NotFound));
}

#[tokio::test]
async fn rooms_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, _) = Client::login(&srv, "ala", RoomChoice::Default).await;
    let gra = create_gra(&mut ala).await;
    drop(ala);
    srv.shutdown().await;
    let srv = start_server(&dir).await;
    let (_ala, w) = Client::login(&srv, "ala", RoomChoice::Room(gra)).await;
    assert_eq!(w.room, Some(gra));
    assert_eq!(w.rooms.len(), 2);
}

#[tokio::test]
async fn oversized_chat_disconnects() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, _) = Client::login(&srv, "ala", RoomChoice::Default).await;
    ala.send_msg(&ClientMsg::Chat { blob: vec![0; MAX_CHAT_BLOB + 1] }).await;
    let err = timeout(Duration::from_secs(5), ala.conn.closed()).await.unwrap();
    assert!(closed_with(&err, server::conn::CLOSE_PROTOCOL), "got {err:?}");
}

#[tokio::test]
async fn chat_flood_is_limited() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (mut ala, _) = Client::login(&srv, "ala", RoomChoice::Default).await;
    let (mut ola, _) = Client::login(&srv, "ola", RoomChoice::Default).await;
    ala.expect(|m| matches!(m, ServerMsg::MemberJoined(_))).await;
    let mut batch = Vec::new();
    for _ in 0..100 {
        batch.extend(encode_frame(&ClientMsg::Chat { blob: b"spam".to_vec() }));
    }
    ala.send.write_all(&batch).await.unwrap();
    let mut received = 0u32;
    while let Ok(Some(m)) = timeout(Duration::from_millis(500), ola.next_msg()).await {
        if matches!(m, ServerMsg::Chat { .. }) {
            received += 1;
        }
    }
    assert!(received > 0 && received <= server::conn::CHAT_BURST + 5, "received {received}");
    assert!(ala.conn.close_reason().is_none());
}
