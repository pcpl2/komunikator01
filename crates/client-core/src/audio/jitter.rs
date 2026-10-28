//! Per-sender adaptive jitter buffer with Opus FEC and packet-loss concealment.

use super::codec::Decoder;
use super::frame::{Pcm, MAX_PACKET};

const SLOTS: usize = 16;
const MIN_DEPTH: u8 = 2;
const MAX_DEPTH: u8 = 10;
/// Concealed frames after the last packet before the talk spurt is considered over.
const MAX_MISSING: u8 = 5;
/// Frames (10 s) without loss before the target depth shrinks by one.
const SHRINK_AFTER: u32 = 500;

/// Decoding seam so the buffer logic can be tested without Opus.
pub trait PacketDecoder {
    fn decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut Pcm) -> anyhow::Result<()>;
}

impl PacketDecoder for Decoder {
    fn decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut Pcm) -> anyhow::Result<()> {
        Decoder::decode(self, packet, fec, out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pop {
    Audio,
    /// Nothing to play; the output frame is zeroed.
    Silence,
}

#[derive(Clone, Copy)]
struct Slot {
    seq: u32,
    len: u16,
    full: bool,
    data: [u8; MAX_PACKET],
}

const EMPTY: Slot = Slot { seq: 0, len: 0, full: false, data: [0; MAX_PACKET] };

/// Sequence distance `a - b`, correct across u32 wraparound.
fn dist(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

pub struct JitterBuffer {
    slots: [Slot; SLOTS],
    playing: bool,
    next_seq: u32,
    target: u8,
    missing_run: u8,
    clean_frames: u32,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            slots: [EMPTY; SLOTS],
            playing: false,
            next_seq: 0,
            target: MIN_DEPTH,
            missing_run: 0,
            clean_frames: 0,
        }
    }

    pub fn target_depth(&self) -> u8 {
        self.target
    }

    pub(crate) fn buffered(&self) -> usize {
        self.slots.iter().filter(|s| s.full).count()
    }

    pub fn push(&mut self, seq: u32, payload: &[u8]) {
        if payload.len() > MAX_PACKET {
            return;
        }
        let reference = if self.playing { Some(self.next_seq) } else { self.lowest() };
        if let Some(r) = reference {
            let d = dist(seq, r);
            if self.playing && d < 0 {
                // Arrived after we concealed it: the network is jittering more than we buffer.
                if -d <= self.missing_run as i32 {
                    self.grow();
                }
                return;
            }
            if d >= SLOTS as i32 || d <= -(SLOTS as i32) {
                self.reset();
            }
        }
        let slot = &mut self.slots[seq as usize % SLOTS];
        if slot.full && slot.seq == seq {
            return;
        }
        slot.seq = seq;
        slot.len = payload.len() as u16;
        slot.full = true;
        slot.data[..payload.len()].copy_from_slice(payload);
    }

    pub fn pop<D: PacketDecoder>(&mut self, dec: &mut D, out: &mut Pcm) -> Pop {
        if !self.playing {
            if self.buffered() < self.target as usize {
                out.fill(0.0);
                return Pop::Silence;
            }
            self.next_seq = self.lowest().expect("buffer is not empty");
            self.playing = true;
            self.missing_run = 0;
        }

        let depth = self.buffered();
        let seq = self.next_seq;
        let result = if let Some(slot) = self.take(seq) {
            self.missing_run = 0;
            self.clean_frames += 1;
            dec.decode(Some(&slot.data[..slot.len as usize]), false, out)
        } else if self.missing_run >= MAX_MISSING {
            self.playing = false;
            self.missing_run = 0;
            out.fill(0.0);
            return Pop::Silence;
        } else {
            self.missing_run += 1;
            self.clean_frames = 0;
            match self.peek(seq.wrapping_add(1)) {
                Some(next) => dec.decode(Some(&next.data[..next.len as usize]), true, out),
                None => {
                    // Later packets already waiting means a real gap, not the end of speech.
                    if self.buffered() > 0 {
                        self.grow();
                    }
                    dec.decode(None, false, out)
                }
            }
        };
        self.next_seq = seq.wrapping_add(1);

        if self.clean_frames >= SHRINK_AFTER && depth > self.target as usize {
            self.target = (self.target - 1).max(MIN_DEPTH);
            self.clean_frames = 0;
        }
        if result.is_err() {
            out.fill(0.0);
        }
        Pop::Audio
    }

    fn grow(&mut self) {
        self.target = (self.target + 1).min(MAX_DEPTH);
        self.clean_frames = 0;
    }

    fn reset(&mut self) {
        self.slots = [EMPTY; SLOTS];
        self.playing = false;
        self.missing_run = 0;
    }

    fn peek(&self, seq: u32) -> Option<&Slot> {
        let s = &self.slots[seq as usize % SLOTS];
        (s.full && s.seq == seq).then_some(s)
    }

    fn take(&mut self, seq: u32) -> Option<Slot> {
        let s = &mut self.slots[seq as usize % SLOTS];
        if s.full && s.seq == seq {
            s.full = false;
            Some(*s)
        } else {
            None
        }
    }

    /// Oldest buffered sequence number, wraparound-aware.
    fn lowest(&self) -> Option<u32> {
        let mut full = self.slots.iter().filter(|s| s.full).map(|s| s.seq);
        let first = full.next()?;
        Some(full.fold(first, |low, s| if dist(s, low) < 0 { s } else { low }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::frame::FRAME_SAMPLES;

    /// Records which packet (first payload byte) was decoded and how.
    #[derive(Default)]
    struct Fake {
        calls: Vec<(Option<u8>, bool)>,
    }

    impl PacketDecoder for Fake {
        fn decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut Pcm) -> anyhow::Result<()> {
            self.calls.push((packet.map(|p| p[0]), fec));
            out.fill(0.5);
            Ok(())
        }
    }

    fn push(j: &mut JitterBuffer, seq: u32) {
        j.push(seq, &[seq as u8]);
    }

    fn pop(j: &mut JitterBuffer, d: &mut Fake) -> Pop {
        let mut out = [0f32; FRAME_SAMPLES];
        j.pop(d, &mut out)
    }

    #[test]
    fn waits_for_target_depth() {
        let (mut j, mut d) = (JitterBuffer::new(), Fake::default());
        push(&mut j, 10);
        assert_eq!(pop(&mut j, &mut d), Pop::Silence);
        push(&mut j, 11);
        assert_eq!(pop(&mut j, &mut d), Pop::Audio);
        assert_eq!(d.calls, [(Some(10), false)]);
    }

    #[test]
    fn reorders() {
        let (mut j, mut d) = (JitterBuffer::new(), Fake::default());
        for s in [11, 10, 12] {
            push(&mut j, s);
        }
        for _ in 0..3 {
            assert_eq!(pop(&mut j, &mut d), Pop::Audio);
        }
        assert_eq!(d.calls, [(Some(10), false), (Some(11), false), (Some(12), false)]);
    }

    #[test]
    fn drops_duplicates_and_late() {
        let (mut j, mut d) = (JitterBuffer::new(), Fake::default());
        push(&mut j, 10);
        push(&mut j, 10);
        assert_eq!(j.buffered(), 1);
        push(&mut j, 11);
        pop(&mut j, &mut d);
        push(&mut j, 9);
        push(&mut j, 10);
        assert_eq!(j.buffered(), 1);
    }

    #[test]
    fn loss_uses_fec_then_plc_and_grows_target() {
        let (mut j, mut d) = (JitterBuffer::new(), Fake::default());
        for s in [10, 11, 13, 14, 17, 18] {
            push(&mut j, s);
        }
        for _ in 0..3 {
            pop(&mut j, &mut d);
        }
        assert_eq!(d.calls[2], (Some(13), true), "seq 12 recovered from 13's FEC");
        assert_eq!(j.target_depth(), 2);
        for _ in 0..3 {
            pop(&mut j, &mut d);
        }
        assert_eq!(d.calls[5], (None, false), "seq 15 concealed");
        assert_eq!(j.target_depth(), 3);
    }

    #[test]
    fn ends_spurt_after_5_missing() {
        let (mut j, mut d) = (JitterBuffer::new(), Fake::default());
        push(&mut j, 10);
        push(&mut j, 11);
        for _ in 0..7 {
            assert_eq!(pop(&mut j, &mut d), Pop::Audio);
        }
        assert_eq!(pop(&mut j, &mut d), Pop::Silence);
        assert_eq!(j.target_depth(), 2, "end of speech is not jitter");
    }

    #[test]
    fn late_packet_grows_target() {
        let (mut j, mut d) = (JitterBuffer::new(), Fake::default());
        push(&mut j, 10);
        push(&mut j, 11);
        for _ in 0..3 {
            pop(&mut j, &mut d);
        }
        push(&mut j, 12);
        assert_eq!(j.target_depth(), 3);
    }

    #[test]
    fn handles_seq_wraparound() {
        let (mut j, mut d) = (JitterBuffer::new(), Fake::default());
        for s in [u32::MAX - 1, u32::MAX, 0, 1] {
            push(&mut j, s);
        }
        for _ in 0..4 {
            assert_eq!(pop(&mut j, &mut d), Pop::Audio);
        }
        let order: Vec<_> = d.calls.iter().map(|c| c.0).collect();
        assert_eq!(order, [Some(254), Some(255), Some(0), Some(1)]);
    }

    #[test]
    fn new_spurt_after_gap_resets() {
        let (mut j, mut d) = (JitterBuffer::new(), Fake::default());
        for s in 10..13 {
            push(&mut j, s);
        }
        for _ in 0..3 {
            pop(&mut j, &mut d);
        }
        push(&mut j, 500);
        push(&mut j, 501);
        assert_eq!(pop(&mut j, &mut d), Pop::Audio);
        assert_eq!(d.calls.last(), Some(&(Some(500u32 as u8), false)));
    }

    #[test]
    fn shrinks_after_10s_without_loss() {
        let (mut j, mut d) = (JitterBuffer::new(), Fake::default());
        push(&mut j, 0);
        push(&mut j, 1);
        for _ in 0..3 {
            pop(&mut j, &mut d);
        }
        push(&mut j, 2);
        assert_eq!(j.target_depth(), 3);
        let mut next = 3;
        for _ in 0..4 {
            push(&mut j, next);
            next += 1;
        }
        for _ in 0..510 {
            pop(&mut j, &mut d);
            push(&mut j, next);
            next += 1;
        }
        assert_eq!(j.target_depth(), 2);
    }
}
