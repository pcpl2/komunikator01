//! Self-update over the frozen update channel (`komunikator-update/1`, docs/updates.md): fetches
//! the signed manifest and the release it names. Nothing downloaded is trusted until the
//! manifest's signature, the version (strictly newer) and the file's size and hash check out.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use protocol::update::{
    is_newer, verify_manifest, Manifest, UpdateError, Version, MAX_MANIFEST, MAX_RELEASE_SIZE, UPDATE_ALPN,
};
use quinn::{Connection, Endpoint, VarInt};
use sha2::{Digest, Sha256};
use tokio::time::timeout;

use crate::net::session::resolve;
use crate::net::{client_config_with_alpn, SERVER_NAME};

/// For each request, connection included.
const TIMEOUT: Duration = Duration::from_secs(30);
const REQ_MANIFEST: u8 = 1;
const REQ_FILE: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    UpToDate,
    Available(Manifest),
}

/// This build's version, as releases are numbered.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is major.minor.patch")
}

/// Only a strictly newer release is an update: a hostile server cannot downgrade.
pub fn decide(manifest: &Manifest, current: Version) -> Decision {
    if is_newer(manifest.version, current) {
        Decision::Available(manifest.clone())
    } else {
        Decision::UpToDate
    }
}

/// The file is exactly the one the signed manifest describes.
pub fn check_file(m: &Manifest, bytes: &[u8]) -> bool {
    bytes.len() as u64 == m.size && <[u8; 32]>::from(Sha256::digest(bytes)) == m.sha256
}

/// Never restart in the middle of a conversation.
pub fn may_apply(in_room_with_others: bool) -> bool {
    !in_room_with_others
}

/// The server's signed manifest, unverified; `None` when it publishes no release.
pub async fn fetch_manifest(server: &str, fingerprint: [u8; 32]) -> anyhow::Result<Option<Vec<u8>>> {
    let bytes = request(server, fingerprint, &[REQ_MANIFEST], 4, MAX_MANIFEST as u64).await?;
    Ok((!bytes.is_empty()).then_some(bytes))
}

/// The release file `name`, refused when it is longer than `max` or the server has none.
pub async fn fetch_file(server: &str, fingerprint: [u8; 32], name: &str, max: u64) -> anyhow::Result<Vec<u8>> {
    let len = u8::try_from(name.len()).map_err(|_| anyhow!("za długa nazwa pliku wydania"))?;
    let req = [&[REQ_FILE, len][..], name.as_bytes()].concat();
    let bytes = request(server, fingerprint, &req, 8, max).await?;
    if bytes.is_empty() {
        bail!("serwer nie udostępnia pliku wydania");
    }
    Ok(bytes)
}

/// Fetches and verifies the manifest against the built-in release key.
pub async fn check(server: &str, fingerprint: [u8; 32], key: &[u8; 32], current: Version) -> anyhow::Result<Decision> {
    let Some(bytes) = fetch_manifest(server, fingerprint).await? else { return Ok(Decision::UpToDate) };
    let m = verify_manifest(key, &bytes).map_err(|e| anyhow!(describe(e)))?;
    Ok(decide(&m, current))
}

/// Downloads the release a verified manifest names and checks it against the manifest.
pub async fn download(server: &str, fingerprint: [u8; 32], m: &Manifest) -> anyhow::Result<Vec<u8>> {
    // The signature does not bound the size; this does, before a byte is read.
    if m.size > MAX_RELEASE_SIZE {
        bail!("wydanie jest za duże");
    }
    let bytes = fetch_file(server, fingerprint, &m.file, m.size).await?;
    if !check_file(m, &bytes) {
        bail!("pobrany plik nie zgadza się z podpisanym wydaniem");
    }
    Ok(bytes)
}

fn describe(e: UpdateError) -> &'static str {
    match e {
        UpdateError::Malformed => "uszkodzony opis wydania",
        UpdateError::BadSignature => "opis wydania nie ma poprawnego podpisu",
        UpdateError::BadFileName => "niepoprawna nazwa pliku w opisie wydania",
    }
}

/// One request on a fresh connection; returns the body after its `len_bytes` little-endian
/// length, refusing anything longer than `max`.
async fn request(server: &str, fingerprint: [u8; 32], req: &[u8], len_bytes: usize, max: u64) -> anyhow::Result<Vec<u8>> {
    let work = async {
        let (_endpoint, conn) = connect(server, fingerprint).await?;
        let result = exchange(&conn, req, len_bytes, max).await;
        conn.close(VarInt::from_u32(0), b"");
        result
    };
    // Dropping the unfinished work on timeout drops its connection too.
    timeout(TIMEOUT, work).await.map_err(|_| anyhow!("serwer nie odpowiada w czasie"))?
}

async fn connect(server: &str, fingerprint: [u8; 32]) -> anyhow::Result<(Endpoint, Connection)> {
    let addr = resolve(server).await.ok_or_else(|| anyhow!("nie znaleziono serwera {server}"))?;
    let bind: SocketAddr =
        if addr.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (Ipv6Addr::UNSPECIFIED, 0).into() };
    let mut endpoint = Endpoint::client(bind).context("nie udało się otworzyć gniazda")?;
    endpoint.set_default_client_config(client_config_with_alpn(fingerprint, UPDATE_ALPN)?);
    let conn = endpoint
        .connect(addr, SERVER_NAME)
        .context("brak połączenia z serwerem")?
        .await
        .context("brak połączenia z serwerem")?;
    Ok((endpoint, conn))
}

async fn exchange(conn: &Connection, req: &[u8], len_bytes: usize, max: u64) -> anyhow::Result<Vec<u8>> {
    let broken = "przerwane połączenie z serwerem";
    let (mut send, mut recv) = conn.open_bi().await.context(broken)?;
    send.write_all(req).await.context(broken)?;
    send.finish().context(broken)?;
    let mut len = [0u8; 8];
    recv.read_exact(&mut len[..len_bytes]).await.context(broken)?;
    let len = u64::from_le_bytes(len);
    if len > max {
        bail!("serwer wysyła więcej danych, niż powinien");
    }
    let mut body = vec![0u8; len as usize];
    recv.read_exact(&mut body).await.context(broken)?;
    // The length said it all; anything after it is not a valid answer.
    recv.read_to_end(0).await.context("serwer wysyła więcej danych, niż powinien")?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn manifest(version: &str, bytes: &[u8]) -> Manifest {
        Manifest { version: v(version), file: "komunikator-1.0.0.exe".into(), size: bytes.len() as u64, sha256: Sha256::digest(bytes).into() }
    }

    #[test]
    fn decide_only_newer() {
        let m = manifest("1.2.3", b"x");
        assert_eq!(decide(&m, v("1.2.2")), Decision::Available(m.clone()));
        assert_eq!(decide(&m, v("1.2.3")), Decision::UpToDate);
        assert_eq!(decide(&m, v("2.0.0")), Decision::UpToDate);
    }

    #[test]
    fn check_file_size_and_hash() {
        let m = manifest("1.0.0", b"nowy program");
        assert!(check_file(&m, b"nowy program"));
        assert!(!check_file(&m, b"nowy programX"));
        assert!(!check_file(&m, b"nowy progran"));
        assert!(!check_file(&Manifest { size: m.size + 1, ..m.clone() }, b"nowy program"));
        assert!(!check_file(&m, b""));
    }

    #[test]
    fn update_waits_while_in_conversation() {
        assert!(!may_apply(true));
        assert!(may_apply(false));
    }

    #[test]
    fn current_version_is_the_crate_version() {
        assert_eq!(current_version().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
