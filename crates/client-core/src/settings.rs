//! The settings file (`settings.toml`): connection data, devices, hotkeys, window and volumes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::audio::volume::VolumeBook;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Sounds {
    pub join_leave: bool,
    pub chat: bool,
}

impl Default for Sounds {
    fn default() -> Self {
        Self { join_leave: true, chat: true }
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
    pub capture_device: Option<String>,
    pub render_device: Option<String>,
    /// 0.0..=1.0
    pub vad_sensitivity: f32,
    /// RNNoise on the microphone before the VAD.
    pub noise_suppression: bool,
    /// Send the microphone all the time, without voice detection (mute still applies).
    pub continuous: bool,
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
            capture_device: None,
            render_device: None,
            vad_sensitivity: 0.5,
            noise_suppression: true,
            continuous: false,
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
    /// True when the server and the name are known; the password or session is kept apart
    /// (Windows Credential Manager).
    pub fn has_account(&self) -> bool {
        !self.server.is_empty() && !self.fingerprint.is_empty() && !self.username.is_empty()
    }

    /// Values a hand-edited file may get wrong, brought back into range.
    fn sanitized(mut self) -> Settings {
        self.vad_sensitivity =
            if self.vad_sensitivity.is_nan() { Settings::default().vad_sensitivity } else { self.vad_sensitivity.clamp(0.0, 1.0) };
        self
    }
}

pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// Loads the file. Missing means defaults. A file that cannot be read, is not UTF-8 or does
    /// not parse is set aside as `<file>.broken` (replacing an older one) and defaults are used.
    /// The third value tells the caller what to tell the user: `Some(Ok(path))` where the file
    /// went, `Some(Err(reason))` when it could not be set aside (the next save overwrites it).
    pub fn load(path: &Path) -> (SettingsStore, Settings, Option<Result<PathBuf, String>>) {
        let store = SettingsStore { path: path.to_path_buf() };
        let read = std::fs::read(path);
        let parsed = match &read {
            Ok(bytes) => std::str::from_utf8(bytes)
                .map_err(|e| e.to_string())
                .and_then(|t| toml::from_str::<Settings>(t).map_err(|e| e.to_string()))
                .map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        };
        match parsed {
            Ok(s) => (store, s.map(Settings::sanitized).unwrap_or_default(), None),
            Err(e) => {
                let mut broken = path.as_os_str().to_owned();
                broken.push(".broken");
                let broken = PathBuf::from(broken);
                let _ = std::fs::remove_file(&broken);
                // Renaming fails when another program holds the file open; a copy still keeps it.
                let set_aside = match (std::fs::rename(path, &broken), &read) {
                    (Ok(()), _) => Ok(broken),
                    (Err(_), Ok(bytes)) => std::fs::write(&broken, bytes).map(|()| broken).map_err(|e| e.to_string()),
                    (Err(e), Err(_)) => Err(e.to_string()),
                };
                match &set_aside {
                    Ok(p) => tracing::warn!("unusable settings {}: {e}; set aside as {}", path.display(), p.display()),
                    Err(why) => tracing::warn!("unusable settings {}: {e}; not set aside: {why}", path.display()),
                }
                (store, Settings::default(), Some(set_aside))
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
        assert!(s.sounds.join_leave && s.sounds.chat);
        assert!(s.noise_suppression);
        assert!(!s.continuous);
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
            capture_device: Some("Mikrofon".into()),
            render_device: Some("Słuchawki".into()),
            vad_sensitivity: 0.8,
            noise_suppression: false,
            continuous: true,
            hotkey_mute: Some("Ctrl+Shift+M".into()),
            hotkey_deafen: Some("Ctrl+Shift+D".into()),
            sounds: Sounds { join_leave: true, chat: false },
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
    fn old_mute_sound_key_is_ignored() {
        let (_d, p) = path();
        std::fs::write(&p, "[sounds]
mute = true
chat = false
").unwrap();
        let (_, s, broken) = SettingsStore::load(&p);
        assert!(broken.is_none());
        assert_eq!(s.sounds, Sounds { join_leave: true, chat: false });
    }

    #[test]
    fn partial_file_uses_defaults_for_the_rest() {
        let (_d, p) = path();
        std::fs::write(&p, "server = \"a:1\"\n[sounds]\nchat = false\n").unwrap();
        let (_, s, broken) = SettingsStore::load(&p);
        assert!(broken.is_none());
        assert_eq!(s.server, "a:1");
        assert!(!s.sounds.chat);
        assert!(s.sounds.join_leave);
        assert_eq!(s.vad_sensitivity, 0.5);
        assert_eq!(s.window, None);
    }

    #[test]
    fn file_from_before_noise_suppression_turns_it_on() {
        let (_d, p) = path();
        std::fs::write(&p, "vad_sensitivity = 0.3
").unwrap();
        let (_, s, _) = SettingsStore::load(&p);
        assert!(s.noise_suppression);
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
            assert_eq!(broken, Some(Ok(expected.clone())));
            assert_eq!(std::fs::read(&expected).unwrap(), junk);
            assert!(!p.exists());
        }
    }

    /// Rename fails (the file is open without delete sharing), so the bytes are copied instead.
    #[cfg(windows)]
    #[test]
    fn broken_settings_are_copied_when_they_cannot_be_moved() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        let (d, p) = path();
        std::fs::write(&p, b"this is [not toml").unwrap();
        let _held = std::fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ).open(&p).unwrap();
        let (_, s, broken) = SettingsStore::load(&p);
        let expected = d.path().join("settings.toml.broken");
        assert_eq!(s, Settings::default());
        assert_eq!(broken, Some(Ok(expected.clone())));
        assert_eq!(std::fs::read(&expected).unwrap(), b"this is [not toml");
        assert!(p.exists(), "rename should have failed, leaving the original");
    }

    #[test]
    fn broken_settings_that_cannot_be_set_aside_report_why() {
        let (d, p) = path();
        std::fs::write(&p, b"this is [not toml").unwrap();
        // A directory where the copy should go: neither rename nor write can succeed.
        std::fs::create_dir(d.path().join("settings.toml.broken")).unwrap();
        let (_, s, broken) = SettingsStore::load(&p);
        assert_eq!(s, Settings::default());
        assert!(matches!(broken, Some(Err(ref reason)) if !reason.is_empty()), "{broken:?}");
    }

    #[test]
    fn vad_sensitivity_is_clamped_and_nan_is_default() {
        for (text, want) in [("nan", 0.5), ("-3.0", 0.0), ("7.5", 1.0), ("0.25", 0.25), ("inf", 1.0)] {
            let (_d, p) = path();
            std::fs::write(&p, format!("vad_sensitivity = {text}\n")).unwrap();
            let (_, s, broken) = SettingsStore::load(&p);
            assert!(broken.is_none());
            assert_eq!(s.vad_sensitivity, want, "{text}");
        }
    }

    #[test]
    fn save_creates_missing_directories() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("komunikator").join("nested").join("settings.toml");
        let (store, s, _) = SettingsStore::load(&p);
        store.save(&s).unwrap();
        let (_, loaded, broken) = SettingsStore::load(&p);
        assert!(broken.is_none());
        assert_eq!(loaded, s);
    }

    #[test]
    fn has_account_rules() {
        let full = Settings { server: "s".into(), fingerprint: "f".into(), username: "u".into(), ..Settings::default() };
        assert!(full.has_account());
        assert!(!Settings::default().has_account());
        for clear in [
            |s: &mut Settings| s.server.clear(),
            |s: &mut Settings| s.fingerprint.clear(),
            |s: &mut Settings| s.username.clear(),
        ] {
            let mut s = full.clone();
            clear(&mut s);
            assert!(!s.has_account());
        }
    }
}
