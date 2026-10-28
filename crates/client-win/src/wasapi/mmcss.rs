//! Registers the current thread with MMCSS so audio keeps running under game load.

use windows::core::w;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW};

pub struct MmcssGuard(Option<HANDLE>);

/// Best effort: on failure the thread just runs at normal priority.
pub fn enter_pro_audio() -> MmcssGuard {
    let mut task_index = 0u32;
    match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
        Ok(h) => MmcssGuard(Some(h)),
        Err(e) => {
            tracing::warn!("MMCSS registration failed: {e}");
            MmcssGuard(None)
        }
    }
}

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        if let Some(h) = self.0 {
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(h);
            }
        }
    }
}
