//! Invite strings: everything a friend needs to join, as one block of text to paste.

/// Longest host name DNS allows; with `:port` it still fits the one-byte length.
pub const MAX_SERVER_LEN: usize = 253;

const PREFIX: &str = "kom1";
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const GROUP: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    /// `host:port`
    pub server: String,
    /// SHA-256 of the server certificate.
    pub fingerprint: [u8; 32],
    pub code: [u8; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InviteError {
    #[error("to nie wygląda na zaproszenie")]
    Format,
    #[error("zaproszenie zawiera literówkę, sprawdź je i wpisz jeszcze raz")]
    Checksum,
}

/// `kom1-` + Crockford base32 of `[len][server][fingerprint][code][crc16]`, in groups of five.
///
/// Panics when the server address is longer than [`MAX_SERVER_LEN`] (callers validate it).
pub fn encode_invite(i: &Invite) -> String {
    assert!(i.server.len() <= MAX_SERVER_LEN, "server address too long");
    let mut bytes = vec![i.server.len() as u8];
    bytes.extend_from_slice(i.server.as_bytes());
    bytes.extend_from_slice(&i.fingerprint);
    bytes.extend_from_slice(&i.code);
    bytes.extend_from_slice(&crc16(&bytes).to_be_bytes());
    let text = base32_encode(&bytes);
    let groups: Vec<&str> =
        text.as_bytes().chunks(GROUP).map(|c| std::str::from_utf8(c).expect("ascii")).collect();
    format!("{PREFIX}-{}", groups.join("-"))
}

/// Case-insensitive; skips `-`, spaces and newlines; reads `O` as 0 and `I`/`L` as 1.
pub fn decode_invite(s: &str) -> Result<Invite, InviteError> {
    let s = s.trim();
    let rest = match s.get(..PREFIX.len()) {
        Some(p) if p.eq_ignore_ascii_case(PREFIX) => &s[PREFIX.len()..],
        _ => return Err(InviteError::Format),
    };
    let bytes = base32_decode(rest).ok_or(InviteError::Format)?;
    let Some(split) = bytes.len().checked_sub(2) else { return Err(InviteError::Format) };
    let (body, crc) = bytes.split_at(split);
    // Checked before the structure so any single typo reads as a typo, not as garbage.
    if crc16(body).to_be_bytes() != crc {
        return Err(InviteError::Checksum);
    }
    let (&len, body) = body.split_first().ok_or(InviteError::Format)?;
    let len = len as usize;
    if len == 0 || len > MAX_SERVER_LEN || body.len() != len + 32 + 16 {
        return Err(InviteError::Format);
    }
    let server = std::str::from_utf8(&body[..len]).map_err(|_| InviteError::Format)?;
    let fingerprint = body[len..len + 32].try_into().expect("32 bytes");
    let code = body[len + 32..].try_into().expect("16 bytes");
    Ok(Invite { server: server.to_string(), fingerprint, code })
}

/// CRC-16/CCITT-FALSE.
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ALPHABET[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// `None` for a character outside the alphabet or for padding bits that are not zero.
fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.chars() {
        if c == '-' || c.is_whitespace() {
            continue;
        }
        let value = match c.to_ascii_uppercase() {
            'O' => 0,
            'I' | 'L' => 1,
            u => ALPHABET.iter().position(|&a| a as char == u)? as u32,
        };
        acc = (acc << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (bits < 5 && acc == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(server: &str) -> Invite {
        Invite {
            server: server.to_string(),
            fingerprint: std::array::from_fn(|i| (i * 7 + 3) as u8),
            code: std::array::from_fn(|i| (255 - i * 11) as u8),
        }
    }

    #[test]
    fn invite_roundtrip() {
        for server in ["serwer.example.pl:7700", "[::1]:7700", "127.0.0.1:1", &"a".repeat(253)] {
            let invite = sample(server);
            let text = encode_invite(&invite);
            assert!(text.starts_with("kom1-"));
            assert!(text[5..].split('-').all(|g| (1..=5).contains(&g.len())));
            assert_eq!(decode_invite(&text), Ok(invite), "{server}");
        }
    }

    #[test]
    fn invite_tolerates_case_spaces_and_confusables() {
        let invite = sample("serwer.example.pl:7700");
        let text = encode_invite(&invite);
        assert_eq!(decode_invite(&text.to_lowercase()), Ok(invite.clone()));
        let spaced = format!("  {}\r\n", text.replace('-', " "));
        assert_eq!(decode_invite(&spaced), Ok(invite.clone()));
        let nodash = text.replace('-', "");
        assert_eq!(decode_invite(&nodash), Ok(invite.clone()));
        // 0 -> O, 1 -> I or L, in the data part only (the prefix is literal)
        let confused = format!("{}{}", &text[..5], text[5..].replace('0', "o").replace('1', "l"));
        assert_ne!(confused, text);
        assert_eq!(decode_invite(&confused), Ok(invite));
    }

    #[test]
    fn invite_detects_typos() {
        let text = encode_invite(&sample("serwer.example.pl:7700"));
        let data_chars: Vec<usize> =
            (5..text.len() - 1).filter(|&i| text.as_bytes()[i] != b'-').collect();
        for &i in &data_chars {
            let old = text.as_bytes()[i];
            let new = if old == b'2' { b'3' } else { b'2' };
            let mut bad = text.clone().into_bytes();
            bad[i] = new;
            let bad = String::from_utf8(bad).unwrap();
            assert_eq!(decode_invite(&bad), Err(InviteError::Checksum), "position {i}");
        }
        let truncated = &text[..text.len() - 6];
        assert!(decode_invite(truncated).is_err());
    }

    #[test]
    fn invite_rejects_garbage() {
        for s in ["", "   ", "hello", "kom1", "kom1-", "kom2-ABCDE", "kom1-UUUUU", "kom1-ŻŻŻŻŻ", "kom1-0000-0000"] {
            assert!(decode_invite(s).is_err(), "{s:?}");
        }
        assert_eq!(decode_invite("kom1-?????"), Err(InviteError::Format));
    }
}
