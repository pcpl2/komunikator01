//! UTF-16 helpers for the W-suffixed Win32 APIs.

/// NUL-terminated UTF-16 copy of `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Decodes up to the first NUL (or the whole buffer).
pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Scales a 96-dpi pixel value to `dpi`, rounding half up.
pub fn scale(px: i32, dpi: u32) -> i32 {
    (px * dpi as i32 + 48).div_euclid(96)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_roundtrip_polish() {
        let w = wide("Zażółć gęślą jaźń");
        assert_eq!(*w.last().unwrap(), 0);
        assert_eq!(from_wide(&w), "Zażółć gęślą jaźń");
        assert_eq!(from_wide(&[72, 105]), "Hi");
    }

    #[test]
    fn scale_rounds() {
        assert_eq!(scale(100, 96), 100);
        assert_eq!(scale(100, 144), 150);
        assert_eq!(scale(10, 120), 13); // 12.5 rounds up
        assert_eq!(scale(7, 144), 11); // 10.5 rounds up
    }
}
