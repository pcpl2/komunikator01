//! Short synthesized notification tones, played from memory.
use client_core::app::SoundKind;
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT, SND_PURGE};

const RATE: u32 = 48_000;
/// 8000 / 32767 is about -12.2 dBFS: audible over voice without startling anyone.
const PEAK: f32 = 8000.0;
const FADE: usize = (RATE as usize) * 5 / 1000;

fn ms(n: u32) -> usize {
    (RATE * n / 1000) as usize
}

/// Appends a sine sweep `f0` -> `f1` Hz lasting `len` samples, with 5 ms fades.
fn sweep(out: &mut Vec<i16>, f0: f32, f1: f32, len: usize) {
    let mut phase = 0f32;
    for i in 0..len {
        let t = i as f32 / len as f32;
        phase += std::f32::consts::TAU * (f0 + (f1 - f0) * t) / RATE as f32;
        let fade = (i.min(len - 1 - i) as f32 / FADE as f32).min(1.0);
        out.push((phase.sin() * fade * PEAK) as i16);
    }
}

/// Mono 16-bit 48 kHz PCM WAV, one distinct tone per event.
pub fn tone_wav(kind: SoundKind) -> Vec<u8> {
    let mut s = Vec::new();
    match kind {
        SoundKind::Muted => sweep(&mut s, 660.0, 440.0, ms(120)),
        SoundKind::Unmuted => sweep(&mut s, 440.0, 660.0, ms(120)),
        SoundKind::Joined => sweep(&mut s, 880.0, 880.0, ms(100)),
        SoundKind::Left => sweep(&mut s, 330.0, 330.0, ms(100)),
        SoundKind::Chat => {
            sweep(&mut s, 990.0, 990.0, ms(40));
            s.extend(std::iter::repeat_n(0, ms(30)));
            sweep(&mut s, 990.0, 990.0, ms(40));
        }
    }
    let data_len = (s.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for v in s {
        w.extend_from_slice(&v.to_le_bytes());
    }
    w
}

const ALL: [SoundKind; 5] = [
    SoundKind::Muted,
    SoundKind::Unmuted,
    SoundKind::Joined,
    SoundKind::Left,
    SoundKind::Chat,
];

/// The five WAVs, built once; PlaySoundW reads them straight from memory.
pub struct Sounds {
    wavs: [Vec<u8>; 5],
}

impl Sounds {
    pub fn new() -> Self {
        Self { wavs: ALL.map(tone_wav) }
    }

    /// Fire and forget; a new tone replaces one still playing. SND_ASYNC keeps reading the
    /// buffer from a background thread, which `Drop` stops before the buffers are freed.
    pub fn play(&self, kind: SoundKind) {
        let idx = match kind {
            SoundKind::Muted => 0,
            SoundKind::Unmuted => 1,
            SoundKind::Joined => 2,
            SoundKind::Left => 3,
            SoundKind::Chat => 4,
        };
        unsafe {
            let _ = PlaySoundW(
                PCWSTR(self.wavs[idx].as_ptr().cast()),
                None,
                SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
            );
        }
    }
}

impl Drop for Sounds {
    fn drop(&mut self) {
        // Stop any async playback still reading `wavs`.
        unsafe {
            let _ = PlaySoundW(PCWSTR::null(), None, SND_PURGE);
        }
    }
}

impl Default for Sounds {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [SoundKind; 5] = [
        SoundKind::Muted,
        SoundKind::Unmuted,
        SoundKind::Joined,
        SoundKind::Left,
        SoundKind::Chat,
    ];

    fn u16_at(b: &[u8], i: usize) -> u16 {
        u16::from_le_bytes([b[i], b[i + 1]])
    }
    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
    }

    #[test]
    fn sounds_dropped_while_playing_is_safe() {
        for _ in 0..50 {
            for k in KINDS {
                let s = Sounds::new();
                s.play(k);
            }
        }
    }

    #[test]
    fn tone_wav_is_valid_pcm() {
        let mut all = Vec::new();
        for k in KINDS {
            let w = tone_wav(k);
            assert_eq!(&w[0..4], b"RIFF");
            assert_eq!(u32_at(&w, 4) as usize, w.len() - 8);
            assert_eq!(&w[8..16], b"WAVEfmt ");
            assert_eq!(u32_at(&w, 16), 16);
            assert_eq!(u16_at(&w, 20), 1); // PCM
            assert_eq!(u16_at(&w, 22), 1); // mono
            assert_eq!(u32_at(&w, 24), 48_000);
            assert_eq!(u32_at(&w, 28), 96_000);
            assert_eq!(u16_at(&w, 32), 2);
            assert_eq!(u16_at(&w, 34), 16);
            assert_eq!(&w[36..40], b"data");
            let n = u32_at(&w, 40) as usize;
            assert_eq!(n, w.len() - 44);
            let ms = (n / 2) * 1000 / 48_000;
            assert!((60..=150).contains(&ms), "{k:?}: {ms} ms");
            let peak = w[44..]
                .chunks(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs())
                .max()
                .unwrap();
            let limit = 32767.0 * 10f64.powf(-12.0 / 20.0);
            assert!(peak > 1000 && f64::from(peak) <= limit, "{k:?}: peak {peak}");
            // 5 ms fades: the first sample is near silence.
            assert!(i16::from_le_bytes([w[44], w[45]]).unsigned_abs() < 100);
            all.push(w);
        }
        for i in 0..all.len() {
            for j in i + 1..all.len() {
                assert_ne!(all[i], all[j], "{:?} and {:?} sound identical", KINDS[i], KINDS[j]);
            }
        }
    }
}
