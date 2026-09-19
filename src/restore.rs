//! System Restore point creation.
//!
//! `CREATERESTOREPOINT` asks the Volume Shadow Copy service to snapshot the
//! system state through `srclient.dll`. The DLL is loaded dynamically (it is
//! not linked at build time) and `SRSetRestorePointW` is called with a
//! single pair of events: `BEGIN_SYSTEM_CHANGE` immediately followed by
//! `END_SYSTEM_CHANGE`, which is the supported way to create a point without
//! holding a transaction. Creating a point requires System Restore to be
//! enabled; when it is not, the call fails with a well-known status.

use windows_sys::Win32::Foundation::FreeLibrary;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const SRCLIENT: &str = "srclient.dll";
const EXPORT: &[u8] = b"SRSetRestorePointW\0";

/// Event type: we are starting a system change.
const BEGIN_SYSTEM_CHANGE: u32 = 100;
/// Event type: the change is finished and can be snapshotted.
const END_SYSTEM_CHANGE: u32 = 101;
/// Restore point type: the change modified system settings.
const MODIFY_SETTINGS: u32 = 12;
/// Description buffer length, matches the Windows `MAX_DESC_W`.
const MAX_DESC: usize = 256;

#[repr(C)]
struct RestorePointInfo {
    event_type: u32,
    restore_point_type: u32,
    sequence_number: i64,
    description: [u16; MAX_DESC],
}

#[repr(C)]
struct StatManagerStatus {
    status: u32,
    sequence_number: i64,
}

type SrSetRestorePointW = unsafe extern "system" fn(
    outer_info: *mut RestorePointInfo,
    status: *mut StatManagerStatus,
) -> i32;

/// Creates a restore point named `description`. Errors carry a readable reason
/// (restore disabled, disk full, …).
pub fn create(description: &str) -> Result<(), String> {
    let library = unsafe {
        let path = win32_wide(SRCLIENT);
        LoadLibraryW(path.as_ptr())
    };
    if library.is_null() {
        return Err(format!(
            "cannot load {}: error {}",
            SRCLIENT,
            std::io::Error::last_os_error()
        ));
    }
    let proc = unsafe { GetProcAddress(library, EXPORT.as_ptr().cast()) };
    if proc.is_none() {
        unsafe { FreeLibrary(library) };
        return Err("srclient.dll does not export SRSetRestorePointW".into());
    }
    // SAFETY: the export signature was matched above; the structs stay alive
    // for the whole call, and the sequence begins and ends in the same call.
    unsafe {
        let function: SrSetRestorePointW = std::mem::transmute(proc);
        let mut info = RestorePointInfo {
            event_type: BEGIN_SYSTEM_CHANGE,
            restore_point_type: MODIFY_SETTINGS,
            sequence_number: 0,
            description: [0u16; MAX_DESC],
        };
        fill_description(&mut info.description, description);
        let mut status = StatManagerStatus {
            status: 0,
            sequence_number: 0,
        };
        let started = function(&mut info, &mut status);
        if started == 0 {
            FreeLibrary(library);
            return Err(status_message(status.status));
        }
        info.event_type = END_SYSTEM_CHANGE;
        info.sequence_number = status.sequence_number;
        let finished = function(&mut info, &mut status);
        FreeLibrary(library);
        if finished == 0 {
            return Err(status_message(status.status));
        }
        Ok(())
    }
}

fn win32_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn fill_description(buffer: &mut [u16; MAX_DESC], text: &str) {
    let units = text.encode_utf16().take(MAX_DESC.saturating_sub(1));
    for (slot, unit) in buffer.iter_mut().zip(units) {
        *slot = unit;
    }
}

fn status_message(status: u32) -> String {
    let message = match status {
        0 => "the restore point was created".into(),
        ERROR_BAD_ENVIRONMENT => "system restore is disabled".into(),
        ERROR_DISK_FULL => "not enough free disk space".into(),
        ERROR_VOLUME_TOO_SMALL => "the system volume is too small".into(),
        0xC0000022 => "access denied: are you running as administrator?".into(),
        other => format!("restore point creation failed (status {other:#x})"),
    };
    message
}

const ERROR_BAD_ENVIRONMENT: u32 = 10;
const ERROR_DISK_FULL: u32 = 112;
const ERROR_VOLUME_TOO_SMALL: u32 = 1211;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn description_is_padded_and_truncated() {
        let mut buffer = [0u16; MAX_DESC];
        fill_description(&mut buffer, "é");
        assert_eq!(buffer[0], 'é' as u16);
        assert_eq!(buffer[1], 0, "null terminator");

        let long = "x".repeat(MAX_DESC + 50);
        fill_description(&mut buffer, &long);
        assert_eq!(buffer[MAX_DESC - 1], 0, "kept room for the terminator");
        assert!(buffer[..MAX_DESC - 1]
            .iter()
            .all(|&unit| unit == 'x' as u16));
    }

    #[test]
    fn known_statuses_map_to_readable_text() {
        assert!(status_message(0).contains("created"));
        assert!(status_message(ERROR_BAD_ENVIRONMENT).contains("disabled"));
        assert!(status_message(ERROR_DISK_FULL).contains("space"));
        assert!(
            status_message(0xDEAD_BEEF).contains("0xdeadbeef"),
            "fallback keeps the code"
        );
    }
}
