//! Owns the capture and render threads and restarts them on another device while the
//! network session (and its audio channels) carries on.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use client_core::audio::activity::Activity;
use client_core::audio::codec::Encoder;
use client_core::audio::frame::{EncodedFrame, IncomingFrame};
use client_core::audio::vad::Vad;
use client_core::audio::volume::GainTable;
use tokio::sync::mpsc;

use crate::pipeline::{capture_loop, render_loop, Controls};
use crate::{FrameSink, FrameSource};

/// Builds the audio stream for a device id (`None` = system default). Runs on the audio
/// thread, so the stream itself need not be `Send`.
pub type CaptureOpener = Arc<dyn Fn(Option<&str>) -> Box<dyn FrameSource> + Send + Sync>;
pub type RenderOpener = Arc<dyn Fn(Option<&str>) -> Box<dyn FrameSink> + Send + Sync>;

pub struct AudioEngine {
    open_capture: CaptureOpener,
    open_render: RenderOpener,
    audio_out: mpsc::Sender<EncodedFrame>,
    /// Held here while no render thread runs; the thread hands it back when it stops.
    audio_in: Option<mpsc::Receiver<IncomingFrame>>,
    controls: Arc<Controls>,
    gains: Arc<GainTable>,
    activity: Arc<Activity>,
    sensitivity: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    capture: Option<JoinHandle<()>>,
    render: Option<JoinHandle<mpsc::Receiver<IncomingFrame>>>,
}

#[cfg(windows)]
impl AudioEngine {
    /// Starts both threads on WASAPI devices (`None` = default communications device).
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        capture: Option<String>,
        render: Option<String>,
        sensitivity: f32,
        audio_out: mpsc::Sender<EncodedFrame>,
        audio_in: mpsc::Receiver<IncomingFrame>,
        controls: Arc<Controls>,
        gains: Arc<GainTable>,
        activity: Arc<Activity>,
    ) -> AudioEngine {
        use crate::supervisor::Supervised;
        use crate::wasapi::{WasapiCapture, WasapiRender};
        use std::time::Instant;

        let open_capture: CaptureOpener = Arc::new(|device| {
            let device = device.map(str::to_owned);
            Box::new(Supervised::new(move || WasapiCapture::open(device.as_deref()), Instant::now))
        });
        let open_render: RenderOpener = Arc::new(|device| {
            let device = device.map(str::to_owned);
            Box::new(Supervised::new(move || WasapiRender::open(device.as_deref()), Instant::now))
        });
        AudioEngine::with_openers(
            open_capture,
            open_render,
            capture,
            render,
            sensitivity,
            audio_out,
            audio_in,
            controls,
            gains,
            activity,
        )
    }
}

impl AudioEngine {
    /// Like `start`, with the stream openers supplied (tests pass fakes).
    #[allow(clippy::too_many_arguments)]
    pub fn with_openers(
        open_capture: CaptureOpener,
        open_render: RenderOpener,
        capture: Option<String>,
        render: Option<String>,
        sensitivity: f32,
        audio_out: mpsc::Sender<EncodedFrame>,
        audio_in: mpsc::Receiver<IncomingFrame>,
        controls: Arc<Controls>,
        gains: Arc<GainTable>,
        activity: Arc<Activity>,
    ) -> AudioEngine {
        let mut engine = AudioEngine {
            open_capture,
            open_render,
            audio_out,
            audio_in: Some(audio_in),
            controls,
            gains,
            activity,
            sensitivity: Arc::new(AtomicU32::new(sensitivity.to_bits())),
            stop: Arc::new(AtomicBool::new(false)),
            capture: None,
            render: None,
        };
        engine.spawn(capture, render);
        engine
    }

    /// Takes effect on the next captured frame.
    pub fn set_sensitivity(&self, s: f32) {
        self.sensitivity.store(s.to_bits(), Ordering::Relaxed);
    }

