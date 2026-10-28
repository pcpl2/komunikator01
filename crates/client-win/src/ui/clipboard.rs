//! Clipboard text (CF_UNICODETEXT).
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

use super::wide::wide;

const CF_UNICODETEXT: u32 = 13;

/// Replaces the clipboard with `text`. False when the clipboard is busy or allocation fails.
pub fn set_text(hwnd: HWND, text: &str) -> bool {
    let w = wide(text);
    let bytes = w.len() * 2;
    unsafe {
        if OpenClipboard(Some(hwnd)).is_err() {
            return false;
        }
        let ok = (|| {
            EmptyClipboard().ok()?;
            let mem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes).ok()?;
            let dst = GlobalLock(mem);
            if dst.is_null() {
                let _ = GlobalFree(Some(mem));
                return None;
            }
            std::ptr::copy_nonoverlapping(w.as_ptr().cast::<u8>(), dst.cast::<u8>(), bytes);
            let _ = GlobalUnlock(mem);
            // On success the clipboard owns `mem`; on failure we still do.
            if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0))).is_err() {
                let _ = GlobalFree(Some(mem));
                return None;
            }
            Some(())
        })()
        .is_some();
        let _ = CloseClipboard();
        ok
    }
}
