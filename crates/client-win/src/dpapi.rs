//! Per-user (CurrentUser scope) DPAPI helpers and the protected settings password.

use client_core::settings::Settings;
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.is_ascii() || s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Stores the password in `s` as hex of its DPAPI blob; only this Windows user can read it back.
pub fn store_password(s: &mut Settings, pw: &str) -> anyhow::Result<()> {
    s.password_protected = hex(&protect(pw.as_bytes())?);
    Ok(())
}

/// The stored password, or `None` when unset, undecryptable (other user/machine) or not UTF-8.
pub fn load_password(s: &Settings) -> Option<String> {
    let blob = unhex(&s.password_protected).filter(|b| !b.is_empty())?;
    String::from_utf8(unprotect(&blob).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_stored_protected() {
        let mut s = Settings::default();
        store_password(&mut s, "tajne hasło 123").unwrap();
        let pw = "tajne hasło 123";
        assert!(!s.password_protected.is_empty());
        assert!(!s.password_protected.contains(pw));
        assert!(!s.password_protected.contains(&hex(pw.as_bytes())));
        assert_eq!(load_password(&s).as_deref(), Some(pw));
    }

    #[test]
    fn unset_or_garbage_password_loads_as_none() {
        let mut s = Settings::default();
        assert_eq!(load_password(&s), None);
        s.password_protected = "zz".into();
        assert_eq!(load_password(&s), None);
        s.password_protected = "00ff".into();
        assert_eq!(load_password(&s), None);
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(unhex("00ABff"), Some(vec![0x00, 0xab, 0xff]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }
}
