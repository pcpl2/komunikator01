//! Opus wrappers fixed to 48 kHz mono, 20 ms frames, 32 kbit/s CBR with in-band FEC.

use opus::{Application, Bitrate, Channels};

use super::frame::{Pcm, MAX_PACKET, SAMPLE_RATE};

const BITRATE: i32 = 32_000;
const EXPECTED_LOSS_PERCENT: i32 = 10;

pub struct Encoder(opus::Encoder);

impl Encoder {
    pub fn new() -> anyhow::Result<Self> {
        let mut e = opus::Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip)?;
        e.set_bitrate(Bitrate::Bits(BITRATE))?;
        // CBR: under encryption, VBR packet sizes leak what is being said.
        e.set_vbr(false)?;
        e.set_inband_fec(true)?;
        e.set_packet_loss_perc(EXPECTED_LOSS_PERCENT)?;
        Ok(Self(e))
    }

    pub fn encode(&mut self, pcm: &Pcm, out: &mut [u8; MAX_PACKET]) -> anyhow::Result<usize> {
        Ok(self.0.encode_float(pcm, out)?)
    }
}

pub struct Decoder(opus::Decoder);

impl Decoder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self(opus::Decoder::new(SAMPLE_RATE, Channels::Mono)?))
    }

    /// Forgets the previous speaker before the decoder is reused for another one.
    pub fn reset(&mut self) -> anyhow::Result<()> {
        Ok(self.0.reset_state()?)
    }

    /// Decodes `packet`, or conceals a lost frame when `None`. With `fec`, `packet`
    /// is the frame after the lost one and its redundancy data is used instead.
    pub fn decode(&mut self, packet: Option<&[u8]>, fec: bool, out: &mut Pcm) -> anyhow::Result<()> {
        let n = self.0.decode_float(packet.unwrap_or(&[]), out, fec)?;
        out[n..].fill(0.0);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::audio::frame::{Pcm, FRAME_SAMPLES, SAMPLE_RATE};

    /// Sine at `freq` Hz with peak `amp`, continuing phase across frames via `start`.
    pub(crate) fn sine(freq: f32, amp: f32, start: usize) -> Pcm {
        std::array::from_fn(|i| {
            let t = (start + i) as f32 / SAMPLE_RATE as f32;
            amp * (2.0 * std::f32::consts::PI * freq * t).sin()
        })
    }

    pub(crate) fn rms(p: &[f32]) -> f32 {
        (p.iter().map(|x| x * x).sum::<f32>() / p.len() as f32).sqrt()
    }

    #[test]
    fn encode_is_cbr_size() {
        let mut enc = Encoder::new().unwrap();
        let mut out = [0u8; 256];
        for n in 0..50 {
            let len = enc.encode(&sine(440.0, 0.14, n * FRAME_SAMPLES), &mut out).unwrap();
            assert!((78..=82).contains(&len), "frame {n}: {len} bytes");
        }
    }

    #[test]
    fn roundtrip_preserves_tone() {
        let mut enc = Encoder::new().unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut packet = [0u8; 256];
        let mut out = [0f32; FRAME_SAMPLES];
        for n in 0..20 {
            let input = sine(440.0, 0.14, n * FRAME_SAMPLES);
            let len = enc.encode(&input, &mut packet).unwrap();
            dec.decode(Some(&packet[..len]), false, &mut out).unwrap();
            if n >= 10 {
                let db = 20.0 * (rms(&out) / rms(&input)).log10();
                assert!(db.abs() < 3.0, "frame {n}: {db} dB");
            }
        }
    }

    #[test]
    fn plc_produces_frame() {
        let mut dec = Decoder::new().unwrap();
        let mut out = [1f32; FRAME_SAMPLES];
        dec.decode(None, false, &mut out).unwrap();
    }
}
