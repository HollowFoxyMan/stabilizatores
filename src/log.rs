//! Append-only action log.
//!
//! Every apply / revert / self-heal / best-effort side step (DoH, DNS flush)
//! is recorded with a local timestamp in
//! `%APPDATA%\stabilizatores\logs\stabilizatores.log`. The file grows up to
//! 1 MiB and is then rotated to `stabilizatores.log.1`. Logging never fails
//! the caller: write errors are silently dropped.

use std::fs::{self, OpenOptions};
use std::io::Write;

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

/// Maximum size before the log rotates.
const MAX_BYTES: u64 = 1 << 20;

/// Appends one line with a local timestamp. Best effort.
pub fn log(line: &str) {
    let Ok(dir) = crate::win::app_data_dir() else {
        return;
    };
    let logs = dir.join("logs");
    if fs::create_dir_all(&logs).is_err() {
        return;
    }
    let path = logs.join("stabilizatores.log");
    let rotating = fs::metadata(&path)
        .map(|meta| meta.len() > MAX_BYTES)
        .unwrap_or(false);
    if rotating {
        let _ = fs::rename(&path, logs.join("stabilizatores.log.1"));
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = writeln!(file, "[{}] {line}", now());
}

/// Reads the `limit` most recent log lines, oldest first. Missing or empty
/// logs yield an empty list; unlike [`log`] this can fail only on I/O beyond
/// our control and is therefore best-effort too.
pub fn read(limit: usize) -> Vec<String> {
    let Ok(dir) = crate::win::app_data_dir() else {
        return Vec::new();
    };
    let path = dir.join("logs").join("stabilizatores.log");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines = text
        .lines()
        .rev()
        .take(limit)
        .map(str::to_string)
        .collect::<Vec<_>>();
    lines.into_iter().rev().collect()
}

fn now() -> String {
    unsafe {
        let mut time: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut time);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_has_expected_shape() {
        let stamp = now();
        assert_eq!(stamp.len(), 19, "stamp is YYYY-MM-DD HH:MM:SS: {stamp}");
    }
}
