//! Dark mode following Windows' app theme (Settings → Personalisation → Colours).
//!
//! The title bar uses the documented DWM attribute, the controls Windows' own dark visual
//! styles (`DarkMode_Explorer`, `DarkMode_CFD`) plus WM_CTLCOLOR* colours. Menus need
//! uxtheme's undocumented `SetPreferredAppMode` (Windows 10 1903+), looked up by ordinal and
//! skipped where it is missing: menus then stay light.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use windows::core::{w, BOOL, PCSTR, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreatePen, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, EndPaint, FillRect, FrameRect, GdiFlush, GetDC, LineTo, MoveToEx, DT_END_ELLIPSIS, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, HGDIOBJ, PAINTSTRUCT, PS_SOLID, GetSysColor, InvalidateRect, ReleaseDC, SelectObject, SetBkColor, SetBkMode, SetTextColor, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, COLOR_BTNTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, DIB_RGB_COLORS, HBRUSH, HDC,
    SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::Controls::{
    SetWindowTheme, LVM_SETBKCOLOR, LVM_SETTEXTBKCOLOR, LVM_SETTEXTCOLOR, PBM_SETBARCOLOR, PBM_SETBKCOLOR,
    SB_SETBKCOLOR, TVM_SETBKCOLOR, TVM_SETLINECOLOR, TVM_SETTEXTCOLOR,
};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, IsWindowEnabled};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowTextLengthW, GetWindowTextW, HideCaret, ShowCaret, WM_CHAR, WM_ENABLE, WM_GETFONT, WM_KEYDOWN, WM_KEYUP,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_SETFOCUS, WM_SETFONT, WM_SYSCHAR, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, GetClassNameW, GetClientRect, GetWindowLongW, SendMessageW, SystemParametersInfoW,
    GWL_STYLE, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_CTLCOLORBTN, WM_CTLCOLORDLG,
    WM_CTLCOLOREDIT, WM_CTLCOLORLISTBOX, WM_CTLCOLORSTATIC, WM_PAINT,
};

use super::wide::wide;

/// 0x00BBGGRR.
const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

/// Windows 11's dark palette.
const WINDOW: COLORREF = rgb(0x20, 0x20, 0x20);
const FIELD: COLORREF = rgb(0x19, 0x19, 0x19);
const TEXT: COLORREF = rgb(0xF0, 0xF0, 0xF0);
const LINES: COLORREF = rgb(0x60, 0x60, 0x60);
const FOCUSED: COLORREF = rgb(0x2D, 0x2D, 0x2D);
const ACCENT: COLORREF = rgb(0x4C, 0xA0, 0xE0);
const ERROR_DARK: COLORREF = rgb(0xFF, 0x70, 0x70);
const ERROR_LIGHT: COLORREF = rgb(0xC0, 0x20, 0x00);
const LEVEL: COLORREF = rgb(0x4C, 0xC2, 0x78);
/// Restores a common control's own colour.
const CLR_DEFAULT: isize = 0xFF00_0000u32 as isize;

static DARK: AtomicBool = AtomicBool::new(false);

/// Whether the program is drawn dark now.
pub fn dark() -> bool {
    DARK.load(Ordering::Relaxed)
}

/// Reads Windows' choice and lets menus follow it; true when it changed. Called at start
/// and on WM_SETTINGCHANGE ("ImmersiveColorSet").
pub fn refresh() -> bool {
    static FIRST: AtomicBool = AtomicBool::new(true);
    let now = is_dark(apps_use_light_theme(), high_contrast());
    let changed = DARK.swap(now, Ordering::Relaxed) != now;
    if changed || FIRST.swap(false, Ordering::Relaxed) {
        menus_follow(now);
    }
    changed
}

fn apps_use_light_theme() -> Option<u32> {
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = wide("AppsUseLightTheme");
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    r.is_ok().then_some(value)
}

