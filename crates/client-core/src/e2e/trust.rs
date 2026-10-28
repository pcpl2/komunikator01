//! Trust-on-first-use store of peer device keys, and safety codes for out-of-band comparison.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::IdentityPublic;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    Verified,
    Known,
    NewUser,
    NewDevice,
    KeyChanged,
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    user: String,
    device: String,
    sign: String,
    verified: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileFormat {
    #[serde(default)]
    device: Vec<Record>,
}

#[derive(Debug)]
pub struct KnownDevices {
    path: PathBuf,
    records: Vec<Record>,
    save_failed: Cell<bool>,
    recovered_from: Option<PathBuf>,
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Writes via `<path>.tmp`: create, write, fsync, rename; the temp file is removed on error.
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

impl KnownDevices {
    /// Loads the store. A missing file is an empty store. A file that cannot be parsed is
    /// moved to `<file>.broken` (replacing an older one) and the store starts empty; the
    /// caller must tell the user (see [`KnownDevices::recovered_from`]). Other read errors
    /// (e.g. access denied) are returned and the file is left alone.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut recovered_from = None;
        let records = match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<FileFormat>(&text) {
                Ok(f) => f.device,
                Err(e) => {
                    let mut broken = path.as_os_str().to_owned();
                    broken.push(".broken");
                    let broken = PathBuf::from(broken);
                    tracing::warn!("malformed trust file {}: {e}; moved to {}", path.display(), broken.display());
                    if broken.exists() {
                        std::fs::remove_file(&broken)?;
                    }
                    std::fs::rename(path, &broken)?;
                    recovered_from = Some(broken);
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path: path.to_path_buf(), records, save_failed: Cell::new(false), recovered_from })
    }

    /// Where an unreadable trust file was moved during [`KnownDevices::load`], if that happened.
    /// All previously known devices are then forgotten and will show up as new.
    pub fn recovered_from(&self) -> Option<&Path> {
        self.recovered_from.as_deref()
    }

    fn position(&self, user: &str, id: &IdentityPublic) -> Option<usize> {
        let device = to_hex(&id.device);
        self.records.iter().position(|r| r.user == user && r.device == device)
    }

    fn save(&self) -> anyhow::Result<()> {
        let text = toml::to_string(&FileFormat {
            device: self
                .records
                .iter()
                .map(|r| Record {
                    user: r.user.clone(),
                    device: r.device.clone(),
                    sign: r.sign.clone(),
                    verified: r.verified,
                })
                .collect(),
        })?;
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        write_atomic(&self.path, text.as_bytes())
    }

    fn persist(&self) {
        match self.save() {
            Ok(()) => self.save_failed.set(false),
            Err(e) => {
                self.save_failed.set(true);
                tracing::warn!("could not save trust store {}: {e}", self.path.display());
            }
        }
    }

    /// True if the most recent attempt to persist the store failed (reset by the next success).
    pub fn save_failed(&self) -> bool {
        self.save_failed.get()
    }

    fn insert(&mut self, user: &str, id: &IdentityPublic) {
        self.records.push(Record {
            user: user.to_string(),
            device: to_hex(&id.device),
            sign: to_hex(&id.sign),
            verified: false,
        });
        self.persist();
    }

    /// Classifies a peer key. Stores first-seen users/devices; never overwrites a changed key.
    pub fn check(&mut self, user: &str, id: &IdentityPublic) -> Trust {
        if let Some(i) = self.position(user, id) {
            let r = &self.records[i];
            return if r.sign != to_hex(&id.sign) {
                Trust::KeyChanged
            } else if r.verified {
                Trust::Verified
            } else {
                Trust::Known
            };
        }
        let known_user = self.records.iter().any(|r| r.user == user);
        self.insert(user, id);
        if known_user { Trust::NewDevice } else { Trust::NewUser }
    }

    /// Accepts the given key for (user, device), replacing any stored key; unverified.
    pub fn accept(&mut self, user: &str, id: &IdentityPublic) {
        match self.position(user, id) {
            Some(i) => {
                self.records[i].sign = to_hex(&id.sign);
                self.records[i].verified = false;
                self.persist();
            }
            None => self.insert(user, id),
        }
    }

    /// Marks the matching (user, device, key) record as verified out of band.
    pub fn verify(&mut self, user: &str, id: &IdentityPublic) {
        if let Some(i) = self.position(user, id) {
            if self.records[i].sign == to_hex(&id.sign) {
                self.records[i].verified = true;
                self.persist();
            }
        }
    }
}

