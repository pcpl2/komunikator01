//! The update channel (`komunikator-update/1`, frozen; see docs/updates.md): hands out the signed
//! manifest and the one file it names. No login, and nothing is verified here: the server has no
//! release key, so clients trust only what checks out against theirs.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protocol::update::{plain_file_name, unverified_file_name, MAX_MANIFEST, MAX_RELEASE_SIZE};
use quinn::{Connection, SendStream, VarInt};
use tokio::io::AsyncReadExt;
use tokio::time::timeout;

use crate::conn::CLOSE_PROTOCOL;

const REQ_MANIFEST: u8 = 1;
const REQ_FILE: u8 = 2;
/// The longest request: kind, name length, 255 name bytes.
const MAX_REQUEST: usize = 2 + u8::MAX as usize;
const PER_IP: usize = 4;
const TOTAL: usize = 20;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 64 MB at about 110 kB/s; a slower client keeps a slot no longer than this.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(600);
/// How long to wait for the peer to acknowledge the end of the response before closing.
const LINGER: Duration = Duration::from_secs(10);
const CHUNK: usize = 64 * 1024;
/// Application close code: too many update connections.
const CLOSE_BUSY: u32 = 3;

/// Where releases live (`data_dir/updates`) and who is downloading them.
pub(crate) struct Host {
    dir: PathBuf,
    slots: Mutex<Counts>,
}

#[derive(Default)]
struct Counts {
    per_ip: HashMap<IpAddr, usize>,
    total: usize,
}

/// One update connection's place in the limits; given back on drop.
struct Slot {
    host: Arc<Host>,
    ip: IpAddr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut c = self.host.slots.lock().unwrap();
        c.total -= 1;
        if let Some(n) = c.per_ip.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                c.per_ip.remove(&self.ip);
            }
        }
    }
}

impl Host {
    pub(crate) fn new(data_dir: &std::path::Path) -> Host {
        Host { dir: data_dir.join("updates"), slots: Mutex::default() }
    }

    fn take_slot(self: &Arc<Self>, ip: IpAddr) -> Option<Slot> {
        let mut c = self.slots.lock().unwrap();
        let n = c.per_ip.get(&ip).copied().unwrap_or(0);
        if n >= PER_IP || c.total >= TOTAL {
            return None;
        }
        c.per_ip.insert(ip, n + 1);
        c.total += 1;
        Some(Slot { host: self.clone(), ip })
    }

    async fn manifest(&self) -> Option<Vec<u8>> {
        let (file, len) = open_regular(&self.dir.join("manifest.bin"), MAX_MANIFEST as u64).await?;
        let mut bytes = Vec::with_capacity(len as usize);
        file.take(MAX_MANIFEST as u64 + 1).read_to_end(&mut bytes).await.ok()?;
        (bytes.len() <= MAX_MANIFEST).then_some(bytes)
    }
}

/// Opens a regular file of at most `max` bytes; a symlink is never followed.
async fn open_regular(path: &Path, max: u64) -> Option<(tokio::fs::File, u64)> {
    let meta = tokio::fs::symlink_metadata(path).await.ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    let file = tokio::fs::File::open(path).await.ok()?;
    let len = file.metadata().await.ok()?.len();
    (len <= max).then_some((file, len))
}

enum Request {
    Manifest,
    File(Vec<u8>),
}

fn parse(req: &[u8]) -> Option<Request> {
    match req {
        [REQ_MANIFEST] => Some(Request::Manifest),
        [REQ_FILE, len, name @ ..] if *len as usize == name.len() => Some(Request::File(name.to_vec())),
        _ => None,
    }
}

