use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use protocol::update::{sign_manifest, Manifest, Version, MAX_RELEASE_SIZE, UPDATE_ALPN};
use protocol::{encode_frame, Auth, ClientMsg, FrameDecoder, RoomChoice, ServerMsg, PROTOCOL_VERSION};
use quinn::{Connection, Endpoint};
use server::{RunningServer, ServerConfig, UserEntry};
use tokio::time::timeout;

async fn start_server(dir: &tempfile::TempDir) -> RunningServer {
    let users =
        vec![UserEntry { name: "ala".into(), password_hash: server::auth::hash_password("x"), admin: false }];
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.path().to_owned(),
        users,
        public_address: None,
    };
    server::start(config).await.unwrap()
}

async fn update_conn(addr: SocketAddr, fp: [u8; 32]) -> (Endpoint, Connection) {
    let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(client_core::net::client_config_with_alpn(fp, UPDATE_ALPN).unwrap());
    let conn = endpoint.connect(addr, client_core::net::SERVER_NAME).unwrap().await.unwrap();
    (endpoint, conn)
}

/// One request on a fresh stream; `None` when the server refused or broke the connection.
async fn request(conn: &Connection, req: &[u8]) -> Option<Vec<u8>> {
    let fut = async {
        let (mut send, mut recv) = conn.open_bi().await.ok()?;
        send.write_all(req).await.ok()?;
        send.finish().ok()?;
        recv.read_to_end(MAX_RELEASE_SIZE as usize + 16).await.ok()
    };
    timeout(Duration::from_secs(10), fut).await.ok().flatten()
}

async fn fetch(srv: &RunningServer, req: &[u8]) -> Vec<u8> {
    let (_endpoint, conn) = update_conn(srv.local_addr(), srv.fingerprint()).await;
    request(&conn, req).await.expect("server answers")
}

fn file_request(name: &str) -> Vec<u8> {
    let mut r = vec![2, name.len() as u8];
    r.extend_from_slice(name.as_bytes());
    r
}

fn publish(data_dir: &Path, file: &str) -> Vec<u8> {
    let key = SigningKey::from_bytes(&[3; 32]);
    let m = Manifest { version: Version { major: 1, minor: 0, patch: 1 }, file: file.into(), size: 1, sha256: [0; 32] };
    let bytes = sign_manifest(&key, &m);
    std::fs::create_dir_all(data_dir.join("updates")).unwrap();
    std::fs::write(data_dir.join("updates").join("manifest.bin"), &bytes).unwrap();
    bytes
}

const EMPTY_FILE: [u8; 8] = [0; 8];

#[tokio::test]
async fn no_release_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    assert_eq!(fetch(&srv, &[1]).await, [0, 0, 0, 0]);
    assert_eq!(fetch(&srv, &file_request("a.exe")).await, EMPTY_FILE);
    srv.shutdown().await;
}

#[tokio::test]
async fn serves_manifest_and_named_file() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let manifest = publish(dir.path(), "komunikator-1.0.1.exe");
    // Several chunks, not a multiple of any power of two.
    let content: Vec<u8> = (0..300_001u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(dir.path().join("updates").join("komunikator-1.0.1.exe"), &content).unwrap();

    let got = fetch(&srv, &[1]).await;
    assert_eq!(got[..4], (manifest.len() as u32).to_le_bytes());
    assert_eq!(got[4..], manifest[..]);

    let got = fetch(&srv, &file_request("komunikator-1.0.1.exe")).await;
    assert_eq!(got[..8], (content.len() as u64).to_le_bytes());
    assert!(got[8..] == content[..], "file content differs");
    srv.shutdown().await;
}

#[tokio::test]
async fn file_name_must_be_plain() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let name = r"..\x";
    publish(dir.path(), name);
    std::fs::write(dir.path().join("x"), b"secret").unwrap();
    std::fs::write(dir.path().join("updates").join(name), b"secret").unwrap();
    assert_eq!(fetch(&srv, &file_request(name)).await, EMPTY_FILE);
    srv.shutdown().await;
}

#[tokio::test]
async fn other_names_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    publish(dir.path(), "a.exe");
    let updates = dir.path().join("updates");
    std::fs::write(updates.join("a.exe"), b"a").unwrap();
    std::fs::write(updates.join("b.exe"), b"b").unwrap();
    assert_eq!(fetch(&srv, &file_request("b.exe")).await, EMPTY_FILE);
    assert_eq!(fetch(&srv, &file_request("manifest.bin")).await, EMPTY_FILE);
    assert_eq!(fetch(&srv, &file_request("A.exe")).await, EMPTY_FILE);
    let mut ok = 1u64.to_le_bytes().to_vec();
    ok.push(b'a');
    assert_eq!(fetch(&srv, &file_request("a.exe")).await, ok);
    srv.shutdown().await;
}

#[tokio::test]
async fn bad_requests_get_no_answer() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    for req in [&[][..], &[3], &[1, 0], &[2, 5, b'a'], &[2, 1, b'a', b'b']] {
        let (_endpoint, conn) = update_conn(srv.local_addr(), srv.fingerprint()).await;
        assert_eq!(request(&conn, req).await, None, "{req:?}");
    }
    srv.shutdown().await;
}

