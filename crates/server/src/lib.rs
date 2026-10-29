//! Voice relay server: authenticates clients and forwards their audio datagrams.

pub mod admin;
pub mod auth;
pub mod config;
pub mod conn;
pub mod room;
pub mod store;
pub mod tls;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustls::crypto::ring;
use rustls::pki_types::PrivateKeyDer;

pub use config::{ServerConfig, UserEntry};
pub use conn::TokenBucket;

const LOGIN_MAX_FAILURES: u32 = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const BIND_RETRIES: u32 = 30;
const CONCURRENT_PASSWORD_CHECKS: usize = 4;
const BIND_RETRY_DELAY: Duration = Duration::from_millis(100);

pub struct RunningServer {
    endpoint: quinn::Endpoint,
    fingerprint: [u8; 32],
    accept: tokio::task::JoinHandle<()>,
    watcher: tokio::task::JoinHandle<()>,
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
        self.watcher.abort();
        let _ = self.accept.await;
        let _ = self.watcher.await;
        self.endpoint.wait_idle().await;
    }
}

pub async fn start(config: ServerConfig) -> anyhow::Result<RunningServer> {
    let identity = tls::load_or_create_identity(&config.data_dir)?;
    let reset_key = tls::load_or_create_reset_key(&config.data_dir)?;
    let endpoint = bind_endpoint(quic_config(&identity)?, &reset_key, config.bind).await?;
    let dummy_hash =
        tokio::task::spawn_blocking(|| auth::hash_password("dummy-password-for-timing")).await?;
    let store = store::Store::open(&config.data_dir.join("komunikator.db"))?;
    let report = store.import_config_users(&config.users, unix_now())?;
    if report.imported > 0 {
        tracing::info!(imported = report.imported, "accounts imported from the config");
    }
    // The database is the source of truth once an account exists; config edits are ignored.
    for name in &report.differing {
        tracing::warn!(
            user = ?name,
            "[[users]] entry differs from the account database (password hash or admin flag) and is \
             ignored; change the account with `komunikator-server reset-password`, `make-admin`, \
             `revoke-admin` or `block`, then remove the entry from the config"
        );
    }
    for name in &report.collisions {
        tracing::warn!(
            user = ?name,
            "[[users]] entry skipped: an account with the same name in other letter case exists \
             (names are case-insensitive); remove the entry from the config"
        );
    }
    let rooms = store.rooms()?;
    let shared = Arc::new(conn::Shared {
        registry: Mutex::new(room::Registry::new(rooms)),
        store: Mutex::new(store),
        limiter: Mutex::new(auth::LoginLimiter::new(LOGIN_MAX_FAILURES, LOGIN_WINDOW)),
        dummy_hash,
        verify_slots: tokio::sync::Semaphore::new(CONCURRENT_PASSWORD_CHECKS),
        retired: Mutex::new(Default::default()),
    });
    let watcher = tokio::spawn(conn::watch_store(shared.clone()));
    let accept_endpoint = endpoint.clone();
    let accept = tokio::spawn(async move {
        while let Some(incoming) = accept_endpoint.accept().await {
            tokio::spawn(conn::handle(incoming, shared.clone()));
        }
    });
    Ok(RunningServer { endpoint, fingerprint: identity.fingerprint, accept, watcher })
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Retries briefly while a previous instance's socket is still being released.
async fn bind_endpoint(
    config: quinn::ServerConfig,
    reset_key: &[u8; 32],
    addr: SocketAddr,
) -> anyhow::Result<quinn::Endpoint> {
    let mut endpoint_config = quinn::EndpointConfig::new(Arc::new(::ring::hmac::Key::new(
        ::ring::hmac::HMAC_SHA256,
        reset_key,
    )));
    // Connection IDs carry a keyed hash; with a random key a restarted server would drop
    // old clients' packets as invalid instead of answering them with a stateless reset.
    let cid_key = u64::from_le_bytes(reset_key[..8].try_into().expect("8 bytes"));
    endpoint_config.cid_generator(move || {
        Box::new(quinn_proto::HashedConnectionIdGenerator::from_key(cid_key))
    });
    let mut attempts = 0;
    loop {
        match std::net::UdpSocket::bind(addr) {
            Ok(socket) => {
                return Ok(quinn::Endpoint::new(
                    endpoint_config,
                    Some(config),
                    socket,
                    Arc::new(quinn::TokioRuntime),
                )?)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && attempts < BIND_RETRIES => {
                attempts += 1;
                tokio::time::sleep(BIND_RETRY_DELAY).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// TLS 1.3 + ALPN + transport limits for the relay endpoint.
pub fn quic_config(identity: &tls::ServerIdentity) -> anyhow::Result<quinn::ServerConfig> {
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
