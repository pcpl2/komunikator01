//! The GUI starts with a configured settings file, shows its window and tray icon, saves
//! its settings (window position included) and exits cleanly.
#![cfg(windows)]

use std::process::Command;
use std::time::{Duration, Instant};

use client_core::settings::{Settings, SettingsStore};
use client_win::dpapi::store_password;

#[test]
fn gui_smoke_starts_saves_and_exits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    let mut s = Settings {
        // Nothing listens there: the session keeps reconnecting, which must not block exit.
        server: "127.0.0.1:1".into(),
        fingerprint: "ab".repeat(32),
        username: "smoke".into(),
        ..Settings::default()
    };
    store_password(&mut s, "haslo").unwrap();
    let (store, _, _) = SettingsStore::load(&path);
    store.save(&s).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_komunikator"))
        .arg("--smoke")
        .arg("--data-dir")
        .arg(dir.path())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > end {
            let _ = child.kill();
            panic!("komunikator --smoke did not exit within 10 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(0));

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.lines().any(|l| l.trim() == "[window]"), "no [window] table in:\n{text}");
    let (_, saved, broken) = SettingsStore::load(&path);
    assert!(broken.is_none());
    assert!(saved.window.is_some_and(|w| w.w > 0 && w.h > 0));
    assert_eq!(saved.username, "smoke");
}
