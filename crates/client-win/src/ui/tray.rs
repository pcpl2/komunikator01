//! The notification-area icon: state colour, unread badge, tooltip and its menu.

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW, NOTIFY_ICON_MESSAGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, PostMessageW,
    SetForegroundWindow, TrackPopupMenu, HICON, MF_CHECKED, MF_SEPARATOR, MF_STRING,
    SM_CXSMICON, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_NULL,
};

use super::icons::{make_icon, tray_pixels, TrayState};
use super::wide::wide;

const TRAY_ID: u32 = 1;

pub enum TrayCommand {
    Show,
    ToggleMute,
    ToggleDeafen,
    Exit,
}

pub struct Tray {
    hwnd: HWND,
    callback: u32,
    icon: HICON,
    shown: Option<(TrayState, bool, &'static str)>,
    added: bool,
}

impl Tray {
    /// Nothing is shown until the first `update`. Clicks arrive as `callback` messages.
    pub fn new(hwnd: HWND, callback: u32) -> Tray {
        Tray { hwnd, callback, icon: HICON::default(), shown: None, added: false }
    }

    /// Changes the icon only when the state, badge or tooltip changed.
    pub fn update(&mut self, state: TrayState, badge: bool, tip: &'static str, dpi: u32) {
        if self.added && self.shown == Some((state, badge, tip)) {
            return;
        }
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) }.max(16) as u32;
        let icon = make_icon(&tray_pixels(state, badge, size), size);
        let mut data = self.data();
        data.hIcon = icon;
        let tip_w = wide(tip);
        let n = tip_w.len().min(data.szTip.len() - 1);
        data.szTip[..n].copy_from_slice(&tip_w[..n]);
        let ok = self.notify(if self.added { NIM_MODIFY } else { NIM_ADD }, &data);
        // Explorer may not be running yet; TaskbarCreated brings us back here.
        self.added = ok;
        let old = std::mem::replace(&mut self.icon, icon);
        if !old.is_invalid() {
            let _ = unsafe { DestroyIcon(old) };
        }
        self.shown = ok.then_some((state, badge, tip));
    }

    /// Explorer restarted (TaskbarCreated) or the DPI changed: draw everything again.
    pub fn reset(&mut self) {
        self.added = false;
        self.shown = None;
    }

    pub fn remove(&mut self) {
        if self.added {
            self.notify(NIM_DELETE, &self.data());
            self.added = false;
        }
    }

    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: self.callback,
            ..Default::default()
        }
    }

    fn notify(&self, msg: NOTIFY_ICON_MESSAGE, data: &NOTIFYICONDATAW) -> bool {
        unsafe { Shell_NotifyIconW(msg, data) }.as_bool()
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
        if !self.icon.is_invalid() {
            let _ = unsafe { DestroyIcon(self.icon) };
        }
    }
}

/// Shows the tray menu at the cursor and returns the choice. Runs a modal loop, so the caller
/// must not hold any borrow of the window state.
pub fn menu(hwnd: HWND, muted: bool, deafened: bool) -> Option<TrayCommand> {
    const SHOW: usize = 1;
    const MUTE: usize = 2;
    const DEAFEN: usize = 3;
    const EXIT: usize = 4;
    let check = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let _ = AppendMenuW(menu, MF_STRING, SHOW, windows::core::w!("Pokaż okno"));
        let _ = AppendMenuW(menu, check(muted), MUTE, windows::core::w!("Wycisz mikrofon"));
        let _ = AppendMenuW(menu, check(deafened), DEAFEN, windows::core::w!("Wycisz wszystkich"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, EXIT, windows::core::w!("Wyjdź"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Without these two the menu does not close when clicking elsewhere (KB135788).
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match cmd.0 as usize {
            SHOW => Some(TrayCommand::Show),
            MUTE => Some(TrayCommand::ToggleMute),
            DEAFEN => Some(TrayCommand::ToggleDeafen),
            EXIT => Some(TrayCommand::Exit),
            _ => None,
        }
    }
}