#[tokio::test]
async fn one_request_per_connection() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (_endpoint, conn) = update_conn(srv.local_addr(), srv.fingerprint()).await;
    assert_eq!(request(&conn, &[1]).await.unwrap(), [0, 0, 0, 0]);
    assert_eq!(request(&conn, &[1]).await, None);
    srv.shutdown().await;
}

#[tokio::test]
async fn main_alpn_still_logs_in() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(client_core::net::client_config(srv.fingerprint()).unwrap());
    let conn = endpoint.connect(srv.local_addr(), client_core::net::SERVER_NAME).unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        username: "ala".into(),
        auth: Auth::Password("x".into()),
        session: 1,
        room: RoomChoice::Default,
    };
    send.write_all(&encode_frame(&hello)).await.unwrap();
    let mut dec = FrameDecoder::new();
    let msg = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(m) = dec.next::<ServerMsg>().unwrap() {
                return m;
            }
            let mut buf = [0u8; 4096];
            let n = recv.read(&mut buf).await.unwrap().unwrap();
            dec.push(&buf[..n]);
        }
    })
    .await
    .unwrap();
    assert!(matches!(msg, ServerMsg::Welcome { .. }), "{msg:?}");
    srv.shutdown().await;
}

#[tokio::test]
async fn update_connections_are_limited() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (addr, fp) = (srv.local_addr(), srv.fingerprint());
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(update_conn(addr, fp).await);
    }
    // Let the server take a slot for each before the next one arrives.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_e, extra) = update_conn(addr, fp).await;
    assert_eq!(request(&extra, &[1]).await, None, "fifth connection from one IP must be closed");

    let (_e, first) = held.remove(0);
    first.close(0u32.into(), b"done");
    let mut served = false;
    for _ in 0..50 {
        let (_e, conn) = update_conn(addr, fp).await;
        if request(&conn, &[1]).await.is_some() {
            served = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(served, "a freed slot must be reusable");
    srv.shutdown().await;
}

#[tokio::test]
async fn forward_slash_names_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    publish(dir.path(), "../x");
    std::fs::write(dir.path().join("x"), b"secret").unwrap();
    assert_eq!(fetch(&srv, &file_request("../x")).await, EMPTY_FILE);
    srv.shutdown().await;
}

#[tokio::test]
async fn oversized_manifest_is_not_served() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let mut big = publish(dir.path(), "a.exe");
    big.resize(4097, 0);
    std::fs::write(dir.path().join("updates").join("manifest.bin"), &big).unwrap();
    std::fs::write(dir.path().join("updates").join("a.exe"), b"a").unwrap();
    assert_eq!(fetch(&srv, &[1]).await, [0, 0, 0, 0]);
    assert_eq!(fetch(&srv, &file_request("a.exe")).await, EMPTY_FILE);
    srv.shutdown().await;
}

#[tokio::test]
async fn overlong_request_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    let (_endpoint, conn) = update_conn(srv.local_addr(), srv.fingerprint()).await;
    let mut req = file_request(&"a".repeat(255));
    req.push(0);
    assert_eq!(req.len(), 258);
    assert_eq!(request(&conn, &req).await, None);
    srv.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_release_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    publish(dir.path(), "a.exe");
    std::fs::write(dir.path().join("secret"), b"secret").unwrap();
    std::os::unix::fs::symlink(dir.path().join("secret"), dir.path().join("updates").join("a.exe")).unwrap();
    assert_eq!(fetch(&srv, &file_request("a.exe")).await, EMPTY_FILE);
    srv.shutdown().await;
}

/// A client that never lets the server send (stream window 0) must not keep its slot.
#[tokio::test]
async fn stalled_reader_releases_its_slot() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start_server(&dir).await;
    publish(dir.path(), "a.exe");
    let (addr, fp) = (srv.local_addr(), srv.fingerprint());
    let mut stalled = Vec::new();
    for _ in 0..4 {
        let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut cfg = client_core::net::client_config_with_alpn(fp, UPDATE_ALPN).unwrap();
        let mut transport = quinn::TransportConfig::default();
        transport.stream_receive_window(quinn::VarInt::from_u32(0));
        transport.keep_alive_interval(Some(Duration::from_millis(500)));
        cfg.transport_config(std::sync::Arc::new(transport));
        endpoint.set_default_client_config(cfg);
        let conn = endpoint.connect(addr, client_core::net::SERVER_NAME).unwrap().await.unwrap();
        let (mut send, recv) = conn.open_bi().await.unwrap();
        send.write_all(&[1]).await.unwrap();
        send.finish().unwrap();
        stalled.push((endpoint, conn, send, recv));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_e, extra) = update_conn(addr, fp).await;
    assert_eq!(request(&extra, &[1]).await, None, "slots are taken by the stalled readers");

    let start = std::time::Instant::now();
    let served = loop {
        let (_e, conn) = update_conn(addr, fp).await;
        if request(&conn, &[1]).await.is_some() {
            break true;
        }
        if start.elapsed() > Duration::from_secs(20) {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(served, "stalled readers must give their slots back");
    drop(stalled);
    srv.shutdown().await;
}
