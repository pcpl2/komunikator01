//! Modal dialogs built in code (no resource templates): first run / account, settings, safety
//! code and new room. Each runs its own message loop with the owner disabled; the dialog
//! procedure only queues what happened and the loop acts on it, so no window state is
//! borrowed while messages are pumped. Validation and value mapping are pure functions.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;

use client_core::app::trust_label;
use client_core::e2e::Trust;
use client_core::settings::{Settings, Sounds};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, GetMonitorInfoW, GetSysColorBrush, MonitorFromPoint, MonitorFromWindow,
    SetBkMode, SetTextColor, COLOR_BTNFACE, HBRUSH, HDC, HFONT, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, EM_LIMITTEXT, EM_SETSEL, HKM_GETHOTKEY, HKM_SETHOTKEY, HOTKEY_CLASSW, ICC_BAR_CLASSES,
    ICC_HOTKEY_CLASS, ICC_PROGRESS_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, PBM_SETPOS, PBM_SETRANGE32,
    PROGRESS_CLASSW, TBM_SETPAGESIZE, TBM_SETPOS, TBM_SETRANGE, TBM_SETTICFREQ, TBS_AUTOTICKS, TRACKBAR_CLASSW,
    WC_BUTTONW, WC_COMBOBOXW, WC_EDITW, WC_STATICW,
};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForMonitor, GetDpiForWindow, SystemParametersInfoForDpi, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::hotkey;
use super::wide::{from_wide, scale, wide};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Server,
    Fingerprint,
    User,
    Password,
}

/// Checks the first-run / account form; the error names the field and says what is wrong.
pub fn validate_account(server: &str, fingerprint: &str, user: &str, password: &str) -> Result<(), (Field, &'static str)> {
    let server = server.trim();
    if server.is_empty() {
        return Err((Field::Server, "podaj adres serwera"));
    }
    match server.rsplit_once(':') {
        None | Some((_, "")) => return Err((Field::Server, "brakuje portu, np. serwer.pl:4433")),
        Some(("", _)) => return Err((Field::Server, "brakuje nazwy serwera przed dwukropkiem")),
        Some((_, port)) => {
            if !port.parse::<u16>().is_ok_and(|p| p != 0) {
                return Err((Field::Server, "port to liczba od 1 do 65535"));
            }
        }
    }
    if protocol::parse_fingerprint(fingerprint).is_none() {
        return Err((Field::Fingerprint, "odcisk to 64 znaki: cyfry 0–9 i litery a–f"));
    }
    if user.trim().is_empty() {
        return Err((Field::User, "podaj nazwę użytkownika"));
    }
    if password.is_empty() {
        return Err((Field::Password, "podaj hasło"));
    }
    Ok(())
}

/// Trackbar position 0–100 to the VAD sensitivity 0.0–1.0.
pub fn sensitivity_from_trackbar(pos: u32) -> f32 {
    pos.min(100) as f32 / 100.0
}

pub fn trackbar_from_sensitivity(s: f32) -> u32 {
    if s.is_nan() {
        return 50;
    }
    (s.clamp(0.0, 1.0) * 100.0).round() as u32
}

/// Mic level for the meter: -60 dBFS (or silence) is empty, 0 dBFS full.
pub fn meter_from_dbfs(db: f32) -> u32 {
    if db.is_nan() {
        return 0;
    }
    ((db.clamp(-60.0, 0.0) + 60.0) / 60.0 * 100.0).round() as u32
}

/// The room name as the server will store it, or why it cannot be one.
pub fn validate_room_name(name: &str) -> Result<String, &'static str> {
    protocol::normalize_room_name(name).ok_or("nazwa pokoju: od 1 do 32 znaków, bez znaków sterujących")
}

/// The typed safety-code group, checked against the code shown.
pub fn check_code_group(code: &str, typed: &str) -> Result<String, &'static str> {
    let typed = typed.trim();
    if typed.len() != 4 || !typed.bytes().all(|b| b.is_ascii_digit()) {
        return Err("wpisz 4 cyfry");
    }
    if code.split(' ').next() != Some(typed) {
        return Err("to nie jest pierwsza grupa tego kodu");
    }
    Ok(typed.to_owned())
}

const HOTKEY_NEEDS_MODIFIER: &str = "dodaj Ctrl, Alt lub Shift (bez nich tylko F13–F24, Pause, ScrollLock)";

/// The hotkey setting for a native hotkey control value. An unchanged control keeps the
/// saved text, so a `Win+…` key (which the control cannot show) survives.
pub fn hotkey_setting(value: u16, saved: Option<&str>) -> Result<Option<String>, &'static str> {
    if value == 0 {
        return Ok(None);
    }
    if let Some(s) = saved.filter(|s| hotkey::parse(s).is_some_and(|h| hotkey::to_control_value(&h) == value)) {
        return Ok(Some(s.to_owned()));
    }
    hotkey::from_control_value(value).map(|h| Some(hotkey::format(&h))).ok_or(HOTKEY_NEEDS_MODIFIER)
}

/// Combo box entries for a device list: labels, the device id of each (`None` = default) and
/// which one is selected. A saved device that is not plugged in stays selectable.
pub fn device_choices(list: &[(String, String)], saved: Option<&str>) -> (Vec<String>, Vec<Option<String>>, usize) {
    let mut labels = vec!["Domyślne urządzenie".to_owned()];
    let mut ids = vec![None];
    for (id, name) in list {
        labels.push(name.clone());
        ids.push(Some(id.clone()));
    }
    let selected = match saved {
        None => 0,
        Some(s) => match ids.iter().position(|i| i.as_deref() == Some(s)) {
            Some(i) => i,
            None => {
                labels.push("Zapisane urządzenie (niepodłączone)".to_owned());
                ids.push(Some(s.to_owned()));
                ids.len() - 1
            }
        },
    };
    (labels, ids, selected)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountInput {
    pub server: String,
    pub fingerprint: String,
    pub username: String,
    pub password: String,
}

/// What the settings dialog edits.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsInput {
    pub capture: Option<String>,
    pub render: Option<String>,
    pub sensitivity: f32,
    pub hotkey_mute: Option<String>,
    pub hotkey_deafen: Option<String>,
    pub sounds: Sounds,
    pub autostart: bool,
    pub run_as_admin: bool,
}

