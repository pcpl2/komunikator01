//! Acoustic echo cancellation (WebRTC AEC3, via sonora): removes from the microphone what
//! this computer's speakers played, so people on speakers don't send everyone their own voice.

use sonora::config::{EchoCanceller as Aec3, HighPassFilter};
use sonora::{AudioProcessing, Config, StreamConfig};

use super::frame::{Pcm, FRAME_SAMPLES, SAMPLE_RATE};

/// WebRTC processes 10 ms at a time.
const CHUNK: usize = SAMPLE_RATE as usize / 100;
/// A first guess of speaker-to-microphone latency (WASAPI buffers plus the room); AEC3
/// estimates the real delay itself.
const STREAM_DELAY_MS: i32 = 60;

const _: () = assert!(FRAME_SAMPLES.is_multiple_of(CHUNK));

pub struct EchoCanceller {
    apm: AudioProcessing,
}

impl Default for EchoCanceller {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoCanceller {
    /// Builds the processor; done once, outside the real-time loop.
    pub fn new() -> Self {
        let config = Config {
            echo_canceller: Some(Aec3::default()),
            high_pass_filter: Some(HighPassFilter::default()),
            ..Default::default()
        };
        let mono = StreamConfig::new(SAMPLE_RATE, 1);
        let apm = AudioProcessing::builder().config(config).capture_config(mono).render_config(mono).build();
        Self { apm }
    }

    /// Feeds what went to the speakers (the far end the echo comes from).
    pub fn render(&mut self, far: &Pcm) {
        let mut scratch = [0f32; CHUNK];
        for chunk in far.chunks_exact(CHUNK) {
            if let Err(e) = self.apm.process_render_f32(&[chunk], &mut [&mut scratch]) {
                tracing::warn!("echo reference rejected: {e:?}");
            }
        }
    }

    /// Removes the echo of earlier `render` frames from the microphone, in place.
    pub fn capture(&mut self, near: &mut Pcm) {
        let mut input = [0f32; CHUNK];
        for chunk in near.chunks_exact_mut(CHUNK) {
            input.copy_from_slice(chunk);
            let _ = self.apm.set_stream_delay_ms(STREAM_DELAY_MS);
            if let Err(e) = self.apm.process_capture_f32(&[&input], &mut [chunk]) {
                tracing::warn!("echo cancellation failed: {e:?}");
                chunk.copy_from_slice(&input);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::frame::FRAME_SAMPLES;

    /// Speech-like (low-passed, loudness-modulated) noise, deterministic.
    struct Voice {
        seed: u32,
        y: f32,
        n: usize,
    }

    impl Voice {
        fn new(seed: u32) -> Self {
            Self { seed, y: 0.0, n: 0 }
        }

        fn frame(&mut self, amp: f32) -> Pcm {
            std::array::from_fn(|_| {
                self.seed = self.seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let white = (self.seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                self.y = 0.7 * self.y + 0.3 * white;
                // Syllables: 4 Hz loudness envelope.
                let env = 0.6 + 0.4 * (2.0 * std::f32::consts::PI * 4.0 * self.n as f32 / 48_000.0).sin();
                self.n += 1;
                self.y * amp * env
            })
        }
    }

    fn energy(frames: &[Pcm]) -> f32 {
        frames.iter().flat_map(|f| f.iter()).map(|s| s * s).sum()
    }

    fn db(ratio: f32) -> f32 {
        10.0 * ratio.log10()
    }

    /// Room echo: the far end 40 ms later at -10 dB, as a speaker heard by the mic.
    const DELAY: usize = 1920;
    const ECHO_GAIN: f32 = 0.3;

    #[test]
    fn echo_of_the_speakers_is_removed() {
        let mut aec = EchoCanceller::new();
        let mut far = Voice::new(1);
        let mut history = vec![0.0f32; DELAY];
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for i in 0..400 {
            let f = far.frame(0.3);
            history.extend_from_slice(&f);
            let mut near: Pcm = std::array::from_fn(|j| history[j] * ECHO_GAIN);
            history.drain(..FRAME_SAMPLES);
            aec.render(&f);
            let input = near;
            aec.capture(&mut near);
            // The last two of eight seconds, once the filter has converged.
            if i >= 300 {
                before.push(input);
                after.push(near);
            }
        }
        let erle = db(energy(&before) / energy(&after).max(1e-12));
        assert!(erle > 20.0, "echo reduced by only {erle:.1} dB");
    }

    #[test]
    fn my_own_voice_passes_when_the_speakers_are_silent() {
        let mut aec = EchoCanceller::new();
        let mut me = Voice::new(2);
        let silence = [0.0; FRAME_SAMPLES];
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for i in 0..200 {
            let mut near = me.frame(0.2);
            let input = near;
            aec.render(&silence);
            aec.capture(&mut near);
            if i >= 100 {
                before.push(input);
                after.push(near);
            }
        }
        let loss = db(energy(&before) / energy(&after).max(1e-12));
        assert!(loss < 3.0, "own voice lost {loss:.1} dB");
    }
}
