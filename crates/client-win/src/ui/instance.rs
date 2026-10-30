//! One running copy per data directory. A second start hands over to the first (shows its
//! window) instead of logging in again, which would replace the first one's session while
//! the first kept the global hotkeys.

use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, HANDLE, LPARAM, WIN32_ERROR, WPARAM,
};
use windows::Win32::System::Threading::{CreateMutexW, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, PostMessageW, RegisterWindowMessageW,
};

use super::wide::wide;
use super::window::CLASS;

/// The registered message a second start posts to the running window.
pub const SHOW_MESSAGE: PCWSTR = windows::core::w!("komunikator-show");

/// One fixed name for every copy, whatever its data directory: the installer (Inno Setup
/// `AppMutex`, installer/komunikator.iss) asks to close the program while it exists.
pub const APP_MUTEX: &str = "komunikator-app";

/// Held while this copy runs (its data-directory mutex, then the app mutex); the names are
/// free again once it drops.
pub struct InstanceGuard(Option<HANDLE>, Option<HANDLE>);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        for h in [self.0, self.1].into_iter().flatten() {
            let _ = unsafe { CloseHandle(h) };
        }
    }
}

/// `Some` when this is the only copy for `data_dir`. Otherwise asks the running copy to show
/// its window and returns `None`; the caller exits.
pub fn claim(data_dir: &Path) -> Option<InstanceGuard> {
    let Some(mut guard) = try_claim(data_dir) else {
        show_running();
        return None;
    };
    guard.1 = hold_app_mutex();
    Some(guard)
}

/// Creates (or joins) the app mutex; `None` only when Windows refuses, which costs no more
/// than the installer not noticing this copy.
fn hold_app_mutex() -> Option<HANDLE> {
    let name = wide(APP_MUTEX);
    unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.ok()
}

/// A window program is running for `data_dir`. Only looks: the name stays free for one that
/// starts now.
pub fn running(data_dir: &Path) -> bool {
    let name = wide(&mutex_name(data_dir));
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) } {
        Ok(handle) => {
            let _ = unsafe { CloseHandle(handle) };
            true
        }
        // An elevated copy's mutex may exist and still refuse to open.
        Err(e) => WIN32_ERROR::from_error(&e) == Some(ERROR_ACCESS_DENIED),
    }
}

/// The mutex alone: `None` when another copy holds it.
fn try_claim(data_dir: &Path) -> Option<InstanceGuard> {
    let name = wide(&mutex_name(data_dir));
    let created = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) };
    let code = match &created {
        Ok(_) => Ok(unsafe { GetLastError() }),
        Err(e) => Err(WIN32_ERROR::from_error(e).unwrap_or_default()),
    };
    match (outcome(code), created) {
        (Claimed::Ours, Ok(handle)) => Some(InstanceGuard(Some(handle), None)),
        (Claimed::Running, Ok(handle)) => {
            let _ = unsafe { CloseHandle(handle) };
            None
        }
        (Claimed::Running, Err(_)) => None,
        _ => Some(InstanceGuard(None, None)),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Claimed {
    Ours,
    Running,
    /// No mutex at all; running twice beats not running at all.
    NoGuard,
}

/// What `CreateMutexW` said: `Ok(last error)` with a handle, `Err(code)` without one.
fn outcome(created: Result<WIN32_ERROR, WIN32_ERROR>) -> Claimed {
    match created {
        Ok(ERROR_ALREADY_EXISTS) => Claimed::Running,
        Ok(_) => Claimed::Ours,
        // An elevated copy's mutex carries its token's DACL; a non-elevated start may not open it.
        Err(ERROR_ACCESS_DENIED) => Claimed::Running,
        Err(_) => Claimed::NoGuard,
    }
}

fn show_running() {
    unsafe {
        // Not found when the other copy is still in its first-run dialog; nothing to show.
        let Ok(hwnd) = FindWindowW(CLASS, PCWSTR::null()) else { return };
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        // We were just started by the user, so we may pass the right to take the foreground.
        let _ = AllowSetForegroundWindow(pid);
        let _ = PostMessageW(Some(hwnd), RegisterWindowMessageW(SHOW_MESSAGE), WPARAM(0), LPARAM(0));
    }
}

/// `Local\komunikator-<hash>`: per session, per data directory (case and a trailing
/// separator ignored, as Windows paths compare). FNV-1a, so every build agrees.
fn mutex_name(data_dir: &Path) -> String {
    let path = data_dir.to_string_lossy().to_lowercase();
    let path = path.trim_end_matches(['\\', '/']);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("Local\\komunikator-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutex_name_depends_on_the_data_dir_only() {
        let a = mutex_name(Path::new(r"C:\Users\Ola\AppData\Roaming\komunikator"));
        assert_eq!(a, mutex_name(Path::new(r"c:\users\ola\appdata\roaming\KOMUNIKATOR\")));
        assert_ne!(a, mutex_name(Path::new(r"C:\Users\Ewa\AppData\Roaming\komunikator")));
        assert!(a.starts_with("Local\\komunikator-"));
        // Kernel object names allow no further backslash after the namespace.
        assert_eq!(a.matches('\\').count(), 1);
        assert_eq!(a.len(), "Local\\komunikator-".len() + 16);
    }

    #[test]
    fn second_claim_fails_until_the_first_is_released() {
        // A fresh directory, so no real copy of the program is involved.
        let dir = tempfile::tempdir().unwrap();
        let first = try_claim(dir.path());
        assert!(first.as_ref().is_some_and(|g| g.0.is_some()));
        assert!(try_claim(dir.path()).is_none());
        let other = tempfile::tempdir().unwrap();
        assert!(try_claim(other.path()).is_some());
        drop(first);
        assert!(try_claim(dir.path()).is_some());
    }

    #[test]
    fn running_sees_a_window_program_without_claiming_its_name() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!running(dir.path()));
        // Looking must not take the name: a window program started now still gets it.
        let guard = try_claim(dir.path());
        assert!(guard.as_ref().is_some_and(|g| g.0.is_some()));
        assert!(running(dir.path()));
        drop(guard);
        assert!(!running(dir.path()));
    }

    #[test]
    fn the_installer_sees_the_fixed_app_mutex() {
        let held = hold_app_mutex();
        assert!(held.is_some());
        let name = wide(APP_MUTEX);
        let opened = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) };
        let _ = unsafe { CloseHandle(opened.unwrap()) };
        // No namespace prefix: Inno Setup's AppMutex looks the name up in the user's session.
        assert!(!APP_MUTEX.contains('\\'));
        let _ = unsafe { CloseHandle(held.unwrap()) };
    }

    #[test]
    fn mutex_held_by_an_elevated_copy_counts_as_running() {
        use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_NAME, ERROR_SUCCESS};
        assert_eq!(outcome(Ok(ERROR_SUCCESS)), Claimed::Ours);
        assert_eq!(outcome(Ok(ERROR_ALREADY_EXISTS)), Claimed::Running);
        // A non-elevated start may not even open an elevated copy's mutex.
        assert_eq!(outcome(Err(ERROR_ACCESS_DENIED)), Claimed::Running);
        assert_eq!(outcome(Err(ERROR_INVALID_NAME)), Claimed::NoGuard);
    }
}
