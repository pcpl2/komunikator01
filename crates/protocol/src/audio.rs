//! Audio datagram layout.
//!
//! Upstream (client → server):   `[kind u8][seq u32 LE][payload]`
//! Downstream (server → client): `[kind u8][sender u16 LE][seq u32 LE][payload]`
//!
//! The server stamps the sender from the connection, so clients cannot spoof it.

use crate::MemberId;

pub const AUDIO_KIND: u8 = 1;
pub const MAX_AUDIO_PAYLOAD: usize = 1000;
/// Datagrams larger than this are a protocol violation.
pub const MAX_DATAGRAM: usize = 1200;

const UP_HEADER: usize = 1 + 4;
const DOWN_HEADER: usize = 1 + 2 + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DatagramError {
    #[error("datagram too short")]
    TooShort,
    #[error("unknown datagram kind {0}")]
    UnknownKind(u8),
    #[error("payload of {0} bytes is too large")]
    PayloadTooLarge(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Downstream<'a> {
    pub sender: MemberId,
    pub seq: u32,
    pub payload: &'a [u8],
}

pub fn encode_upstream(seq: u32, payload: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.push(AUDIO_KIND);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(payload);
}

pub fn parse_upstream(d: &[u8]) -> Result<(u32, &[u8]), DatagramError> {
    let payload = checked_payload(d, UP_HEADER)?;
    let seq = u32::from_le_bytes(d[1..5].try_into().unwrap());
    Ok((seq, payload))
}

pub fn upstream_to_downstream(sender: MemberId, d: &[u8]) -> Result<Vec<u8>, DatagramError> {
    let (seq, payload) = parse_upstream(d)?;
    let mut out = Vec::with_capacity(DOWN_HEADER + payload.len());
    out.push(AUDIO_KIND);
    out.extend_from_slice(&sender.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn parse_downstream(d: &[u8]) -> Result<Downstream<'_>, DatagramError> {
    let payload = checked_payload(d, DOWN_HEADER)?;
    Ok(Downstream {
        sender: u16::from_le_bytes(d[1..3].try_into().unwrap()),
        seq: u32::from_le_bytes(d[3..7].try_into().unwrap()),
        payload,
    })
}

fn checked_payload(d: &[u8], header: usize) -> Result<&[u8], DatagramError> {
    let kind = *d.first().ok_or(DatagramError::TooShort)?;
    if kind != AUDIO_KIND {
        return Err(DatagramError::UnknownKind(kind));
    }
    let payload = d.get(header..).ok_or(DatagramError::TooShort)?;
    if payload.len() > MAX_AUDIO_PAYLOAD {
        return Err(DatagramError::PayloadTooLarge(payload.len()));
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(seq: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        encode_upstream(seq, payload, &mut out);
        out
    }

    #[test]
    fn upstream_roundtrip() {
        assert_eq!(parse_upstream(&up(7, b"abc")), Ok((7, &b"abc"[..])));
    }

    #[test]
    fn relay_stamps_sender() {
        let down = upstream_to_downstream(42, &up(7, b"abc")).unwrap();
        let d = parse_downstream(&down).unwrap();
        assert_eq!((d.sender, d.seq, d.payload), (42, 7, &b"abc"[..]));
    }

    #[test]
    fn rejects_short_and_unknown() {
        assert_eq!(parse_upstream(&[]), Err(DatagramError::TooShort));
        assert_eq!(parse_upstream(&[9, 0, 0, 0, 0]), Err(DatagramError::UnknownKind(9)));
        assert_eq!(parse_downstream(&[AUDIO_KIND, 0]).err(), Some(DatagramError::TooShort));
    }

    #[test]
    fn rejects_large_payload() {
        let big = up(1, &[0u8; 1001]);
        assert_eq!(parse_upstream(&big), Err(DatagramError::PayloadTooLarge(1001)));
        assert_eq!(upstream_to_downstream(1, &big), Err(DatagramError::PayloadTooLarge(1001)));
    }
}
