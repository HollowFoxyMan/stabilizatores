//! Thin wrappers around the Windows API used by the rest of the program.
//!
//! Covers elevation, the console (VT output, raw keyboard input and clean
//! restore on drop), the console title, the Ctrl+C handler, autostart in the
//! registry, and DNS cache flushing.

use std::ffi::c_void;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::System::Console::{
    FlushConsoleInputBuffer, GetConsoleMode, GetConsoleTitleW, GetStdHandle, PeekConsoleInputW,
    ReadConsoleInputW, SetConsoleCtrlHandler, SetConsoleMode, SetConsoleTitleW,
    ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, INPUT_RECORD, KEY_EVENT,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Elevated when the shell executes the runas verb?
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "stabilizatores";

/// Set by the console control handler when Ctrl+C / Ctrl+Break is pressed.
static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn exit_requested() -> bool {
    EXIT_REQUESTED.load(Ordering::SeqCst)
}

/// Returns true when the current process runs with an elevated token.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut length: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut length,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Re-launches the current executable elevated via ShellExecuteW("runas").
///
/// Returns true when the elevated copy was started. The original values read
/// from `std::env::args` are forwarded so the child sees the same arguments.
pub fn relaunch_elevated() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut command = String::new();
    for (index, arg) in std::env::args().enumerate() {
        if index == 0 {
            continue;
        }
        if !command.is_empty() {
            command.push(' ');
        }
        command.push('"');
        command.push_str(&arg.replace('"', "\"\""));
        command.push('"');
    }
    let file = wide(&exe.to_string_lossy());
    let operation = wide("runas");
    // Bind the wide arguments so the buffers live until ShellExecuteW returns.
    let params = if command.is_empty() {
        None
    } else {
        Some(wide(&command))
    };
    let parameters = params
        .as_ref()
        .map_or(std::ptr::null(), |buffer| buffer.as_ptr().cast::<u16>());
    unsafe {
        let result = ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            parameters,
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
        !result.is_null() && result as isize > 32
    }
}

/// Installs a Ctrl+C / Ctrl+Break handler that only flags a clean exit
/// instead of terminating the process immediately.
pub fn install_ctrl_handler() -> bool {
    unsafe extern "system" fn handler(_event: u32) -> i32 {
        EXIT_REQUESTED.store(true, Ordering::SeqCst);
        1
    }
    unsafe { SetConsoleCtrlHandler(Some(handler), 1) != 0 }
}

/// RAII guard that owns the console while the program runs.
///
/// Captures the original input and output modes and restores them when
/// dropped, so a parent console is left exactly as it was. Exposes Virtual
/// Terminal processing (output) and raw key reading (input).
pub struct Console {
    input: HANDLE,
    input_mode: u32,
    output: HANDLE,
    output_mode: u32,
    title: Vec<u16>,
    /// Virtual Terminal processing was enabled on the output.
    pub vt: bool,
}

impl Console {
    /// Enables VT output and raw input on the standard handles, remembering
    /// the previous modes and window title. Returns `None` when either
    /// standard stream is not an interactive console (for example when
    /// output is piped).
    pub fn setup() -> Option<Console> {
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            if input == INVALID_HANDLE_VALUE || output == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut input_mode: u32 = 0;
            let mut output_mode: u32 = 0;
            if GetConsoleMode(input, &mut input_mode) == 0
                || GetConsoleMode(output, &mut output_mode) == 0
            {
                return None;
            }
            let vt = SetConsoleMode(output, output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0;
            SetConsoleMode(input, ENABLE_PROCESSED_INPUT);
            FlushConsoleInputBuffer(input);

            let mut title = vec![0u16; 256];
            let written = GetConsoleTitleW(title.as_mut_ptr(), title.len() as u32) as usize;
            title.truncate(written.min(title.len()));

            Some(Console {
                input,
                input_mode,
                output,
                output_mode,
                title,
                vt,
            })
        }
    }

