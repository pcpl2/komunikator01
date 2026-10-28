//! Sums decoded streams with per-source gain.

use super::frame::Pcm;

/// Writes the gain-weighted sum of `sources` into `out`, hard-clipped to [-1, 1].
pub fn mix(sources: &[(&Pcm, f32)], out: &mut Pcm) {
    out.fill(0.0);
    for (pcm, gain) in sources {
        accumulate(out, pcm, *gain);
    }
    clip(out);
}

/// Adds `pcm * gain` to `out`; call `clip` once all sources are in.
pub fn accumulate(out: &mut Pcm, pcm: &Pcm, gain: f32) {
    for (o, s) in out.iter_mut().zip(pcm.iter()) {
        *o += s * gain;
    }
}

pub fn clip(out: &mut Pcm) {
    for o in out.iter_mut() {
        *o = o.clamp(-1.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::frame::{Pcm, FRAME_SAMPLES};

    #[test]
    fn mix_sums_and_clips() {
        let a: Pcm = [0.6; FRAME_SAMPLES];
        let mut out: Pcm = [9.0; FRAME_SAMPLES];
        mix(&[(&a, 1.0), (&a, 1.0)], &mut out);
        assert!(out.iter().all(|&x| x == 1.0));
        mix(&[(&a, 0.5)], &mut out);
        assert!(out.iter().all(|&x| (x - 0.3).abs() < 1e-6));
        mix(&[], &mut out);
        assert!(out.iter().all(|&x| x == 0.0));
        let neg: Pcm = [-0.8; FRAME_SAMPLES];
        mix(&[(&neg, 2.0)], &mut out);
        assert!(out.iter().all(|&x| x == -1.0));
    }
}
