//! Windows client: WASAPI audio and the capture/render pipeline.

pub mod supervisor;
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
}

/// Blocking sink of 20 ms mono frames; paces the render thread.
pub trait FrameSink {
    fn write_frame(&mut self, pcm: &Pcm) -> Result<(), AudioError>;
}