    /// Reads a single key event, if any is pending. Returns `None` when the
    /// queue is empty, so callers can continue doing other work in between.
    pub fn poll_key(&self) -> Option<char> {
        unsafe {
            let mut pending: u32 = 0;
            let mut record: INPUT_RECORD = std::mem::zeroed();
            if PeekConsoleInputW(self.input, &mut record, 1, &mut pending) == 0 || pending == 0 {
                return None;
            }
            let mut read: u32 = 0;
            if ReadConsoleInputW(self.input, &mut record, 1, &mut read) == 0 || read == 0 {
                return None;
            }
            if u32::from(record.EventType) != KEY_EVENT {
                return None;
            }
            let key = record.Event.KeyEvent;
            if key.bKeyDown == 0 {
                return None;
            }
            let code = key.uChar.UnicodeChar;
            if code == 0 {
                return None;
            }
            char::from_u32(code as u32)
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.input, self.input_mode);
            SetConsoleMode(self.output, self.output_mode);
            if !self.title.is_empty() {
                SetConsoleTitleW(self.title.as_ptr());
            }
        }
    }
}

/// Sets the console window title.
pub fn set_title(title: &str) {
    unsafe {
        SetConsoleTitleW(wide(title).as_ptr());
    }
}

/// Flushes the DNS resolver cache (equivalent of `ipconfig /flushdns`).
pub fn flush_dns_cache() {
    unsafe {
        DnsFlushResolverCache();
    }
}

/// Sets or clears the "run on login" value in HKCU\...\Run.
pub fn set_autostart(enabled: bool) -> io::Result<()> {
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            &mut key,
        ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        let result = if enabled {
            match std::env::current_exe() {
                Ok(exe) => {
                    let data = wide(&exe.to_string_lossy());
                    RegSetValueExW(
                        key,
                        wide(RUN_VALUE).as_ptr(),
                        0,
                        REG_SZ,
                        data.as_ptr() as *const u8,
                        (data.len() * 2) as u32,
                    )
                }
                Err(_) => {
                    RegCloseKey(key);
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "cannot resolve the executable path",
                    ));
                }
            }
        } else {
            RegDeleteValueW(key, wide(RUN_VALUE).as_ptr())
        };
        RegCloseKey(key);
        if result != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

/// Returns true when the "run on login" value is present.
pub fn autostart_enabled() -> bool {
    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        let ok = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        );
        if ok != 0 {
            return false;
        }
        let exists = RegQueryValueExW(
            key,
            wide(RUN_VALUE).as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) == 0;
        RegCloseKey(key);
        exists
    }
}

/// Reads a plain text config file (UTF-8) from the given path.
pub fn app_data_dir() -> io::Result<PathBuf> {
    let base = std::env::var("APPDATA")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "APPDATA is not set"))?;
    Ok(Path::new(&base).join("stabilizatores"))
}

/// Widens a string into a NUL-terminated UTF-16 buffer.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Compact `YYYY-MM-DD-HHMMSS` timestamp for filenames, derived from the Unix
/// clock (leap-second naive is fine for snapshots and reports).
pub fn timestamp_compact() -> String {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = unix / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let rem = unix % 86_400;
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{year:04}-{month:02}-{day:02}-{hour:02}{minute:02}{second:02}")
}

/// Howard Hinnant's civil-from-days algorithm (public domain).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[link(name = "dnsapi")]
unsafe extern "system" {
    fn DnsFlushResolverCache() -> i32;
}

use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_terminates_with_nul() {
        let buf = wide("abc");
        assert_eq!(buf, vec![b'a' as u16, b'b' as u16, b'c' as u16, 0]);
    }

    #[test]
    fn wide_handles_non_ascii() {
        let mut buf = wide("\u{0441}\u{0442}аб");
        assert_eq!(buf.pop(), Some(0));
        assert_ne!(buf[0], 0);
    }

    #[test]
    #[ignore]
    fn autostart_roundtrip() {
        let was = autostart_enabled();
        set_autostart(!was).expect("setting autostart should work");
        assert_eq!(autostart_enabled(), !was);
        set_autostart(was).expect("restoring autostart should work");
    }
}
