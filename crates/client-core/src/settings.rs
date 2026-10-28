//! The settings file (`settings.toml`): connection data, devices, hotkeys, window and volumes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::audio::volume::VolumeBook;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Sounds {
    pub mute: bool,
    pub join_leave: bool,
    pub chat: bool,
}

impl Default for Sounds {
    fn default() -> Self {
        Self { mute: true, join_leave: true, chat: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub server: String,
    pub fingerprint: String,
    pub username: String,
    /// Hex of the DPAPI blob holding the password; empty when unset.
    pub password_protected: String,
    pub capture_device: Option<String>,
    pub render_device: Option<String>,
    /// 0.0..=1.0
    pub vad_sensitivity: f32,
    /// e.g. "Ctrl+Shift+M"
    pub hotkey_mute: Option<String>,
    pub hotkey_deafen: Option<String>,
    pub sounds: Sounds,
    pub autostart: bool,
    pub run_as_admin: bool,
    pub always_on_top: bool,
    pub window: Option<WindowRect>,
    pub volumes: VolumeBook,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: String::new(),
            fingerprint: String::new(),
            username: String::new(),
            password_protected: String::new(),
            capture_device: None,
            render_device: None,
            vad_sensitivity: 0.5,
            hotkey_mute: None,
            hotkey_deafen: None,
            sounds: Sounds::default(),
            autostart: false,
            run_as_admin: false,
            always_on_top: false,
            window: None,
            volumes: VolumeBook::default(),
        }
    }
}

impl Settings {
    /// True when there is enough to connect without asking the user.
    pub fn is_configured(&self) -> bool {
        !self.server.is_empty()
            && !self.fingerprint.is_empty()
            && !self.username.is_empty()
            && !self.password_protected.is_empty()
    }
}

pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// Loads the file. Missing means defaults. A file that cannot be read, is not UTF-8 or does
    /// not parse is moved to `<file>.broken` (replacing an older one) and defaults are used; the
    /// third value is where it went, so the caller can tell the user.
    pub fn load(path: &Path) -> (SettingsStore, Settings, Option<PathBuf>) {
        let store = SettingsStore { path: path.to_path_buf() };
        let parsed = match std::fs::read(path) {
            Ok(bytes) => std::str::from_utf8(&bytes)
                .map_err(|e| e.to_string())
                .and_then(|t| toml::from_str::<Settings>(t).map_err(|e| e.to_string()))
                .map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        };
        match parsed {
            Ok(s) => (store, s.unwrap_or_default(), None),
            Err(e) => {
                let mut broken = path.as_os_str().to_owned();
                broken.push(".broken");
                let broken = PathBuf::from(broken);
                tracing::warn!("unusable settings {}: {e}; moved to {}", path.display(), broken.display());
                let _ = std::fs::remove_file(&broken);
                let moved = std::fs::rename(path, &broken).is_ok().then_some(broken);
                (store, Settings::default(), moved)
            }
        }
    }

    /// Writes via a temp file and rename so a crash never leaves half a file.
    pub fn save(&self, s: &Settings) -> std::io::Result<()> {
        use std::io::Write;
        let text = toml::to_string(s).map_err(std::io::Error::other)?;
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp = self.path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let result = (|| {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &self.path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("settings.toml");
        (d, p)
    }

    #[test]
    fn defaults_when_missing() {
        let (_d, p) = path();
        let (_, s, broken) = SettingsStore::load(&p);
        assert_eq!(s, Settings::default());
        assert_eq!(s.vad_sensitivity, 0.5);
        assert!(s.sounds.mute && s.sounds.join_leave && s.sounds.chat);
        assert!(broken.is_none());
    }

    #[test]
    fn roundtrip_keeps_every_field() {
        let (_d, p) = path();
        let mut volumes = VolumeBook::default();
        volumes.set_percent("żółw", 20);
        volumes.toggle_muted("Zażółć");
        let s = Settings {
            server: "example.org:4433".into(),
            fingerprint: "ab12".into(),
            username: "Gżegżółka".into(),
            password_protected: "deadbeef".into(),
            capture_device: Some("Mikrofon".into()),
            render_device: Some("Słuchawki".into()),
            vad_sensitivity: 0.8,
            hotkey_mute: Some("Ctrl+Shift+M".into()),
            hotkey_deafen: Some("Ctrl+Shift+D".into()),
            sounds: Sounds { mute: false, join_leave: true, chat: false },
            autostart: true,
            run_as_admin: true,
            always_on_top: true,
            window: Some(WindowRect { x: -1200, y: 30, w: 800, h: 600 }),
            volumes,
        };
        let (store, _, _) = SettingsStore::load(&p);
        store.save(&s).unwrap();
        assert!(!p.with_extension("toml.tmp").exists());
        let (_, loaded, broken) = SettingsStore::load(&p);
        assert_eq!(loaded, s);
        assert!(broken.is_none());
    }

    #[test]
    fn partial_file_uses_defaults_for_the_rest() {
        let (_d, p) = path();
        std::fs::write(&p, "server = \"a:1\"\n[sounds]\nchat = false\n").unwrap();
        let (_, s, broken) = SettingsStore::load(&p);
        assert!(broken.is_none());
        assert_eq!(s.server, "a:1");
        assert!(!s.sounds.chat);
        assert!(s.sounds.mute && s.sounds.join_leave);
        assert_eq!(s.vad_sensitivity, 0.5);
        assert_eq!(s.window, None);
    }

    #[test]
    fn broken_settings_are_set_aside() {
        for junk in [&b"this is [not toml"[..], &[0xff, 0xfe, 0x00][..]] {
            let (d, p) = path();
            std::fs::write(&p, junk).unwrap();
            std::fs::write(d.path().join("settings.toml.broken"), b"older").unwrap();
            let (_, s, broken) = SettingsStore::load(&p);
            let expected = d.path().join("settings.toml.broken");
            assert_eq!(s, Settings::default());
            assert_eq!(broken, Some(expected.clone()));
            assert_eq!(std::fs::read(&expected).unwrap(), junk);
            assert!(!p.exists());
        }
    }

    #[test]
    fn is_configured_rules() {
        let full = Settings {
            server: "s".into(),
            fingerprint: "f".into(),
            username: "u".into(),
            password_protected: "p".into(),
            ..Settings::default()
        };
        assert!(full.is_configured());
        assert!(!Settings::default().is_configured());
        for clear in [
            |s: &mut Settings| s.server.clear(),
            |s: &mut Settings| s.fingerprint.clear(),
            |s: &mut Settings| s.username.clear(),
            |s: &mut Settings| s.password_protected.clear(),
        ] {
            let mut s = full.clone();
            clear(&mut s);
            assert!(!s.is_configured());
        }
    }
}