    /// Reopens both streams on the given devices; the session channels survive.
    pub fn restart_devices(&mut self, capture: Option<String>, render: Option<String>) {
        self.halt();
        self.spawn(capture, render);
    }

    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.capture.take() {
            let _ = h.join();
        }
        match self.render.take().map(JoinHandle::join) {
            Some(Ok(rx)) => self.audio_in = Some(rx),
            Some(Err(_)) => tracing::error!("render thread panicked; incoming audio is lost"),
            None => {}
        }
    }

    fn spawn(&mut self, capture: Option<String>, render: Option<String>) {
        self.stop = Arc::new(AtomicBool::new(false));
        let spawned = std::thread::Builder::new().name("capture".into()).spawn({
            let (open, out, controls) =
                (self.open_capture.clone(), self.audio_out.clone(), self.controls.clone());
            let (activity, sensitivity, stop) =
                (self.activity.clone(), self.sensitivity.clone(), self.stop.clone());
            move || {
                #[cfg(windows)]
                let _mmcss = crate::wasapi::enter_pro_audio();
                let src = open(capture.as_deref());
                let vad = Vad::new(f32::from_bits(sensitivity.load(Ordering::Relaxed)));
                match Encoder::new() {
                    Ok(enc) => capture_loop(src, vad, enc, out, controls, activity.clone(), sensitivity, stop),
                    Err(e) => tracing::warn!("capture not started: {e}"),
                }
                // Otherwise the UI shows the last frame's state ("speaking") forever.
                activity.set_me(false, -100.0);
            }
        });
        match spawned {
            Ok(h) => self.capture = Some(h),
            Err(e) => tracing::error!("cannot start capture thread: {e}"),
        }
        let Some(audio_in) = self.audio_in.take() else {
            tracing::error!("no incoming audio channel; render not started");
            return;
        };
        let spawned = std::thread::Builder::new().name("render".into()).spawn({
            let (open, controls, gains) =
                (self.open_render.clone(), self.controls.clone(), self.gains.clone());
            let (activity, stop) = (self.activity.clone(), self.stop.clone());
            move || {
                #[cfg(windows)]
                let _mmcss = crate::wasapi::enter_pro_audio();
                let sink = open(render.as_deref());
                render_loop(sink, audio_in, controls, gains, activity, stop)
            }
        });
        match spawned {
            Ok(h) => self.render = Some(h),
            Err(e) => tracing::error!("cannot start render thread: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioError;
    use client_core::audio::frame::{Pcm, FRAME_SAMPLES, SAMPLE_RATE};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    fn sine(n: usize) -> Pcm {
        std::array::from_fn(|i| {
            let t = (n * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
            0.141 * (2.0 * std::f32::consts::PI * 300.0 * t).sin()
        })
    }

    struct Mic(usize);

    impl FrameSource for Mic {
        fn read_frame(&mut self, out: &mut Pcm) -> Result<(), AudioError> {
            std::thread::sleep(Duration::from_millis(5));
            *out = sine(self.0);
            self.0 += 1;
            Ok(())
        }
    }

    struct Speaker;

    impl FrameSink for Speaker {
        fn write_frame(&mut self, _: &Pcm) -> Result<(), AudioError> {
            std::thread::sleep(Duration::from_millis(5));
            Ok(())
        }
    }

    fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn restart_keeps_session_channels() {
        let opened: Arc<Mutex<Vec<(&str, Option<String>)>>> = Arc::default();
        let open_capture: CaptureOpener = {
            let opened = opened.clone();
            Arc::new(move |d| {
                opened.lock().unwrap().push(("capture", d.map(str::to_owned)));
                Box::new(Mic(0))
            })
        };
        let open_render: RenderOpener = {
            let opened = opened.clone();
            Arc::new(move |d| {
                opened.lock().unwrap().push(("render", d.map(str::to_owned)));
                Box::new(Speaker)
            })
        };
        let (out_tx, mut out_rx) = mpsc::channel(4096);
        let (in_tx, in_rx) = mpsc::channel(4096);
        let activity = Arc::new(Activity::new());
        let mut engine = AudioEngine::with_openers(
            open_capture,
            open_render,
            None,
            None,
            0.5,
            out_tx,
            in_rx,
            Arc::new(Controls::default()),
            Arc::new(GainTable::new()),
            activity.clone(),
        );
        wait_for("first frame", || out_rx.try_recv().is_ok());

        engine.restart_devices(Some("mic2".into()), Some("spk2".into()));

        // The same sender and receiver still work: capture sends again, render hears a voice.
        while out_rx.try_recv().is_ok() {}
        wait_for("frame after restart", || out_rx.try_recv().is_ok());
        let mut enc = Encoder::new().unwrap();
        let mut buf = [0u8; 256];
        for n in 0..50 {
            let len = enc.encode(&sine(n), &mut buf).unwrap();
            in_tx.try_send(IncomingFrame::new(9, n as u32, &buf[..len]).unwrap()).unwrap();
        }
        let mut heard = Vec::new();
        wait_for("remote voice", || {
            activity.speaking(&mut heard);
            heard == [9]
        });
        engine.stop();

        let opened = opened.lock().unwrap();
        assert!(opened.contains(&("capture", Some("mic2".into()))));
        assert!(opened.contains(&("render", Some("spk2".into()))));
    }

    #[test]
    fn stopped_capture_is_not_speaking() {
        let (out_tx, _out_rx) = mpsc::channel(4096);
        let (_in_tx, in_rx) = mpsc::channel(16);
        let activity = Arc::new(Activity::new());
        let engine = AudioEngine::with_openers(
            Arc::new(|_| Box::new(Mic(0))),
            Arc::new(|_| Box::new(Speaker)),
            None,
            None,
            0.5,
            out_tx,
            in_rx,
            Arc::new(Controls::default()),
            Arc::new(GainTable::new()),
            activity.clone(),
        );
        wait_for("me speaking", || activity.me_speaking());
        engine.stop();
        assert!(!activity.me_speaking());
        assert_eq!(activity.mic_level_dbfs(), -100.0);
    }
}
