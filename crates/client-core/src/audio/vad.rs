//! Energy-based voice activity detection with an adaptive noise floor and hangover.

use super::frame::Pcm;

/// Frames the gate stays open after the last loud frame (300 ms).
const HANGOVER_FRAMES: u32 = 15;
/// Noise floor = minimum level over the last BLOCKS x BLOCK_FRAMES frames (5 s).
const BLOCK_FRAMES: u32 = 50;
const BLOCKS: usize = 5;
const MIN_THRESHOLD_DB: f32 = -50.0;
const SILENCE_DB: f32 = -100.0;

/// Opens when a frame is louder than the noise floor by a sensitivity-dependent margin.
pub struct Vad {
    sensitivity: f32,
    level_db: f32,
    block_mins: [f32; BLOCKS],
    current_min: f32,
    frames_in_block: u32,
    next_block: usize,
    hangover: u32,
}

impl Vad {
    /// `sensitivity` in 0.0–1.0; higher opens on quieter speech.
    pub fn new(sensitivity: f32) -> Self {
        Self {
            sensitivity: sensitivity.clamp(0.0, 1.0),
            level_db: SILENCE_DB,
            block_mins: [f32::INFINITY; BLOCKS],
            current_min: f32::INFINITY,
            frames_in_block: 0,
            next_block: 0,
            hangover: 0,
        }
    }

    pub fn set_sensitivity(&mut self, s: f32) {
        self.sensitivity = s.clamp(0.0, 1.0);
    }

    /// RMS level of the last processed frame, for a level meter.
    pub fn level_dbfs(&self) -> f32 {
        self.level_db
    }

    /// True when this frame should be sent.
    pub fn process(&mut self, pcm: &Pcm) -> bool {
        let rms = (pcm.iter().map(|x| x * x).sum::<f32>() / pcm.len() as f32).sqrt();
        self.level_db = (20.0 * rms.log10()).max(SILENCE_DB);
        // The floor comes from earlier frames only, so a first loud frame still opens.
        let margin = 18.0 - 12.0 * self.sensitivity;
        let threshold = (self.noise_floor() + margin).max(MIN_THRESHOLD_DB);
        self.track_floor();
        if self.level_db > threshold {
            self.hangover = HANGOVER_FRAMES;
            true
        } else if self.hangover > 0 {
            self.hangover -= 1;
            true
        } else {
            false
        }
    }

    fn noise_floor(&self) -> f32 {
        let floor = self.block_mins.iter().copied().fold(self.current_min, f32::min);
        if floor.is_finite() { floor } else { SILENCE_DB }
    }

    fn track_floor(&mut self) {
        self.current_min = self.current_min.min(self.level_db);
        self.frames_in_block += 1;
        if self.frames_in_block == BLOCK_FRAMES {
            self.block_mins[self.next_block] = self.current_min;
            self.next_block = (self.next_block + 1) % BLOCKS;
            self.current_min = f32::INFINITY;
            self.frames_in_block = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::codec::tests::sine;
    use crate::audio::frame::FRAME_SAMPLES;

    /// Deterministic white noise with the given RMS in dBFS.
    fn noise(db: f32, seed: &mut u32) -> Pcm {
        let amp = 10f32.powf(db / 20.0) * 3f32.sqrt();
        std::array::from_fn(|_| {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            amp * ((*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
        })
    }

    const SPEECH_AMP: f32 = 0.141; // sine at about -20 dBFS RMS

    #[test]
    fn silence_is_closed() {
        let mut v = Vad::new(0.5);
        assert!((0..100).all(|_| !v.process(&[0.0; FRAME_SAMPLES])));
    }

    #[test]
    fn speech_opens_immediately() {
        let mut v = Vad::new(0.5);
        let mut seed = 1;
        for _ in 0..50 {
            v.process(&noise(-60.0, &mut seed));
        }
        assert!(v.process(&sine(300.0, SPEECH_AMP, 0)));
    }

    #[test]
    fn hangover_is_15_frames() {
        let mut v = Vad::new(0.5);
        assert!(v.process(&sine(300.0, SPEECH_AMP, 0)));
        for i in 0..15 {
            assert!(v.process(&[0.0; FRAME_SAMPLES]), "frame {i} after speech");
        }
        assert!(!v.process(&[0.0; FRAME_SAMPLES]));
    }

    #[test]
    fn steady_fan_noise_closes() {
        let mut v = Vad::new(0.5);
        let mut seed = 7;
        let open: Vec<bool> = (0..500).map(|_| v.process(&noise(-40.0, &mut seed))).collect();
        assert!(open[400..].iter().all(|o| !o));
    }

    #[test]
    fn fan_starting_after_silence_closes_within_6s() {
        let mut v = Vad::new(0.5);
        for _ in 0..100 {
            v.process(&[0.0; FRAME_SAMPLES]);
        }
        let mut seed = 3;
        let open: Vec<bool> = (0..400).map(|_| v.process(&noise(-40.0, &mut seed))).collect();
        assert!(open[300..].iter().all(|o| !o));
    }

    #[test]
    fn speech_over_fan_still_opens() {
        let mut v = Vad::new(0.5);
        let mut seed = 5;
        for _ in 0..300 {
            v.process(&noise(-40.0, &mut seed));
        }
        assert!(v.process(&sine(300.0, SPEECH_AMP, 0)));
    }
}
