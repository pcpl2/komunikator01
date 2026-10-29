//! Replacing the running exe with a verified release (`client_core::update` checks it first).
//! Windows lets a running exe be renamed but not overwritten, so the new one is written
//! beside it (`.new`), the running one is renamed aside (`.old`) and the new one takes its name.
//! Only these two fixed names are ever written: the manifest's file name is never a local path.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use protocol::update::Version;

/// The release public key: 64 hex chars on the first line that is not a comment. All zeros
/// (the committed placeholder) means this build does not update itself.
pub const RELEASE_KEY: &str = include_str!("../release-key.pub");

const UPDATED_FLAG: &str = "--updated";

/// The built-in key, or `None` when updates are disabled.
pub fn release_key() -> Option<[u8; 32]> {
    parse_release_key(RELEASE_KEY)
}

fn parse_release_key(text: &str) -> Option<[u8; 32]> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#'))?;
    protocol::parse_fingerprint(line).filter(|k| *k != [0; 32])
}

/// The exe's real name for the running file: a copy started as `komunikator.exe.old` (after
/// a swap cut short) still means `komunikator.exe`, so cleanup can put it back.
pub fn canonical_exe(running: &Path) -> PathBuf {
    match running.file_name().and_then(OsStr::to_str) {
        Some(name) if name.len() > 4 && name.to_ascii_lowercase().ends_with(".old") => {
            running.with_file_name(&name[..name.len() - 4])
        }
        _ => running.to_owned(),
    }
}

/// Written by the window program at every start, so `komunikator-cli --check-update` knows
/// which version it would replace (and never installs an older one).
const VERSION_STAMP: &str = "komunikator.version";

pub fn write_version_stamp(exe: &Path, v: Version) -> io::Result<()> {
    fs::write(exe.with_file_name(VERSION_STAMP), format!("{v}\n"))
}

/// The newer of `own` and the version the window program next to `exe` last stamped.
pub fn installed_version(exe: &Path, own: Version) -> Version {
    let stamp = fs::read_to_string(exe.with_file_name(VERSION_STAMP)).ok();
    stamp.and_then(|s| Version::parse(s.trim())).map_or(own, |s| s.max(own))
}

/// A file error as the player should read it, without raw system codes.
pub fn describe_io(e: &io::Error) -> &'static str {
    const SHARING_VIOLATION: i32 = 32;
    const LOCK_VIOLATION: i32 = 33;
    if matches!(e.raw_os_error(), Some(SHARING_VIOLATION | LOCK_VIOLATION)) {
        return "plik programu jest używany przez inny proces";
    }
    match e.kind() {
        io::ErrorKind::PermissionDenied => "brak prawa zapisu w folderze programu",
        io::ErrorKind::NotFound => "brak pliku programu",
        io::ErrorKind::StorageFull => "brak miejsca na dysku",
        _ => "nie udało się zapisać pliku programu",
    }
}

fn beside(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or(OsStr::new("komunikator.exe")).to_owned();
    name.push(suffix);
    exe.with_file_name(name)
}

/// Writes the verified release next to the exe and flushes it to disk.
pub fn stage(exe: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let new = beside(exe, ".new");
    let mut f = fs::File::create(&new)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(new)
}

/// Puts the staged release in place of the exe; on failure the exe is left as it was.
pub fn swap(exe: &Path) -> io::Result<()> {
    let (old, new) = (beside(exe, ".old"), beside(exe, ".new"));
    // Left by an earlier update; if it is still locked, the rename below fails cleanly.
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old)?;
    if let Err(e) = fs::rename(&new, exe) {
        let _ = fs::rename(&old, exe);
        return Err(e);
    }
    Ok(())
}

/// Undoes a swap interrupted between its two renames, then removes what an update left.
/// The old copy may still be running for a moment after a restart; then it is removed later.
pub fn cleanup_on_start(exe: &Path) {
    let (old, new) = (beside(exe, ".old"), beside(exe, ".new"));
    if !exe.exists() && old.exists() {
        if let Err(e) = fs::rename(&old, exe) {
            tracing::warn!("could not restore {}: {e}", exe.display());
        }
    }
    for p in [old, new] {
        if p.exists() {
            let _ = fs::remove_file(p);
        }
    }
}

/// Starts the updated exe on its own, with the same arguments plus `--updated`; the caller
/// then exits (after releasing anything the new copy needs, like the single-instance guard).
pub fn restart(exe: &Path, args: &[OsString]) -> io::Result<()> {
    let mut cmd = Command::new(exe);
    cmd.args(restart_args(args));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(DETACHED_PROCESS);
    }
    cmd.spawn().map(drop)
}

