//! The client's side of the update channel against a real server with a signed release.

use client_core::update::{check, check_file, download, fetch_file, fetch_manifest, Decision};
use ed25519_dalek::SigningKey;
use protocol::update::{sign_manifest, verify_manifest, Manifest, Version};
use server::{RunningServer, ServerConfig};
use sha2::{Digest, Sha256};

const FILE: &str = "komunikator-9.0.0.exe";

fn key() -> SigningKey {
    SigningKey::from_bytes(&[5; 32])
}

async fn start(dir: &tempfile::TempDir) -> RunningServer {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.path().to_owned(),
        users: Vec::new(),
        public_address: None,
    };
    server::start(config).await.unwrap()
}

/// Publishes `content` as release 9.0.0, the way `komunikator-release sign` lays it out.
fn publish(dir: &tempfile::TempDir, content: &[u8], manifest_size: u64) -> Manifest {
    let m = Manifest {
        version: Version { major: 9, minor: 0, patch: 0 },
        file: FILE.into(),
        size: manifest_size,
        sha256: Sha256::digest(content).into(),
    };
    let updates = dir.path().join("updates");
    std::fs::create_dir_all(&updates).unwrap();
    std::fs::write(updates.join(FILE), content).unwrap();
    std::fs::write(updates.join("manifest.bin"), sign_manifest(&key(), &m)).unwrap();
    m
}

#[tokio::test]
async fn fetch_from_test_server() {
    let dir = tempfile::tempdir().unwrap();
    let srv = start(&dir).await;
    let (addr, fp) = (srv.local_addr().to_string(), srv.fingerprint());
    let public = key().verifying_key().to_bytes();
    let old = Version { major: 0, minor: 1, patch: 0 };

    // Nothing published yet.
    assert_eq!(fetch_manifest(&addr, fp).await.unwrap(), None);
    assert_eq!(check(&addr, fp, &public, old).await.unwrap(), Decision::UpToDate);

    let content: Vec<u8> = (0..200_003u32).map(|i| (i * 13 % 251) as u8).collect();
    let m = publish(&dir, &content, content.len() as u64);

    let bytes = fetch_manifest(&addr, fp).await.unwrap().expect("a manifest");
    assert_eq!(verify_manifest(&public, &bytes), Ok(m.clone()));
    let file = fetch_file(&addr, fp, FILE, m.size).await.unwrap();
    assert!(check_file(&m, &file));
    // A file longer than the caller allows is refused, not read.
    assert!(fetch_file(&addr, fp, FILE, m.size - 1).await.is_err());
    // A name the manifest does not give gets nothing.
    assert!(fetch_file(&addr, fp, "inny.exe", m.size).await.is_err());

    // The whole flow: newer only, signed by the built-in key only.
    assert_eq!(check(&addr, fp, &public, old).await.unwrap(), Decision::Available(m.clone()));
    assert_eq!(check(&addr, fp, &public, m.version).await.unwrap(), Decision::UpToDate);
    let other = SigningKey::from_bytes(&[6; 32]).verifying_key().to_bytes();
    assert!(check(&addr, fp, &other, old).await.is_err());
    assert_eq!(download(&addr, fp, &m).await.unwrap(), content);

    // A file that does not match the signed size and hash is refused.
    let tampered = publish(&dir, &content, content.len() as u64);
    let mut evil = content.clone();
    evil[7] ^= 1;
    std::fs::write(dir.path().join("updates").join(FILE), &evil).unwrap();
    assert!(download(&addr, fp, &tampered).await.is_err());

    // A signed size beyond the limit is refused before anything is downloaded.
    let huge = Manifest { size: protocol::update::MAX_RELEASE_SIZE + 1, ..m };
    assert!(download(&addr, fp, &huge).await.is_err());
    srv.shutdown().await;
}