fn high_contrast() -> bool {
    let mut hc = HIGHCONTRASTW { cbSize: size_of::<HIGHCONTRASTW>() as u32, ..Default::default() };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cbSize,
            Some((&mut hc as *mut HIGHCONTRASTW).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.is_ok() && hc.dwFlags.contains(HCF_HIGHCONTRASTON)
}

/// Windows' build number, e.g. 26100; 0 when unknown.
fn build() -> u32 {
    let key = wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let name = wide("CurrentBuild");
    let mut buf = [0u16; 16];
    let mut size = size_of_val(&buf) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if r.is_err() {
        return 0;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().parse().unwrap_or(0)
}

/// Popup menus (tray, right-click) dark or light. uxtheme ordinals 135 (SetPreferredAppMode)
/// and 136 (FlushMenuThemes) have this meaning from Windows 10 1903 (build 18362).
fn menus_follow(dark: bool) {
    const FORCE_DARK: i32 = 2;
    const FORCE_LIGHT: i32 = 3;
    if build() < 18362 {
        return;
    }
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        let ordinal = |n: usize| GetProcAddress(uxtheme, PCSTR(n as *const u8));
        if let Some(f) = ordinal(135) {
            let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_preferred_app_mode(if dark { FORCE_DARK } else { FORCE_LIGHT });
        }
        if let Some(f) = ordinal(136) {
            let flush_menu_themes: unsafe extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }
    }
}

/// (window, field) brushes; made once and kept for the program's life.
fn brushes() -> (HBRUSH, HBRUSH) {
    static BRUSHES: OnceLock<(isize, isize)> = OnceLock::new();
    let (w, f) = *BRUSHES
        .get_or_init(|| unsafe { (CreateSolidBrush(WINDOW).0 as isize, CreateSolidBrush(FIELD).0 as isize) });
    (HBRUSH(w as *mut c_void), HBRUSH(f as *mut c_void))
}

/// WM_SETTINGCHANGE's `lp` names "ImmersiveColorSet" when the app theme was switched.
pub fn is_theme_change(lp: LPARAM) -> bool {
    if lp.0 == 0 {
        return false;
    }
    let name = unsafe { PCWSTR(lp.0 as *const u16).to_string() };
    name.is_ok_and(|n| n == "ImmersiveColorSet")
}

/// The answer to a WM_CTLCOLOR* message in dark mode; `None` leaves light mode to Windows.
pub fn ctlcolor(msg: u32, dc: HDC) -> Option<LRESULT> {
    if !dark() {
        return None;
    }
    let (window, field) = brushes();
    let (bg, brush) = match msg {
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => (FIELD, field),
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLORDLG => (WINDOW, window),
        _ => return None,
    };
    unsafe {
        SetTextColor(dc, TEXT);
        SetBkColor(dc, bg);
    }
    Some(LRESULT(brush.0 as isize))
}

/// Error text (red) on the window background, in either mode.
pub fn error_label(dc: HDC, light_background: HBRUSH) -> LRESULT {
    unsafe {
        SetTextColor(dc, if dark() { ERROR_DARK } else { ERROR_LIGHT });
        SetBkMode(dc, TRANSPARENT);
    }
    LRESULT(if dark() { brushes().0 } else { light_background }.0 as isize)
}

/// The colour of owner-drawn text (the status bar).
pub fn text_color() -> COLORREF {
    if dark() {
        TEXT
    } else {
        COLORREF(unsafe { GetSysColor(COLOR_BTNTEXT) })
    }
}

/// Paints a top-level window's background in dark mode; false leaves it to the class brush.
pub fn erase(hwnd: HWND, dc: HDC) -> bool {
    if !dark() {
        return false;
    }
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
        FillRect(dc, &rc, brushes().0);
    }
    true
}