fn restart_args(args: &[OsString]) -> Vec<OsString> {
    let mut out: Vec<OsString> = args.iter().filter(|a| *a != UPDATED_FLAG).cloned().collect();
    out.push(UPDATED_FLAG.into());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZEROS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    /// A stand-in exe in a fresh temp dir: no real binary is ever touched.
    fn exe(dir: &tempfile::TempDir) -> PathBuf {
        let exe = dir.path().join("komunikator.exe");
        std::fs::write(&exe, b"stary").unwrap();
        exe
    }

    fn read(p: &Path) -> Vec<u8> {
        std::fs::read(p).unwrap()
    }

    #[test]
    fn stage_and_swap_replace_the_exe() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe(&dir);
        let new = stage(&exe, b"nowy").unwrap();
        assert_eq!(new, dir.path().join("komunikator.exe.new"));
        assert_eq!(read(&new), b"nowy");
        swap(&exe).unwrap();
        assert_eq!(read(&exe), b"nowy");
        assert_eq!(read(&dir.path().join("komunikator.exe.old")), b"stary");
        assert!(!new.exists());
        // The next start clears what is left.
        cleanup_on_start(&exe);
        assert_eq!(read(&exe), b"nowy");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn swap_rolls_back_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe(&dir);
        // Nothing staged: the second rename fails and the old exe goes back.
        assert!(swap(&exe).is_err());
        assert_eq!(read(&exe), b"stary");
        assert!(!dir.path().join("komunikator.exe.old").exists());
    }

    #[test]
    fn swap_replaces_a_stale_old_copy() {
        let dir = tempfile::tempdir().unwrap();
        let exe = exe(&dir);
        std::fs::write(dir.path().join("komunikator.exe.old"), b"bardzo stary").unwrap();
        stage(&exe, b"nowy").unwrap();
        swap(&exe).unwrap();
        assert_eq!(read(&exe), b"nowy");
        assert_eq!(read(&dir.path().join("komunikator.exe.old")), b"stary");
    }

    #[test]
    fn interrupted_swap_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("komunikator.exe");
        // Stopped between the two renames: the exe is gone, the old one sits aside.
        std::fs::write(dir.path().join("komunikator.exe.old"), b"stary").unwrap();
        std::fs::write(dir.path().join("komunikator.exe.new"), b"nowy").unwrap();
        cleanup_on_start(&exe);
        assert_eq!(read(&exe), b"stary");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn zero_key_disables_updates() {
        let text = format!("# komentarz\n\n{ZEROS}\n");
        assert_eq!(parse_release_key(&text), None);
        let real = format!("# komentarz\n{}\n", "ab".repeat(32));
        assert_eq!(parse_release_key(&real), Some([0xab; 32]));
        assert_eq!(parse_release_key(&format!("{}\r\n", "01".repeat(32))), Some([1; 32]));
        for bad in ["", "# tylko komentarz\n", "abc\n", &"zz".repeat(32)] {
            assert_eq!(parse_release_key(bad), None, "{bad:?}");
        }
        // The built-in file is well formed: a real key or the placeholder.
        let line = RELEASE_KEY.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#'));
        assert!(line.and_then(protocol::parse_fingerprint).is_some(), "release-key.pub: {RELEASE_KEY:?}");
    }

    #[test]
    fn started_from_the_old_copy_means_the_real_name() {
        let dir = Path::new(r"C:\Gry\Komunikator");
        assert_eq!(canonical_exe(&dir.join("komunikator.exe")), dir.join("komunikator.exe"));
        assert_eq!(canonical_exe(&dir.join("komunikator.exe.old")), dir.join("komunikator.exe"));
        assert_eq!(canonical_exe(&dir.join("komunikator.exe.OLD")), dir.join("komunikator.exe"));
        assert_eq!(canonical_exe(&dir.join(".old")), dir.join(".old"));
    }

    #[test]
    fn interrupted_swap_recovers_when_started_from_the_old_copy() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("komunikator.exe.old");
        std::fs::write(&old, b"stary").unwrap();
        let exe = canonical_exe(&old);
        cleanup_on_start(&exe);
        assert_eq!(read(&exe), b"stary");
        assert!(!old.exists());
    }

    #[test]
    fn io_errors_are_described_in_polish() {
        use std::io::{Error, ErrorKind};
        assert_eq!(describe_io(&Error::from(ErrorKind::PermissionDenied)), "brak prawa zapisu w folderze programu");
        assert_eq!(describe_io(&Error::from_raw_os_error(5)), "brak prawa zapisu w folderze programu");
        assert_eq!(describe_io(&Error::from_raw_os_error(32)), "plik programu jest używany przez inny proces");
        assert!(!describe_io(&Error::other("x")).contains("os error"));
    }

    #[test]
    fn the_cli_never_installs_an_older_release_than_the_window_program() {
        use client_core::update::{decide, Decision};
        use protocol::update::{Manifest, Version};
        let dir = tempfile::tempdir().unwrap();
        let exe = exe(&dir);
        let own = Version { major: 1, minor: 0, patch: 0 };
        assert_eq!(installed_version(&exe, own), own, "no stamp yet");
        write_version_stamp(&exe, Version { major: 9, minor: 0, patch: 0 }).unwrap();
        let installed = installed_version(&exe, own);
        assert_eq!(installed, Version { major: 9, minor: 0, patch: 0 });
        let m = Manifest { version: Version { major: 5, minor: 0, patch: 0 }, file: "k.exe".into(), size: 1, sha256: [0; 32] };
        assert_eq!(decide(&m, installed), Decision::UpToDate);
        // An older stamp never lowers the bar.
        write_version_stamp(&exe, Version { major: 0, minor: 9, patch: 0 }).unwrap();
        assert_eq!(installed_version(&exe, own), own);
        std::fs::write(dir.path().join("komunikator.version"), "śmieci").unwrap();
        assert_eq!(installed_version(&exe, own), own);
    }

    #[test]
    fn restart_args_carry_the_updated_flag_once() {
        let args: Vec<OsString> = ["--minimized", "--updated", "--data-dir", "x"].map(OsString::from).into();
        assert_eq!(restart_args(&args), ["--minimized", "--data-dir", "x", "--updated"].map(OsString::from));
    }
}
