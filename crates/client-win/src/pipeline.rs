//! Capture and render loops connecting audio devices to the network session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use client_core::audio::codec::{Decoder, Encoder};
use client_core::audio::frame::{EncodedFrame, IncomingFrame, Pcm, FRAME_SAMPLES, MAX_PACKET};
use client_core::audio::jitter::{JitterBuffer, Pop};
use client_core::audio::mixer::{accumulate, clip};
use client_core::audio::vad::Vad;
use protocol::MemberId;
use tokio::sync::mpsc;

use crate::{FrameSink, FrameSource};

const MAX_VOICES: usize = 16;
/// A sender silent this long (60 s) gives up its decoder slot.
const VOICE_IDLE_FRAMES: u32 = 3_000;

/// Shared switches flipped by the UI, hotkeys or the console.
#[derive(Default)]
pub struct Controls {
    pub muted: AtomicBool,
    pub deafened: AtomicBool,
}

/// Reads the microphone until `stop` or a source error. Sequence numbers advance every
/// 20 ms whether or not a frame is sent, so receivers can tell pauses from loss.
pub fn capture_loop(
    mut src: impl FrameSource,
    mut vad: Vad,
    mut enc: Encoder,
    out: mpsc::Sender<EncodedFrame>,
    controls: Arc<Controls>,
    stop: Arc<AtomicBool>,
) {
    let mut pcm = [0f32; FRAME_SAMPLES];
    let mut packet = [0u8; MAX_PACKET];
    let mut seq: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = src.read_frame(&mut pcm) {
            tracing::warn!("capture stopped: {e:?}");
            return;
        }
        let this_seq = seq;
        seq = seq.wrapping_add(1);
        let speaking = vad.process(&pcm);
        if !speaking || controls.muted.load(Ordering::Relaxed) {
            continue;
        }
        match enc.encode(&pcm, &mut packet) {
            Ok(len) => {
                if let Some(f) = EncodedFrame::new(this_seq, &packet[..len]) {
                    // A full queue means the network is stuck; dropping keeps latency bounded.
                    let _ = out.try_send(f);
                }
            }
            Err(e) => tracing::warn!("opus encode failed: {e}"),
        }
    }
}

struct Voice {
    id: MemberId,
    jitter: JitterBuffer,
    decoder: Decoder,
    idle_frames: u32,
    pcm: Pcm,
}

/// Plays everyone else until `stop` or a sink error; the sink's blocking write paces the loop.
pub fn render_loop(
    mut sink: impl FrameSink,
    mut inp: mpsc::Receiver<IncomingFrame>,
    controls: Arc<Controls>,
    stop: Arc<AtomicBool>,
) {
    let mut voices: Vec<Option<Box<Voice>>> = (0..MAX_VOICES).map(|_| None).collect();
    let mut out = [0f32; FRAME_SAMPLES];
    while !stop.load(Ordering::Relaxed) {
        while let Ok(f) = inp.try_recv() {
            if let Some(v) = voice_for(&mut voices, f.sender) {
                v.jitter.push(f.seq, f.payload());
                v.idle_frames = 0;
            }
        }

        out.fill(0.0);
        for slot in voices.iter_mut() {
            let Some(v) = slot else { continue };
            v.idle_frames += 1;
            if v.jitter.pop(&mut v.decoder, &mut v.pcm) == Pop::Audio {
                accumulate(&mut out, &v.pcm, 1.0);
            }
            if v.idle_frames > VOICE_IDLE_FRAMES {
                *slot = None;
            }
        }
        clip(&mut out);
        if controls.deafened.load(Ordering::Relaxed) {
            out.fill(0.0);
        }
        if let Err(e) = sink.write_frame(&out) {
            tracing::warn!("render stopped: {e:?}");
            return;
        }
    }
}

