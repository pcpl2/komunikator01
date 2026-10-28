//! Microphone capture, accumulated into 20 ms frames.

use windows::Win32::Media::Audio::{eCapture, IAudioCaptureClient, AUDCLNT_BUFFERFLAGS_SILENT};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::System::Threading::WaitForSingleObject;

use super::{open_stream, Stream};
use crate::{AudioError, FrameSource};
use client_core::audio::frame::{Pcm, FRAME_SAMPLES};

const BUFFER_MS: i64 = 20;
/// Pending samples kept between reads; generous so a late read never overflows.
const PENDING_CAP: usize = FRAME_SAMPLES * 8;
const WAIT_MS: u32 = 200;

pub struct WasapiCapture {
    // Field order: the capture client is released before the stream stops.
    capture: IAudioCaptureClient,
    stream: Stream,
    pending: Box<[f32; PENDING_CAP]>,
    pending_len: usize,
}

impl WasapiCapture {
    /// `None` opens the default communications microphone.
    pub fn open(device_id: Option<&str>) -> Result<Self, AudioError> {
        let stream = open_stream(eCapture, device_id, BUFFER_MS)?;
        unsafe {
            let capture: IAudioCaptureClient = stream.client.GetService()?;
            stream.client.Start()?;
            Ok(Self { capture, stream, pending: Box::new([0.0; PENDING_CAP]), pending_len: 0 })
        }
    }

    /// Moves every packet WASAPI has ready into `pending`.
    fn drain_packets(&mut self) -> Result<(), AudioError> {
        unsafe {
            loop {
                if self.capture.GetNextPacketSize()? == 0 {
                    return Ok(());
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize;
                let room = PENDING_CAP - self.pending_len;
                let take = n.min(room);
                let dst = &mut self.pending[self.pending_len..self.pending_len + take];
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                    dst.fill(0.0);
                } else {
                    dst.copy_from_slice(std::slice::from_raw_parts(data as *const f32, take));
                }
                self.pending_len += take;
                self.capture.ReleaseBuffer(frames)?;
            }
        }
    }
}

impl FrameSource for WasapiCapture {
    fn read_frame(&mut self, out: &mut Pcm) -> Result<(), AudioError> {
        while self.pending_len < FRAME_SAMPLES {
            let woke = unsafe { WaitForSingleObject(self.stream.event, WAIT_MS) };
            if woke != WAIT_OBJECT_0 {
                return Err(AudioError::Other("capture event timed out".into()));
            }
            self.drain_packets()?;
        }
        out.copy_from_slice(&self.pending[..FRAME_SAMPLES]);
        self.pending.copy_within(FRAME_SAMPLES..self.pending_len, 0);
        self.pending_len -= FRAME_SAMPLES;
        Ok(())
    }
}
