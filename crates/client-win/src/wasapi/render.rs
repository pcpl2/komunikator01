//! Speaker output fed 20 ms at a time; blocks until there is room, which paces the caller.

use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::{eRender, IAudioRenderClient};
use windows::Win32::System::Threading::WaitForSingleObject;

use super::{open_stream, Stream};
use crate::{AudioError, FrameSink};
use client_core::audio::frame::{Pcm, FRAME_SAMPLES};

const BUFFER_MS: i64 = 40;
const WAIT_MS: u32 = 200;

pub struct WasapiRender {
    render: IAudioRenderClient,
    stream: Stream,
    buffer_frames: u32,
}

impl WasapiRender {
    /// None opens the default communications output.
    pub fn open(device_id: Option<&str>) -> Result<Self, AudioError> {
        let stream = open_stream(eRender, device_id, BUFFER_MS)?;
        unsafe {
            let render: IAudioRenderClient = stream.client.GetService()?;
            let buffer_frames = stream.client.GetBufferSize()?;
            stream.client.Start()?;
            Ok(Self { render, stream, buffer_frames })
        }
    }
}

impl FrameSink for WasapiRender {
    fn write_frame(&mut self, pcm: &Pcm) -> Result<(), AudioError> {
        let n = FRAME_SAMPLES as u32;
        unsafe {
            while self.buffer_frames - self.stream.client.GetCurrentPadding()? < n {
                if WaitForSingleObject(self.stream.event, WAIT_MS) != WAIT_OBJECT_0 {
                    return Err(AudioError::Other("render event timed out".into()));
                }
            }
            let data = self.render.GetBuffer(n)?;
            std::slice::from_raw_parts_mut(data as *mut f32, FRAME_SAMPLES).copy_from_slice(pcm);
            self.render.ReleaseBuffer(n, 0)?;
        }
        Ok(())
    }
}
