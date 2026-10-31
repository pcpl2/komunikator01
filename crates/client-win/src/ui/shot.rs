//! A picture of one of our own windows, for checking how it looks (`--smoke` with
//! `KOMUNIKATOR_SMOKE_SHOT`, and the dialog tests). Never used on other programs' windows.

use std::path::Path;

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, ClientToScreen, CreateCompatibleBitmap, SRCCOPY, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect};

/// Saves `hwnd` (frame included) as a 32-bit BMP.
pub fn save_bmp(hwnd: HWND, path: &Path) -> std::io::Result<()> {
    const PW_RENDERFULLCONTENT: u32 = 2;
    let mut rc = RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut rc) };
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    if w <= 0 || h <= 0 {
        return Err(std::io::Error::other("window has no size"));
    }
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(dc, bmp.into());
        // The frame (drawn by DWM) through PrintWindow, then the client area as it was
        // actually painted: PrintWindow repaints controls itself, skipping any recolouring
        // done after their WM_PAINT.
        let _ = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT));
        let mut client = RECT::default();
        let _ = GetClientRect(hwnd, &mut client);
        let mut origin = POINT::default();
        let _ = ClientToScreen(hwnd, &mut origin);
        let src = GetDC(Some(hwnd));
        let _ = BitBlt(dc, origin.x - rc.left, origin.y - rc.top, client.right, client.bottom, Some(src), 0, 0, SRCCOPY);
        ReleaseDC(Some(hwnd), src);
        SelectObject(dc, old);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        GetDIBits(dc, bmp, 0, h as u32, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
    }
    let mut file = Vec::with_capacity(54 + pixels.len());
    let size = 54 + pixels.len() as u32;
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&size.to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    file.extend_from_slice(&54u32.to_le_bytes());
    file.extend_from_slice(&40u32.to_le_bytes());
    file.extend_from_slice(&w.to_le_bytes());
    file.extend_from_slice(&(-h).to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&32u16.to_le_bytes());
    file.extend_from_slice(&[0u8; 24]);
    file.extend_from_slice(&pixels);
    std::fs::write(path, file)
}
