//! Keeps an audio stream alive across device loss by reopening it.

use std::time::{Duration, Instant};

use client_core::audio::frame::Pcm;

use crate::{AudioError, FrameSink, FrameSource};

const RETRY_EVERY: Duration = Duration::from_secs(1);
/// While no device is open, each call waits one frame so the audio loop keeps its pace.
const IDLE_FRAME: Duration = Duration::from_millis(20);

/// Wraps a stream opener; on any stream error the stream is dropped and reopened
/// at most once per second. Meanwhile reads yield silence and writes are dropped.
pub struct Supervised<T, F, C> {
    opener: F,
    clock: C,
    stream: Option<T>,
    next_attempt: Instant,
    idle_wait: bool,
}

impl<T, F, C> Supervised<T, F, C>
where
    F: FnMut() -> Result<T, AudioError>,
    C: Fn() -> Instant,
{
    pub fn new(opener: F, clock: C) -> Self {
        let next_attempt = clock();
        let mut s = Self { opener, clock, stream: None, next_attempt, idle_wait: !cfg!(test) };
        s.ensure_open();
        s
    }

    fn ensure_open(&mut self) -> bool {
        if self.stream.is_none() && (self.clock)() >= self.next_attempt {
            match (self.opener)() {
                Ok(s) => self.stream = Some(s),
                Err(e) => {
                    tracing::warn!("audio device unavailable: {e:?}");
                    self.next_attempt = (self.clock)() + RETRY_EVERY;
                }
            }
        }
        self.stream.is_some()
    }

    fn lost(&mut self, e: AudioError) {
        tracing::warn!("audio stream lost: {e:?}");
        self.stream = None;
        self.next_attempt = (self.clock)() + RETRY_EVERY;
    }

    /// Drops the stream so the next call reopens it at once on the new default device.
    fn switch_device(&mut self) {
        tracing::info!("default audio device changed; reopening");
        self.stream = None;
        self.next_attempt = (self.clock)();
    }

    fn idle(&self) {
        if self.idle_wait {
            std::thread::sleep(IDLE_FRAME);
        }
    }
}

impl<T: FrameSource, F, C> FrameSource for Supervised<T, F, C>
where
    F: FnMut() -> Result<T, AudioError>,
    C: Fn() -> Instant,
{
    fn read_frame(&mut self, out: &mut Pcm) -> Result<(), AudioError> {
        if self.ensure_open() {
            let stream = self.stream.as_mut().expect("open");
            match stream.read_frame(out) {
                Ok(()) => {
                    if stream.device_changed() {
                        self.switch_device();
                    }
                    return Ok(());
                }
                Err(e) => self.lost(e),
            }
        } else {
            self.idle();
        }
        out.fill(0.0);
        Ok(())
    }
}

impl<T: FrameSink, F, C> FrameSink for Supervised<T, F, C>
where
    F: FnMut() -> Result<T, AudioError>,
    C: Fn() -> Instant,
{
    fn write_frame(&mut self, pcm: &Pcm) -> Result<(), AudioError> {
        if self.ensure_open() {
            let stream = self.stream.as_mut().expect("open");
            match stream.write_frame(pcm) {
                Ok(()) if stream.device_changed() => self.switch_device(),
                Ok(()) => {}
                Err(e) => self.lost(e),
            }
        } else {
            self.idle();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use client_core::audio::frame::FRAME_SAMPLES;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    struct FakeSource {
        ok_frames: usize,
    }

    impl FrameSource for FakeSource {
        fn read_frame(&mut self, out: &mut Pcm) -> Result<(), AudioError> {
            if self.ok_frames == 0 {
                return Err(AudioError::DeviceInvalidated);
            }
            self.ok_frames -= 1;
            out.fill(0.5);
            Ok(())
        }
    }

    fn clock() -> (Rc<Cell<Instant>>, impl Fn() -> Instant) {
        let t = Rc::new(Cell::new(Instant::now()));
        let c = t.clone();
        (t, move || c.get())
    }

    #[test]
    fn supervisor_reopens_after_invalidation() {
        let opens = Rc::new(Cell::new(0));
        let o = opens.clone();
        let (t, now) = clock();
        let mut s = Supervised::new(
            move || {
                o.set(o.get() + 1);
                Ok(FakeSource { ok_frames: 3 })
            },
            now,
        );
        let mut out = [0f32; FRAME_SAMPLES];
        for _ in 0..3 {
            s.read_frame(&mut out).unwrap();
            assert_eq!(out[0], 0.5);
        }
        s.read_frame(&mut out).unwrap();
        assert_eq!(out[0], 0.0, "silence while the device is gone");
        s.read_frame(&mut out).unwrap();
        assert_eq!(opens.get(), 1, "no reopen before 1 s");
        t.set(t.get() + Duration::from_secs(1));
        s.read_frame(&mut out).unwrap();
        assert_eq!(opens.get(), 2);
        assert_eq!(out[0], 0.5);
    }

    #[test]
    fn supervisor_retries_until_open_succeeds() {
        let opens = Rc::new(Cell::new(0));
        let o = opens.clone();
        let (t, now) = clock();
        let mut s = Supervised::new(
            move || {
                o.set(o.get() + 1);
                if o.get() < 3 {
                    Err(AudioError::Other("busy".into()))
                } else {
                    Ok(FakeSource { ok_frames: 10 })
                }
            },
            now,
        );
        let mut out = [0f32; FRAME_SAMPLES];
        for _ in 0..3 {
            s.read_frame(&mut out).unwrap();
            t.set(t.get() + Duration::from_secs(1));
        }
        assert_eq!(opens.get(), 3);
        assert_eq!(out[0], 0.5);
    }

    /// Works, but reports once that the system default device changed.
    struct Switching {
        report_change: bool,
    }

    impl FrameSource for Switching {
        fn read_frame(&mut self, out: &mut Pcm) -> Result<(), AudioError> {
            out.fill(0.5);
            Ok(())
        }

        fn device_changed(&mut self) -> bool {
            std::mem::take(&mut self.report_change)
        }
    }

    #[test]
    fn supervisor_follows_default_device_change_immediately() {
        let opens = Rc::new(Cell::new(0));
        let o = opens.clone();
        let (_t, now) = clock();
        let mut s = Supervised::new(
            move || {
                o.set(o.get() + 1);
                Ok(Switching { report_change: o.get() == 1 })
            },
            now,
        );
        let mut out = [0f32; FRAME_SAMPLES];
        s.read_frame(&mut out).unwrap();
        s.read_frame(&mut out).unwrap();
        assert_eq!(opens.get(), 2, "reopened on the new default without waiting");
        assert_eq!(out[0], 0.5);
    }

    struct FakeSink(Rc<Cell<usize>>);

    impl FrameSink for FakeSink {
        fn write_frame(&mut self, _: &Pcm) -> Result<(), AudioError> {
            self.0.set(self.0.get() + 1);
            Err(AudioError::DeviceInvalidated)
        }
    }

    #[test]
    fn sink_drops_frames_while_reopening() {
        let writes = Rc::new(Cell::new(0));
        let w = writes.clone();
        let (_t, now) = clock();
        let mut s = Supervised::new(move || Ok(FakeSink(w.clone())), now);
        let pcm = [0f32; FRAME_SAMPLES];
        s.write_frame(&pcm).unwrap();
        s.write_frame(&pcm).unwrap();
        assert_eq!(writes.get(), 1, "second frame dropped until reopen");
    }
}
