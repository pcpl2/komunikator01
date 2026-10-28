//! Global hotkeys via RegisterHotKey only (never hooks).
use windows::core::HRESULT;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};

pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;
pub const MOD_WIN: u32 = 0x8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyError {
    Taken,
    Invalid,
}

const VK_PAUSE: u16 = 0x13;
const VK_SCROLL: u16 = 0x91;
const VK_F1: u16 = 0x70;
const VK_NUMPAD0: u16 = 0x60;

// Native hotkey control (HKM_GETHOTKEY) modifier bits, high byte of the value.
const HOTKEYF_SHIFT: u16 = 0x1;
const HOTKEYF_CONTROL: u16 = 0x2;
const HOTKEYF_ALT: u16 = 0x4;

const ERROR_HOTKEY_ALREADY_REGISTERED: u32 = 1409;

fn key_from_name(name: &str) -> Option<u16> {
    let up = name.to_ascii_uppercase();
    let b = up.as_bytes();
    if b.len() == 1 && (b[0].is_ascii_uppercase() || b[0].is_ascii_digit()) {
        return Some(u16::from(b[0]));
    }
    if up == "PAUSE" {
        return Some(VK_PAUSE);
    }
    if up == "SCROLLLOCK" {
        return Some(VK_SCROLL);
    }
    if let Some(n) = up.strip_prefix("NUM") {
        return match n.as_bytes() {
            [d] if d.is_ascii_digit() => Some(VK_NUMPAD0 + u16::from(d - b'0')),
            _ => None,
        };
    }
    if let Some(n) = up.strip_prefix('F') {
        if n.bytes().all(|c| c.is_ascii_digit()) && !n.starts_with('0') {
            let n: u16 = n.parse().ok()?;
            if (1..=24).contains(&n) {
                return Some(VK_F1 + n - 1);
            }
        }
    }
    None
}

fn key_name(vk: u16) -> Option<String> {
    Some(match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from(vk as u8).to_string(),
        VK_PAUSE => "Pause".into(),
        VK_SCROLL => "ScrollLock".into(),
        0x60..=0x69 => format!("Num{}", vk - VK_NUMPAD0),
        0x70..=0x87 => format!("F{}", vk - VK_F1 + 1),
        _ => return None,
    })
}

/// Parses "Ctrl+Shift+M" (case-insensitive). A modifier is required unless the key is
/// F13-F24, Pause or ScrollLock, which are safe to bind on their own.
pub fn parse(s: &str) -> Option<Hotkey> {
    let mut mods = 0;
    let mut vk = None;
    for part in s.split('+').map(str::trim) {
        let bit = match part.to_ascii_lowercase().as_str() {
            "ctrl" => MOD_CONTROL,
            "alt" => MOD_ALT,
            "shift" => MOD_SHIFT,
            "win" => MOD_WIN,
            _ => 0,
        };
        if bit != 0 {
            if mods & bit != 0 || vk.is_some() {
                return None;
            }
            mods |= bit;
        } else {
            if vk.is_some() {
                return None;
            }
            vk = Some(key_from_name(part)?);
        }
    }
    let vk = vk?;
    let standalone = matches!(vk, VK_PAUSE | VK_SCROLL | 0x7C..=0x87);
    if mods == 0 && !standalone {
        return None;
    }
    Some(Hotkey { mods, vk })
}

/// Canonical text, modifiers in the order Ctrl+Alt+Shift+Win.
pub fn format(h: &Hotkey) -> String {
    let mut out = String::new();
    for (bit, name) in [
        (MOD_CONTROL, "Ctrl"),
        (MOD_ALT, "Alt"),
        (MOD_SHIFT, "Shift"),
        (MOD_WIN, "Win"),
    ] {
        if h.mods & bit != 0 {
            out.push_str(name);
            out.push('+');
        }
    }
    out.push_str(&key_name(h.vk).unwrap_or_else(|| format!("0x{:02X}", h.vk)));
    out
}

/// Value as exchanged with the native hotkey control (low byte vk, high byte HOTKEYF_*).
/// The control has no Win modifier, so it is dropped.
pub fn to_control_value(h: &Hotkey) -> u16 {
    let mut f = 0;
    if h.mods & MOD_SHIFT != 0 {
        f |= HOTKEYF_SHIFT;
    }
    if h.mods & MOD_CONTROL != 0 {
        f |= HOTKEYF_CONTROL;
    }
    if h.mods & MOD_ALT != 0 {
        f |= HOTKEYF_ALT;
    }
    (f << 8) | h.vk
}

