//! WASAPI capture and render streams (48 kHz mono f32, communications category). The
//! microphone is opened raw where the driver allows it: the program does its own echo
//! cancellation and noise suppression, and Windows' voice processing dulls the voice.

mod capture;
mod mmcss;
mod render;

pub use capture::WasapiCapture;
pub use mmcss::{enter_pro_audio, MmcssGuard};
pub use render::WasapiRender;

use std::mem::ManuallyDrop;

use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eRender, AudioCategory_Communications, EDataFlow, IAudioClient,
    IAudioClient2, IAudioClientDuckingControl, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDCLNT_STREAMOPTIONS, AUDCLNT_STREAMOPTIONS_NONE, AUDCLNT_STREAMOPTIONS_RAW,
    AUDIO_DUCKING_OPTIONS_DO_NOT_DUCK_OTHER_STREAMS,
    AudioClientProperties, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::CreateEventW;

use crate::AudioError;
use client_core::audio::frame::SAMPLE_RATE;

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

impl From<windows::core::Error> for AudioError {
    fn from(e: windows::core::Error) -> Self {
        if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
            AudioError::DeviceInvalidated
        } else {
            AudioError::Other(e.to_string())
        }
    }
}

/// An initialized, event-driven shared-mode client plus its event handle.
struct Stream {
    /// Released by hand in `drop`, before the event it signals is closed.
    client: ManuallyDrop<IAudioClient>,
    event: HANDLE,
    tracker: Option<DefaultTracker>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            ManuallyDrop::drop(&mut self.client);
            let _ = CloseHandle(self.event);
        }
    }
}

/// Opens `device_id` (or the default communications device) for 48 kHz mono f32.
/// `buffer_ms` is the requested WASAPI buffer length.
fn open_stream(flow: EDataFlow, device_id: Option<&str>, buffer_ms: i64) -> Result<Stream, AudioError> {
    unsafe {
        // S_FALSE (already initialized on this thread) is fine.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device: IMMDevice = match device_id {
            Some(id) => enumerator.GetDevice(&HSTRING::from(id))?,
            None => enumerator.GetDefaultAudioEndpoint(flow, eCommunications)?,
        };
        let mut client = None;
        for &options in processing_options(flow) {
            match init_client(&device, options, buffer_ms) {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                // Not every driver offers raw capture; the processed stream still works.
                Err(e) if options == AUDCLNT_STREAMOPTIONS_RAW => {
                    tracing::info!("raw microphone not available, using Windows processing: {e}")
                }
                Err(e) => return Err(e.into()),
            }
        }
        let client = client.ok_or_else(|| AudioError::Other("no stream options left".into()))?;

        // A communications stream would otherwise make Windows lower the game's volume.
        // Best effort: failing here must not keep the device from opening.
        let ducking = client.cast::<IAudioClientDuckingControl>().and_then(|d| {
            d.SetDuckingOptionsForCurrentStream(AUDIO_DUCKING_OPTIONS_DO_NOT_DUCK_OTHER_STREAMS)
        });
        if let Err(e) = ducking {
            tracing::warn!("cannot opt out of ducking: {e}");
        }

        let tracker = match device_id {
            Some(_) => None,
            None => Some(DefaultTracker { enumerator, flow, id: device_id_of(&device)?, frames: 0 }),
        };
        let event = CreateEventW(None, false, false, None)?;
        let stream = Stream { client: ManuallyDrop::new(client), event, tracker };
        stream.client.SetEventHandle(stream.event)?;
        Ok(stream)
    }
}

/// Stream options to try in order: the microphone without Windows' processing first.
fn processing_options(flow: EDataFlow) -> &'static [AUDCLNT_STREAMOPTIONS] {
    if flow == eCapture {
        &[AUDCLNT_STREAMOPTIONS_RAW, AUDCLNT_STREAMOPTIONS_NONE]
    } else {
        &[AUDCLNT_STREAMOPTIONS_NONE]
    }
}

/// A fresh client on `device`, initialized for 48 kHz mono f32 with `options`.
unsafe fn init_client(
    device: &IMMDevice,
    options: AUDCLNT_STREAMOPTIONS,
    buffer_ms: i64,
) -> windows::core::Result<IAudioClient> {
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
    // Communications category enables the system echo/noise processing where available
    // (unless `options` asks for raw).
    let props = AudioClientProperties {
        cbSize: size_of::<AudioClientProperties>() as u32,
        bIsOffload: false.into(),
        eCategory: AudioCategory_Communications,
        Options: options,
    };
    client.cast::<IAudioClient2>()?.SetClientProperties(&props)?;

    let format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: 1,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, buffer_ms * 10_000, 0, &format, None)?;
    Ok(client)
}

/// Notices when Windows switches the default communications device (e.g. a headset is
/// plugged in), so a stream opened on "the default" can follow it.
struct DefaultTracker {
    enumerator: IMMDeviceEnumerator,
    flow: EDataFlow,
    id: String,
    frames: u32,
}

/// Check about once a second (50 frames of 20 ms).
const DEFAULT_CHECK_FRAMES: u32 = 50;

impl DefaultTracker {
    fn changed(&mut self) -> bool {
        self.frames += 1;
        if self.frames < DEFAULT_CHECK_FRAMES {
            return false;
        }
        self.frames = 0;
        let current = unsafe { self.enumerator.GetDefaultAudioEndpoint(self.flow, eCommunications) }
            .and_then(|d| device_id_of(&d));
        matches!(current, Ok(id) if id != self.id)
    }
}

impl Stream {
    fn default_changed(&mut self) -> bool {
        self.tracker.as_mut().is_some_and(DefaultTracker::changed)
    }
}

fn device_id_of(device: &IMMDevice) -> windows::core::Result<String> {
    unsafe {
        let raw = device.GetId()?;
        let id = raw.to_string();
        CoTaskMemFree(Some(raw.0 as _));
        id.map_err(|e| windows::core::Error::new(windows::core::HRESULT(-1), e.to_string()))
    }
}

/// (id, friendly name) of active capture devices.
pub fn list_capture_devices() -> Vec<(String, String)> {
    list_devices(eCapture).unwrap_or_default()
}

/// (id, friendly name) of active render devices.
pub fn list_render_devices() -> Vec<(String, String)> {
    list_devices(eRender).unwrap_or_default()
}

fn list_devices(flow: EDataFlow) -> windows::core::Result<Vec<(String, String)>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..devices.GetCount()? {
            let device = devices.Item(i)?;
            let raw_id = device.GetId()?;
            let id = raw_id.to_string().unwrap_or_default();
            CoTaskMemFree(Some(raw_id.0 as _));
            let name = device
                .OpenPropertyStore(STGM_READ)?
                .GetValue(&PKEY_Device_FriendlyName)?
                .to_string();
            out.push((id, name));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_microphone_asks_for_raw_first_and_the_speakers_never() {
        assert_eq!(processing_options(eCapture), [AUDCLNT_STREAMOPTIONS_RAW, AUDCLNT_STREAMOPTIONS_NONE]);
        assert_eq!(processing_options(eRender), [AUDCLNT_STREAMOPTIONS_NONE]);
    }
}
