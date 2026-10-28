//! Fixed-size audio frames passed between the audio threads and the network task.
//!
//! They are `Copy` and heap-free so the audio threads never allocate.

use protocol::MemberId;

/// Largest Opus packet we carry; 32 kbit/s CBR frames are ~80 bytes.
pub const MAX_PACKET: usize = 256;

#[derive(Clone, Copy, Debug)]
pub struct EncodedFrame {
    pub seq: u32,
    pub len: u16,
    pub data: [u8; MAX_PACKET],
}

impl EncodedFrame {
    /// `None` when the payload does not fit.
    pub fn new(seq: u32, payload: &[u8]) -> Option<Self> {
        let mut data = [0u8; MAX_PACKET];
        data.get_mut(..payload.len())?.copy_from_slice(payload);
        Some(Self { seq, len: payload.len() as u16, data })
    }

    pub fn payload(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct IncomingFrame {
    pub sender: MemberId,
    pub seq: u32,
    pub len: u16,
    pub data: [u8; MAX_PACKET],
}

impl IncomingFrame {
    pub fn new(sender: MemberId, seq: u32, payload: &[u8]) -> Option<Self> {
        let f = EncodedFrame::new(seq, payload)?;
        Some(Self { sender, seq, len: f.len, data: f.data })
    }

    pub fn payload(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}