pub(crate) async fn serve(conn: Connection, host: Arc<Host>) {
    let ip = conn.remote_address().ip();
    let Some(_slot) = host.take_slot(ip) else {
        tracing::info!(%ip, "update connection refused: too many");
        conn.close(VarInt::from_u32(CLOSE_BUSY), b"busy");
        return;
    };
    let read = async {
        let (send, mut recv) = conn.accept_bi().await.ok()?;
        Some((send, recv.read_to_end(MAX_REQUEST).await.ok().and_then(|b| parse(&b))))
    };
    let Ok(Some((mut send, req))) = timeout(REQUEST_TIMEOUT, read).await else { return };
    // One request per connection: the peer may open nothing more to make the server buffer.
    conn.set_max_concurrent_bi_streams(VarInt::from_u32(0));
    conn.set_max_concurrent_uni_streams(VarInt::from_u32(0));
    let Some(req) = req else {
        conn.close(VarInt::from_u32(CLOSE_PROTOCOL), b"protocol violation");
        return;
    };
    let (kind, sent) = match req {
        // A peer that never reads must not hold its slot: every reply has a deadline.
        Request::Manifest => {
            ("manifest", timeout(REQUEST_TIMEOUT, send_manifest(&mut send, &host)).await.unwrap_or(Err(())))
        }
        Request::File(name) => {
            ("file", timeout(TRANSFER_TIMEOUT, send_file(&mut send, &host, &name)).await.unwrap_or(Err(())))
        }
    };
    match sent {
        Ok(bytes) => {
            tracing::info!(%ip, kind, bytes, "update request served");
            if send.finish().is_ok() {
                let _ = timeout(LINGER, send.stopped()).await;
            }
        }
        Err(()) => {
            tracing::info!(%ip, kind, "update request failed");
            let _ = send.reset(VarInt::from_u32(0));
        }
    }
    // Exactly one request per connection: closing here refuses any further stream.
    conn.close(VarInt::from_u32(0), b"");
}

/// Returns the number of payload bytes sent (0 when there is no release).
async fn send_manifest(send: &mut SendStream, host: &Host) -> Result<u64, ()> {
    let bytes = host.manifest().await.unwrap_or_default();
    send.write_all(&(bytes.len() as u32).to_le_bytes()).await.map_err(drop)?;
    send.write_all(&bytes).await.map_err(drop)?;
    Ok(bytes.len() as u64)
}

/// Opens the release file only when `name` is exactly the one the current manifest names.
async fn open_release(host: &Host, name: &[u8]) -> Option<(tokio::fs::File, u64)> {
    let named = unverified_file_name(&host.manifest().await?)?;
    if named.as_bytes() != name || !plain_file_name(&named) {
        return None;
    }
    open_regular(&host.dir.join(&named), MAX_RELEASE_SIZE).await
}

/// Streams the file in chunks; length 0 when it may not or cannot be served.
async fn send_file(send: &mut SendStream, host: &Host, name: &[u8]) -> Result<u64, ()> {
    let Some((file, len)) = open_release(host, name).await else {
        send.write_all(&0u64.to_le_bytes()).await.map_err(drop)?;
        return Ok(0);
    };
    send.write_all(&len.to_le_bytes()).await.map_err(drop)?;
    let mut file = file.take(len);
    let mut buf = vec![0u8; CHUNK];
    let mut sent = 0u64;
    while sent < len {
        let n = file.read(&mut buf).await.map_err(drop)?;
        if n == 0 {
            // The file shrank since it was opened; a short stream must not look complete.
            return Err(());
        }
        send.write_all(&buf[..n]).await.map_err(drop)?;
        sent += n as u64;
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_limited_per_ip_and_in_total() {
        let host = Arc::new(Host::new(std::path::Path::new("unused")));
        let ip = |n: u8| IpAddr::from([10, 0, 0, n]);
        let mine: Vec<_> = (0..PER_IP).map(|_| host.take_slot(ip(1)).unwrap()).collect();
        assert!(host.take_slot(ip(1)).is_none());
        drop(mine);
        assert!(host.take_slot(ip(1)).is_some());
        let all: Vec<_> = (0..TOTAL).map(|i| host.take_slot(ip(2 + (i / PER_IP) as u8)).unwrap()).collect();
        assert!(host.take_slot(ip(200)).is_none());
        drop(all);
        assert!(host.take_slot(ip(200)).is_some());
        assert_eq!(host.slots.lock().unwrap().total, 0);
        assert!(host.slots.lock().unwrap().per_ip.is_empty());
    }

    #[test]
    fn requests_parse_exactly() {
        assert!(matches!(parse(&[1]), Some(Request::Manifest)));
        assert!(matches!(parse(&[2, 1, b'a']), Some(Request::File(n)) if n == b"a"));
        for bad in [&[][..], &[0], &[1, 0], &[2], &[2, 2, b'a'], &[2, 0, b'a']] {
            assert!(parse(bad).is_none(), "{bad:?}");
        }
    }
}