/// Applies the current mode to a top-level window: its title bar and every child.
pub fn apply(hwnd: HWND) {
    unsafe extern "system" fn each(child: HWND, _: LPARAM) -> BOOL {
        control(child);
        BOOL::from(true)
    }
    let on = BOOL::from(dark());
    let set = |attr: i32| unsafe {
        DwmSetWindowAttribute(hwnd, DWMWINDOWATTRIBUTE(attr), (&on as *const BOOL).cast(), size_of::<BOOL>() as u32)
    };
    // DWMWA_USE_IMMERSIVE_DARK_MODE: 20 from Windows 10 20H1, 19 before.
    if set(20).is_err() {
        let _ = set(19);
    }
    unsafe {
        let _ = EnumChildWindows(Some(hwnd), Some(each), LPARAM(0));
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

/// Applies the current mode to one child control.
pub fn control(h: HWND) {
    let dark = dark();
    let mut name = [0u16; 64];
    let len = unsafe { GetClassNameW(h, &mut name) }.max(0) as usize;
    let class = String::from_utf16_lossy(&name[..len]);
    let style = unsafe { GetWindowLongW(h, GWL_STYLE) } as u32;
    let send = |msg: u32, lp: isize| unsafe { SendMessageW(h, msg, Some(WPARAM(0)), Some(LPARAM(lp))) };
    let color = |c: COLORREF| c.0 as isize;
    match kind(&class, style) {
        Kind::PushButton => visual_style(h, dark.then_some(w!("DarkMode_Explorer"))),
        Kind::CheckBox => unthemed_when(h, dark),
        Kind::Edit => {
            allow_dark(h, dark);
            visual_style(h, dark.then_some(w!("DarkMode_CFD")));
        }
        Kind::ComboBox => {
            // No visual style darkens a drop-down list's closed box: drawn here in dark mode.
            let _ = unsafe { SetWindowSubclass(h, Some(combo_proc), 1, 0) };
        }
        Kind::Hotkey => {
            // Replaces itself when applied again (same id).
            let _ = unsafe { SetWindowSubclass(h, Some(hotkey_proc), 1, 0) };
        }
        Kind::Tree => {
            visual_style(h, dark.then_some(w!("DarkMode_Explorer")));
            let (bg, fg) = if dark { (color(FIELD), color(TEXT)) } else { (-1, -1) };
            send(TVM_SETBKCOLOR, bg);
            send(TVM_SETTEXTCOLOR, fg);
            send(TVM_SETLINECOLOR, if dark { color(LINES) } else { CLR_DEFAULT });
        }
        Kind::List => {
            visual_style(h, dark.then_some(w!("DarkMode_Explorer")));
            let (bg, fg) = if dark {
                (color(FIELD), color(TEXT))
            } else {
                unsafe { (GetSysColor(COLOR_WINDOW) as isize, GetSysColor(COLOR_WINDOWTEXT) as isize) }
            };
            send(LVM_SETBKCOLOR, bg);
            send(LVM_SETTEXTBKCOLOR, bg);
            send(LVM_SETTEXTCOLOR, fg);
        }
        Kind::Status => {
            unthemed_when(h, dark);
            send(SB_SETBKCOLOR, if dark { color(WINDOW) } else { CLR_DEFAULT });
        }
        Kind::Progress => {
            unthemed_when(h, dark);
            send(PBM_SETBKCOLOR, if dark { color(FIELD) } else { CLR_DEFAULT });
            send(PBM_SETBARCOLOR, if dark { color(LEVEL) } else { CLR_DEFAULT });
        }
        Kind::Plain => {}
    }
    let _ = unsafe { InvalidateRect(Some(h), None, true) };
}

/// uxtheme ordinal 133 (AllowDarkModeForWindow): some visual styles (DarkMode_CFD on combo
/// boxes) only turn dark for windows allowed to.
fn allow_dark(h: HWND, on: bool) {
    static ALLOW: OnceLock<Option<usize>> = OnceLock::new();
    let f = *ALLOW.get_or_init(|| unsafe {
        if build() < 18362 {
            return None;
        }
        let uxtheme = LoadLibraryW(w!("uxtheme.dll")).ok()?;
        GetProcAddress(uxtheme, PCSTR(133 as *const u8)).map(|f| f as usize)
    });
    if let Some(f) = f {
        let allow: unsafe extern "system" fn(HWND, BOOL) -> BOOL = unsafe { std::mem::transmute(f) };
        let _ = unsafe { allow(h, BOOL::from(on)) };
    }
}

/// Paints a hotkey field dark: the control paints itself (white, black text) as usual, then
/// its pixels are read back and turned into the dark palette by `remap_to_dark`. The control
/// ignores a DC handed to it and may also draw straight away (focus, keys), so after any such
/// message it is made to repaint in full: the recolouring then always starts from all white.
unsafe extern "system" fn hotkey_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, _: usize) -> LRESULT {
    const HKM_SETHOTKEY: u32 = WM_USER + 1;
    if !dark() {
        return unsafe { DefSubclassProc(h, msg, wp, lp) };
    }
    match msg {
        WM_PAINT => unsafe {
            // The whole field, so no part already recoloured is read back and inverted again.
            let _ = InvalidateRect(Some(h), None, true);
            let r = DefSubclassProc(h, msg, wp, lp);
            recolor_in_place(h);
            r
        },
        WM_SETFOCUS | WM_KILLFOCUS | WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_CHAR
        | WM_SYSCHAR | WM_ENABLE | WM_SETFONT | WM_LBUTTONDOWN | HKM_SETHOTKEY => unsafe {
            let r = DefSubclassProc(h, msg, wp, lp);
            let _ = InvalidateRect(Some(h), None, true);
            r
        },
        _ => unsafe { DefSubclassProc(h, msg, wp, lp) },
    }
}

/// Draws a closed drop-down list in dark mode: field, frame, the chosen text and an arrow.
unsafe extern "system" fn combo_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, _: usize) -> LRESULT {
    if dark() && msg == WM_PAINT {
        paint_combo(h);
        return LRESULT(0);
    }
    unsafe { DefSubclassProc(h, msg, wp, lp) }
}

