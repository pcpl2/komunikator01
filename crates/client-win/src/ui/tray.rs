//! The notification-area icon: state colour, unread badge, tooltip and its menu.

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
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

/// One `Shell_NotifyIconW` operation on our icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Add,
    Modify,
    Delete,
}

/// Whether the shell has our icon, as far as we know. The belief can be wrong (Explorer
/// restarted, a call timed out), so each show falls back to the other operation and a
/// failure never changes the belief; only `TaskbarCreated` says for sure the icon is gone.
#[derive(Default)]
struct ShellIcon {
    added: bool,
}

impl ShellIcon {
    /// Adds or modifies the icon; true when the shell took it.
    fn show(&mut self, mut notify: impl FnMut(Op) -> bool) -> bool {
        let (first, second) = if self.added { (Op::Modify, Op::Add) } else { (Op::Add, Op::Modify) };
        let ok = notify(first) || notify(second);
        if ok {
            self.added = true;
        }
        ok
    }

    /// Always asks, so no ghost icon stays behind whatever we believed.
    fn delete(&mut self, mut notify: impl FnMut(Op) -> bool) {
        notify(Op::Delete);
        self.added = false;
    }

    fn taskbar_created(&mut self) {
        self.added = false;
    }
}

pub struct Tray {
    hwnd: HWND,
    callback: u32,
    icon: HICON,
    shown: Option<(TrayState, bool, &'static str)>,
    shell: ShellIcon,
}

impl Tray {
    /// Nothing is shown until the first `update`. Clicks arrive as `callback` messages.
    pub fn new(hwnd: HWND, callback: u32) -> Tray {
        Tray { hwnd, callback, icon: HICON::default(), shown: None, shell: ShellIcon::default() }
    }

    /// Changes the icon only when the state, badge or tooltip changed.
    pub fn update(&mut self, state: TrayState, badge: bool, tip: &'static str) {
        if self.shown == Some((state, badge, tip)) {
            return;
        }
        // The taskbar draws at the system DPI whatever monitor the window is on.
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) }.max(16) as u32;
        let icon = make_icon(&tray_pixels(state, badge, size), size);
        let mut data = self.data();
        data.hIcon = icon;
        let tip_w = wide(tip);
        let n = tip_w.len().min(data.szTip.len() - 1);
        data.szTip[..n].copy_from_slice(&tip_w[..n]);
        // Explorer may not be running yet; TaskbarCreated brings us back here.
        let ok = self.shell.show(|op| notify(op, &data));
        let old = std::mem::replace(&mut self.icon, icon);
        if !old.is_invalid() {
            let _ = unsafe { DestroyIcon(old) };
        }
        self.shown = ok.then_some((state, badge, tip));
    }

    /// Explorer restarted (TaskbarCreated): the icon is gone, add it again.
    pub fn taskbar_created(&mut self) {
        self.shell.taskbar_created();
        self.shown = None;
    }

    pub fn remove(&mut self) {
        let data = self.data();
        self.shell.delete(|op| notify(op, &data));
        self.shown = None;
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
}

fn notify(op: Op, data: &NOTIFYICONDATAW) -> bool {
    let msg: NOTIFY_ICON_MESSAGE = match op {
        Op::Add => NIM_ADD,
        Op::Modify => NIM_MODIFY,
        Op::Delete => NIM_DELETE,
    };
    unsafe { Shell_NotifyIconW(msg, data) }.as_bool()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake shell: records calls and answers with whether it has the icon.
    #[derive(Default)]
    struct Shell {
        has_icon: bool,
        /// The next MODIFY fails even though the icon is there (a busy shell).
        fail_next_modify: bool,
        calls: Vec<Op>,
    }

    impl Shell {
        fn notify(&mut self, op: Op) -> bool {
            self.calls.push(op);
            match op {
                Op::Add if self.has_icon => false,
                Op::Add => {
                    self.has_icon = true;
                    true
                }
                Op::Modify if std::mem::take(&mut self.fail_next_modify) => false,
                Op::Modify => self.has_icon,
                Op::Delete => std::mem::take(&mut self.has_icon),
            }
        }
    }

    #[test]
    fn first_show_adds_then_modifies() {
        let (mut icon, mut shell) = (ShellIcon::default(), Shell::default());
        assert!(icon.show(|op| shell.notify(op)));
        assert!(icon.show(|op| shell.notify(op)));
        assert_eq!(shell.calls, [Op::Add, Op::Modify]);
    }

    #[test]
    fn failed_modify_does_not_stick() {
        let (mut icon, mut shell) = (ShellIcon::default(), Shell::default());
        icon.show(|op| shell.notify(op));
        shell.fail_next_modify = true;
        assert!(!icon.show(|op| shell.notify(op)));
        // Still believed added: the next change goes through as a MODIFY.
        shell.calls.clear();
        assert!(icon.show(|op| shell.notify(op)));
        assert_eq!(shell.calls, [Op::Modify]);
    }

    #[test]
    fn icon_the_shell_already_has_is_modified() {
        // The old trap: we think it is gone (e.g. a lost ADD reply), the shell still has it.
        let mut shell = Shell { has_icon: true, ..Shell::default() };
        let mut icon = ShellIcon::default();
        assert!(icon.show(|op| shell.notify(op)));
        assert_eq!(shell.calls, [Op::Add, Op::Modify]);
        shell.calls.clear();
        assert!(icon.show(|op| shell.notify(op)));
        assert_eq!(shell.calls, [Op::Modify]);
    }

    #[test]
    fn icon_the_shell_lost_is_added_again() {
        let (mut icon, mut shell) = (ShellIcon::default(), Shell::default());
        icon.show(|op| shell.notify(op));
        // Explorer restarted; TaskbarCreated may come late or not at all.
        shell.has_icon = false;
        assert!(icon.show(|op| shell.notify(op)));
        assert!(shell.has_icon);
        icon.taskbar_created();
        shell.calls.clear();
        assert!(icon.show(|op| shell.notify(op)));
        assert_eq!(shell.calls, [Op::Add, Op::Modify]);
    }

    #[test]
    fn exit_always_deletes() {
        let (mut icon, mut shell) = (ShellIcon::default(), Shell::default());
        icon.show(|op| shell.notify(op));
        shell.fail_next_modify = true;
        // Both fail; we no longer know for sure, so delete anyway.
        shell.has_icon = true;
        icon.show(|op| shell.notify(op));
        icon.delete(|op| shell.notify(op));
        assert!(!shell.has_icon);
        // Even one never confirmed as added.
        let mut shell = Shell { has_icon: true, ..Shell::default() };
        ShellIcon::default().delete(|op| shell.notify(op));
        assert_eq!(shell.calls, [Op::Delete]);
        assert!(!shell.has_icon);
    }
}
