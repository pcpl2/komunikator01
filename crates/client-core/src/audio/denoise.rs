//! Microphone noise suppression (RNNoise, via nnnoiseless) applied before the VAD, so fan,
//! keyboard and mic hiss neither get sent nor keep the gate open.

use nnnoiseless::DenoiseState;

use super::frame::{Pcm, FRAME_SAMPLES};

const CHUNK: usize = DenoiseState::FRAME_SIZE;
/// RNNoise expects 16-bit sample values in f32.
const SCALE: f32 = 32_768.0;

const _: () = assert!(FRAME_SAMPLES.is_multiple_of(CHUNK));

pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
}

impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl Denoiser {
    /// Allocates the model state; done once, outside the real-time loop.
    pub fn new() -> Self {
        Self { state: DenoiseState::new() }
    }

    /// Denoises `pcm` in place. Adds RNNoise's 10 ms of delay; never allocates.
    pub fn process(&mut self, pcm: &mut Pcm) {
        let mut input = [0f32; CHUNK];
        let mut output = [0f32; CHUNK];
        for chunk in pcm.chunks_exact_mut(CHUNK) {
            for (i, s) in input.iter_mut().zip(chunk.iter()) {
                *i = s * SCALE;
            }
            self.state.process_frame(&mut output, &input);
            for (s, o) in chunk.iter_mut().zip(output.iter()) {
                *s = (o / SCALE).clamp(-1.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::codec::tests::{rms, sine};

    fn db(x: f32) -> f32 {
        20.0 * x.max(1e-9).log10()
    }

    /// Low-passed noise (fan or mic hiss) at roughly `level_db` dBFS RMS, deterministic.
    fn hiss(level_db: f32, seed: &mut u32, y: &mut f32) -> Pcm {
        const LP: f32 = 0.9;
        let amp = 10f32.powf(level_db / 20.0) * 3f32.sqrt();
        let mut p = [0.0; FRAME_SAMPLES];
        for s in p.iter_mut() {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let white = ((*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp;
            *y = LP * *y + (1.0 - LP) * white;
            *s = *y * (1.0 / (1.0 - LP)).sqrt();
        }
        p
    }

    #[test]
    fn steady_hiss_is_suppressed() {
        let mut d = Denoiser::new();
        let (mut seed, mut y) = (1, 0.0);
        let (mut before, mut after) = (0.0, 0.0);
        // Measured over the second second, once the model has settled.
        for i in 0..100 {
            let mut f = hiss(-35.0, &mut seed, &mut y);
            let level = rms(&f);
            d.process(&mut f);
            if i >= 50 {
                before += level;
                after += rms(&f);
            }
        }
        let reduction = db(before) - db(after);
        assert!(reduction > 30.0, "hiss reduced by only {reduction:.1} dB");
    }

    #[test]
    fn silence_stays_silent() {
        let mut d = Denoiser::new();
        let mut f = [0.0; FRAME_SAMPLES];
        for _ in 0..10 {
            d.process(&mut f);
        }
        assert!(f.iter().all(|s| s.abs() < 1e-4));
    }

    #[test]
    fn output_stays_in_range_for_full_scale_input() {
        let mut d = Denoiser::new();
        for i in 0..20 {
            let mut f = sine(440.0, 1.0, i * FRAME_SAMPLES);
            d.process(&mut f);
            assert!(f.iter().all(|s| s.is_finite() && (-1.0..=1.0).contains(s)));
        }
    }
}
