//! Offline release signing: the secret key lives only on the maintainer's machine.
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use ed25519_dalek::SigningKey;
use protocol::update::{sign_manifest, Manifest, Version, MAX_RELEASE_SIZE};
use sha2::{Digest, Sha256};

/// Lowercase hex, the format of the secret file and of `release-key.pub`.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex_32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Writes a fresh random seed to a new file; never overwrites an existing key.
pub fn keygen(path: &Path) -> anyhow::Result<[u8; 32]> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("brak losowości systemowej: {e}"))?;
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .with_context(|| format!("nie można utworzyć pliku klucza {} (czy już istnieje?)", path.display()))?;
    writeln!(file, "{}", to_hex(&seed))?;
    Ok(seed)
}

/// Reads the secret seed from a file written by `keygen`.
pub fn load_key(path: &Path) -> anyhow::Result<SigningKey> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("nie można odczytać klucza {}", path.display()))?;
    let seed = from_hex_32(&text).ok_or_else(|| anyhow::anyhow!("plik klucza ma nieprawidłowy format"))?;
    Ok(SigningKey::from_bytes(&seed))
}

pub fn public_hex(key: &Path) -> anyhow::Result<String> {
    Ok(to_hex(&load_key(key)?.verifying_key().to_bytes()))
}

/// Copies the exe into `out` under its release name and writes the signed manifest next to it.
pub fn sign(key: &Path, exe: &Path, version: &str, out: &Path) -> anyhow::Result<Manifest> {
    let version = Version::parse(version)
        .ok_or_else(|| anyhow::anyhow!("wersja musi mieć postać x.y.z (same cyfry)"))?;
    let size = std::fs::metadata(exe)
        .with_context(|| format!("nie można odczytać {}", exe.display()))?
        .len();
    anyhow::ensure!(size <= MAX_RELEASE_SIZE, "plik exe jest większy niż {MAX_RELEASE_SIZE} bajtów");
    let signing = load_key(key)?;
    let bytes = std::fs::read(exe)?;
    let file = format!("komunikator-{version}.exe");
    let manifest = Manifest { version, file, size: bytes.len() as u64, sha256: Sha256::digest(&bytes).into() };
    std::fs::create_dir_all(out)?;
    std::fs::write(out.join(&manifest.file), &bytes)?;
    std::fs::write(out.join("manifest.bin"), sign_manifest(&signing, &manifest))?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use protocol::update::{verify_manifest, MAX_RELEASE_SIZE};
    use sha2::{Digest, Sha256};

    #[test]
    fn keygen_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("release.key");
        keygen(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(keygen(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn sign_produces_verifiable_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("release.key");
        let seed = keygen(&key).unwrap();
        let exe = dir.path().join("komunikator.exe");
        std::fs::write(&exe, b"MZ fake exe bytes").unwrap();
        let out = dir.path().join("out");

        let m = sign(&key, &exe, "1.2.3", &out).unwrap();

        let public = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        let bytes = std::fs::read(out.join("manifest.bin")).unwrap();
        assert_eq!(verify_manifest(&public, &bytes).unwrap(), m);
        assert_eq!(m.file, "komunikator-1.2.3.exe");
        let copied = std::fs::read(out.join(&m.file)).unwrap();
        assert_eq!(copied, b"MZ fake exe bytes");
        assert_eq!(m.size, copied.len() as u64);
        assert_eq!(m.sha256, <[u8; 32]>::from(Sha256::digest(&copied)));
    }

    #[test]
    fn sign_rejects_bad_version_and_oversize() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("release.key");
        keygen(&key).unwrap();
        let exe = dir.path().join("a.exe");
        std::fs::write(&exe, b"x").unwrap();
        let out = dir.path().join("out");
        assert!(sign(&key, &exe, "1.2", &out).is_err());
        assert!(sign(&key, &exe, "v1.2.3", &out).is_err());

        let big = dir.path().join("big.exe");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_RELEASE_SIZE + 1).unwrap();
        assert!(sign(&key, &big, "1.2.3", &out).is_err());
        assert!(!out.join("manifest.bin").exists());
    }
}