/// None when the control is empty or holds a key we cannot name.
pub fn from_control_value(v: u16) -> Option<Hotkey> {
    let vk = v & 0xFF;
    key_name(vk)?;
    let f = v >> 8;
    let mut mods = 0;
    if f & HOTKEYF_SHIFT != 0 {
        mods |= MOD_SHIFT;
    }
    if f & HOTKEYF_CONTROL != 0 {
        mods |= MOD_CONTROL;
    }
    if f & HOTKEYF_ALT != 0 {
        mods |= MOD_ALT;
    }
    Some(Hotkey { mods, vk })
}

pub fn describe(e: &HotkeyError) -> &'static str {
    match e {
        HotkeyError::Taken => "skrót jest już zajęty przez inny program",
        HotkeyError::Invalid => "niepoprawny skrót",
    }
}

/// RegisterHotKey with MOD_NOREPEAT, so WM_HOTKEY arrives once per key press.
pub fn register(hwnd: HWND, id: i32, h: &Hotkey) -> Result<(), HotkeyError> {
    let mods = HOT_KEY_MODIFIERS(h.mods) | MOD_NOREPEAT;
    unsafe { RegisterHotKey(Some(hwnd), id, mods, u32::from(h.vk)) }.map_err(|e| {
        if e.code() == HRESULT::from_win32(ERROR_HOTKEY_ALREADY_REGISTERED) {
            HotkeyError::Taken
        } else {
            HotkeyError::Invalid
        }
    })
}

pub fn unregister(hwnd: HWND, id: i32) {
    let _ = unsafe { UnregisterHotKey(Some(hwnd), id) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
    };

    #[test]
    fn hotkey_parse_and_format_roundtrip() {
        let h = parse("ctrl+shift+m").unwrap();
        assert_eq!(h, Hotkey { mods: MOD_CONTROL | MOD_SHIFT, vk: b'M' as u16 });
        assert_eq!(format(&h), "Ctrl+Shift+M");
        for s in ["Alt+F9", "Ctrl+Alt+Shift+Win+0", "Win+Num5", "F13", "Pause", "Shift+ScrollLock", "Ctrl+F24"] {
            assert_eq!(format(&parse(s).unwrap()), s);
        }
        assert_eq!(parse(" CTRL + alt + f9 ").unwrap(), parse("Ctrl+Alt+F9").unwrap());
    }

    #[test]
    fn hotkey_requires_modifier() {
        assert!(parse("M").is_none());
        assert!(parse("F9").is_none());
        assert!(parse("Num1").is_none());
        assert!(parse("F13").is_some());
        assert!(parse("F24").is_some());
        assert!(parse("Pause").is_some());
        assert!(parse("ScrollLock").is_some());
    }

    #[test]
    fn hotkey_rejects_garbage() {
        for s in [
            "", "Ctrl+", "Ctrl+Shift", "Ctrl+Foo", "Ctrl+F25", "Ctrl+F0", "Ctrl+M+K",
            "Ctrl+Ctrl+M", "Ctrl++M", "Ctrl+Num10",
        ] {
            assert!(parse(s).is_none(), "{s:?} should be rejected");
        }
    }

    #[test]
    fn hotkey_control_value_roundtrip() {
        let h = parse("Ctrl+Alt+Shift+F9").unwrap();
        let v = to_control_value(&h);
        assert_eq!(v & 0xFF, 0x78); // VK_F9
        assert_eq!(v >> 8, 0x1 | 0x2 | 0x4); // HOTKEYF_SHIFT | CONTROL | ALT
        assert_eq!(from_control_value(v), Some(h));
        // The native control cannot express Win: it is dropped.
        assert_eq!(to_control_value(&parse("Win+M").unwrap()) >> 8, 0);
        assert_eq!(from_control_value(0), None);
    }

    #[test]
    fn hotkey_conflict_is_reported() {
        unsafe fn msg_window() -> HWND {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("t"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
            .unwrap()
        }
        let h = parse("Ctrl+Alt+Shift+F20").unwrap();
        unsafe {
            let (a, b) = (msg_window(), msg_window());
            assert_eq!(register(a, 1, &h), Ok(()));
            assert_eq!(register(b, 1, &h), Err(HotkeyError::Taken));
            unregister(a, 1);
            assert_eq!(register(b, 1, &h), Ok(()));
            unregister(b, 1);
            let _ = DestroyWindow(a);
            let _ = DestroyWindow(b);
        }
        assert_eq!(describe(&HotkeyError::Taken), "skrót jest już zajęty przez inny program");
        assert_eq!(describe(&HotkeyError::Invalid), "niepoprawny skrót");
    }
}
