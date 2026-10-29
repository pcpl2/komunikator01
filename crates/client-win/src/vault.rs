//! The saved login in Windows Credential Manager: one generic credential per data directory
//! holding the session token or the password. `settings.toml` keeps only the server and the
//! name; deleting the credential (Control Panel → Credential Manager) logs the program out.

use std::path::Path;

use anyhow::Context;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

use crate::startup::default_data_dir;
use crate::ui::wide::wide;

/// What the next login uses.
#[derive(Clone, PartialEq, Eq)]
pub enum Secret {
    Token([u8; 32]),
    Password(String),
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Secret::Token(_) => "Token(..)",
            Secret::Password(_) => "Password(..)",
        })
    }
}

const TOKEN_TAG: u8 = b'T';
const PASSWORD_TAG: u8 = b'P';

impl Secret {
    fn encode(&self) -> Vec<u8> {
        let (tag, body) = match self {
            Secret::Token(t) => (TOKEN_TAG, &t[..]),
            Secret::Password(p) => (PASSWORD_TAG, p.as_bytes()),
        };
        let mut out = Vec::with_capacity(1 + body.len());
        out.push(tag);
        out.extend_from_slice(body);
        out
    }

    fn decode(blob: &[u8]) -> Option<Secret> {
        match blob.split_first()? {
            (&TOKEN_TAG, body) => Some(Secret::Token(body.try_into().ok()?)),
            (&PASSWORD_TAG, body) => Some(Secret::Password(String::from_utf8(body.to_vec()).ok()?)),
            _ => None,
        }
    }
}

/// The credential of one data directory.
pub struct Vault {
    target: String,
}

impl Vault {
    /// `komunikator` for the default data directory, `komunikator (<path>)` for any other, so a
    /// second profile (`--data-dir`) has a login of its own.
    pub fn for_data_dir(dir: &Path) -> Vault {
        let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        let dir = abs(dir);
        let target =
            if dir == abs(&default_data_dir()) { "komunikator".to_owned() } else { format!("komunikator ({})", dir.display()) };
        Vault { target }
    }

    /// The name shown in Credential Manager.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The saved secret, or `None` when there is none or it cannot be read.
    pub fn load(&self) -> Option<Secret> {
        let target = wide(&self.target);
        let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
        unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut cred) }.ok()?;
        // SAFETY: on success `cred` points to a credential allocated by the system; the blob is
        // copied out before the single CredFree.
        let blob = unsafe {
            let c = &*cred;
            let blob = if c.CredentialBlob.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec()
            };
            CredFree(cred as *const _);
            blob
        };
        Secret::decode(&blob)
    }

    pub fn token(&self) -> Option<[u8; 32]> {
        match self.load()? {
            Secret::Token(t) => Some(t),
            Secret::Password(_) => None,
        }
    }

    pub fn password(&self) -> Option<String> {
        match self.load()? {
            Secret::Password(p) => Some(p),
            Secret::Token(_) => None,
        }
    }

    /// Saves `secret` in place of whatever was saved; `account` (e.g. `ola@serwer.pl:7700`) is
    /// the user name Credential Manager shows.
    pub fn store(&self, secret: &Secret, account: &str) -> anyhow::Result<()> {
        let mut target = wide(&self.target);
        let mut user = wide(account);
        let mut blob = secret.encode();
        let cred = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            UserName: PWSTR(user.as_mut_ptr()),
            CredentialBlobSize: u32::try_from(blob.len())?,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        unsafe { CredWriteW(&cred, 0) }.context("Menedżer poświadczeń Windows")
    }

    /// Forgets the saved secret; nothing saved is not an error.
    pub fn clear(&self) -> anyhow::Result<()> {
        let target = wide(&self.target);
        match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => Ok(()),
            r => r.context("Menedżer poświadczeń Windows"),
        }
    }
}

/// The user name a saved secret is shown under.
pub fn account_label(username: &str, server: &str) -> String {
    format!("{username}@{server}")
}

/// A vault under a unique test name, emptied on drop.
#[cfg(test)]
pub(crate) struct ScratchVault(pub Vault);

#[cfg(test)]
impl ScratchVault {
    pub fn new() -> ScratchVault {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        ScratchVault(Vault { target: format!("komunikator-test-{}-{n}", std::process::id()) })
    }
}

#[cfg(test)]
impl std::ops::Deref for ScratchVault {
    type Target = Vault;
    fn deref(&self) -> &Vault {
        &self.0
    }
}

#[cfg(test)]
impl Drop for ScratchVault {
    fn drop(&mut self) {
        let _ = self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_roundtrip_and_replace_each_other() {
        let v = ScratchVault::new();
        assert_eq!(v.load(), None);
        v.store(&Secret::Password("tajne hasło".into()), "ola@serwer.pl:7700").unwrap();
        assert_eq!(v.password().as_deref(), Some("tajne hasło"));
        assert_eq!(v.token(), None);
        v.store(&Secret::Token([9; 32]), "ola@serwer.pl:7700").unwrap();
        assert_eq!(v.token(), Some([9; 32]));
        assert_eq!(v.password(), None);
        v.clear().unwrap();
        assert_eq!(v.load(), None);
        v.clear().unwrap();
    }

    #[test]
    fn garbage_blobs_are_no_secret() {
        assert_eq!(Secret::decode(b""), None);
        assert_eq!(Secret::decode(b"T123"), None);
        assert_eq!(Secret::decode(b"X"), None);
        assert_eq!(Secret::decode(&[PASSWORD_TAG, 0xff]), None);
        assert_eq!(Secret::decode(&Secret::Token([1; 32]).encode()), Some(Secret::Token([1; 32])));
    }

    #[test]
    fn profiles_get_their_own_credential() {
        assert_eq!(Vault::for_data_dir(&default_data_dir()).target(), "komunikator");
        let other = Vault::for_data_dir(Path::new(r"D:\profil2"));
        assert_eq!(other.target(), r"komunikator (D:\profil2)");
    }
}
