//! Who is speaking right now, for the UI: written by the audio threads, polled by the
//! UI thread. Fixed atomic slots, so the audio threads never lock or allocate.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

use protocol::MemberId;

const SLOTS: usize = 32;
/// A voice counts as speaking for this many frames (300 ms) after it was last heard.
const SPEAKING_FRAMES: u32 = 15;

/// Speaking state of everyone else (written by render) and of the local microphone
/// (written by capture). Id 0 is never a member and marks a free slot.
pub struct Activity {
    ids: [AtomicU16; SLOTS],
    last_frame: [AtomicU32; SLOTS],
    frame: AtomicU32,
    me_speaking: AtomicBool,
    mic_level_bits: AtomicU32,
}

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

impl Activity {
    pub fn new() -> Activity {
        Activity {
            ids: std::array::from_fn(|_| AtomicU16::new(0)),
            last_frame: std::array::from_fn(|_| AtomicU32::new(0)),
            frame: AtomicU32::new(0),
            me_speaking: AtomicBool::new(false),
            mic_level_bits: AtomicU32::new((-100.0f32).to_bits()),
        }
    }

    /// Render thread: `id` produced audio in this frame. A full table drops its stalest entry.
    pub fn heard(&self, id: MemberId) {
        let now = self.frame.load(Ordering::Relaxed);
        let mut free = None;
        let mut stalest = (0, 0u32);
        for i in 0..SLOTS {
            let slot_id = self.ids[i].load(Ordering::Relaxed);
            if slot_id == id {
                self.last_frame[i].store(now, Ordering::Relaxed);
                return;
            }
            if slot_id == 0 {
                free.get_or_insert(i);
            } else {
                let age = now.wrapping_sub(self.last_frame[i].load(Ordering::Relaxed));
                if age >= stalest.1 {
                    stalest = (i, age);
                }
            }
        }
        let i = free.unwrap_or(stalest.0);
        // Frame first, so a reader that sees the id also sees a matching time.
        self.last_frame[i].store(now, Ordering::Relaxed);
        self.ids[i].store(id, Ordering::Release);
    }

    /// Render thread: one 20 ms frame has passed.
    pub fn tick(&self) {
        self.frame.fetch_add(1, Ordering::Relaxed);
    }

    /// Capture thread: whether the local microphone is transmitting, and its level.
    pub fn set_me(&self, speaking: bool, level_dbfs: f32) {
        self.me_speaking.store(speaking, Ordering::Relaxed);
        self.mic_level_bits.store(level_dbfs.to_bits(), Ordering::Relaxed);
    }

    /// Fills `out` with the ids heard within the last 300 ms.
    pub fn speaking(&self, out: &mut Vec<MemberId>) {
        out.clear();
        let now = self.frame.load(Ordering::Relaxed);
        for i in 0..SLOTS {
            let id = self.ids[i].load(Ordering::Acquire);
            if id != 0 && now.wrapping_sub(self.last_frame[i].load(Ordering::Relaxed)) < SPEAKING_FRAMES {
                out.push(id);
            }
        }
    }

    pub fn me_speaking(&self) -> bool {
        self.me_speaking.load(Ordering::Relaxed)
    }

    pub fn mic_level_dbfs(&self) -> f32 {
        f32::from_bits(self.mic_level_bits.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speaking(a: &Activity) -> Vec<MemberId> {
        let mut v = Vec::new();
        a.speaking(&mut v);
        v.sort();
        v
    }

    #[test]
    fn activity_reports_recent_speakers() {
        let a = Activity::new();
        a.tick();
        a.heard(7);
        for _ in 0..10 {
            a.tick();
        }
        assert_eq!(speaking(&a), [7]);
        for _ in 0..10 {
            a.tick();
        }
        assert!(speaking(&a).is_empty());
    }

    #[test]
    fn activity_full_table_overwrites_stalest() {
        let a = Activity::new();
        a.heard(1);
        a.tick();
        for id in 2..=SLOTS as MemberId {
            a.heard(id);
        }
        // Id 1 was heard longest ago; a newcomer takes its slot.
        a.heard(100);
        let heard = speaking(&a);
        assert!(heard.contains(&100));
        assert!(!heard.contains(&1));
        assert_eq!(heard.len(), SLOTS);
    }

    #[test]
    fn mic_level_roundtrip() {
        let a = Activity::new();
        assert!(!a.me_speaking());
        a.set_me(true, -23.5);
        assert!(a.me_speaking());
        assert_eq!(a.mic_level_dbfs(), -23.5);
    }
}
