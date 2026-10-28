//! Tray icon rendering.
use windows::core::BOOL;
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Connected,
    Muted,
    Deafened,
    Disconnected,
}

/// Fills a disc of radius `r` around (`cx`, `cy`), sampled at pixel centres.
fn disc(px: &mut [u32], size: u32, cx: f32, cy: f32, r: f32, argb: u32) {
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            if dx * dx + dy * dy <= r * r {
                px[(y * size + x) as usize] = argb;
            }
        }
    }
}

/// BGRA pixels (0xAARRGGBB): a status disc (green connected, red muted, dark red deafened,
/// grey disconnected), optionally with an orange unread-chat dot top-right; transparent elsewhere.
pub fn tray_pixels(state: TrayState, badge: bool, size: u32) -> Vec<u32> {
    let mut px = vec![0u32; (size * size) as usize];
    let colour = match state {
        TrayState::Connected => 0xFF2E_CC40,
        TrayState::Muted => 0xFFE5_3935,
        TrayState::Deafened => 0xFF8B_0000,
        TrayState::Disconnected => 0xFF80_8080,
    };
    let s = size as f32;
    disc(&mut px, size, s / 2.0, s / 2.0, s / 2.0 - 1.0, colour);
    if badge {
        let r = s / 5.0;
        disc(&mut px, size, s - r - 0.5, r + 0.5, r, 0xFFFF_9800);
    }
    px
}

/// Builds an HICON from `tray_pixels` output (caller owns it, DestroyIcon when done).
/// Returns a null icon if Windows refuses.
pub fn make_icon(pixels: &[u32], size: u32) -> HICON {
    assert_eq!(pixels.len(), (size * size) as usize);
    unsafe {
        let color = CreateBitmap(size as i32, size as i32, 1, 32, Some(pixels.as_ptr().cast()));
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, None);
        let info = ICONINFO {
            fIcon: BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_pixels_states_differ_and_badge_is_drawn() {
        let s = 16u32;
        let states = [
            TrayState::Connected,
            TrayState::Muted,
            TrayState::Deafened,
            TrayState::Disconnected,
        ];
        let px: Vec<_> = states.iter().map(|&st| tray_pixels(st, false, s)).collect();
        for p in &px {
            assert_eq!(p.len(), 256);
            assert_eq!(p[0] >> 24, 0, "corner must be transparent");
            assert_eq!(p[(8 * s + 8) as usize] >> 24, 0xFF, "centre must be opaque");
        }
        for i in 0..4 {
            for j in i + 1..4 {
                assert_ne!(px[i], px[j]);
            }
        }
        let with = tray_pixels(TrayState::Connected, true, s);
        assert_ne!(with, px[0]);
        // Badge sits top-right: that region changes, the bottom-left does not.
        let idx = |x: u32, y: u32| (y * s + x) as usize;
        assert_ne!(with[idx(13, 2)], px[0][idx(13, 2)]);
        assert_eq!(with[idx(2, 13)], px[0][idx(2, 13)]);
    }
}