impl SettingsInput {
    pub fn from_settings(s: &Settings) -> SettingsInput {
        SettingsInput {
            capture: s.capture_device.clone(),
            render: s.render_device.clone(),
            sensitivity: s.vad_sensitivity,
            hotkey_mute: s.hotkey_mute.clone(),
            hotkey_deafen: s.hotkey_deafen.clone(),
            sounds: s.sounds.clone(),
            autostart: s.autostart,
            run_as_admin: s.run_as_admin,
        }
    }

    pub fn apply_to(self, s: &mut Settings) {
        s.capture_device = self.capture;
        s.render_device = self.render;
        s.vad_sensitivity = self.sensitivity;
        s.hotkey_mute = self.hotkey_mute;
        s.hotkey_deafen = self.hotkey_deafen;
        s.sounds = self.sounds;
        s.autostart = self.autostart;
        s.run_as_admin = self.run_as_admin;
    }
}

/// Registers (mute, deafen) as they would be after OK; an error text for each that failed.
pub type TryHotkeys<'a> = dyn Fn(Option<&str>, Option<&str>) -> [Option<&'static str>; 2] + 'a;

/// What the settings dialog needs from the window.
pub struct SettingsEnv<'a> {
    /// (id, name) of capture and render devices.
    pub capture_devices: Vec<(String, String)>,
    pub render_devices: Vec<(String, String)>,
    /// Current microphone level in dBFS.
    pub mic_level: &'a dyn Fn() -> f32,
    pub try_hotkeys: &'a TryHotkeys<'a>,
    /// Opens the account dialog over the given owner and applies it.
    pub open_account: &'a dyn Fn(HWND),
}

