//! A server killed without closing connections must not leave clients hanging for the
//! idle timeout: the restarted server's stateless resets have to end old connections.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use protocol::audio::encode_upstream;
use protocol::{encode_frame, ClientMsg, RoomChoice, PROTOCOL_VERSION};
use quinn::Endpoint;
use tokio::time::timeout;

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Starts the real binary and returns it with the fingerprint it prints.
fn spawn_server(config: &std::path::Path) -> (Child, [u8; 32]) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_komunikator-server"))
        .args(["run", "--config"])
        .arg(config)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    while let Some(Ok(line)) = lines.next() {
        if let Some(hex) = line.strip_prefix("fingerprint: ") {
            // Keep draining stdout so the server's logging never hits a closed pipe.
            std::thread::spawn(move || lines.for_each(drop));
            return (child, protocol::parse_fingerprint(hex).unwrap());
        }
    }
    panic!("server exited before printing its fingerprint");
}

#[tokio::test]
async fn killed_server_restart_ends_old_connections_quickly() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config = dir.path().join("server.toml");
    let hash = server::auth::hash_password("x");
    let data_dir = dir.path().join("data").display().to_string().replace('\\', "/");
    std::fs::write(
        &config,
        format!(
            "bind = \"127.0.0.1:{port}\"\ndata_dir = \"{data_dir}\"\n[[users]]\nname = \"ala\"\npassword_hash = \"{hash}\"\n"
        ),
    )
    .unwrap();

    let (mut first, fp) = spawn_server(&config);
    let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    endpoint.set_default_client_config(client_core::net::client_config(fp).unwrap());
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let conn = endpoint.connect(addr, client_core::net::SERVER_NAME).unwrap().await.unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    let hello = ClientMsg::Hello { version: PROTOCOL_VERSION, username: "ala".into(), password: "x".into(), session: 1, room: RoomChoice::Default };
    send.write_all(&encode_frame(&hello)).await.unwrap();

    first.kill().unwrap();
    first.wait().unwrap();
    let (mut second, fp2) = spawn_server(&config);
    assert_eq!(fp, fp2);

    let mut d = Vec::new();
    encode_upstream(0, 1, b"x", &mut d);
    let _ = conn.send_datagram(d.into());
    let closed = timeout(Duration::from_secs(3), conn.closed()).await;
    second.kill().unwrap();
    assert!(closed.is_ok(), "old connection still open 3 s after the server came back");
}
