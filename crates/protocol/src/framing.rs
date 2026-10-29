//! Length-prefixed (u32 LE) postcard frames for the control stream.

use serde::{de::DeserializeOwned, Serialize};

/// Upper bound for a single frame body; larger prefixes are a protocol violation.
pub const MAX_FRAME: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(usize),
    #[error("malformed frame")]
    Malformed,
}

pub fn encode_frame<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = postcard::to_allocvec(msg).expect("serializing to a Vec cannot fail");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Reassembles frames from arbitrarily split stream reads.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        let Some(prefix) = self.buf.get(..4) else {
            return Ok(None);
        };
        let len = u32::from_le_bytes(prefix.try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            return Err(FrameError::TooLarge(len));
        }
        let Some(body) = self.buf.get(4..4 + len) else {
            return Ok(None);
        };
        let msg = postcard::from_bytes(body).map_err(|_| FrameError::Malformed)?;
        self.buf.drain(..4 + len);
        Ok(Some(msg))
    }

    /// The body of the complete frame at the front, e.g. one `next` just failed to decode.
    pub fn front_body(&self) -> Option<&[u8]> {
        let len = u32::from_le_bytes(self.buf.get(..4)?.try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            return None;
        }
        self.buf.get(4..4 + len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientMsg;

    fn hello() -> ClientMsg {
        ClientMsg::Hello {
            version: 1,
            username: "ala".into(),
            auth: crate::Auth::Password("x".into()),
            session: 7,
            room: crate::RoomChoice::Room(3),
        }
    }

    #[test]
    fn roundtrip_single() {
        let mut d = FrameDecoder::new();
        d.push(&encode_frame(&hello()));
        assert_eq!(d.next::<ClientMsg>().unwrap(), Some(hello()));
        assert_eq!(d.next::<ClientMsg>().unwrap(), None);
    }

    #[test]
    fn split_across_pushes() {
        let bytes = encode_frame(&hello());
        let mut d = FrameDecoder::new();
        for (i, b) in bytes.iter().enumerate() {
            d.push(std::slice::from_ref(b));
            let got = d.next::<ClientMsg>().unwrap();
            if i + 1 < bytes.len() {
                assert_eq!(got, None);
            } else {
                assert_eq!(got, Some(hello()));
            }
        }
    }

    #[test]
    fn two_frames_in_one_push() {
        let mut bytes = encode_frame(&hello());
        bytes.extend(encode_frame(&hello()));
        let mut d = FrameDecoder::new();
        d.push(&bytes);
        assert_eq!(d.next::<ClientMsg>().unwrap(), Some(hello()));
        assert_eq!(d.next::<ClientMsg>().unwrap(), Some(hello()));
        assert_eq!(d.next::<ClientMsg>().unwrap(), None);
    }

    #[test]
    fn rejects_oversized_length() {
        let mut d = FrameDecoder::new();
        d.push(&(MAX_FRAME as u32 + 1).to_le_bytes());
        assert_eq!(d.next::<ClientMsg>(), Err(FrameError::TooLarge(65_537)));
    }

    #[test]
    fn rejects_garbage() {
        let mut d = FrameDecoder::new();
        d.push(&3u32.to_le_bytes());
        d.push(&[0xFF; 3]);
        assert_eq!(d.next::<ClientMsg>(), Err(FrameError::Malformed));
    }

    #[test]
    fn relay_messages_roundtrip() {
        let msg = ClientMsg::Relay { to: 5, blob: vec![1; crate::MAX_BLOB] };
        let mut dec = FrameDecoder::new();
        dec.push(&encode_frame(&msg));
        assert_eq!(dec.next::<ClientMsg>().unwrap(), Some(msg));
    }
}
