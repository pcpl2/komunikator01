//! Wire format shared by the client and the server.

pub mod audio;
pub mod control;
pub mod framing;

pub use control::{
    hello_version, normalize_room_name, normalize_username, ClientMsg, MemberId, MemberInfo, RejectReason, RoomChoice,
    RoomErrorReason, RoomId, RoomInfo, ServerMsg, DEFAULT_ROOM_NAME, MAX_CHAT_BLOB, MAX_ROOMS,
    MAX_ROOM_MEMBERS, MAX_ROOM_NAME_CHARS, MAX_USERNAME_CHARS,
};
pub use framing::{encode_frame, FrameDecoder, FrameError, MAX_FRAME};

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u16 = 3;
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
    fn hello_and_rejected_keep_their_wire_index() {
        let hello = ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            username: "ala".into(),
            password: "x".into(),
            session: 1,
            room: RoomChoice::Default,
        };
        assert_eq!(encode_frame(&hello)[4], 0);
        let rejected = ServerMsg::Rejected { reason: RejectReason::BadCredentials };
        assert_eq!(encode_frame(&rejected)[4], 3);
    }

    #[test]
    fn hello_version_reads_any_hello_prefix() {
        #[derive(serde::Serialize)]
        enum V2 {
            Hello { version: u16, username: String, password: String, session: u64 },
        }
        let v2 = V2::Hello { version: 2, username: "ala".into(), password: "x".into(), session: 1 };
        let frame = encode_frame(&v2);
        assert_eq!(hello_version(&frame[4..]), Some(2));
        let announce = encode_frame(&ClientMsg::Announce { blob: vec![1] });
        assert_eq!(hello_version(&announce[4..]), None);
        let mut dec = FrameDecoder::new();
        dec.push(&frame);
        assert!(dec.next::<ClientMsg>().is_err());
        assert_eq!(dec.front_body(), Some(&frame[4..]));
    }

    #[test]
    fn username_rules() {
        assert_eq!(normalize_username("Ala"), Some("Ala".to_string()));
        assert_eq!(normalize_username("ża_1.x-y"), Some("ża_1.x-y".to_string()));
        assert_eq!(normalize_username(""), None);
        assert_eq!(normalize_username("a b"), None);
        assert_eq!(normalize_username("a\u{202E}b"), None);
        assert_eq!(normalize_username("a\u{0007}b"), None);
        assert!(normalize_username(&"a".repeat(32)).is_some());
        assert_eq!(normalize_username(&"a".repeat(33)), None);
    }

    #[test]
    fn normalize_room_name_rules() {
        assert_eq!(normalize_room_name("  Gra  "), Some("Gra".to_string()));
        assert_eq!(normalize_room_name(""), None);
        assert_eq!(normalize_room_name("   "), None);
        assert!(normalize_room_name(&"ż".repeat(32)).is_some());
        assert_eq!(normalize_room_name(&"ż".repeat(33)), None);
        assert_eq!(normalize_room_name("a
b"), None);
        for bidi in ['\u{200E}', '\u{200F}', '\u{202A}', '\u{202E}', '\u{2066}', '\u{2069}'] {
            assert_eq!(normalize_room_name(&format!("Gra{bidi}x")), None, "{bidi:?}");
        }
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
