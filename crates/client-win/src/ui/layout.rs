//! The main window's controls: creation, placement for a client size and DPI, fonts, and the
//! window's first rectangle.

use std::ffi::c_void;

use client_core::settings::WindowRect;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, GetMonitorInfoW, MonitorFromPoint, MonitorFromRect, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::UI::Controls::{
    EM_LIMITTEXT, LVCF_WIDTH, LVCOLUMNW, LVM_INSERTCOLUMNW, LVM_SETEXTENDEDLISTVIEWSTYLE,
    LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_EX_LABELTIP, LVS_NOCOLUMNHEADER, LVS_REPORT,
    LVS_SHOWSELALWAYS, LVS_SINGLESEL, SBARS_SIZEGRIP, SB_SETPARTS, STATUSCLASSNAMEW,
    TVM_SETEXTENDEDSTYLE, TVS_EX_DOUBLEBUFFER, TVS_HASBUTTONS, TVS_HASLINES, TVS_LINESATROOT,
    TVS_SHOWSELALWAYS, WC_BUTTONW, WC_EDITW, WC_LISTVIEWW, WC_TREEVIEWW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, SystemParametersInfoForDpi, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, GetClientRect, GetWindowRect, MoveWindow, SendMessageW, BS_AUTOCHECKBOX,
    BS_PUSHBUTTON, ES_AUTOHSCROLL, HMENU, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_SETFONT, WM_SIZE, WS_CHILD, WS_EX_CLIENTEDGE, WS_TABSTOP, WS_VISIBLE,
};

use super::render::{fit_chat_column, Controls};
use super::wide::{scale, wide};

pub const ID_MUTE: u16 = 101;
pub const ID_DEAFEN: u16 = 102;
pub const ID_LEAVE: u16 = 103;
pub const ID_NEW_ROOM: u16 = 104;
pub const ID_SETTINGS: u16 = 105;
pub const ID_TOP: u16 = 106;
pub const ID_TREE: u16 = 107;
pub const ID_CHAT: u16 = 108;
pub const ID_INPUT: u16 = 109;
pub const ID_SEND: u16 = 110;
pub const ID_STATUS: u16 = 111;

/// Chat input limit, as the session's.
const CHAT_LIMIT: usize = 2000;

pub fn create_controls(parent: HWND, instance: HINSTANCE) -> Controls {
    let child = |class: PCWSTR, text: &str, style: u32, ex: WINDOW_EX_STYLE, id: u16| {
        let t = wide(text);
        unsafe {
            CreateWindowExW(
                ex,
                class,
                PCWSTR(t.as_ptr()),
                WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | style),
                0,
                0,
                0,
                0,
                Some(parent),
                Some(HMENU(id as usize as *mut c_void)),
                Some(instance),
                None,
            )
            .unwrap_or_default()
        }
    };
    let button = |text: &str, id: u16| child(WC_BUTTONW, text, WS_TABSTOP.0 | BS_PUSHBUTTON as u32, WINDOW_EX_STYLE::default(), id);
    let c = Controls {
        mute: button("Mikrofon: wł.", ID_MUTE),
        deafen: button("Dźwięk: wł.", ID_DEAFEN),
        leave: button("Opuść pokój", ID_LEAVE),
        new_room: button("Nowy pokój…", ID_NEW_ROOM),
        settings: button("Ustawienia…", ID_SETTINGS),
        tree: child(
            WC_TREEVIEWW,
            "",
            WS_TABSTOP.0 | TVS_HASLINES | TVS_LINESATROOT | TVS_HASBUTTONS | TVS_SHOWSELALWAYS,
            WS_EX_CLIENTEDGE,
            ID_TREE,
        ),
        chat: child(
            WC_LISTVIEWW,
            "",
            WS_TABSTOP.0 | LVS_REPORT | LVS_NOCOLUMNHEADER | LVS_SINGLESEL | LVS_SHOWSELALWAYS,
            WS_EX_CLIENTEDGE,
            ID_CHAT,
        ),
        top: child(WC_BUTTONW, "Zawsze na wierzchu", WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32, WINDOW_EX_STYLE::default(), ID_TOP),
        input: child(WC_EDITW, "", WS_TABSTOP.0 | ES_AUTOHSCROLL as u32, WS_EX_CLIENTEDGE, ID_INPUT),
        send: button("Wyślij", ID_SEND),
        status: child(STATUSCLASSNAMEW, "", SBARS_SIZEGRIP, WINDOW_EX_STYLE::default(), ID_STATUS),
    };
    unsafe {
        SendMessageW(c.tree, TVM_SETEXTENDEDSTYLE, Some(WPARAM(TVS_EX_DOUBLEBUFFER as usize)), Some(LPARAM(TVS_EX_DOUBLEBUFFER as isize)));
        let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP;
        SendMessageW(c.chat, LVM_SETEXTENDEDLISTVIEWSTYLE, Some(WPARAM(ex as usize)), Some(LPARAM(ex as isize)));
        let col = LVCOLUMNW { mask: LVCF_WIDTH, cx: 100, ..Default::default() };
        SendMessageW(c.chat, LVM_INSERTCOLUMNW, Some(WPARAM(0)), Some(LPARAM(&col as *const _ as isize)));
        SendMessageW(c.input, EM_LIMITTEXT, Some(WPARAM(CHAT_LIMIT)), None);
        // Open in Task 6 (new room and settings dialogs).
        let _ = EnableWindow(c.new_room, false);
        let _ = EnableWindow(c.settings, false);
        let _ = EnableWindow(c.leave, false);
    }
    c
}

