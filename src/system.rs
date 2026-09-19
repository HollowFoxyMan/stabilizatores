//! Multimedia network throttling removal.
//!
//! Windows reserves a configurable amount of network bandwidth for the
//! Multimedia Class Scheduler Service. Setting `NetworkThrottlingIndex` to
//! `0xFFFFFFFF` raises that quota to the maximum, which removes artificial
//! bandwidth limiting that can otherwise interfere with gaming traffic.

use crate::config::{Archive, GROUP_SYSTEM};
use crate::registry;

const SUBKEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Multimedia\SystemProfile";
const NAME: &str = "NetworkThrottlingIndex";

/// Maximum (unthrottled) index value.
const UNTHROTTLED: u32 = 0xFFFF_FFFF;

fn read() -> Result<Option<u32>, String> {
    registry::read_dword(SUBKEY, NAME).map_err(|error| format!("read {SUBKEY}\\{NAME}: {error}"))
}

fn write(value: u32) -> Result<(), String> {
    registry::write_dword(SUBKEY, NAME, value)
        .map_err(|error| format!("write {SUBKEY}\\{NAME}: {error}"))
}

/// Removes the network throttling, archiving the original value first.
pub fn apply(archive: &mut Archive) -> Result<(), String> {
    let current = read()?;
    if archive.ensure(GROUP_SYSTEM, SUBKEY, NAME, current) {
        crate::config::save_archive(archive).map_err(|e| e.to_string())?;
    }
    write(UNTHROTTLED)
}

/// Restores the archived original throttling value.
pub fn revert(archive: &mut Archive) -> Result<(), String> {
    let result = archive.restore_group(GROUP_SYSTEM);
    crate::config::save_archive(archive).map_err(|e| e.to_string())?;
    result.map(|_| ()).map_err(|e| e.to_string())
}

/// True when the throttling index is set to the maximum.
pub fn status() -> bool {
    matches!(read(), Ok(Some(value)) if value == UNTHROTTLED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unthrottled_value_is_max() {
        assert_eq!(UNTHROTTLED, u32::MAX);
    }

    /// End-to-end registry smoke test. Requires an elevated shell and writes
    /// one registry key briefly; the original is restored afterwards.
    #[test]
    #[ignore]
    fn apply_revert_roundtrip() {
        use crate::config::Archive;

        let original = read().expect("read current value");

        let mut archive = Archive::default();
        apply(&mut archive).expect("apply should succeed");
        assert!(status(), "throttling must read as removed after apply");

        revert(&mut archive).expect("revert should succeed");
        assert!(archive.values.is_empty(), "archive must be drained");
        assert_eq!(
            read().expect("read current value"),
            original,
            "original value must be restored"
        );
    }
}