/// The voice slot for `id`, claiming a free one for a new sender.
fn voice_for(voices: &mut [Option<Box<Voice>>], id: MemberId) -> Option<&mut Voice> {
    let index = match voices.iter().position(|v| v.as_ref().is_some_and(|v| v.id == id)) {
        Some(i) => i,
        None => {
            let free = voices.iter().position(Option::is_none)?;
            let decoder = Decoder::new().ok()?;
            voices[free] = Some(Box::new(Voice {
                id,
                jitter: JitterBuffer::new(),
                decoder,
                idle_frames: 0,
                pcm: [0.0; FRAME_SAMPLES],
            }));
            free
        }
    };
    voices[index].as_deref_mut()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioError;
    use client_core::audio::frame::{IncomingFrame, FRAME_SAMPLES, SAMPLE_RATE};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn sine(n: usize) -> Pcm {
        std::array::from_fn(|i| {
            let t = (n * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
            0.141 * (2.0 * std::f32::consts::PI * 300.0 * t).sin()
        })
    }

    /// Plays a script of frames, then reports an error to end the loop.
    struct Script(std::vec::IntoIter<Pcm>);

    impl FrameSource for Script {
        fn read_frame(&mut self, out: &mut Pcm) -> Result<(), AudioError> {
            *out = self.0.next().ok_or(AudioError::Other("end".into()))?;
            Ok(())
        }
    }

    fn run_capture(frames: Vec<Pcm>, muted: bool) -> Vec<u32> {
        let (tx, mut rx) = mpsc::channel(1024);
        let controls = Arc::new(Controls::default());
        controls.muted.store(muted, Ordering::Relaxed);
        capture_loop(
            Script(frames.into_iter()),
            Vad::new(0.5),
            Encoder::new().unwrap(),
            tx,
            controls,
            Arc::new(AtomicBool::new(false)),
        );
        std::iter::from_fn(|| rx.try_recv().ok()).map(|f| f.seq).collect()
    }

    #[test]
    fn muted_sends_nothing() {
        assert!(run_capture((0..20).map(sine).collect(), true).is_empty());
    }

    #[test]
    fn silence_sends_nothing() {
        assert!(run_capture(vec![[0.0; FRAME_SAMPLES]; 20], false).is_empty());
    }

    #[test]
    fn speech_sends_consecutive_seq() {
        assert_eq!(run_capture((0..3).map(sine).collect(), false), [0, 1, 2]);
    }

    #[test]
    fn seq_advances_during_silence() {
        let mut frames = vec![[0.0; FRAME_SAMPLES]; 10];
        frames.extend((0..2).map(sine));
        assert_eq!(run_capture(frames, false), [10, 11]);
    }

    /// Records frames, then fails after `limit` writes to end the loop.
    struct Recorder {
        frames: Rc<RefCell<Vec<Pcm>>>,
        limit: usize,
    }

    impl FrameSink for Recorder {
        fn write_frame(&mut self, pcm: &Pcm) -> Result<(), AudioError> {
            let mut f = self.frames.borrow_mut();
            if f.len() == self.limit {
                return Err(AudioError::Other("end".into()));
            }
            f.push(*pcm);
            Ok(())
        }
    }

    fn run_render(deafened: bool) -> Vec<Pcm> {
        let (tx, rx) = mpsc::channel(256);
        for sender in [1, 2] {
            let mut enc = Encoder::new().unwrap();
            let mut buf = [0u8; 256];
            for n in 0..10 {
                let len = enc.encode(&sine(n), &mut buf).unwrap();
                tx.try_send(IncomingFrame::new(sender, n as u32, &buf[..len]).unwrap()).unwrap();
            }
        }
        let frames = Rc::new(RefCell::new(Vec::new()));
        let controls = Arc::new(Controls::default());
        controls.deafened.store(deafened, Ordering::Relaxed);
        render_loop(
            Recorder { frames: frames.clone(), limit: 12 },
            rx,
            controls,
            Arc::new(AtomicBool::new(false)),
        );
        frames.take()
    }

    fn peak(frames: &[Pcm]) -> f32 {
        frames.iter().flatten().fold(0.0, |p: f32, x| p.max(x.abs()))
    }

    #[test]
    fn render_mixes_two_senders() {
        let frames = run_render(false);
        assert_eq!(frames.len(), 12);
        assert!(peak(&frames[5..]) > 0.1, "audio plays");
        assert!(peak(&frames[5..]) > 0.16, "two senders are summed");
    }

    #[test]
    fn deafened_outputs_silence() {
        assert_eq!(peak(&run_render(true)), 0.0);
    }
}
