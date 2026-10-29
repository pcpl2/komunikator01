//! Login with passwords, session tokens and invites (protocol v4).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::{encode_frame, Auth, ClientMsg, FrameDecoder, RejectReason, RoomChoice, ServerMsg, PROTOCOL_VERSION};
use quinn::{Connection, ConnectionError, Endpoint, RecvStream, SendStream, VarInt};
use server::store::Store;
use server::{RunningServer, ServerConfig, UserEntry};
use tokio::time::timeout;

fn hash_x() -> String {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| server::auth::hash_password("x")).clone()
}

/// ala and ola from the config, both with password "x".
async fn start_server(dir: &tempfile::TempDir) -> RunningServer {
    let users = ["ala", "ola"]
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

/// A second handle on the server's database, as the admin CLI would open it.
fn store(dir: &tempfile::TempDir) -> Store {
    Store::open(&dir.path().join("komunikator.db")).unwrap()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn invite(dir: &tempfile::TempDir, code: [u8; 16]) -> [u8; 16] {
    store(dir).create_invite(&code, now(), 3600).unwrap();
    code
}

fn pw(password: &str) -> Auth {
    Auth::Password(password.into())
}

fn register(invite: [u8; 16], password: &str) -> Auth {
    Auth::Register { invite, password: password.into() }
}

fn rejected(reason: RejectReason) -> Option<ServerMsg> {
    Some(ServerMsg::Rejected { reason })
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

    async fn hello(&mut self, name: &str, auth: Auth) {
        static NEXT: AtomicU64 = AtomicU64::new(1_000);
        self.hello_as(name, auth, NEXT.fetch_add(1, Ordering::Relaxed)).await;
    }

    /// Hello from client run `session`.
    async fn hello_as(&mut self, name: &str, auth: Auth, session: u64) {
        let msg = ClientMsg::Hello { version: PROTOCOL_VERSION, username: name.into(), auth, session, room: RoomChoice::Default };
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
        timeout(Duration::from_secs(10), fut).await.ok().flatten()
    }
}

/// Connects, says Hello and returns the server's first answer.
async fn attempt(srv: &RunningServer, name: &str, auth: Auth) -> (Client, Option<ServerMsg>) {
    let mut c = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    c.hello(name, auth).await;
    let reply = c.next_msg().await;
    (c, reply)
}

/// Logs in and returns the connection and the session token from `Welcome`.
async fn login(srv: &RunningServer, name: &str, auth: Auth) -> (Client, [u8; 32]) {
    match attempt(srv, name, auth).await {
        (c, Some(ServerMsg::Welcome { token, .. })) => (c, token),
        (_, other) => panic!("expected Welcome, got {other:?}"),
    }
}

fn closed_with(err: &ConnectionError, code: u32) -> bool {
    matches!(err, ConnectionError::ApplicationClosed(c) if c.error_code == VarInt::from_u32(code))
}

#[tokio::test]
async fn password_login_returns_token() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (_a, first) = login(&srv, "ala", pw("x")).await;
    let (_b, second) = login(&srv, "ala", pw("x")).await;
    assert_ne!(first, [0; 32]);
    assert_ne!(first, second, "every login gets a fresh token");
}

#[tokio::test]
async fn token_login_works_once_and_rotates() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (_a, token) = login(&srv, "ala", pw("x")).await;
    // Names are case-insensitive, and the server knows ala by her account's spelling.
    let (_b, rotated) = match attempt(&srv, "ALA", Auth::Token(token)).await {
        (c, Some(ServerMsg::Welcome { you, members, token, .. })) => {
            assert!(members.iter().any(|m| m.id == you && m.name == "ala"));
            (c, token)
        }
        (_, other) => panic!("expected Welcome, got {other:?}"),
    };
    assert_ne!(rotated, token);
    // A client whose Welcome got lost retries with the same token within the grace period.
    let (_b2, retried) = login(&srv, "ala", Auth::Token(token)).await;
    // Once a newer token is used, the old one is dead.
    let (_c, _) = login(&srv, "ala", Auth::Token(retried)).await;
    let (_, reply) = attempt(&srv, "ala", Auth::Token(token)).await;
    assert_eq!(reply, rejected(RejectReason::TokenInvalid));
}

#[tokio::test]
async fn replaced_client_run_does_not_burn_its_token() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let mut first = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    first.hello_as("ala", pw("x"), 1).await;
    let Some(ServerMsg::Welcome { token, .. }) = first.next_msg().await else { panic!("no welcome") };
    // Another run logs in; the first is replaced and retired.
    let (_second, second_token) = login(&srv, "ala", pw("x")).await;
    assert_eq!(first.next_msg().await, Some(ServerMsg::SessionReplaced));
    // The retired run reconnects with its token (it missed the notice): refused...
    let mut back = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    back.hello_as("ALA", Auth::Token(token), 1).await;
    assert_eq!(back.next_msg().await, Some(ServerMsg::SessionReplaced));
    // ...and its token is untouched (not even spent: using a newer token would end a spent
    // one's grace period), so the user can still restart without the password.
    let (_third, _) = login(&srv, "ala", Auth::Token(second_token)).await;
    let mut fresh = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    fresh.hello_as("ala", Auth::Token(token), 3).await;
    assert!(matches!(fresh.next_msg().await, Some(ServerMsg::Welcome { .. })));
}

