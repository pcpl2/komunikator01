//! Capture and render loops connecting audio devices to the network session.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use client_core::audio::activity::Activity;
use client_core::audio::codec::{Decoder, Encoder};
use client_core::audio::frame::{EncodedFrame, IncomingFrame, Pcm, FRAME_SAMPLES, MAX_PACKET};
use client_core::audio::jitter::{JitterBuffer, Pop};
use client_core::audio::mixer::{accumulate, clip};
use client_core::audio::vad::Vad;
use client_core::audio::volume::GainTable;
use protocol::MemberId;
use tokio::sync::mpsc;

use crate::{FrameSink, FrameSource};

const MAX_VOICES: usize = 16;

/// Shared switches flipped by the UI, hotkeys or the console.
#[derive(Default)]
pub struct Controls {
    pub muted: AtomicBool,
    pub deafened: AtomicBool,
}

/// Reads the microphone until `stop` or a source error. Sequence numbers advance every
/// 20 ms whether or not a frame is sent, so receivers can tell pauses from loss.
/// `sensitivity` (f32 bits) is read every frame so the UI can retune the VAD live.
#[allow(clippy::too_many_arguments)]
pub fn capture_loop(
    mut src: impl FrameSource,
    mut vad: Vad,
    mut enc: Encoder,
    out: mpsc::Sender<EncodedFrame>,
    controls: Arc<Controls>,
    activity: Arc<Activity>,
    sensitivity: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
) {
    let mut pcm = [0f32; FRAME_SAMPLES];
    let mut applied = sensitivity.load(Ordering::Relaxed);
    vad.set_sensitivity(f32::from_bits(applied));
    let mut packet = [0u8; MAX_PACKET];
    let mut seq: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        if let Err(e) = src.read_frame(&mut pcm) {
            tracing::warn!("capture stopped: {e:?}");
            return;
        }
        let this_seq = seq;
        seq = seq.wrapping_add(1);
        let wanted = sensitivity.load(Ordering::Relaxed);
        if wanted != applied {
            applied = wanted;
            vad.set_sensitivity(f32::from_bits(wanted));
        }
        let speaking = vad.process(&pcm);
        // Deafen silences the mic too; the flags stay separate so undeafening restores
        // whatever mute state the user had.
        let sending = speaking
            && !controls.muted.load(Ordering::Relaxed)
            && !controls.deafened.load(Ordering::Relaxed);
        activity.set_me(sending, vad.level_dbfs());
        if !sending {
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
    /// `None` while the slot has never been used.
    id: Option<MemberId>,
    jitter: JitterBuffer,
    decoder: Decoder,
    idle_frames: u32,
    pcm: Pcm,
}

/// Plays everyone else until `stop` or a sink error; the sink's blocking write paces the loop.
/// Returns the input channel so a restarted loop can keep receiving on the same session.
pub fn render_loop(
    mut sink: impl FrameSink,
    mut inp: mpsc::Receiver<IncomingFrame>,
    controls: Arc<Controls>,
    gains: Arc<GainTable>,
    activity: Arc<Activity>,
    stop: Arc<AtomicBool>,
) -> mpsc::Receiver<IncomingFrame> {
    // Every slot and decoder is made up front so the real-time loop never allocates.
    let voices: Result<Vec<Box<Voice>>, _> = (0..MAX_VOICES)
        .map(|_| {
            Decoder::new().map(|decoder| {
                Box::new(Voice {
                    id: None,
                    jitter: JitterBuffer::new(),
                    decoder,
                    idle_frames: 0,
                    pcm: [0.0; FRAME_SAMPLES],
                })
            })
        })
        .collect();
    let mut voices = match voices {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("render not started: {e}");
            return inp;
        }
    };
    let mut out = [0f32; FRAME_SAMPLES];
    while !stop.load(Ordering::Relaxed) {
        while let Ok(f) = inp.try_recv() {
            let v = voice_for(&mut voices, f.sender);
            v.jitter.push(f.seq, f.payload());
            v.idle_frames = 0;
        }

        out.fill(0.0);
        for v in voices.iter_mut().filter(|v| v.id.is_some()) {
            v.idle_frames = v.idle_frames.saturating_add(1);
            if v.jitter.pop(&mut v.decoder, &mut v.pcm) == Pop::Audio {
                if let Some(id) = v.id {
                    accumulate(&mut out, &v.pcm, gains.get(id));
                    activity.heard(id);
                }
            }
        }
        activity.tick();
        clip(&mut out);
        if controls.deafened.load(Ordering::Relaxed) {
            out.fill(0.0);
        }
        if let Err(e) = sink.write_frame(&out) {
            tracing::warn!("render stopped: {e:?}");
            return inp;
        }
    }
    inp
}

