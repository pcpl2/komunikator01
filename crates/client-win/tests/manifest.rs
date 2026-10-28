//! The client binary must carry the application manifest (common controls v6, per-monitor DPI).
#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Foundation::FreeLibrary;
use windows::Win32::System::LibraryLoader::{
    FindResourceW, LoadLibraryExW, LOAD_LIBRARY_AS_DATAFILE,
};

const RT_MANIFEST: usize = 24;

#[test]
fn exe_has_manifest() {
    let exe = env!("CARGO_BIN_EXE_komunikator");
    let wide: Vec<u16> = exe.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let module = LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_LIBRARY_AS_DATAFILE).unwrap();
        let res = FindResourceW(
            Some(module),
            PCWSTR(1 as *const u16),
            PCWSTR(RT_MANIFEST as *const u16),
        );
        let found = !res.is_invalid();
        let _ = FreeLibrary(module);
        assert!(found, "RT_MANIFEST id 1 missing in {exe}");
    }
}
