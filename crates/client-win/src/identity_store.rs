//! Device identity on disk: `identity.bin` = DPAPI (current user) of `Identity::to_bytes`.

use std::path::{Path, PathBuf};

use client_core::e2e::Identity;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityStatus {
    Loaded,
    Created,
    /// The old file could not be read or decrypted; it was moved to `broken`.
    Replaced { broken: PathBuf },
}

const FILE: &str = "identity.bin";
const BROKEN: &str = "identity.bin.broken";

/// Runs a DPAPI call and copies the result out of the `LocalAlloc`ed output blob.
fn with_blob(
    input: &[u8],
    call: impl FnOnce(&CRYPT_INTEGER_BLOB, &mut CRYPT_INTEGER_BLOB) -> windows::core::Result<()>,
) -> anyhow::Result<Vec<u8>> {
    let len = u32::try_from(input.len())?;
    let inb = CRYPT_INTEGER_BLOB { cbData: len, pbData: input.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    call(&inb, &mut out)?;
    if out.pbData.is_null() {
        anyhow::bail!("DPAPI returned no data");
    }
    // SAFETY: on success DPAPI hands back `cbData` bytes at `pbData`, allocated with LocalAlloc;
    // we copy them and free the buffer exactly once.
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
    }
    Ok(bytes)
}

fn protect(plain: &[u8]) -> anyhow::Result<Vec<u8>> {
    with_blob(plain, |inb, out| unsafe {
        CryptProtectData(inb, PCWSTR::null(), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, out)
    })
}

fn unprotect(blob: &[u8]) -> anyhow::Result<Vec<u8>> {
    with_blob(blob, |inb, out| unsafe {
        CryptUnprotectData(inb, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, out)
    })
}

fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

fn create(dir: &Path, path: &Path) -> anyhow::Result<Identity> {
    let identity = Identity::generate();
    let protected = protect(&identity.to_bytes())?;
    std::fs::create_dir_all(dir)?;
    write_atomic(path, &protected)?;
    Ok(identity)
}

/// Loads `<dir>/identity.bin`, or creates it. A file that exists but cannot be decrypted or
/// parsed is moved to `identity.bin.broken` (replacing an older one) and a new identity is made.
/// Any other read failure (e.g. access denied) is an error: the file is left alone.
pub fn load_or_create_identity(dir: &Path) -> anyhow::Result<(Identity, IdentityStatus)> {
    let path = dir.join(FILE);
    let blob = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((create(dir, &path)?, IdentityStatus::Created));
        }
        Err(e) => return Err(anyhow::anyhow!("nie można odczytać {}: {e}", path.display())),
    };
    if let Some(identity) = unprotect(&blob).ok().and_then(|plain| Identity::from_bytes(&plain)) {
        return Ok((identity, IdentityStatus::Loaded));
    }
    let broken = dir.join(BROKEN);
    if broken.exists() {
        std::fs::remove_file(&broken)?;
    }
    std::fs::rename(&path, &broken)?;
    Ok((create(dir, &path)?, IdentityStatus::Replaced { broken }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let (a, s1) = load_or_create_identity(dir.path()).unwrap();
        assert_eq!(s1, IdentityStatus::Created);
        let (b, s2) = load_or_create_identity(dir.path()).unwrap();
        assert_eq!(s2, IdentityStatus::Loaded);
        assert_eq!(a.public(), b.public());
    }

    #[test]
    fn file_is_not_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let (id, _) = load_or_create_identity(dir.path()).unwrap();
        let plain = id.to_bytes();
        let file = std::fs::read(dir.path().join("identity.bin")).unwrap();
        assert!(!file.is_empty());
        assert!(!file.windows(plain.len()).any(|w| w == plain.as_slice()));
        // The secret keys are not stored in the clear either.
        assert!(!file.windows(32).any(|w| w == id.dh_secret()));
    }

    #[test]
    fn unreadable_identity_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let junk = b"not a dpapi blob at all".to_vec();
        std::fs::write(dir.path().join("identity.bin"), &junk).unwrap();
        let (id, status) = load_or_create_identity(dir.path()).unwrap();
        let broken = dir.path().join("identity.bin.broken");
        assert_eq!(status, IdentityStatus::Replaced { broken: broken.clone() });
        assert_eq!(std::fs::read(&broken).unwrap(), junk);
        let (again, s) = load_or_create_identity(dir.path()).unwrap();
        assert_eq!(s, IdentityStatus::Loaded);
        assert_eq!(again.public(), id.public());
    }

    #[test]
    fn older_broken_file_is_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("identity.bin.broken"), b"old").unwrap();
        std::fs::write(dir.path().join("identity.bin"), b"newer junk").unwrap();
        load_or_create_identity(dir.path()).unwrap();
        assert_eq!(std::fs::read(dir.path().join("identity.bin.broken")).unwrap(), b"newer junk");
    }
}