pub struct SafetyInfo {
    pub name: String,
    pub trust: Trust,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyChoice {
    /// The typed first group of the code.
    Verify(String),
    Accept(String),
}

// --- Dialog machinery -------------------------------------------------------------------

const CLASS: PCWSTR = w!("KomunikatorDialog");
const ID_OK: u16 = 1; // IDOK
const ID_CANCEL: u16 = 2; // IDCANCEL
/// Static controls with ids from here up to `ID_STATIC` are error messages, drawn in red.
const ID_ERROR: u16 = 900;
const ID_STATIC: u16 = 0xFFFF;
const TIMER_LEVEL: usize = 1;
const SS_CENTER: u32 = 0x1;
const SS_NOPREFIX: u32 = 0x80;
const TBM_GETPOS: u32 = WM_USER;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ev {
    Command { id: u16, code: u16 },
    Timer,
    /// Closed from the title bar, or the application is quitting.
    Cancel,
}

/// Lives behind the dialog's GWLP_USERDATA while the window exists.
struct Shared {
    events: RefCell<VecDeque<Ev>>,
    default_id: Cell<u16>,
}

struct Dialog {
    hwnd: HWND,
    owner: Option<HWND>,
    instance: HINSTANCE,
    dpi: u32,
    font: HFONT,
    big_font: Cell<HFONT>,
    shared: Box<Shared>,
}

fn style(owner: Option<HWND>) -> (WINDOW_STYLE, WINDOW_EX_STYLE) {
    let mut ex = WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT;
    if owner.is_none() {
        // The first-run dialog is the whole program then; give it a taskbar button.
        ex |= WS_EX_APPWINDOW;
    }
    (WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN, ex)
}

impl Dialog {
    /// A hidden dialog window; controls are added, then `show` sizes and opens it.
    fn new(owner: Option<HWND>, title: &str) -> Option<Dialog> {
        unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None).ok()?.into();
            register_class(instance);
            let dpi = match owner {
                Some(o) => GetDpiForWindow(o).max(96),
                None => primary_dpi(),
            };
            let shared = Box::new(Shared { events: RefCell::new(VecDeque::new()), default_id: Cell::new(ID_OK) });
            let (style, ex) = style(owner);
            let t = wide(title);
            let hwnd = CreateWindowExW(
                ex,
                CLASS,
                PCWSTR(t.as_ptr()),
                style,
                0,
                0,
                0,
                0,
                owner,
                None,
                Some(instance),
                Some(&*shared as *const Shared as *const c_void),
            )
            .ok()?;
            Some(Dialog {
                hwnd,
                owner,
                instance,
                dpi,
                font: message_font(dpi, 1, false),
                big_font: Cell::new(HFONT::default()),
                shared,
            })
        }
    }

    fn s(&self, v: i32) -> i32 {
        scale(v, self.dpi)
    }

    /// A child control at 96-dpi coordinates.
    #[allow(clippy::too_many_arguments)]
    fn control(&self, class: PCWSTR, text: &str, style: u32, ex: WINDOW_EX_STYLE, id: u16, x: i32, y: i32, w: i32, h: i32) -> HWND {
        let t = wide(text);
        unsafe {
            let c = CreateWindowExW(
                ex,
                class,
                PCWSTR(t.as_ptr()),
                WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | style),
                self.s(x),
                self.s(y),
                self.s(w),
                self.s(h),
                Some(self.hwnd),
                Some(HMENU(id as usize as *mut c_void)),
                Some(self.instance),
                None,
            )
            .unwrap_or_default();
            SendMessageW(c, WM_SETFONT, Some(WPARAM(self.font.0 as usize)), Some(LPARAM(1)));
            c
        }
    }

    fn label(&self, text: &str, x: i32, y: i32, w: i32, h: i32) -> HWND {
        self.control(WC_STATICW, text, SS_NOPREFIX, WINDOW_EX_STYLE::default(), ID_STATIC, x, y, w, h)
    }

    fn error(&self, id: u16, x: i32, y: i32, w: i32) -> HWND {
        self.control(WC_STATICW, "", SS_NOPREFIX, WINDOW_EX_STYLE::default(), id, x, y, w, 16)
    }

    #[allow(clippy::too_many_arguments)]
    fn edit(&self, id: u16, text: &str, style: u32, limit: usize, x: i32, y: i32, w: i32) -> HWND {
        let style = WS_TABSTOP.0 | ES_AUTOHSCROLL as u32 | style;
        let e = self.control(WC_EDITW, text, style, WS_EX_CLIENTEDGE, id, x, y, w, 23);
        unsafe { SendMessageW(e, EM_LIMITTEXT, Some(WPARAM(limit)), None) };
        e
    }

    fn button(&self, id: u16, text: &str, default: bool, x: i32, y: i32, w: i32) -> HWND {
        let kind = if default { BS_DEFPUSHBUTTON } else { BS_PUSHBUTTON };
        self.control(WC_BUTTONW, text, WS_TABSTOP.0 | kind as u32, WINDOW_EX_STYLE::default(), id, x, y, w, 26)
    }

    fn checkbox(&self, id: u16, text: &str, on: bool, x: i32, y: i32, w: i32) -> HWND {
        let style = WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32;
        let c = self.control(WC_BUTTONW, text, style, WINDOW_EX_STYLE::default(), id, x, y, w, 22);
        unsafe { SendMessageW(c, BM_SETCHECK, Some(WPARAM(usize::from(on))), None) };
        c
    }

    /// Gives one control a bold font `times` as large; freed with the dialog.
    fn set_big_font(&self, control: HWND, times: i32) {
        let f = message_font(self.dpi, times, true);
        unsafe { SendMessageW(control, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1))) };
        let old = self.big_font.replace(f);
        if !old.is_invalid() {
            let _ = unsafe { DeleteObject(old.into()) };
        }
    }

    /// Sizes the window for a client area of `w`×`h` (96 dpi), centres it on the owner and
    /// shows it with the owner disabled.
    fn show(&self, w: i32, h: i32, focus: HWND) {
        let (style, ex) = style(self.owner);
        unsafe {
            let mut r = RECT { left: 0, top: 0, right: self.s(w), bottom: self.s(h) };
            let _ = AdjustWindowRectExForDpi(&mut r, style, false, ex, self.dpi);
            let (ww, wh) = (r.right - r.left, r.bottom - r.top);
            let mon = match self.owner {
                Some(o) => MonitorFromWindow(o, MONITOR_DEFAULTTOPRIMARY),
                None => MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY),
            };
            let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(mon, &mut info);
            let work = info.rcWork;
            let mut area = work;
            if let Some(o) = self.owner.filter(|o| IsWindowVisible(*o).as_bool() && !IsIconic(*o).as_bool()) {
                let _ = GetWindowRect(o, &mut area);
            }
            let x = (area.left + (area.right - area.left - ww) / 2).clamp(work.left, (work.right - ww).max(work.left));
            let y = (area.top + (area.bottom - area.top - wh) / 2).clamp(work.top, (work.bottom - wh).max(work.top));
            let _ = SetWindowPos(self.hwnd, None, x, y, ww, wh, SWP_NOZORDER | SWP_NOACTIVATE);
            if let Some(o) = self.owner {
                let _ = EnableWindow(o, false);
            }
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(Some(focus));
        }
    }

    /// Pumps messages until the dialog has something to say.
    fn next(&self) -> Ev {
        let mut msg = MSG::default();
        loop {
            if let Some(e) = self.shared.events.borrow_mut().pop_front() {
                return e;
            }
            unsafe {
                match GetMessageW(&mut msg, None, 0, 0).0 {
                    0 => {
                        // Leave the quit for the loop outside.
                        PostQuitMessage(msg.wParam.0 as i32);
                        return Ev::Cancel;
                    }
                    -1 => return Ev::Cancel,
                    _ => {}
                }
                // Tab, Enter (default button) and Esc (IDCANCEL).
                if !IsDialogMessageW(self.hwnd, &msg).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }
}

impl Drop for Dialog {
    fn drop(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_LEVEL);
            // Before destroying, so activation returns to the owner and not to another program.
            if let Some(o) = self.owner {
                let _ = EnableWindow(o, true);
            }
            let _ = DestroyWindow(self.hwnd);
            for f in [self.font, self.big_font.get()] {
                if !f.is_invalid() {
                    let _ = DeleteObject(f.into());
                }
            }
        }
    }
}

fn register_class(instance: HINSTANCE) {
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_BAR_CLASSES | ICC_HOTKEY_CLASS | ICC_PROGRESS_CLASS,
        };
        let _ = InitCommonControlsEx(&icc);
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(dialog_proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut c_void),
            lpszClassName: CLASS,
            ..Default::default()
        };
        // Fails harmlessly when already registered.
        RegisterClassExW(&class);
    }
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let shared = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Shared;
        if shared.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let shared = &*shared;
        let push = |e: Ev| {
            if let Ok(mut q) = shared.events.try_borrow_mut() {
                // Timer ticks pile up while a nested dialog runs; one is enough.
                if !(e == Ev::Timer && q.back() == Some(&Ev::Timer)) {
                    q.push_back(e);
                }
            }
        };
        match msg {
            WM_COMMAND => {
                push(Ev::Command { id: (wp.0 & 0xFFFF) as u16, code: ((wp.0 >> 16) & 0xFFFF) as u16 });
                LRESULT(0)
            }
            WM_TIMER => {
                push(Ev::Timer);
                LRESULT(0)
            }
            WM_CLOSE => {
                push(Ev::Cancel);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            // Which button Enter presses.
            DM_GETDEFID => LRESULT(((DC_HASDEFID << 16) | u32::from(shared.default_id.get())) as isize),
            WM_CTLCOLORSTATIC if is_error_label(HWND(lp.0 as *mut c_void)) => {
                let dc = HDC(wp.0 as *mut c_void);
                SetTextColor(dc, COLORREF(0x0000_20C0)); // dark red, 0x00BBGGRR
                SetBkMode(dc, TRANSPARENT);
                LRESULT(GetSysColorBrush(COLOR_BTNFACE).0 as isize)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn is_error_label(h: HWND) -> bool {
    let id = unsafe { GetDlgCtrlID(h) };
    (i32::from(ID_ERROR)..i32::from(ID_STATIC)).contains(&id)
}

fn primary_dpi() -> u32 {
    unsafe {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96, 96);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_err() {
            dx = 96;
        }
        dx.max(96)
    }
}

/// The system message font at `dpi`, `times` as large; the caller deletes it.
fn message_font(dpi: u32, times: i32, bold: bool) -> HFONT {
    let mut ncm = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
    unsafe {
        let _ = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some((&mut ncm as *mut NONCLIENTMETRICSW).cast()),
            0,
            dpi,
        );
        let mut f = ncm.lfMessageFont;
        f.lfHeight *= times;
        if bold {
            f.lfWeight = 700;
        }
        CreateFontIndirectW(&f)
    }
}

fn get_text(h: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(h).max(0) as usize;
        let mut buf = vec![0u16; len + 1];
        let n = GetWindowTextW(h, &mut buf).max(0) as usize;
        from_wide(&buf[..n])
    }
}

