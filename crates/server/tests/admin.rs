//! The admin CLI (in-process) against a live server sharing its data directory.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use protocol::{
    decode_invite, encode_frame, Auth, ClientMsg, FrameDecoder, MemberId, RejectReason, RoomChoice,
    RoomId, ServerMsg, PROTOCOL_VERSION,
};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use server::{admin, RunningServer, ServerConfig, UserEntry};
use tokio::time::timeout;

fn hash_x() -> String {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| server::auth::hash_password("x")).clone()
}

fn config(dir: &tempfile::TempDir, public_address: Option<&str>) -> ServerConfig {
    let users = ["ala", "ola"]
        .iter()
        .map(|n| UserEntry { name: n.to_string(), password_hash: hash_x(), admin: false })
        .collect();
    ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.path().to_owned(),
        users,
        public_address: public_address.map(str::to_owned),
    }
}

struct Client {
    _conn: Connection,
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
        Client { _conn: conn, send, recv, dec: FrameDecoder::new(), _endpoint: endpoint }
    }

    async fn hello(&mut self, name: &str, auth: Auth) {
        static NEXT: AtomicU64 = AtomicU64::new(1_000);
        let msg = ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            username: name.into(),
            auth,
            session: NEXT.fetch_add(1, Ordering::Relaxed),
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
        // The server notices changes made from the shell on a 5 s timer.
        timeout(Duration::from_secs(12), fut).await.ok().flatten()
    }

    async fn expect(&mut self, want: impl Fn(&ServerMsg) -> bool) -> ServerMsg {
        loop {
            match self.next_msg().await {
                Some(m) if want(&m) => return m,
                Some(_) => {}
                None => panic!("stream ended while waiting"),
            }
        }
    }

    async fn send_msg(&mut self, msg: &ClientMsg) {
        self.send.write_all(&encode_frame(msg)).await.unwrap();
    }
}

async fn login(srv: &RunningServer, name: &str, auth: Auth) -> (Client, ServerMsg) {
    let mut c = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    c.hello(name, auth).await;
    let reply = c.next_msg().await.expect("server answered");
    (c, reply)
}

fn password(p: &str) -> Auth {
    Auth::Password(p.into())
}

fn invite_line(text: &str) -> &str {
    text.lines().find(|l| l.starts_with("kom1-")).expect("invite string in the output")
}

#[tokio::test]
async fn cli_invite_then_register() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, None)).await.unwrap();
    let addr = srv.local_addr().to_string();
    let cfg = config(&dir, None);

    // No address anywhere: a Polish explanation of what to set.
    let err = admin::invite(&cfg, 7, None).unwrap_err().to_string();
    assert!(err.contains("public_address") && err.contains("--server"), "{err}");
    let err = admin::invite(&cfg, 7, Some("bez-portu")).unwrap_err().to_string();
    assert!(err.contains("host:port"), "{err}");

    let text = admin::invite(&cfg, 7, Some(&addr)).unwrap();
    assert!(text.contains("Mam zaproszenie"), "{text}");
    let invite = decode_invite(invite_line(&text)).unwrap();
    assert_eq!(invite.server, addr);
    assert_eq!(invite.fingerprint, srv.fingerprint());

    // public_address is used when --server is absent.
    let cfg_pub = config(&dir, Some("gra.example.pl:7700"));
    let text2 = admin::invite(&cfg_pub, 1, None).unwrap();
    assert_eq!(decode_invite(invite_line(&text2)).unwrap().server, "gra.example.pl:7700");

    let register = Auth::Register { invite: invite.code, password: "długie hasło".into() };
    let (_c, reply) = login(&srv, "Żaneta", register.clone()).await;
    assert!(matches!(reply, ServerMsg::Welcome { .. }), "{reply:?}");
    let (_c2, reply) = login(&srv, "Ewa", register).await;
    assert_eq!(reply, ServerMsg::Rejected { reason: RejectReason::InviteInvalid });

    let invites = admin::invites(&cfg).unwrap();
    assert!(invites.contains("Żaneta"), "{invites}");
    assert!(!invites.contains(&invite_line(&text)[5..9]), "codes must not be listed");
    let users = admin::users(&cfg).unwrap();
    assert!(users.contains("Żaneta") && users.contains("ala") && users.contains("ola"), "{users}");
    srv.shutdown().await;
}

#[tokio::test]
async fn cli_manages_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let srv = server::start(config(&dir, None)).await.unwrap();
    let cfg = config(&dir, None);

    admin::make_admin(&cfg, "ALA").unwrap();
    assert!(admin::users(&cfg).unwrap().lines().any(|l| l.contains("ala") && l.contains("tak")));
    admin::revoke_admin(&cfg, "ala").unwrap();
    admin::block(&cfg, "ola").unwrap();
    let (_c, reply) = login(&srv, "ola", password("x")).await;
    assert_eq!(reply, ServerMsg::Rejected { reason: RejectReason::AccountBlocked });
    admin::unblock(&cfg, "ola").unwrap();
    let (_c, reply) = login(&srv, "ola", password("x")).await;
    assert!(matches!(reply, ServerMsg::Welcome { .. }), "{reply:?}");

    for f in [admin::block, admin::unblock, admin::make_admin, admin::revoke_admin] {
        assert!(f(&cfg, "nikt").unwrap_err().to_string().contains("nikt"));
    }

    // Password reset: too short is refused; a good one revokes tokens.
    let (c, welcome) = login(&srv, "ala", password("x")).await;
    let ServerMsg::Welcome { token, .. } = welcome else { panic!("no welcome") };
    drop(c);
    assert!(admin::reset_password(&cfg, "ala", "krótkie").is_err());
    admin::reset_password(&cfg, "ala", "nowe-dlugie-haslo").unwrap();
    let (_c, reply) = login(&srv, "ala", Auth::Token(token)).await;
    assert_eq!(reply, ServerMsg::Rejected { reason: RejectReason::TokenInvalid });
    let (_c, reply) = login(&srv, "ala", password("nowe-dlugie-haslo")).await;
    assert!(matches!(reply, ServerMsg::Welcome { .. }), "{reply:?}");
    assert!(admin::reset_password(&cfg, "nikt", "nowe-dlugie-haslo").is_err());
    srv.shutdown().await;
}

