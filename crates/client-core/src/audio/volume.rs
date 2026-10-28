//! Per-person playback volume: a persistent book keyed by username, and a lock-free
//! table the render thread reads by member id.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};

use protocol::MemberId;
use serde::{Deserialize, Serialize};

const MAX_PERCENT: u16 = 200;
const SLOTS: usize = 64;

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
struct Entry {
    percent: Option<u16>,
    #[serde(default)]
    muted: bool,
}

/// Volumes remembered by username, so they survive reconnects (member ids do not).
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct VolumeBook {
    people: BTreeMap<String, Entry>,
}

impl VolumeBook {
    pub fn percent(&self, name: &str) -> u16 {
        self.people.get(name).and_then(|e| e.percent).unwrap_or(100)
    }

    pub fn set_percent(&mut self, name: &str, p: u16) {
        self.people.entry(name.to_owned()).or_default().percent = Some(p.min(MAX_PERCENT));
    }

    pub fn is_muted(&self, name: &str) -> bool {
        self.people.get(name).is_some_and(|e| e.muted)
    }

    /// Returns the new state. The percent is kept, so unmuting restores it.
    pub fn toggle_muted(&mut self, name: &str) -> bool {
        let e = self.people.entry(name.to_owned()).or_default();
        e.muted = !e.muted;
        e.muted
    }

    pub fn gain(&self, name: &str) -> f32 {
        if self.is_muted(name) {
            0.0
        } else {
            f32::from(self.percent(name)) / 100.0
        }
    }
}

/// Gains by member id for the render thread: fixed atomic slots, so reading never locks
/// or allocates. Id 0 is never a member and marks a free slot. The UI thread is the only
/// writer; a torn update can affect a single 20 ms frame.
pub struct GainTable {
    ids: [AtomicU16; SLOTS],
    gains: [AtomicU32; SLOTS],
}

impl GainTable {
    pub fn new() -> GainTable {
        GainTable {
            ids: std::array::from_fn(|_| AtomicU16::new(0)),
            gains: std::array::from_fn(|_| AtomicU32::new(1.0f32.to_bits())),
        }
    }

    /// False when all slots hold other ids.
    pub fn set(&self, id: MemberId, gain: f32) -> bool {
        let slot = self.find(id).or_else(|| self.find(0));
        let Some(i) = slot else { return false };
        // Gain first, so a reader that sees the id also sees a gain meant for it.
        self.gains[i].store(gain.to_bits(), Ordering::Relaxed);
        self.ids[i].store(id, Ordering::Release);
        true
    }

    pub fn remove(&self, id: MemberId) {
        if let Some(i) = self.find(id) {
            self.ids[i].store(0, Ordering::Release);
        }
    }

    /// Frees every slot; for a new connection, where all member ids changed.
    pub fn clear(&self) {
        for id in &self.ids {
            id.store(0, Ordering::Release);
        }
    }

    /// 1.0 for ids without an entry.
    pub fn get(&self, id: MemberId) -> f32 {
        match self.find(id) {
            Some(i) => f32::from_bits(self.gains[i].load(Ordering::Relaxed)),
            None => 1.0,
        }
    }

    fn find(&self, id: MemberId) -> Option<usize> {
        self.ids.iter().position(|s| s.load(Ordering::Acquire) == id)
    }
}

impl Default for GainTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_book_defaults_and_clamps() {
        let mut b = VolumeBook::default();
        assert_eq!(b.percent("ala"), 100);
        assert_eq!(b.gain("ala"), 1.0);
        b.set_percent("ala", 300);
        assert_eq!(b.percent("ala"), 200);
        b.set_percent("ala", 50);
        assert_eq!(b.gain("ala"), 0.5);
    }

    #[test]
    fn clear_frees_every_slot() {
        let t = GainTable::new();
        for id in 1..=SLOTS as MemberId {
            assert!(t.set(id, 0.5));
        }
        assert!(!t.set(1000, 0.5), "table full");
        t.clear();
        assert_eq!(t.get(1), 1.0);
        assert!(t.set(1000, 0.5));
        assert_eq!(t.get(1000), 0.5);
    }

    #[test]
    fn muting_keeps_percent() {
        let mut b = VolumeBook::default();
        b.set_percent("ala", 150);
        assert!(b.toggle_muted("ala"));
        assert!(b.is_muted("ala"));
        assert_eq!(b.gain("ala"), 0.0);
        assert_eq!(b.percent("ala"), 150);
        assert!(!b.toggle_muted("ala"));
        assert_eq!(b.gain("ala"), 1.5);
    }

    #[test]
    fn volume_book_roundtrips_toml() {
        let mut b = VolumeBook::default();
        b.set_percent("ala", 150);
        b.set_percent("żółw", 20);
        b.toggle_muted("żółw");
        let text = toml::to_string(&b).unwrap();
        let back: VolumeBook = toml::from_str(&text).unwrap();
        assert_eq!(back.percent("ala"), 150);
        assert_eq!(back.percent("żółw"), 20);
        assert!(back.is_muted("żółw"));
        assert!(!back.is_muted("ala"));
    }

    #[test]
    fn gain_table_set_get_remove_and_full() {
        let t = GainTable::new();
        assert_eq!(t.get(7), 1.0);
        assert!(t.set(7, 0.5));
        assert_eq!(t.get(7), 0.5);
        assert!(t.set(7, 0.25), "updating an existing id never needs a new slot");
        assert_eq!(t.get(7), 0.25);
        t.remove(7);
        assert_eq!(t.get(7), 1.0);
        for id in 1..=64 {
            assert!(t.set(id, 0.0));
        }
        assert!(!t.set(65, 0.0));
        t.remove(3);
        assert!(t.set(65, 0.0), "a removed slot is reusable");
    }
}