fn set_text(h: HWND, text: &str) {
    super::render::set_text(h, text);
}

fn focus_all(h: HWND) {
    unsafe {
        let _ = SetFocus(Some(h));
        SendMessageW(h, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
    }
}

fn checked(h: HWND) -> bool {
    unsafe { SendMessageW(h, BM_GETCHECK, None, None) }.0 == 1
}

// --- First run / account ----------------------------------------------------------------

const ID_SERVER: u16 = 201;

/// The account form. `first_run` adds a greeting; `None` means cancelled.
pub fn account_dialog(owner: Option<HWND>, initial: &AccountInput, first_run: bool) -> Option<AccountInput> {
    let title = if first_run { "Komunikator — pierwsze uruchomienie" } else { "Konto" };
    let d = Dialog::new(owner, title)?;
    let (m, w) = (12, 420);
    let fw = w - 2 * m;
    let mut y = m;
    if first_run {
        d.label("Wpisz dane, które dostałeś od osoby prowadzącej serwer.", m, y, fw, 32);
        y += 36;
    }
    let fields = [
        ("Adres serwera (host:port)", initial.server.as_str(), 0, 255),
        ("Odcisk certyfikatu (64 znaki)", initial.fingerprint.as_str(), 0, 128),
        ("Nazwa użytkownika", initial.username.as_str(), 0, 64),
        ("Hasło", initial.password.as_str(), ES_PASSWORD as u32, 256),
    ];
    let mut edits = Vec::new();
    let mut errors = Vec::new();
    for (i, (label, value, style, limit)) in fields.into_iter().enumerate() {
        d.label(label, m, y, fw, 16);
        edits.push(d.edit(ID_SERVER + i as u16, value, style, limit, m, y + 18, fw));
        errors.push(d.error(ID_ERROR + i as u16, m, y + 43, fw));
        y += 64;
    }
    y += 4;
    d.button(ID_OK, "OK", true, w - m - 2 * 88 - 8, y, 88);
    d.button(ID_CANCEL, "Anuluj", false, w - m - 88, y, 88);
    d.show(w, y + 26 + m, edits[0]);
    loop {
        match d.next() {
            Ev::Command { id: ID_OK, .. } => {
                let v: Vec<String> = edits.iter().map(|e| get_text(*e)).collect();
                match validate_account(&v[0], &v[1], &v[2], &v[3]) {
                    Ok(()) => {
                        return Some(AccountInput {
                            server: v[0].trim().to_owned(),
                            fingerprint: v[1].trim().to_owned(),
                            username: v[2].trim().to_owned(),
                            password: v[3].clone(),
                        })
                    }
                    Err((field, text)) => {
                        let i = field as usize;
                        for (j, e) in errors.iter().enumerate() {
                            set_text(*e, if j == i { text } else { "" });
                        }
                        focus_all(edits[i]);
                    }
                }
            }
            Ev::Command { id: ID_CANCEL, .. } | Ev::Cancel => return None,
            Ev::Command { id, code } if u32::from(code) == EN_CHANGE && (ID_SERVER..ID_SERVER + 4).contains(&id) => {
                set_text(errors[usize::from(id - ID_SERVER)], "");
            }
            _ => {}
        }
    }
}

// --- New room ----------------------------------------------------------------------------

/// Asks for a room name; `None` means cancelled.
pub fn new_room_dialog(owner: HWND) -> Option<String> {
    let d = Dialog::new(Some(owner), "Nowy pokój")?;
    let (m, w) = (12, 340);
    let fw = w - 2 * m;
    d.label("Nazwa pokoju (1–32 znaki)", m, m, fw, 16);
    let edit = d.edit(301, "", 0, 32, m, m + 18, fw);
    let error = d.error(ID_ERROR, m, m + 43, fw);
    let y = m + 66;
    d.button(ID_OK, "Utwórz", true, w - m - 2 * 88 - 8, y, 88);
    d.button(ID_CANCEL, "Anuluj", false, w - m - 88, y, 88);
    d.show(w, y + 26 + m, edit);
    loop {
        match d.next() {
            Ev::Command { id: ID_OK, .. } => match validate_room_name(&get_text(edit)) {
                Ok(name) => return Some(name),
                Err(text) => {
                    set_text(error, text);
                    focus_all(edit);
                }
            },
            Ev::Command { id: ID_CANCEL, .. } | Ev::Cancel => return None,
            _ => {}
        }
    }
}

// --- Safety code -------------------------------------------------------------------------

const ID_ACCEPT: u16 = 401;

/// Shows a member's safety code and asks for its first group to verify (or, after a key
/// change, to accept the new key).
pub fn safety_dialog(owner: HWND, info: &SafetyInfo) -> Option<SafetyChoice> {
    let d = Dialog::new(Some(owner), "Kod bezpieczeństwa")?;
    let (m, w) = (12, 420);
    let fw = w - 2 * m;
    let changed = info.trust == Trust::KeyChanged;
    d.label(&format!("Osoba: {}", info.name), m, m, fw, 16);
    d.label(&format!("Stan: {}", trust_label(info.trust)), m, m + 20, fw, 16);
    let style = SS_CENTER | SS_NOPREFIX;
    let code = d.control(WC_STATICW, &info.code, style, WINDOW_EX_STYLE::default(), ID_STATIC, m, m + 46, fw, 40);
    d.set_big_font(code, 2);
    let mut y = m + 96;
    d.label("Porównajcie kod głosowo. Wpisz jego pierwszą grupę:", m, y, fw, 16);
    y += 20;
    let edit = d.edit(402, "", ES_NUMBER as u32, 4, m, y, 80);
    let error = d.error(ID_ERROR, m + 90, y + 4, fw - 90);
    y += 36;
    let bw = 150;
    let verify = d.button(ID_OK, "Zweryfikuj", !changed, w - m - 88 - 8 - 110, y, 110);
    if changed {
        d.button(ID_ACCEPT, "Akceptuj nowy klucz", true, m, y, bw);
        // A changed key is accepted first; only then can it be verified.
        let _ = unsafe { EnableWindow(verify, false) };
        d.shared.default_id.set(ID_ACCEPT);
    }
    d.button(ID_CANCEL, "Zamknij", false, w - m - 88, y, 88);
    d.show(w, y + 26 + m, edit);
    loop {
        let choice: fn(String) -> SafetyChoice = match d.next() {
            Ev::Command { id: ID_OK, .. } if !changed => SafetyChoice::Verify,
            Ev::Command { id: ID_ACCEPT, .. } if changed => SafetyChoice::Accept,
            Ev::Command { id: ID_CANCEL, .. } | Ev::Cancel => return None,
            _ => continue,
        };
        match check_code_group(&info.code, &get_text(edit)) {
            Ok(group) => return Some(choice(group)),
            Err(text) => {
                set_text(error, text);
                focus_all(edit);
            }
        }
    }
}

// --- Settings ----------------------------------------------------------------------------

const ID_CAPTURE: u16 = 501;
const ID_RENDER: u16 = 502;
const ID_SENSITIVITY: u16 = 503;
const ID_LEVEL: u16 = 504;
const ID_HOTKEY_MUTE: u16 = 505;
const ID_HOTKEY_DEAFEN: u16 = 506;
const ID_CLEAR_MUTE: u16 = 507;
const ID_CLEAR_DEAFEN: u16 = 508;
const ID_SOUND_MUTE: u16 = 509;
const ID_SOUND_JOIN: u16 = 510;
const ID_SOUND_CHAT: u16 = 511;
const ID_AUTOSTART: u16 = 512;
const ID_ADMIN: u16 = 513;
const ID_ACCOUNT: u16 = 514;

const HOTKEY_TAKEN_KEPT: &str = "OK zapisze go mimo to";

/// The settings form; `None` means cancelled. Hotkeys are tried for real on OK: a taken one
/// is reported here and saved anyway if OK is pressed again.
pub fn settings_dialog(owner: HWND, current: &SettingsInput, env: SettingsEnv) -> Option<SettingsInput> {
    let d = Dialog::new(Some(owner), "Ustawienia")?;
    let (m, w, lw) = (12, 500, 180);
    let (fx, fw) = (m + lw, w - 2 * m - lw);
    let mut y = m;

    let combo = |id: u16, label: &str, list: &[(String, String)], saved: Option<&str>, y: i32| {
        d.label(label, m, y + 4, lw, 16);
        let style = WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32;
        let c = d.control(WC_COMBOBOXW, "", style, WINDOW_EX_STYLE::default(), id, fx, y, fw, 220);
        let (labels, ids, selected) = device_choices(list, saved);
        for l in &labels {
            let t = wide(l);
            unsafe { SendMessageW(c, CB_ADDSTRING, None, Some(LPARAM(t.as_ptr() as isize))) };
        }
        unsafe { SendMessageW(c, CB_SETCURSEL, Some(WPARAM(selected)), None) };
        (c, ids)
    };
    let (capture, capture_ids) = combo(ID_CAPTURE, "Mikrofon", &env.capture_devices, current.capture.as_deref(), y);
    y += 32;
    let (render, render_ids) = combo(ID_RENDER, "Głośniki", &env.render_devices, current.render.as_deref(), y);
    y += 34;

    d.label("Czułość wykrywania mowy", m, y + 6, lw, 16);
    let style = WS_TABSTOP.0 | TBS_AUTOTICKS;
    let slider = d.control(TRACKBAR_CLASSW, "", style, WINDOW_EX_STYLE::default(), ID_SENSITIVITY, fx, y, fw, 30);
    unsafe {
        SendMessageW(slider, TBM_SETRANGE, Some(WPARAM(1)), Some(LPARAM(100 << 16)));
        SendMessageW(slider, TBM_SETTICFREQ, Some(WPARAM(10)), None);
        SendMessageW(slider, TBM_SETPAGESIZE, None, Some(LPARAM(10)));
        let pos = trackbar_from_sensitivity(current.sensitivity) as isize;
        SendMessageW(slider, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(pos)));
    }
    y += 34;
    d.label("Poziom mikrofonu", m, y, lw, 16);
    let meter = d.control(PROGRESS_CLASSW, "", 0, WINDOW_EX_STYLE::default(), ID_LEVEL, fx + 6, y, fw - 12, 16);
    unsafe { SendMessageW(meter, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(100))) };
    y += 28;

    let hotkey_row = |id: u16, clear: u16, label: &str, saved: Option<&str>, err: u16, y: i32| {
        d.label(label, m, y + 4, lw, 16);
        let h = d.control(HOTKEY_CLASSW, "", WS_TABSTOP.0, WS_EX_CLIENTEDGE, id, fx, y, fw - 96, 23);
        let value = saved.and_then(hotkey::parse).map_or(0, |k| hotkey::to_control_value(&k));
        unsafe { SendMessageW(h, HKM_SETHOTKEY, Some(WPARAM(usize::from(value))), None) };
        d.button(clear, "Wyczyść", false, fx + fw - 88, y - 1, 88);
        (h, d.error(err, m, y + 26, w - 2 * m))
    };
    let mute_saved = current.hotkey_mute.as_deref();
    let (mute_key, mute_err) = hotkey_row(ID_HOTKEY_MUTE, ID_CLEAR_MUTE, "Wycisz mikrofon", mute_saved, ID_ERROR, y);
    y += 46;
    let deafen_saved = current.hotkey_deafen.as_deref();
    let (deafen_key, deafen_err) =
        hotkey_row(ID_HOTKEY_DEAFEN, ID_CLEAR_DEAFEN, "Wycisz wszystkich", deafen_saved, ID_ERROR + 1, y);
    y += 50;

    let cw = w - 2 * m;
    let sounds = &current.sounds;
    let sound_mute = d.checkbox(ID_SOUND_MUTE, "Dźwięk przy wyciszeniu", sounds.mute, m, y, cw);
    let sound_join = d.checkbox(ID_SOUND_JOIN, "Dźwięk przy wejściu/wyjściu osoby", sounds.join_leave, m, y + 24, cw);
    let sound_chat = d.checkbox(ID_SOUND_CHAT, "Dźwięk nowej wiadomości", sounds.chat, m, y + 48, cw);
    let autostart = d.checkbox(ID_AUTOSTART, "Uruchamiaj z Windows", current.autostart, m, y + 72, cw);
    let admin_text = "Uruchamiaj jako administrator (gdy skróty nie działają w grze)";
    let admin = d.checkbox(ID_ADMIN, admin_text, current.run_as_admin, m, y + 96, cw);
    y += 130;
    d.button(ID_ACCOUNT, "Konto…", false, m, y, 88);
    d.button(ID_OK, "OK", true, w - m - 2 * 88 - 8, y, 88);
    d.button(ID_CANCEL, "Anuluj", false, w - m - 88, y, 88);
    d.show(w, y + 26 + m, capture);

    let level = || {
        let pos = meter_from_dbfs((env.mic_level)());
        unsafe { SendMessageW(meter, PBM_SETPOS, Some(WPARAM(pos as usize)), None) };
    };
    level();
    unsafe { SetTimer(Some(d.hwnd), TIMER_LEVEL, 100, None) };
    let selected = |c: HWND, ids: &[Option<String>]| {
        let i = unsafe { SendMessageW(c, CB_GETCURSEL, None, None) }.0;
        usize::try_from(i).ok().and_then(|i| ids.get(i).cloned()).flatten()
    };
    let key_value = |h: HWND| (unsafe { SendMessageW(h, HKM_GETHOTKEY, None, None) }.0 & 0xFFFF) as u16;
    // Hotkeys already reported as taken; OK with the same ones keeps them.
    let mut acknowledged: Option<(Option<String>, Option<String>)> = None;
    loop {
        match d.next() {
            Ev::Timer => level(),
            Ev::Command { id: ID_CLEAR_MUTE, .. } => clear_hotkey(mute_key, mute_err),
            Ev::Command { id: ID_CLEAR_DEAFEN, .. } => clear_hotkey(deafen_key, deafen_err),
            Ev::Command { id: ID_ACCOUNT, .. } => (env.open_account)(d.hwnd),
            Ev::Command { id: ID_CANCEL, .. } | Ev::Cancel => return None,
            Ev::Command { id: ID_OK, .. } => {
                let mute = hotkey_setting(key_value(mute_key), mute_saved);
                let deafen = hotkey_setting(key_value(deafen_key), deafen_saved);
                set_text(mute_err, mute.as_ref().err().copied().unwrap_or(""));
                set_text(deafen_err, deafen.as_ref().err().copied().unwrap_or(""));
                let (Ok(mute), Ok(deafen)) = (mute, deafen) else { continue };
                if mute.is_some() && mute == deafen {
                    set_text(deafen_err, "ten sam skrót co „Wycisz mikrofon”");
                    continue;
                }
                let keys = (mute.clone(), deafen.clone());
                let taken = (env.try_hotkeys)(mute.as_deref(), deafen.as_deref());
                if taken.iter().any(Option::is_some) && acknowledged.as_ref() != Some(&keys) {
                    for (e, t) in [mute_err, deafen_err].into_iter().zip(taken) {
                        set_text(e, &t.map(|t| format!("{t}; {HOTKEY_TAKEN_KEPT}")).unwrap_or_default());
                    }
                    acknowledged = Some(keys);
                    continue;
                }
                let pos = unsafe { SendMessageW(slider, TBM_GETPOS, None, None) }.0;
                return Some(SettingsInput {
                    capture: selected(capture, &capture_ids),
                    render: selected(render, &render_ids),
                    sensitivity: sensitivity_from_trackbar(u32::try_from(pos).unwrap_or(0)),
                    hotkey_mute: mute,
                    hotkey_deafen: deafen,
                    sounds: Sounds { mute: checked(sound_mute), join_leave: checked(sound_join), chat: checked(sound_chat) },
                    autostart: checked(autostart),
                    run_as_admin: checked(admin),
                });
            }
            _ => {}
        }
    }
}