#[tokio::test]
async fn token_of_another_user_dies_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (_a, token) = login(&srv, "ala", pw("x")).await;
    let (_, reply) = attempt(&srv, "ola", Auth::Token(token)).await;
    assert_eq!(reply, rejected(RejectReason::TokenInvalid));
    // No grace period for a token shown under the wrong name.
    let (_, reply) = attempt(&srv, "ala", Auth::Token(token)).await;
    assert_eq!(reply, rejected(RejectReason::TokenInvalid));
}

#[tokio::test]
async fn token_for_other_user_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (_a, token) = login(&srv, "ala", pw("x")).await;
    let (_, reply) = attempt(&srv, "ola", Auth::Token(token)).await;
    assert_eq!(reply, rejected(RejectReason::TokenInvalid));
    let (_, reply) = attempt(&srv, "ala", Auth::Token([5; 32])).await;
    assert_eq!(reply, rejected(RejectReason::TokenInvalid));
}

#[tokio::test]
async fn register_with_invite_creates_account() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let code = invite(&dir, [1; 16]);
    let (c, token) = match attempt(&srv, " Żaneta ", register(code, "długie hasło")).await {
        (c, Some(ServerMsg::Welcome { you, members, token, .. })) => {
            assert!(members.iter().any(|m| m.id == you && m.name == "Żaneta"));
            (c, token)
        }
        (_, other) => panic!("expected Welcome, got {other:?}"),
    };
    drop(c);
    let account = store(&dir).account("żaneta").unwrap().expect("account created");
    assert_eq!(account.name, "Żaneta");
    assert!(!account.admin && !account.blocked);
    assert!(server::auth::verify_password(&account.hash, "długie hasło"));
    assert_eq!(store(&dir).invites().unwrap()[0].used_by.as_deref(), Some("Żaneta"));
    let (_t, _) = login(&srv, "Żaneta", Auth::Token(token)).await;
    let (_p, _) = login(&srv, "żaneta", pw("długie hasło")).await;
    let (_, reply) = attempt(&srv, "Ewa", register(code, "długie hasło")).await;
    assert_eq!(reply, rejected(RejectReason::InviteInvalid));
}

#[tokio::test]
async fn invite_is_single_use_under_race() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let code = invite(&dir, [2; 16]);
    let mut a = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    let mut b = Client::connect(srv.local_addr(), srv.fingerprint()).await;
    a.hello("Ewa", register(code, "hasło-ewy-1")).await;
    b.hello("Iza", register(code, "hasło-izy-1")).await;
    let (ra, rb) = tokio::join!(a.next_msg(), b.next_msg());
    let welcomes = [&ra, &rb].iter().filter(|r| matches!(r, Some(ServerMsg::Welcome { .. }))).count();
    let invalid = [&ra, &rb].iter().filter(|r| **r == &rejected(RejectReason::InviteInvalid)).count();
    assert_eq!((welcomes, invalid), (1, 1), "got {ra:?} and {rb:?}");
    assert_eq!(store(&dir).accounts().unwrap().len(), 3);
}

#[tokio::test]
async fn register_rejects_bad_name_and_short_password() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let code = invite(&dir, [3; 16]);
    let (_, reply) = attempt(&srv, "zła nazwa", register(code, "długie hasło")).await;
    assert_eq!(reply, rejected(RejectReason::NameInvalid));
    // Seven characters, more than seven bytes.
    let (_, reply) = attempt(&srv, "Ewa", register(code, "żółwiak")).await;
    assert_eq!(reply, rejected(RejectReason::PasswordTooShort));
    let (_, reply) = attempt(&srv, "ALA", register(code, "długie hasło")).await;
    assert_eq!(reply, rejected(RejectReason::NameTaken));
    // None of the failures used up the invite.
    let (_c, _) = login(&srv, "Ewa", register(code, "żółwiaki")).await;
}

#[tokio::test]
async fn blocked_account_is_rejected_after_correct_password() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    assert!(store(&dir).set_blocked("ola", true).unwrap());
    let (_, reply) = attempt(&srv, "ola", pw("zle")).await;
    assert_eq!(reply, rejected(RejectReason::BadCredentials));
    let (_, reply) = attempt(&srv, "ola", pw("x")).await;
    assert_eq!(reply, rejected(RejectReason::AccountBlocked));
}

#[tokio::test]
async fn blocked_online_user_is_kicked() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (ala, _) = login(&srv, "ala", pw("x")).await;
    let (ola, _) = login(&srv, "ola", pw("x")).await;
    assert!(store(&dir).set_blocked("ala", true).unwrap());
    let err = timeout(Duration::from_secs(10), ala.conn.closed()).await.expect("ala still online");
    assert!(closed_with(&err, protocol::CLOSE_BLOCKED), "got {err:?}");
    assert!(ola.conn.close_reason().is_none());
}

#[tokio::test]
async fn register_attempts_are_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    for i in 0..5 {
        let (_, reply) = attempt(&srv, &format!("gracz{i}"), register([0xEE; 16], "długie hasło")).await;
        assert_eq!(reply, rejected(RejectReason::InviteInvalid));
    }
    let code = invite(&dir, [4; 16]);
    let (_, reply) = attempt(&srv, "Ewa", register(code, "długie hasło")).await;
    assert_eq!(reply, rejected(RejectReason::RateLimited));
}
