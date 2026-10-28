//! Wire format shared by the client and the server.

pub mod audio;
pub mod control;
pub mod framing;

pub use control::{
    normalize_room_name, ClientMsg, MemberId, MemberInfo, RejectReason, RoomChoice,
    RoomErrorReason, RoomId, RoomInfo, ServerMsg, DEFAULT_ROOM_NAME, MAX_CHAT_BLOB, MAX_ROOMS,
    MAX_ROOM_MEMBERS, MAX_ROOM_NAME_CHARS,
};
pub use framing::{encode_frame, FrameDecoder, FrameError, MAX_FRAME};

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u16 = 2;
/// Largest key-exchange blob accepted in Announce/Relay.
pub const MAX_BLOB: usize = 1024;
pub const ALPN: &[u8] = b"komunikator/1";
pub const KEEP_ALIVE_MS: u64 = 5_000;
pub const IDLE_TIMEOUT_MS: u64 = 15_000;
pub const DEFAULT_PORT: u16 = 7700;

/// Lowercase hex of a SHA-256 certificate fingerprint (64 chars).
pub fn fingerprint_hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_fingerprint(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_room_name_rules() {
        assert_eq!(normalize_room_name("  Gra  "), Some("Gra".to_string()));
        assert_eq!(normalize_room_name(""), None);
        assert_eq!(normalize_room_name("   "), None);
        assert!(normalize_room_name(&"ż".repeat(32)).is_some());
        assert_eq!(normalize_room_name(&"ż".repeat(33)), None);
        assert_eq!(normalize_room_name("a
b"), None);
    }

    #[test]
    fn fingerprint_hex_roundtrip() {
        let fp: [u8; 32] = std::array::from_fn(|i| i as u8 * 7);
        let hex = fingerprint_hex(&fp);
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, hex.to_lowercase());
        assert_eq!(parse_fingerprint(&hex), Some(fp));
        assert_eq!(parse_fingerprint("zz"), None);
        assert_eq!(parse_fingerprint(&"g".repeat(64)), None);
    }
}
