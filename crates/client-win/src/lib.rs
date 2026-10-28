//! Windows client: WASAPI audio and the capture/render pipeline.

#[cfg(windows)]
pub mod dpapi;
#[cfg(windows)]
pub mod identity_store;
pub mod engine;
pub mod pipeline;
#[cfg(windows)]
pub mod startup;
pub mod supervisor;
#[cfg(windows)]
pub mod ui;
#[cfg(windows)]
pub mod wasapi;

use client_core::audio::frame::Pcm;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// The endpoint went away (unplugged, default changed); reopen it.
    DeviceInvalidated,
    Other(String),
}

/// Blocking source of 20 ms mono frames; paces the capture thread.
pub trait FrameSource {
    fn read_frame(&mut self, out: &mut Pcm) -> Result<(), AudioError>;

    /// True when the stream should move to a new system default device.
    fn device_changed(&mut self) -> bool {
        false
    }
}

/// Blocking sink of 20 ms mono frames; paces the render thread.
pub trait FrameSink {
    fn write_frame(&mut self, pcm: &Pcm) -> Result<(), AudioError>;

    /// True when the stream should move to a new system default device.
    fn device_changed(&mut self) -> bool {
        false
    }
}

impl FrameSource for Box<dyn FrameSource> {
    fn read_frame(&mut self, out: &mut Pcm) -> Result<(), AudioError> {
        (**self).read_frame(out)
    }

    fn device_changed(&mut self) -> bool {
        (**self).device_changed()
    }
}

impl FrameSink for Box<dyn FrameSink> {
    fn write_frame(&mut self, pcm: &Pcm) -> Result<(), AudioError> {
        (**self).write_frame(pcm)
    }

    fn device_changed(&mut self) -> bool {
        (**self).device_changed()
    }
}