#[tokio::test]
async fn cli_manages_rooms() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir, None);
    server::start(cfg.clone()).await.unwrap().shutdown().await; // creates the database
    assert!(admin::rooms(&cfg).unwrap().contains("Ogólny"));
    admin::create_room(&cfg, "  Gra  ").unwrap();
    let gra = room_id(&admin::rooms(&cfg).unwrap(), "Gra");
    assert!(admin::create_room(&cfg, "gra").is_err(), "names are unique ignoring case");
    assert!(admin::create_room(&cfg, " ").is_err());
    assert!(admin::delete_room(&cfg, 999).is_err());
    admin::delete_room(&cfg, gra).unwrap();
    assert!(!admin::rooms(&cfg).unwrap().contains("Gra"));
}

/// The id column of the line listing `name`.
fn room_id(text: &str, name: &str) -> RoomId {
    text.lines()
        .find(|l| l.contains(name))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("room {name} not listed in {text}"))
}

#[tokio::test]
async fn room_created_by_cli_is_broadcast() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir, None);
    let srv = server::start(cfg.clone()).await.unwrap();
    let (mut ala, _) = login(&srv, "ala", password("x")).await;
    admin::create_room(&cfg, "Gra").unwrap();
    let ServerMsg::RoomCreated(info) = ala.expect(|m| matches!(m, ServerMsg::RoomCreated(_))).await
    else {
        unreachable!()
    };
    assert_eq!(info.name, "Gra");
    assert_eq!(info.id, room_id(&admin::rooms(&cfg).unwrap(), "Gra"));
    // It can be joined like any other room.
    ala.send_msg(&ClientMsg::JoinRoom { room: info.id }).await;
    ala.expect(|m| matches!(m, ServerMsg::MemberMoved { room: Some(r), .. } if *r == info.id)).await;
    srv.shutdown().await;
}

#[tokio::test]
async fn room_deleted_by_cli_is_broadcast() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir, None);
    let srv = server::start(cfg.clone()).await.unwrap();
    admin::create_room(&cfg, "Gra").unwrap();
    let gra = room_id(&admin::rooms(&cfg).unwrap(), "Gra");
    let (mut ala, welcome) = login(&srv, "ala", password("x")).await;
    let ServerMsg::Welcome { you: ala_id, rooms, .. } = welcome else { panic!("no welcome") };
    // The server may pick the room up from the database only after ala logged in.
    if !rooms.iter().any(|r| r.id == gra) {
        ala.expect(|m| matches!(m, ServerMsg::RoomCreated(r) if r.id == gra)).await;
    }
    let (mut ola, _) = login(&srv, "ola", password("x")).await;
    ala.send_msg(&ClientMsg::JoinRoom { room: gra }).await;
    let joined = |m: &ServerMsg| matches!(m, ServerMsg::MemberMoved { room: Some(r), .. } if *r == gra);
    ala.expect(joined).await;
    ola.expect(joined).await;

    admin::delete_room(&cfg, gra).unwrap();
    let moved = |id: MemberId| {
        move |m: &ServerMsg| matches!(m, ServerMsg::MemberMoved { id: i, room: None } if *i == id)
    };
    let deleted = |m: &ServerMsg| matches!(m, ServerMsg::RoomDeleted { room } if *room == gra);
    for c in [&mut ala, &mut ola] {
        c.expect(moved(ala_id)).await;
        c.expect(deleted).await;
    }
    srv.shutdown().await;
}

#[test]
fn admin_commands_refuse_a_data_dir_the_server_never_used() {
    let dir = tempfile::tempdir().unwrap();
    let missing = tempfile::TempDir::new().unwrap();
    let mut cfg = config(&missing, Some("gra.example.pl:7700"));
    cfg.data_dir = missing.path().join("pomyłka");
    let calls: [(&str, Box<dyn Fn(&ServerConfig) -> anyhow::Result<String>>); 4] = [
        ("users", Box::new(admin::users)),
        ("invite", Box::new(|c| admin::invite(c, 7, None))),
        ("block", Box::new(|c| admin::block(c, "ala"))),
        ("create-room", Box::new(|c| admin::create_room(c, "Gra"))),
    ];
    for (what, call) in &calls {
        let err = call(&cfg).unwrap_err().to_string();
        assert!(err.contains(&cfg.data_dir.display().to_string()) && err.contains("cert.der"), "{what}: {err}");
    }
    assert!(!cfg.data_dir.exists(), "an admin command created the data directory");

    // The certificate alone is not enough either: the database is missing.
    cfg.data_dir = dir.path().to_owned();
    server::tls::load_or_create_identity(&cfg.data_dir).unwrap();
    let err = admin::users(&cfg).unwrap_err().to_string();
    assert!(err.contains("komunikator.db"), "{err}");
    assert!(!dir.path().join("komunikator.db").exists());
}
