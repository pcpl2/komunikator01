//! Per-user (CurrentUser scope) DPAPI helpers.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

/// Runs a DPAPI call and copies the result out of the `LocalAlloc`ed output blob.
fn with_blob(
    input: &[u8],
    call: impl FnOnce(&CRYPT_INTEGER_BLOB, &mut CRYPT_INTEGER_BLOB) -> windows::core::Result<()>,
) -> anyhow::Result<Vec<u8>> {
    let len = u32::try_from(input.len())?;
    let inb = CRYPT_INTEGER_BLOB { cbData: len, pbData: input.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    call(&inb, &mut out)?;
    if out.pbData.is_null() {
        anyhow::bail!("DPAPI returned no data");
    }
    // SAFETY: on success DPAPI hands back `cbData` bytes at `pbData`, allocated with LocalAlloc;
    // we copy them and free the buffer exactly once.
    let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(out.pbData as *mut _)));
    }
    Ok(bytes)
}

pub fn protect(plain: &[u8]) -> anyhow::Result<Vec<u8>> {
    with_blob(plain, |inb, out| unsafe {
        CryptProtectData(inb, PCWSTR::null(), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, out)
    })
}

pub fn unprotect(blob: &[u8]) -> anyhow::Result<Vec<u8>> {
    with_blob(blob, |inb, out| unsafe {
        CryptUnprotectData(inb, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, out)
    })
}