/// The voice slot for `id`. A new sender takes an unused slot, or else the one
/// silent the longest (senders that left keep their slot until then).
fn voice_for(voices: &mut [Box<Voice>], id: MemberId) -> &mut Voice {
    if let Some(i) = voices.iter().position(|v| v.id == Some(id)) {
        return &mut voices[i];
    }
    let v = voices
        .iter_mut()
        .max_by_key(|v| if v.id.is_none() { u32::MAX } else { v.idle_frames })
        .expect("MAX_VOICES > 0");
    if v.id.is_some() {
        if let Err(e) = v.decoder.reset() {
            tracing::warn!("opus decoder reset failed: {e}");
        }
        v.jitter = JitterBuffer::new();
    }
    v.id = Some(id);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioError;
    use client_core::audio::frame::{IncomingFrame, FRAME_SAMPLES, SAMPLE_RATE};
    use client_core::audio::activity::Activity;
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
        run_capture_with(frames, muted, false)
    }

    fn run_capture_with(frames: Vec<Pcm>, muted: bool, deafened: bool) -> Vec<u32> {
        run_capture_full(frames, muted, deafened, 0.5, 0.5, Arc::new(Activity::new()))
    }

    fn run_capture_full(
        frames: Vec<Pcm>,
        muted: bool,
        deafened: bool,
        vad_sensitivity: f32,
        shared_sensitivity: f32,
        activity: Arc<Activity>,
    ) -> Vec<u32> {
        let (tx, mut rx) = mpsc::channel(1024);
        let controls = Arc::new(Controls::default());
        controls.muted.store(muted, Ordering::Relaxed);
        controls.deafened.store(deafened, Ordering::Relaxed);
        capture_loop(
            Script(frames.into_iter()),
            Vad::new(vad_sensitivity),
            Encoder::new().unwrap(),
            tx,
            controls,
            activity,
            Arc::new(AtomicU32::new(shared_sensitivity.to_bits())),
            Arc::new(AtomicBool::new(false)),
        );
        std::iter::from_fn(|| rx.try_recv().ok()).map(|f| f.seq).collect()
    }

    #[test]
    fn muted_sends_nothing() {
        assert!(run_capture((0..20).map(sine).collect(), true).is_empty());
    }

    #[test]
    fn deafened_sends_nothing() {
        assert!(run_capture_with((0..20).map(sine).collect(), false, true).is_empty());
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
        run_render_with(deafened, GainTable::new())
    }

    fn run_render_with(deafened: bool, gains: GainTable) -> Vec<Pcm> {
        run_render_full(deafened, gains, Arc::new(Activity::new()))
    }

    fn run_render_full(deafened: bool, gains: GainTable, activity: Arc<Activity>) -> Vec<Pcm> {
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
            Arc::new(gains),
            activity,
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
    fn render_applies_per_sender_gain() {
        // Both senders play the same sine, so one sender at gain g sums to (1 + g) times it.
        let one = peak(&run_render_with(false, GainTable::new())[5..]) / 2.0;
        let halved = GainTable::new();
        halved.set(1, 0.5);
        let p = peak(&run_render_with(false, halved)[5..]);
        assert!((p - one * 1.5).abs() < one * 0.1, "halved sender: {p} vs {}", one * 1.5);
        let silenced = GainTable::new();
        silenced.set(1, 0.0);
        let p = peak(&run_render_with(false, silenced)[5..]);
        assert!((p - one).abs() < one * 0.1, "only the other sender remains: {p} vs {one}");
    }

    #[test]
    fn new_sender_is_heard_when_all_slots_were_used() {
        let (tx, rx) = mpsc::channel(256);
        let mut buf = [0u8; 256];
        // 16 earlier senders (e.g. ids left over from reconnects) took every slot.
        for sender in 1..=16 {
            let len = Encoder::new().unwrap().encode(&[0.0; FRAME_SAMPLES], &mut buf).unwrap();
            tx.try_send(IncomingFrame::new(sender, 0, &buf[..len]).unwrap()).unwrap();
        }
        let mut enc = Encoder::new().unwrap();
        for n in 0..10 {
            let len = enc.encode(&sine(n), &mut buf).unwrap();
            tx.try_send(IncomingFrame::new(17, n as u32, &buf[..len]).unwrap()).unwrap();
        }
        let frames = Rc::new(RefCell::new(Vec::new()));
        render_loop(
            Recorder { frames: frames.clone(), limit: 12 },
            rx,
            Arc::new(Controls::default()),
            Arc::new(GainTable::new()),
            Arc::new(Activity::new()),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(peak(&frames.take()[5..]) > 0.1, "sender 17 plays");
    }

    #[test]
    fn render_marks_speakers() {
        let activity = Arc::new(Activity::new());
        run_render_full(false, GainTable::new(), activity.clone());
        let mut heard = Vec::new();
        activity.speaking(&mut heard);
        heard.sort();
        assert_eq!(heard, [1, 2]);
    }

    #[test]
    fn capture_publishes_me_speaking_and_level() {
        let activity = Arc::new(Activity::new());
        run_capture_full((0..5).map(sine).collect(), false, false, 0.5, 0.5, activity.clone());
        assert!(activity.me_speaking());
        let level = activity.mic_level_dbfs();
        assert!((-30.0..-20.0).contains(&level), "0.141 amplitude sine is about -23 dBFS: {level}");

        // Muted: the mic level still moves, but nobody hears the speaker.
        let activity = Arc::new(Activity::new());
        run_capture_full((0..5).map(sine).collect(), true, false, 0.5, 0.5, activity.clone());
        assert!(!activity.me_speaking());
        assert!(activity.mic_level_dbfs() > -30.0);
    }

    /// Noise at -40 dBFS sets the floor; speech at -28 dBFS opens the gate only when
    /// the sensitivity is high.
    fn speech_over_noise_sends(shared_sensitivity: f32) -> bool {
        let mut frames = vec![[0.01; FRAME_SAMPLES]; 100];
        frames.extend(vec![[0.04; FRAME_SAMPLES]; 5]);
        let activity = Arc::new(Activity::new());
        run_capture_full(frames, false, false, 0.0, shared_sensitivity, activity.clone());
        activity.me_speaking()
    }

    #[test]
    fn capture_applies_new_sensitivity() {
        assert!(!speech_over_noise_sends(0.0), "low sensitivity stays closed");
        assert!(speech_over_noise_sends(1.0), "the shared value overrides the Vad's own");
    }

    #[test]
    fn deafened_outputs_silence() {
        assert_eq!(peak(&run_render(true)), 0.0);
    }
}
