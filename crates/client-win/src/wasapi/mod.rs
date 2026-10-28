//! WASAPI capture and render streams (48 kHz mono f32, communications category).

mod capture;
mod mmcss;
mod render;

pub use capture::WasapiCapture;
pub use mmcss::{enter_pro_audio, MmcssGuard};
pub use render::WasapiRender;

use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eRender, AudioCategory_Communications, EDataFlow, IAudioClient,
    IAudioClient2, IAudioClientDuckingControl, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDCLNT_STREAMOPTIONS_NONE, AUDIO_DUCKING_OPTIONS_DO_NOT_DUCK_OTHER_STREAMS,
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
    client: IAudioClient,
    event: HANDLE,
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
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
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;

        // Communications category enables the system echo/noise processing where available.
        let props = AudioClientProperties {
            cbSize: size_of::<AudioClientProperties>() as u32,
            bIsOffload: false.into(),
            eCategory: AudioCategory_Communications,
            Options: AUDCLNT_STREAMOPTIONS_NONE,
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

        // A communications stream would otherwise make Windows lower the game's volume.
        match client.cast::<IAudioClientDuckingControl>() {
            Ok(d) => d.SetDuckingOptionsForCurrentStream(AUDIO_DUCKING_OPTIONS_DO_NOT_DUCK_OTHER_STREAMS)?,
            Err(e) => tracing::warn!("cannot opt out of ducking: {e}"),
        }

        let event = CreateEventW(None, false, false, None)?;
        let stream = Stream { client, event };
        stream.client.SetEventHandle(stream.event)?;
        Ok(stream)
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
