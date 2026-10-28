//! Voice relay server: authenticates clients and forwards their audio datagrams.

pub mod auth;
pub mod config;
pub mod conn;
pub mod room;
pub mod tls;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::crypto::ring;
use rustls::pki_types::PrivateKeyDer;

pub use config::{ServerConfig, UserEntry};
pub use conn::TokenBucket;

const LOGIN_MAX_FAILURES: u32 = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(60);

pub struct RunningServer {
    endpoint: quinn::Endpoint,
    fingerprint: [u8; 32],
    accept: tokio::task::JoinHandle<()>,
}

impl RunningServer {
    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint.local_addr().expect("bound endpoint has an address")
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    pub async fn shutdown(self) {
        self.endpoint.close(0u32.into(), b"shutdown");
        self.accept.abort();
        let _ = self.accept.await;
        self.endpoint.wait_idle().await;
    }
}

pub async fn start(config: ServerConfig) -> anyhow::Result<RunningServer> {
    let identity = tls::load_or_create_identity(&config.data_dir)?;
    let endpoint = quinn::Endpoint::server(quic_config(&identity)?, config.bind)?;
    let shared = Arc::new(conn::Shared {
        room: Mutex::new(room::Room::new()),
        limiter: Mutex::new(auth::LoginLimiter::new(LOGIN_MAX_FAILURES, LOGIN_WINDOW)),
        users: config.users.into_iter().map(|u| (u.name, u.password_hash)).collect(),
    });
    let accept_endpoint = endpoint.clone();
    let accept = tokio::spawn(async move {
        while let Some(incoming) = accept_endpoint.accept().await {
            tokio::spawn(conn::handle(incoming, shared.clone()));
        }
    });
    Ok(RunningServer { endpoint, fingerprint: identity.fingerprint, accept })
}

fn quic_config(identity: &tls::ServerIdentity) -> anyhow::Result<quinn::ServerConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(
            vec![identity.cert.clone()],
            PrivateKeyDer::Pkcs8(identity.key.clone_key()),
        )?;
    tls.alpn_protocols = vec![protocol::ALPN.to_vec()];
    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_millis(protocol::KEEP_ALIVE_MS)));
    transport.max_idle_timeout(Some(Duration::from_millis(protocol::IDLE_TIMEOUT_MS).try_into()?));
    transport.datagram_receive_buffer_size(Some(64 * 1024));
    config.transport_config(Arc::new(transport));
    Ok(config)
}
