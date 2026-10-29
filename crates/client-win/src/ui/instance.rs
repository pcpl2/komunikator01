//! One running copy per data directory. A second start hands over to the first (shows its
//! window) instead of logging in again, which would replace the first one's session while
//! the first kept the global hotkeys.

use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, FindWindowW, GetWindowThreadProcessId, PostMessageW, RegisterWindowMessageW,
};

use super::wide::wide;
use super::window::CLASS;

/// The registered message a second start posts to the running window.
pub const SHOW_MESSAGE: PCWSTR = windows::core::w!("komunikator-show");

/// Held while this copy runs; the name is free again once it drops.
pub struct InstanceGuard(Option<HANDLE>);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            let _ = unsafe { CloseHandle(h) };
        }
    }
}

/// `Some` when this is the only copy for `data_dir`. Otherwise asks the running copy to show
/// its window and returns `None`; the caller exits.
pub fn claim(data_dir: &Path) -> Option<InstanceGuard> {
    let guard = try_claim(data_dir);
    if guard.is_none() {
        show_running();
    }
    guard
}

/// The mutex alone: `None` when another copy holds it.
fn try_claim(data_dir: &Path) -> Option<InstanceGuard> {
    let name = wide(&mutex_name(data_dir));
    // Without a mutex there is no guard; running twice beats not running at all.
    let Ok(handle) = (unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }) else {
        return Some(InstanceGuard(None));
    };
    if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return Some(InstanceGuard(Some(handle)));
    }
    let _ = unsafe { CloseHandle(handle) };
    None
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
}