/// Safety code for comparing two identities out of band: SHA-256 over the smaller `sign`
/// key followed by the larger one, first 8 bytes read as four big-endian u16, each
/// `% 10_000` zero-padded to 4 digits, joined by spaces. Symmetric in its arguments.
pub fn safety_code(a: &IdentityPublic, b: &IdentityPublic) -> String {
    let (lo, hi) = if a.sign <= b.sign { (&a.sign, &b.sign) } else { (&b.sign, &a.sign) };
    let mut h = Sha256::new();
    h.update(lo);
    h.update(hi);
    let d = h.finalize();
    (0..4)
        .map(|i| format!("{:04}", u16::from_be_bytes([d[2 * i], d[2 * i + 1]]) % 10_000))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(sign: u8, device: u8) -> IdentityPublic {
        IdentityPublic { sign: [sign; 32], dh: [9; 32], device: [device; 16] }
    }

    fn store() -> (tempfile::TempDir, KnownDevices) {
        let dir = tempfile::tempdir().unwrap();
        let k = KnownDevices::load(&dir.path().join("known.toml")).unwrap();
        (dir, k)
    }

    #[test]
    fn first_contact_is_new_user_then_known() {
        let (_d, mut k) = store();
        assert_eq!(k.check("ala", &id(1, 1)), Trust::NewUser);
        assert_eq!(k.check("ala", &id(1, 1)), Trust::Known);
    }

    #[test]
    fn second_device_is_new_device() {
        let (_d, mut k) = store();
        k.check("ala", &id(1, 1));
        assert_eq!(k.check("ala", &id(2, 2)), Trust::NewDevice);
        assert_eq!(k.check("ala", &id(2, 2)), Trust::Known);
    }

    #[test]
    fn changed_key_warns_until_accepted() {
        let (_d, mut k) = store();
        k.check("ala", &id(1, 1));
        assert_eq!(k.check("ala", &id(7, 1)), Trust::KeyChanged);
        assert_eq!(k.check("ala", &id(7, 1)), Trust::KeyChanged);
        k.accept("ala", &id(7, 1));
        assert_eq!(k.check("ala", &id(7, 1)), Trust::Known);
    }

    #[test]
    fn verified_survives_reload() {
        let (d, mut k) = store();
        k.check("ala", &id(1, 1));
        k.verify("ala", &id(1, 1));
        assert_eq!(k.check("ala", &id(1, 1)), Trust::Verified);
        let mut k2 = KnownDevices::load(&d.path().join("known.toml")).unwrap();
        assert_eq!(k2.check("ala", &id(1, 1)), Trust::Verified);
    }

    #[test]
    fn accept_resets_verified() {
        let (_d, mut k) = store();
        k.check("ala", &id(1, 1));
        k.verify("ala", &id(1, 1));
        k.accept("ala", &id(7, 1));
        assert_eq!(k.check("ala", &id(7, 1)), Trust::Known);
    }

    #[test]
    fn malformed_file_is_set_aside_and_store_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("known.toml");
        std::fs::write(&p, "this is [not toml").unwrap();
        let mut k = KnownDevices::load(&p).unwrap();
        let broken = dir.path().join("known.toml.broken");
        assert_eq!(k.recovered_from(), Some(broken.as_path()));
        assert_eq!(std::fs::read_to_string(&broken).unwrap(), "this is [not toml");
        assert_eq!(k.check("ala", &id(1, 1)), Trust::NewUser);
        // The new store is written fresh and loads cleanly.
        let k2 = KnownDevices::load(&p).unwrap();
        assert_eq!(k2.recovered_from(), None);
    }

    #[test]
    fn missing_file_is_empty_without_recovery() {
        let (_d, k) = store();
        assert_eq!(k.recovered_from(), None);
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let (d, mut k) = store();
        k.check("ala", &id(1, 1));
        assert!(!k.save_failed());
        assert!(d.path().join("known.toml").exists());
        assert!(!d.path().join("known.toml.tmp").exists());
    }

    #[test]
    fn persists_into_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("deeper").join("known.toml");
        let mut k = KnownDevices::load(&p).unwrap();
        assert_eq!(k.check("ala", &id(1, 1)), Trust::NewUser);
        assert!(!k.save_failed());
        let mut k2 = KnownDevices::load(&p).unwrap();
        assert_eq!(k2.check("ala", &id(1, 1)), Trust::Known);
    }

    #[test]
    fn save_failed_flag_tracks_persist_result() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let mut k = KnownDevices::load(&blocker.join("known.toml")).unwrap();
        assert!(!k.save_failed());
        k.check("ala", &id(1, 1));
        assert!(k.save_failed());
        // Point at a writable location: next successful save clears the flag.
        k.path = dir.path().join("ok.toml");
        k.verify("ala", &id(1, 1));
        assert!(!k.save_failed());
    }

    #[test]
    fn safety_code_is_symmetric() {
        let (a, b) = (id(1, 1), id(2, 2));
        let code = safety_code(&a, &b);
        assert_eq!(code, safety_code(&b, &a));
        let groups: Vec<&str> = code.split(' ').collect();
        assert_eq!(groups.len(), 4);
        assert!(groups.iter().all(|g| g.len() == 4 && g.bytes().all(|c| c.is_ascii_digit())));
        assert_ne!(code, safety_code(&a, &id(3, 3)));
    }
}