fn paint_combo(h: HWND) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(h, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(h, &mut rc);
        let focused = GetFocus() == h;
        let enabled = IsWindowEnabled(h).as_bool();
        let field = CreateSolidBrush(if focused { FOCUSED } else { FIELD });
        FillRect(dc, &rc, field);
        let _ = DeleteObject(field.into());
        let frame = CreateSolidBrush(if focused { ACCENT } else { LINES });
        FrameRect(dc, &rc, frame);
        let _ = DeleteObject(frame.into());

        let arrow_w = (rc.bottom - rc.top).max(8);
        let font = SendMessageW(h, WM_GETFONT, None, None);
        let old_font = SelectObject(dc, HGDIOBJ(font.0 as *mut c_void));
        SetTextColor(dc, if enabled { TEXT } else { LINES });
        SetBkMode(dc, TRANSPARENT);
        let len = GetWindowTextLengthW(h).max(0) as usize;
        let mut text = vec![0u16; len + 1];
        let n = GetWindowTextW(h, &mut text).max(0) as usize;
        let mut text_rc = RECT { left: rc.left + 6, right: rc.right - arrow_w, ..rc };
        DrawTextW(dc, &mut text[..n], &mut text_rc, DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX);
        SelectObject(dc, old_font);

        // A chevron in the middle of the arrow area.
        let pen = CreatePen(PS_SOLID, 1, if enabled { TEXT } else { LINES });
        let old_pen = SelectObject(dc, pen.into());
        let (cx, cy) = (rc.right - arrow_w / 2, (rc.top + rc.bottom) / 2);
        let s = (arrow_w / 6).max(3);
        let _ = MoveToEx(dc, cx - s, cy - s / 2, None);
        let _ = LineTo(dc, cx, cy + s / 2 + 1);
        let _ = MoveToEx(dc, cx + s, cy - s / 2, None);
        let _ = LineTo(dc, cx - 1, cy + s / 2 + 1);
        SelectObject(dc, old_pen);
        let _ = DeleteObject(pen.into());
        let _ = EndPaint(h, &ps);
    }
}

fn recolor_in_place(h: HWND) {
    unsafe {
        let mut rc = RECT::default();
        let _ = GetClientRect(h, &mut rc);
        let (w, ht) = (rc.right, rc.bottom);
        if w <= 0 || ht <= 0 {
            return;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -ht,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // The caret is drawn inverted on top; it must not be read back and recoloured.
        let caret_hidden = HideCaret(Some(h)).is_ok();
        let dc = GetDC(Some(h));
        let mem = CreateCompatibleDC(Some(dc));
        let mut bits: *mut c_void = std::ptr::null_mut();
        if let Ok(bmp) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            let old = SelectObject(mem, bmp.into());
            let _ = BitBlt(mem, 0, 0, w, ht, Some(dc), 0, 0, SRCCOPY);
            let _ = GdiFlush();
            if !bits.is_null() {
                remap_to_dark(std::slice::from_raw_parts_mut(bits.cast::<u8>(), (w * ht * 4) as usize));
            }
            let _ = BitBlt(dc, 0, 0, w, ht, Some(mem), 0, 0, SRCCOPY);
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
        }
        let _ = DeleteDC(mem);
        ReleaseDC(Some(h), dc);
        if caret_hidden {
            let _ = ShowCaret(Some(h));
        }
    }
}

/// BGRA pixels painted dark on light → light on the dark field, by inverted brightness, so
/// anti-aliased text edges stay smooth.
fn remap_to_dark(px: &mut [u8]) {
    let field = FIELD.0.to_le_bytes();
    let text = TEXT.0.to_le_bytes();
    for p in px.chunks_exact_mut(4) {
        let lum = (u32::from(p[2]) * 299 + u32::from(p[1]) * 587 + u32::from(p[0]) * 114) / 1000;
        let ink = 255 - lum;
        // COLORREF bytes are R, G, B; pixels B, G, R.
        for (i, c) in [2usize, 1, 0].into_iter().enumerate() {
            let (f, t) = (u32::from(field[c]), u32::from(text[c]));
            p[i] = ((f * (255 - ink) + t * ink) / 255) as u8;
        }
    }
}