fn clear_hotkey(control: HWND, error: HWND) {
    unsafe {
        SendMessageW(control, HKM_SETHOTKEY, Some(WPARAM(0)), None);
        let _ = SetFocus(Some(control));
    }
    set_text(error, "");
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "abababababababababababababababababababababababababababababababab";

    #[test]
    fn account_validation_messages() {
        assert_eq!(validate_account("serwer.pl:4433", FP, "ola", "tajne"), Ok(()));
        assert_eq!(validate_account(" [::1]:4433 ", &FP.to_uppercase(), " ola ", " "), Ok(()));
        let bad_fp = FP.replace('a', "g");
        let cases = [
            (("", FP, "ola", "x"), (Field::Server, "podaj adres serwera")),
            (("serwer.pl", FP, "ola", "x"), (Field::Server, "brakuje portu, np. serwer.pl:4433")),
            (("serwer.pl:", FP, "ola", "x"), (Field::Server, "brakuje portu, np. serwer.pl:4433")),
            ((":4433", FP, "ola", "x"), (Field::Server, "brakuje nazwy serwera przed dwukropkiem")),
            (("serwer.pl:70000", FP, "ola", "x"), (Field::Server, "port to liczba od 1 do 65535")),
            (("serwer.pl:0", FP, "ola", "x"), (Field::Server, "port to liczba od 1 do 65535")),
            (("serwer.pl:abc", FP, "ola", "x"), (Field::Server, "port to liczba od 1 do 65535")),
            (("serwer.pl:4433", "abc", "ola", "x"), (Field::Fingerprint, "odcisk to 64 znaki: cyfry 0–9 i litery a–f")),
            (("serwer.pl:4433", &bad_fp, "ola", "x"), (Field::Fingerprint, "odcisk to 64 znaki: cyfry 0–9 i litery a–f")),
            (("serwer.pl:4433", FP, "  ", "x"), (Field::User, "podaj nazwę użytkownika")),
            (("serwer.pl:4433", FP, "ola", ""), (Field::Password, "podaj hasło")),
        ];
        for ((server, fp, user, pw), want) in cases {
            assert_eq!(validate_account(server, fp, user, pw), Err(want), "{server:?} {fp:?} {user:?} {pw:?}");
        }
    }

    #[test]
    fn sensitivity_maps_to_vad_range() {
        assert_eq!(sensitivity_from_trackbar(0), 0.0);
        assert_eq!(sensitivity_from_trackbar(50), 0.5);
        assert_eq!(sensitivity_from_trackbar(100), 1.0);
        assert_eq!(sensitivity_from_trackbar(250), 1.0);
        assert_eq!(trackbar_from_sensitivity(0.0), 0);
        assert_eq!(trackbar_from_sensitivity(0.5), 50);
        assert_eq!(trackbar_from_sensitivity(1.0), 100);
        assert_eq!(trackbar_from_sensitivity(0.333), 33);
        assert_eq!(trackbar_from_sensitivity(-1.0), 0);
        assert_eq!(trackbar_from_sensitivity(f32::NAN), 50);
    }

    #[test]
    fn level_meter_scale() {
        assert_eq!(meter_from_dbfs(0.0), 100);
        assert_eq!(meter_from_dbfs(-30.0), 50);
        assert_eq!(meter_from_dbfs(-60.0), 0);
        assert_eq!(meter_from_dbfs(-120.0), 0);
        assert_eq!(meter_from_dbfs(f32::NEG_INFINITY), 0);
        assert_eq!(meter_from_dbfs(f32::NAN), 0);
        assert_eq!(meter_from_dbfs(3.0), 100);
    }

    #[test]
    fn room_name_and_code_group() {
        assert_eq!(validate_room_name("  Ranked "), Ok("Ranked".to_owned()));
        assert!(validate_room_name("   ").is_err());
        assert!(validate_room_name(&"ż".repeat(33)).is_err());
        let code = "3141 1111 2222 3333";
        assert_eq!(check_code_group(code, " 3141 "), Ok("3141".to_owned()));
        assert_eq!(check_code_group(code, "1111"), Err("to nie jest pierwsza grupa tego kodu"));
        assert_eq!(check_code_group(code, "314"), Err("wpisz 4 cyfry"));
        assert_eq!(check_code_group(code, "31a1"), Err("wpisz 4 cyfry"));
    }

    #[test]
    fn hotkey_control_to_setting() {
        let value = |s: &str| hotkey::to_control_value(&hotkey::parse(s).unwrap());
        assert_eq!(hotkey_setting(0, Some("Ctrl+M")), Ok(None));
        assert_eq!(hotkey_setting(value("Ctrl+Shift+M"), None), Ok(Some("Ctrl+Shift+M".into())));
        // The control cannot show Win; an untouched control keeps it.
        assert_eq!(hotkey_setting(value("Win+Alt+M"), Some("Win+Alt+M")), Ok(Some("Win+Alt+M".into())));
        assert_eq!(hotkey_setting(value("Alt+M"), Some("Ctrl+K")), Ok(Some("Alt+M".into())));
        assert_eq!(hotkey_setting(0x78, None), Err(HOTKEY_NEEDS_MODIFIER)); // bare F9
        assert_eq!(hotkey_setting(0x7C, None), Ok(Some("F13".into())));
    }

    #[test]
    fn device_list_choices() {
        let list = vec![("id-a".to_owned(), "Słuchawki".to_owned()), ("id-b".to_owned(), "Głośniki".to_owned())];
        let (labels, ids, sel) = device_choices(&list, None);
        assert_eq!(labels, vec!["Domyślne urządzenie", "Słuchawki", "Głośniki"]);
        assert_eq!(ids, vec![None, Some("id-a".into()), Some("id-b".into())]);
        assert_eq!(sel, 0);
        assert_eq!(device_choices(&list, Some("id-b")).2, 2);
        let (labels, ids, sel) = device_choices(&list, Some("id-gone"));
        assert_eq!(sel, 3);
        assert_eq!(labels[3], "Zapisane urządzenie (niepodłączone)");
        assert_eq!(ids[3].as_deref(), Some("id-gone"));
    }

    /// Drives real dialogs on this thread: a thread timer finds the open dialog among this
    /// thread's own windows and sends it messages, one step per tick.
    mod scripted {
        use super::*;
        use std::cell::RefCell;
        use windows::Win32::System::Threading::GetCurrentThreadId;

        type Script = Box<dyn FnMut(HWND, u32)>;

        thread_local! {
            static SCRIPT: RefCell<Option<(Script, u32)>> = const { RefCell::new(None) };
        }

        pub fn run<T>(script: impl FnMut(HWND, u32) + 'static, f: impl FnOnce() -> T) -> T {
            SCRIPT.with(|s| *s.borrow_mut() = Some((Box::new(script), 0)));
            let timer = unsafe { SetTimer(None, 0, 30, Some(tick)) };
            let out = f();
            unsafe {
                let _ = KillTimer(None, timer);
            }
            SCRIPT.with(|s| *s.borrow_mut() = None);
            out
        }

        unsafe extern "system" fn tick(_: HWND, _: u32, _: usize, _: u32) {
            let Some(dialog) = open_dialog() else { return };
            let Some((mut script, step)) = SCRIPT.with(|s| s.borrow_mut().take()) else { return };
            script(dialog, step);
            SCRIPT.with(|s| *s.borrow_mut() = Some((script, step + 1)));
        }

        fn open_dialog() -> Option<HWND> {
            unsafe extern "system" fn each(h: HWND, lp: LPARAM) -> windows::core::BOOL {
                let mut name = [0u16; 32];
                let n = unsafe { GetClassNameW(h, &mut name) }.max(0) as usize;
                if from_wide(&name[..n]) == "KomunikatorDialog" && unsafe { IsWindowVisible(h) }.as_bool() {
                    unsafe { *(lp.0 as *mut HWND) = h };
                }
                true.into()
            }
            let mut found = HWND::default();
            unsafe {
                let _ = EnumThreadWindows(GetCurrentThreadId(), Some(each), LPARAM(&mut found as *mut HWND as isize));
            }
            (!found.is_invalid()).then_some(found)
        }

        pub fn item(dialog: HWND, id: u16) -> HWND {
            unsafe { GetDlgItem(Some(dialog), i32::from(id)) }.unwrap()
        }

        pub fn press(dialog: HWND, id: u16) {
            unsafe {
                let _ = PostMessageW(Some(dialog), WM_COMMAND, WPARAM(usize::from(id)), LPARAM(0));
            }
        }

        /// A key press queued for one of our own controls, as the keyboard would deliver it.
        pub fn key(control: HWND, vk: usize) {
            unsafe {
                let _ = PostMessageW(Some(control), WM_KEYDOWN, WPARAM(vk), LPARAM(1));
            }
        }

        /// A hidden window to own the dialog, as the main window does.
        pub fn owner() -> HWND {
            unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    WC_STATICW,
                    w!("owner"),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            }
        }
    }

    #[test]
    fn account_dialog_validates_then_returns_values() {
        let initial = AccountInput { server: "serwer.pl".into(), ..AccountInput::default() };
        let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
        let log = seen.clone();
        let result = scripted::run(
            move |d, step| match step {
                0 => {
                    // Missing port: refused, message next to the field.
                    set_text(scripted::item(d, ID_SERVER + 1), FP);
                    set_text(scripted::item(d, ID_SERVER + 2), " ola ");
                    set_text(scripted::item(d, ID_SERVER + 3), "tajne hasło");
                    scripted::press(d, ID_OK);
                }
                1 => {
                    log.borrow_mut().push(get_text(scripted::item(d, ID_ERROR)));
                    set_text(scripted::item(d, ID_SERVER), "serwer.pl:4433");
                }
                2 => {
                    // Editing the field cleared its message; Enter in a field is OK.
                    log.borrow_mut().push(get_text(scripted::item(d, ID_ERROR)));
                    scripted::key(scripted::item(d, ID_SERVER + 2), 0x0D);
                }
                _ => {}
            },
            || account_dialog(None, &initial, true),
        );
        assert_eq!(*seen.borrow(), vec!["brakuje portu, np. serwer.pl:4433".to_owned(), String::new()]);
        assert_eq!(
            result,
            Some(AccountInput {
                server: "serwer.pl:4433".into(),
                fingerprint: FP.into(),
                username: "ola".into(),
                password: "tajne hasło".into(),
            })
        );
        // Esc is Cancel.
        let esc = |d, _| scripted::key(scripted::item(d, ID_SERVER), 0x1B);
        let cancelled = scripted::run(esc, || account_dialog(None, &initial, false));
        assert_eq!(cancelled, None);
    }

    #[test]
    fn settings_dialog_reports_taken_hotkey_then_keeps_it() {
        let owner = scripted::owner();
        let current = SettingsInput::from_settings(&Settings::default());
        let taken = hotkey::parse("Ctrl+Shift+F20").unwrap();
        let tried = RefCell::new(Vec::new());
        let seen = std::rc::Rc::new(RefCell::new(String::new()));
        let log = seen.clone();
        let try_hotkeys = |m: Option<&str>, d: Option<&str>| {
            tried.borrow_mut().push((m.map(str::to_owned), d.map(str::to_owned)));
            [m.map(|_| hotkey::describe(&hotkey::HotkeyError::Taken)), None]
        };
        let env = SettingsEnv {
            capture_devices: vec![("id-a".into(), "Mikrofon USB".into())],
            render_devices: Vec::new(),
            mic_level: &|| -30.0,
            try_hotkeys: &try_hotkeys,
            open_account: &|_| {},
        };
        let result = scripted::run(
            move |d, step| unsafe {
                match step {
                    0 => {
                        let value = hotkey::to_control_value(&taken);
                        SendMessageW(scripted::item(d, ID_HOTKEY_MUTE), HKM_SETHOTKEY, Some(WPARAM(value.into())), None);
                        SendMessageW(scripted::item(d, ID_CAPTURE), CB_SETCURSEL, Some(WPARAM(1)), None);
                        SendMessageW(scripted::item(d, ID_SENSITIVITY), TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(80)));
                        SendMessageW(scripted::item(d, ID_SOUND_CHAT), BM_SETCHECK, Some(WPARAM(0)), None);
                        scripted::press(d, ID_OK);
                    }
                    1 => {
                        *log.borrow_mut() = get_text(scripted::item(d, ID_ERROR));
                        scripted::press(d, ID_OK);
                    }
                    _ => {}
                }
            },
            || settings_dialog(owner, &current, env),
        );
        assert_eq!(*seen.borrow(), "skrót jest już zajęty przez inny program; OK zapisze go mimo to");
        let key = Some("Ctrl+Shift+F20".to_owned());
        assert_eq!(*tried.borrow(), vec![(key.clone(), None), (key.clone(), None)]);
        let mut want = current.clone();
        want.capture = Some("id-a".into());
        want.sensitivity = 0.8;
        want.hotkey_mute = key;
        want.sounds.chat = false;
        assert_eq!(result, Some(want));
        assert!(unsafe { windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(owner) }.as_bool());
        unsafe {
            let _ = DestroyWindow(owner);
        }
    }
}
