//! Start with Windows (HKCU Run key) and UAC elevation helpers.
use std::io;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use super::wide::wide;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "komunikator";

/// Adds or removes the Run entry: quoted exe path plus ` --minimized`.
pub fn set_autostart(enabled: bool) -> io::Result<()> {
    let key = wide(RUN_KEY);
    let name = wide(VALUE);
    let err = unsafe {
        if enabled {
            let exe = std::env::current_exe()?;
            let cmd = wide(&format!("\"{}\" --minimized", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                REG_SZ.0,
                Some(cmd.as_ptr().cast()),
                (cmd.len() * 2) as u32,
            )
        } else {
            let e = RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()));
            if e == ERROR_FILE_NOT_FOUND {
                ERROR_SUCCESS
            } else {
                e
            }
        }
    };
    if err == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(err.0 as i32))
    }
}

pub fn autostart_enabled() -> bool {
    let key = wide(RUN_KEY);
    let name = wide(VALUE);
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        ) == ERROR_SUCCESS
    }
}

/// True when the process runs with an elevated (administrator) token. Games running as
/// admin swallow RegisterHotKey presses from a non-elevated process, hence the check.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elev as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

/// Starts a new elevated instance with the same arguments (UAC prompt). The caller exits
/// when this returns true.
pub fn relaunch_as_admin() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let args = join_args(std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()));
    let (exe, args) = (wide(&exe.to_string_lossy()), wide(&args));
    let r = unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("runas"),
            PCWSTR(exe.as_ptr()),
            PCWSTR(args.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    r.0 as isize > 32
}

/// One command line from arguments, each quoted so `CommandLineToArgvW` gives it back intact.
fn join_args<S: AsRef<str>>(args: impl IntoIterator<Item = S>) -> String {
    args.into_iter().map(|a| quote_arg(a.as_ref())).collect::<Vec<_>>().join(" ")
}

/// Backslashes are literal unless they precede a quote, so those (and the ones before the
/// closing quote) are doubled and embedded quotes escaped.
fn quote_arg(a: &str) -> String {
    const BACKSLASH: char = '\\';
    if !a.is_empty() && !a.contains([' ', '\t', '\n', '\x0b', '"']) {
        return a.to_owned();
    }
    let mut out = String::from('"');
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            BACKSLASH => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n(BACKSLASH, backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_for_command_line_to_argv() {
        let cases = [
            (r"--minimized", r"--minimized"),
            (r"C:\dir\", r"C:\dir\"),
            (r"C:\x y\", r#""C:\x y\\""#),
            (r#"C:\a"b"#, r#""C:\a\"b""#),
            (r#"x\"y z"#, r#""x\\\"y z""#),
            (r"tab	here", "\"tab\there\""),
            ("", r#""""#),
        ];
        for (arg, want) in cases {
            assert_eq!(quote_arg(arg), want, "{arg:?}");
        }
        assert_eq!(join_args([r"--data-dir", r"C:\x y\", "--minimized"]), r#"--data-dir "C:\x y\\" --minimized"#);
    }
}