/// `Some(name)` sets a visual style, `None` restores the default one.
fn visual_style(h: HWND, name: Option<PCWSTR>) {
    let _ = unsafe { SetWindowTheme(h, name.unwrap_or(PCWSTR::null()), PCWSTR::null()) };
}

/// Without a visual style a control takes its colours from WM_CTLCOLOR* and its own colour
/// messages; with the default one (light mode) it looks as before.
fn unthemed_when(h: HWND, unthemed: bool) {
    let _ = unsafe {
        if unthemed {
            SetWindowTheme(h, w!(""), w!(""))
        } else {
            SetWindowTheme(h, PCWSTR::null(), PCWSTR::null())
        }
    };
}

/// Dark when Windows asks apps for the dark theme; never under high contrast, whose own
/// colours the system controls already follow.
fn is_dark(apps_use_light_theme: Option<u32>, high_contrast: bool) -> bool {
    !high_contrast && apps_use_light_theme == Some(0)
}

/// How a child control is darkened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    PushButton,
    /// Check boxes and radio buttons: their themed text ignores WM_CTLCOLOR*, so in dark
    /// mode they go unthemed.
    CheckBox,
    Edit,
    ComboBox,
    Tree,
    List,
    Status,
    Progress,
    /// msctls_hotkey32 paints with the system window colours whatever its parent says, so
    /// in dark mode it is recoloured after painting.
    Hotkey,
    /// Painted by the parent's WM_CTLCOLOR* alone (labels, trackbars, hotkey fields).
    Plain,
}

fn kind(class: &str, style: u32) -> Kind {
    const BS_TYPEMASK: u32 = 0xF;
    match class.to_ascii_lowercase().as_str() {
        "button" => match style & BS_TYPEMASK {
            0 | 1 => Kind::PushButton,
            _ => Kind::CheckBox,
        },
        "edit" => Kind::Edit,
        "combobox" => Kind::ComboBox,
        "systreeview32" => Kind::Tree,
        "syslistview32" => Kind::List,
        "msctls_statusbar32" => Kind::Status,
        "msctls_progress32" => Kind::Progress,
        "msctls_hotkey32" => Kind::Hotkey,
        _ => Kind::Plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_only_when_windows_asks_for_it_and_not_in_high_contrast() {
        assert!(is_dark(Some(0), false));
        assert!(!is_dark(Some(1), false));
        // Older Windows without the setting: light.
        assert!(!is_dark(None, false));
        assert!(!is_dark(Some(0), true));
    }

    #[test]
    fn light_pixels_become_the_field_and_dark_ones_the_text() {
        let (field, text) = (FIELD.0.to_le_bytes(), TEXT.0.to_le_bytes());
        // BGRA pixels; COLORREF bytes are R, G, B.
        let mut px = [0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0x80, 0x80, 0x80, 0];
        remap_to_dark(&mut px);
        assert_eq!(px[..3], [field[2], field[1], field[0]], "white background → field");
        assert_eq!(px[4..7], [text[2], text[1], text[0]], "black text → light text");
        let mid = px[8];
        assert!(mid > field[0] && mid < text[0], "anti-aliased edges stay in between: {mid}");
    }

    #[test]
    fn controls_are_told_apart_by_class_and_button_style() {
        assert_eq!(kind("Button", 0), Kind::PushButton); // BS_PUSHBUTTON
        assert_eq!(kind("Button", 1 | 0x1_0000), Kind::PushButton); // BS_DEFPUSHBUTTON, WS_TABSTOP
        assert_eq!(kind("Button", 3), Kind::CheckBox); // BS_AUTOCHECKBOX
        assert_eq!(kind("Button", 9), Kind::CheckBox); // BS_AUTORADIOBUTTON
        assert_eq!(kind("Edit", 0), Kind::Edit);
        assert_eq!(kind("ComboBox", 0), Kind::ComboBox);
        assert_eq!(kind("SysTreeView32", 0), Kind::Tree);
        assert_eq!(kind("SysListView32", 0), Kind::List);
        assert_eq!(kind("msctls_statusbar32", 0), Kind::Status);
        assert_eq!(kind("msctls_progress32", 0), Kind::Progress);
        assert_eq!(kind("msctls_hotkey32", 0), Kind::Hotkey);
        assert_eq!(kind("Static", 0), Kind::Plain);
        assert_eq!(kind("msctls_trackbar32", 0), Kind::Plain);
    }
}
