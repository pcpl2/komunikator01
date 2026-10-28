//! The main window: message handling, DPI, menus, tray, hotkeys and the message loop.
//! Behaviour lives in `AppState` (through `App`); this file routes Win32 messages to it,
//! places controls with `layout` and draws the result with `render`.
//!
//! Re-entrancy: many calls here send messages straight back to the window procedure (child
//! notifications, menus, message boxes). The state sits in a `RefCell` that is only ever
//! `try_borrow_mut`ed, and nothing that runs a modal loop is called while it is borrowed.

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::c_void;
use std::rc::Rc;

use client_core::app::Action;
use client_core::settings::WindowRect;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{DeleteObject, ScreenToClient, COLOR_BTNFACE, HBRUSH, HFONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, HTREEITEM, ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES, ICC_STANDARD_CLASSES,
    ICC_TREEVIEW_CLASSES, INITCOMMONCONTROLSEX, NMHDR, NMITEMACTIVATE, NM_CLICK, NM_DBLCLK,
    NM_RCLICK, TVGN_DROPHILITE, TVHITTESTINFO, TVHT_ONITEM, TVIF_PARAM, TVITEMW, TVM_GETITEMW,
    TVM_HITTEST, TVM_SELECTITEM,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RETURN;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::app::App;
use super::hotkey;
use super::icons::{make_icon, tray_pixels, TrayState};
use super::layout::{
    self, create_controls, initial_rect, ID_CHAT, ID_DEAFEN, ID_LEAVE, ID_MUTE, ID_SEND, ID_TOP, ID_TREE,
};
use super::render::{Controls, Render};
use super::text::{tray_state, tray_tip, TreeItem, VOLUME_STEPS};
use super::tray::{self, Tray, TrayCommand};
use super::wide::{from_wide, scale, wide};

/// Posted by the session forwarder when events are queued.
pub const WM_APP_SESSION: u32 = WM_APP + 1;
const WM_APP_TRAY: u32 = WM_APP + 2;

const TIMER_ACTIVITY: usize = 1;
const TIMER_SMOKE: usize = 2;
const HOTKEY_MUTE: i32 = 1;
const HOTKEY_DEAFEN: i32 = 2;

const CLASS: PCWSTR = w!("KomunikatorMain");

pub struct Start {
    pub minimized: bool,
    pub smoke: bool,
}

struct Inner {
    app: App,
    render: Render,
    tray: Option<Tray>,
}

pub struct Window {
    hwnd: Cell<HWND>,
    controls: OnceCell<Controls>,
    /// Normal and bold (toggle buttons) message fonts for the current DPI.
    fonts: Cell<(HFONT, HFONT)>,
    dpi: Cell<u32>,
    minimized: Cell<bool>,
    /// Last on-screen rectangle while neither minimised nor maximised; saved on exit.
    normal: Cell<RECT>,
    taskbar_created: u32,
    /// Class icons (large, small); freed with the window state.
    icons: (HICON, HICON),
    inner: RefCell<Inner>,
}

impl Window {
    pub fn create(app: App, start: Start) -> windows::core::Result<Rc<Window>> {
        unsafe {
            let icc = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_TREEVIEW_CLASSES | ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
            };
            let _ = InitCommonControlsEx(&icc);
            let instance: HINSTANCE = GetModuleHandleW(None)?.into();
            let icon = make_icon(&tray_pixels(TrayState::Connected, false, 32), 32);
            let small = make_icon(&tray_pixels(TrayState::Connected, false, 16), 16);
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                hIcon: icon,
                hIconSm: small,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut c_void),
                lpszClassName: CLASS,
                ..Default::default()
            };
            RegisterClassExW(&class);

            let topmost = app.settings().always_on_top;
            let rect = initial_rect(app.settings().window.as_ref());
            let win = Rc::new(Window {
                hwnd: Cell::new(HWND::default()),
                controls: OnceCell::new(),
                fonts: Cell::new((HFONT::default(), HFONT::default())),
                dpi: Cell::new(96),
                minimized: Cell::new(false),
                normal: Cell::new(rect),
                taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
                icons: (icon, small),
                inner: RefCell::new(Inner { app, render: Render::default(), tray: None }),
            });
            // This reference belongs to the window; WM_NCDESTROY gives it back.
            let raw = Rc::into_raw(win.clone());
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                CLASS,
                w!("Komunikator"),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                Some(instance),
                Some(raw.cast()),
            )?;
            // Elevated (run as admin), UIPI would drop these from medium-integrity Explorer,
            // and after an Explorer restart the tray icon, the only way back, would be gone.
            for m in [win.taskbar_created, WM_APP_TRAY] {
                let _ = ChangeWindowMessageFilterEx(hwnd, m, MSGFLT_ALLOW, None);
            }
            win.dpi.set(GetDpiForWindow(hwnd).max(96));
            let _ = win.controls.set(create_controls(hwnd, instance));
            win.apply_fonts();
            win.layout();
            let c = win.controls();
            if topmost {
                SendMessageW(c.top, BM_SETCHECK, Some(WPARAM(1)), None);
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            if let Ok(mut inner) = win.inner.try_borrow_mut() {
                inner.tray = Some(Tray::new(hwnd, WM_APP_TRAY));
            }
            win.apply_hotkeys();
            if start.minimized {
                win.act(Action::WindowHidden);
            } else {
                let _ = ShowWindow(hwnd, if start.smoke { SW_SHOWNOACTIVATE } else { SW_SHOW });
                SetTimer(Some(hwnd), TIMER_ACTIVITY, 100, None);
                win.redraw();
            }
            if start.smoke {
                SetTimer(Some(hwnd), TIMER_SMOKE, 1000, None);
            }
            Ok(win)
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd.get()
    }

    /// Runs until `Wyjdź` (or the smoke timer); returns the exit code.
    pub fn message_loop(&self) -> i32 {
        let hwnd = self.hwnd();
        let input = self.controls().input;
        let mut msg = MSG::default();
        unsafe {
            loop {
                match GetMessageW(&mut msg, None, 0, 0).0 {
                    0 => return msg.wParam.0 as i32,
                    -1 => return 1,
                    _ => {}
                }
                // Enter in the chat line sends; a single-line edit would only beep.
                if msg.message == WM_KEYDOWN && msg.wParam.0 == VK_RETURN.0 as usize && msg.hwnd == input {
                    let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(ID_SEND as usize), LPARAM(0));
                    continue;
                }
                // Tab moves between controls.
                if IsDialogMessageW(hwnd, &msg).as_bool() {
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Stores the window rectangle in the settings and writes the file.
    pub fn save_settings(&self) {
        let r = self.normal.get();
        self.with_app(|app| {
            if r.right > r.left && r.bottom > r.top {
                app.settings_mut().window =
                    Some(WindowRect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top });
            }
            app.save_settings();
        });
    }

    pub fn with_app(&self, f: impl FnOnce(&mut App)) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            f(&mut inner.app);
        }
    }

    /// Registers the hotkeys from the settings again (at start and after a settings change);
    /// failures go to the status bar.
    pub fn apply_hotkeys(&self) {
        let hwnd = self.hwnd();
        let Ok(inner) = self.inner.try_borrow() else { return };
        let s = inner.app.settings();
        let keys = [
            (HOTKEY_MUTE, s.hotkey_mute.clone(), "Wycisz mikrofon"),
            (HOTKEY_DEAFEN, s.hotkey_deafen.clone(), "Wycisz wszystkich"),
        ];
        drop(inner);
        let mut errors = Vec::new();
        for (id, text, label) in keys {
            hotkey::unregister(hwnd, id);
            let Some(text) = text.filter(|t| !t.trim().is_empty()) else { continue };
            let result = hotkey::parse(&text)
                .ok_or(hotkey::HotkeyError::Invalid)
                .and_then(|h| hotkey::register(hwnd, id, &h));
            if let Err(e) = result {
                errors.push(format!("{label} ({text}): {}", hotkey::describe(&e)));
            }
        }
        if !errors.is_empty() {
            self.with_app(|app| app.set_local_notice(format!("skrót: {}", errors.join("; "))));
            self.redraw();
        }
    }

    /// First step of exiting: the window and tray icon go away at once and nothing more
    /// arrives (timers, hotkeys), while the session and audio shut down behind them.
    pub fn close(&self) {
        let hwnd = self.hwnd();
        unsafe {
            let _ = KillTimer(Some(hwnd), TIMER_ACTIVITY);
            let _ = KillTimer(Some(hwnd), TIMER_SMOKE);
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        hotkey::unregister(hwnd, HOTKEY_MUTE);
        hotkey::unregister(hwnd, HOTKEY_DEAFEN);
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.tray = None;
        }
    }

    pub fn destroy(&self) {
        let _ = unsafe { DestroyWindow(self.hwnd()) };
    }

    fn controls(&self) -> &Controls {
        self.controls.get().expect("controls are created with the window")
    }

    fn act(&self, a: Action) {
        let hwnd = self.hwnd();
        let Ok(mut inner) = self.inner.try_borrow_mut() else { return };
        inner.app.act(hwnd, a);
        self.draw(&mut inner);
    }

    fn redraw(&self) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            self.draw(&mut inner);
        }
    }

    fn draw(&self, inner: &mut Inner) {
        let Some(c) = self.controls.get() else { return };
        let Inner { app, render, tray } = inner;
        let notice = app.take_local_notice();
        let v = app.view();
        render.draw(c, v, app.view_version(), notice);
        if let Some(t) = tray {
            t.update(tray_state(v), v.unread > 0, tray_tip(v), self.dpi.get());
        }
    }

    fn handle(&self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let hwnd = self.hwnd();
        match msg {
            WM_GETMINMAXINFO => {
                let dpi = self.dpi.get();
                let info = unsafe { &mut *(lp.0 as *mut MINMAXINFO) };
                info.ptMinTrackSize = POINT { x: scale(640, dpi), y: scale(420, dpi) };
                Some(LRESULT(0))
            }
            WM_SIZE => {
                // Minimised counts as hidden: new chat then rings and badges the tray.
                let minimized = wp.0 == SIZE_MINIMIZED as usize;
                if minimized != self.minimized.replace(minimized) {
                    unsafe {
                        if minimized {
                            let _ = KillTimer(Some(hwnd), TIMER_ACTIVITY);
                        } else {
                            SetTimer(Some(hwnd), TIMER_ACTIVITY, 100, None);
                        }
                    }
                    self.act(if minimized { Action::WindowHidden } else { Action::WindowShown });
                }
                if !minimized {
                    self.layout();
                    self.remember_rect();
                }
                Some(LRESULT(0))
            }
            WM_MOVE => {
                self.remember_rect();
                None
            }
            WM_DPICHANGED => {
                self.dpi.set(u32::from(loword(wp.0)));
                self.apply_fonts();
                let r = unsafe { *(lp.0 as *const RECT) };
                let _ = unsafe {
                    SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE)
                };
                self.reset_tray();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.hide();
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                Some(LRESULT(0))
            }
            WM_ENDSESSION if wp.0 != 0 => {
                self.save_settings();
                self.close();
                Some(LRESULT(0))
            }
            WM_COMMAND => {
                self.on_command(loword(wp.0), hiword(wp.0));
                Some(LRESULT(0))
            }
            WM_NOTIFY => self.on_notify(unsafe { &*(lp.0 as *const NMHDR) }, lp),
            WM_TIMER => {
                match wp.0 {
                    TIMER_ACTIVITY => {
                        if let Ok(mut inner) = self.inner.try_borrow_mut() {
                            if inner.app.poll_activity() {
                                self.draw(&mut inner);
                            }
                        }
                    }
                    TIMER_SMOKE => unsafe { PostQuitMessage(0) },
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_HOTKEY => {
                match wp.0 as i32 {
                    HOTKEY_MUTE => self.act(Action::ToggleMute),
                    HOTKEY_DEAFEN => self.act(Action::ToggleDeafen),
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_APP_SESSION => {
                if let Ok(mut inner) = self.inner.try_borrow_mut() {
                    inner.app.drain_events(hwnd);
                    self.draw(&mut inner);
                } else {
                    // Events stay queued; ask again once the current handler is done.
                    let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_SESSION, WPARAM(0), LPARAM(0)) };
                }
                Some(LRESULT(0))
            }
            WM_APP_TRAY => {
                match loword(lp.0 as usize) as u32 {
                    WM_LBUTTONUP => self.act(Action::ToggleMute),
                    WM_RBUTTONUP | WM_CONTEXTMENU => self.tray_menu(),
                    _ => {}
                }
                Some(LRESULT(0))
            }
            m if m == self.taskbar_created && m != 0 => {
                self.reset_tray();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn on_command(&self, id: u16, code: u16) {
        let c = self.controls();
        match id {
            ID_MUTE => self.act(Action::ToggleMute),
            ID_DEAFEN => self.act(Action::ToggleDeafen),
            ID_LEAVE => self.act(Action::LeaveRoom),
            ID_TOP if u32::from(code) == BN_CLICKED => {
                let on = unsafe { SendMessageW(c.top, BM_GETCHECK, None, None) }.0 == 1;
                set_topmost(self.hwnd(), on);
                self.with_app(|app| {
                    app.settings_mut().always_on_top = on;
                    app.save_settings();
                });
                self.redraw();
            }
            ID_SEND => {
                let text = window_text(c.input);
                if !text.trim().is_empty() {
                    set_window_text(c.input, "");
                    self.act(Action::SendChat(text));
                }
            }
            _ => {}
        }
    }

    fn on_notify(&self, hdr: &NMHDR, lp: LPARAM) -> Option<LRESULT> {
        match (hdr.idFrom as u16, hdr.code) {
            (ID_TREE, NM_DBLCLK) => {
                if let Some((_, TreeItem::Room(id))) = self.tree_item_at_cursor() {
                    self.act(Action::JoinRoom(id));
                    // Joining, not expanding or collapsing.
                    return Some(LRESULT(1));
                }
                None
            }
            (ID_TREE, NM_RCLICK) => {
                if let Some((item, what)) = self.tree_item_at_cursor() {
                    let tree = self.controls().tree;
                    let select = |i: HTREEITEM| unsafe {
                        SendMessageW(tree, TVM_SELECTITEM, Some(WPARAM(TVGN_DROPHILITE as usize)), Some(LPARAM(i.0)))
                    };
                    select(item);
                    match what {
                        TreeItem::Room(id) => self.room_menu(id),
                        TreeItem::Member(id) => self.member_menu(id),
                    }
                    select(HTREEITEM(0));
                }
                Some(LRESULT(1))
            }
            (ID_CHAT, NM_CLICK) => {
                let row = unsafe { &*(lp.0 as *const NMITEMACTIVATE) }.iItem;
                let link = self.links_at(row).into_iter().next();
                if let Some(link) = link {
                    self.act(Action::CopyLink(link));
                }
                None
            }
            (ID_CHAT, NM_RCLICK) => {
                let row = unsafe { &*(lp.0 as *const NMITEMACTIVATE) }.iItem;
                let links = self.links_at(row);
                let items: Vec<_> = links.iter().map(|l| (format!("Kopiuj: {l}"), MF_STRING)).collect();
                if let Some(i) = popup(self.hwnd(), &items) {
                    self.act(Action::CopyLink(links[i].clone()));
                }
                Some(LRESULT(1))
            }
            _ => None,
        }
    }

    fn links_at(&self, row: i32) -> Vec<String> {
        let Ok(row) = usize::try_from(row) else { return Vec::new() };
        self.inner.try_borrow().map(|i| i.render.links_at(row).to_vec()).unwrap_or_default()
    }

    fn room_menu(&self, id: protocol::RoomId) {
        let items = [("Wejdź".to_owned(), MF_STRING), ("Usuń pokój".to_owned(), MF_STRING)];
        match popup(self.hwnd(), &items) {
            Some(0) => self.act(Action::JoinRoom(id)),
            Some(1) => self.act(Action::DeleteRoom(id)),
            _ => {}
        }
    }

    fn member_menu(&self, id: protocol::MemberId) {
        let member = self.inner.try_borrow().ok().and_then(|i| i.render.member(id).map(|(m, _)| m.clone()));
        let Some(m) = member.filter(|m| !m.me) else { return };
        let check = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };
        let mut items: Vec<_> = VOLUME_STEPS.iter().map(|p| (format!("{p}%"), check(*p == m.percent))).collect();
        items.push((String::new(), MF_SEPARATOR));
        items.push(("Wycisz u mnie".to_owned(), check(m.locally_muted)));
        // Opens in Task 6 (safety code dialog).
        items.push(("Kod bezpieczeństwa…".to_owned(), MF_STRING | MF_GRAYED));
        let mute = VOLUME_STEPS.len() + 1;
        match popup(self.hwnd(), &items) {
            Some(i) if i < VOLUME_STEPS.len() => {
                self.act(Action::SetVolume { name: m.name, percent: VOLUME_STEPS[i] })
            }
            Some(i) if i == mute => self.act(Action::ToggleLocalMute(m.name)),
            _ => {}
        }
    }

    fn tree_item_at_cursor(&self) -> Option<(HTREEITEM, TreeItem)> {
        let tree = self.controls().tree;
        let mut hit = TVHITTESTINFO::default();
        unsafe {
            // Where the click was, not where the cursor is by now.
            let pos = GetMessagePos();
            hit.pt = POINT { x: (pos & 0xFFFF) as i16 as i32, y: (pos >> 16) as i16 as i32 };
            let _ = ScreenToClient(tree, &mut hit.pt);
            SendMessageW(tree, TVM_HITTEST, None, Some(LPARAM(&mut hit as *mut _ as isize)));
            if hit.hItem.0 == 0 || (hit.flags.0 & TVHT_ONITEM.0) == 0 {
                return None;
            }
            let mut it = TVITEMW { mask: TVIF_PARAM, hItem: hit.hItem, ..Default::default() };
            SendMessageW(tree, TVM_GETITEMW, None, Some(LPARAM(&mut it as *mut _ as isize)));
            TreeItem::from_param(it.lParam.0).map(|t| (hit.hItem, t))
        }
    }

    fn tray_menu(&self) {
        let Ok(inner) = self.inner.try_borrow() else { return };
        let (muted, deafened) = (inner.app.view().muted, inner.app.view().deafened);
        drop(inner);
        match tray::menu(self.hwnd(), muted, deafened) {
            Some(TrayCommand::Show) => self.show(),
            Some(TrayCommand::ToggleMute) => self.act(Action::ToggleMute),
            Some(TrayCommand::ToggleDeafen) => self.act(Action::ToggleDeafen),
            Some(TrayCommand::Exit) => unsafe { PostQuitMessage(0) },
            None => {}
        }
    }

    fn reset_tray(&self) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            if let Some(t) = &mut inner.tray {
                t.reset();
            }
            self.draw(&mut inner);
        }
    }

    fn show(&self) {
        let hwnd = self.hwnd();
        let iconic = unsafe { IsIconic(hwnd) }.as_bool();
        unsafe {
            let _ = ShowWindow(hwnd, if iconic { SW_RESTORE } else { SW_SHOW });
            let _ = SetForegroundWindow(hwnd);
        }
        // Restoring from minimised already did both in WM_SIZE.
        if !iconic {
            unsafe { SetTimer(Some(hwnd), TIMER_ACTIVITY, 100, None) };
            self.act(Action::WindowShown);
        }
    }

    fn hide(&self) {
        let hwnd = self.hwnd();
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
            // Nobody sees who is speaking; do not poll for it.
            let _ = KillTimer(Some(hwnd), TIMER_ACTIVITY);
        }
        self.act(Action::WindowHidden);
    }

    fn remember_rect(&self) {
        let hwnd = self.hwnd();
        unsafe {
            if IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() && !IsZoomed(hwnd).as_bool() {
                let mut r = RECT::default();
                if GetWindowRect(hwnd, &mut r).is_ok() {
                    self.normal.set(r);
                }
            }
        }
    }

    fn apply_fonts(&self) {
        let Some(c) = self.controls.get() else { return };
        let fonts = layout::make_fonts(self.dpi.get());
        layout::set_fonts(c, fonts);
        let (old_normal, old_bold) = self.fonts.replace(fonts);
        for f in [old_normal, old_bold] {
            if !f.is_invalid() {
                let _ = unsafe { DeleteObject(f.into()) };
            }
        }
    }

    fn layout(&self) {
        if let Some(c) = self.controls.get() {
            layout::layout(self.hwnd(), c, self.dpi.get());
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let (normal, bold) = self.fonts.get();
        for f in [normal, bold] {
            if !f.is_invalid() {
                let _ = unsafe { DeleteObject(f.into()) };
            }
        }
        for i in [self.icons.0, self.icons.1] {
            if !i.is_invalid() {
                let _ = unsafe { DestroyIcon(i) };
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            let w = cs.lpCreateParams as *const Window;
            (*w).hwnd.set(hwnd);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, w as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Window;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        if msg == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Rc::from_raw(ptr));
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        // Each call holds its own reference, so a nested WM_NCDESTROY cannot free it under us.
        Rc::increment_strong_count(ptr);
        let w = Rc::from_raw(ptr);
        w.handle(msg, wp, lp).unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
    }
}

fn set_topmost(hwnd: HWND, on: bool) {
    let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
    let _ = unsafe { SetWindowPos(hwnd, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) };
}

/// A popup menu at the cursor; returns the index of the chosen item. Runs a modal loop.
fn popup(hwnd: HWND, items: &[(String, MENU_ITEM_FLAGS)]) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        for (i, (text, flags)) in items.iter().enumerate() {
            let t = wide(text);
            let _ = AppendMenuW(menu, *flags, i + 1, PCWSTR(t.as_ptr()));
        }
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
        let _ = DestroyMenu(menu);
        usize::try_from(cmd.0).ok().and_then(|c| c.checked_sub(1))
    }
}

fn window_text(h: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(h).max(0) as usize;
        let mut buf = vec![0u16; len + 1];
        let n = GetWindowTextW(h, &mut buf).max(0) as usize;
        from_wide(&buf[..n])
    }
}

fn set_window_text(h: HWND, text: &str) {
    super::render::set_text(h, text);
}

fn loword(v: usize) -> u16 {
    (v & 0xFFFF) as u16
}

fn hiword(v: usize) -> u16 {
    ((v >> 16) & 0xFFFF) as u16
}