/// Places every control for the window's client size at `dpi`.
pub fn layout(hwnd: HWND, c: &Controls, dpi: u32) {
    let s = |v: i32| scale(v, dpi);
    let mut rc = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let (w, h) = (rc.right, rc.bottom);

    let mut sr = RECT::default();
    unsafe {
        SendMessageW(c.status, WM_SIZE, None, None);
        let _ = GetWindowRect(c.status, &mut sr);
        let parts = [w * 45 / 100, -1];
        SendMessageW(c.status, SB_SETPARTS, Some(WPARAM(2)), Some(LPARAM(parts.as_ptr() as isize)));
    }
    let status_h = sr.bottom - sr.top;

    let (m, gap, bar_h, row_h) = (s(8), s(6), s(34), s(26));
    let mut x = m;
    for (b, bw) in [(c.mute, 150), (c.deafen, 150), (c.leave, 88), (c.new_room, 88), (c.settings, 88)] {
        if b == c.leave {
            x += gap;
        }
        place(b, x, m, s(bw), bar_h);
        x += s(bw) + gap;
    }
    let top = m + bar_h + m;
    let bottom = (h - status_h - m - row_h - gap).max(top);
    let left_w = ((w - 3 * m) * 40 / 100).max(0);
    place(c.tree, m, top, left_w, bottom - top);
    place(c.top, m, bottom + gap, left_w, row_h);
    let rx = 2 * m + left_w;
    let rw = (w - rx - m).max(0);
    let send_w = s(80);
    place(c.chat, rx, top, rw, bottom - top);
    place(c.input, rx, bottom + gap, (rw - send_w - gap).max(0), row_h);
    place(c.send, rx + rw - send_w, bottom + gap, send_w, row_h);
    fit_chat_column(c.chat);
}

/// Normal and bold (toggle buttons) message fonts for `dpi`; the caller deletes them.
pub fn make_fonts(dpi: u32) -> (HFONT, HFONT) {
    let mut ncm = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
    unsafe {
        let _ = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some((&mut ncm as *mut NONCLIENTMETRICSW).cast()),
            0,
            dpi,
        );
        let normal = CreateFontIndirectW(&ncm.lfMessageFont);
        let mut bold = ncm.lfMessageFont;
        bold.lfWeight = 700;
        (normal, CreateFontIndirectW(&bold))
    }
}

pub fn set_fonts(c: &Controls, (normal, bold): (HFONT, HFONT)) {
    let set = |h: HWND, f: HFONT| unsafe {
        SendMessageW(h, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
    };
    for h in [c.leave, c.new_room, c.settings, c.top, c.tree, c.chat, c.input, c.send, c.status] {
        set(h, normal);
    }
    set(c.mute, bold);
    set(c.deafen, bold);
}

/// The saved rectangle when it is still on a monitor; otherwise a default size centred on
/// the primary monitor's work area.
pub fn initial_rect(saved: Option<&WindowRect>) -> RECT {
    if let Some(s) = saved.filter(|s| s.w > 0 && s.h > 0) {
        let r = RECT { left: s.x, top: s.y, right: s.x + s.w, bottom: s.y + s.h };
        if !unsafe { MonitorFromRect(&r, MONITOR_DEFAULTTONULL) }.is_invalid() {
            return r;
        }
    }
    unsafe {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut info);
        let (mut dx, mut dy) = (96, 96);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_err() {
            dx = 96;
        }
        let work = info.rcWork;
        let w = scale(820, dx).min(work.right - work.left);
        let h = scale(560, dx).min(work.bottom - work.top);
        let x = work.left + (work.right - work.left - w) / 2;
        let y = work.top + (work.bottom - work.top - h) / 2;
        RECT { left: x, top: y, right: x + w, bottom: y + h }
    }
}

fn place(h: HWND, x: i32, y: i32, w: i32, hgt: i32) {
    let _ = unsafe { MoveWindow(h, x, y, w.max(0), hgt.max(0), true) };
}
