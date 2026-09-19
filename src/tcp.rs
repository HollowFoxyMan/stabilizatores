//! TCP/IP parameter tuning.
//!
//! Two families of registry values are handled:
//!
//! * **machine-wide** values under `Tcpip\Parameters` — the TIME-WAIT timer
//!   and the ephemeral port ceiling;
//! * **per-interface** values under `Tcpip\Parameters\Interfaces\{GUID}` —
//!   Nagle's algorithm, delayed ACK frequency and delayed ACK timing.
//!
//! The interface GUIDs come from the adapter enumeration and only adapters
//! that carry a real `Tcpip` registry key are counted as applicable.

use crate::config::{Archive, GROUP_TCP};
use crate::{interfaces, registry};

/// Machine-wide TCP parameters subkey.
pub const GLOBAL_SUBKEY: &str = r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters";

/// Per-adapter parameters subkey.
const IFACES_SUBKEY: &str = r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces";

/// Machine-wide values written by `apply`.
const GLOBAL_VALUES: &[(&str, u32)] = &[
    // TIME-WAIT delay in seconds: 240 (default) -> 30.
    ("TcpTimedWaitDelay", 30),
    // Highest ephemeral port: 16384 (default) -> 65534.
    ("MaxUserPort", 65534),
];

/// Per-interface values written by `apply`.
const IFACE_VALUES: &[(&str, u32)] = &[
    // Disable Nagle's algorithm for immediate small-packet delivery.
    ("TCPNoDelay", 1),
    // Ack every segment instead of batching.
    ("TcpAckFrequency", 1),
    // Do not delay DELAY-ACK ticks.
    ("TcpDelAckTicks", 0),
];

/// State of the TCP tuning across the machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpStatus {
    /// Number of applicable interfaces that are fully tuned.
    pub ifaces_ok: usize,
    /// Number of applicable interfaces (those with a `Tcpip` key).
    pub ifaces_total: usize,
    /// Whether all machine-wide values match the target.
    pub global_ok: bool,
}

impl TcpStatus {
    /// True when there is nothing left to tune.
    pub fn aligned(&self) -> bool {
        self.global_ok && (self.ifaces_total == 0 || self.ifaces_ok == self.ifaces_total)
    }
}

fn iface_subkey(guid: &str) -> String {
    format!(r"{IFACES_SUBKEY}\{guid}")
}

fn read_opt(subkey: &str, name: &str) -> Result<Option<u32>, String> {
    registry::read_dword(subkey, name).map_err(|error| format!("read {subkey}\\{name}: {error}"))
}

/// Applies all TCP optimizations, archiving the original values first.
pub fn apply(archive: &mut Archive) -> Result<TcpStatus, String> {
    let mut dirty = false;
    let mut write_errors: Vec<String> = Vec::new();

    for (name, value) in GLOBAL_VALUES {
        let current = read_opt(GLOBAL_SUBKEY, name)?;
        dirty |= archive.ensure(GROUP_TCP, GLOBAL_SUBKEY, name, current);
        if let Err(error) = registry::write_dword(GLOBAL_SUBKEY, name, *value) {
            write_errors.push(format!("write {GLOBAL_SUBKEY}\\{name}: {error}"));
        }
    }

    let mut status = TcpStatus {
        global_ok: GLOBAL_VALUES.iter().all(
            |(name, value)| matches!(read_opt(GLOBAL_SUBKEY, name), Ok(Some(v)) if v == *value),
        ),
        ifaces_ok: 0,
        ifaces_total: 0,
    };

    for iface in interfaces::physical() {
        let subkey = iface_subkey(&iface.guid);
        if !registry::key_exists(&subkey) {
            continue;
        }
        status.ifaces_total += 1;
        let mut ok = true;
        for (name, value) in IFACE_VALUES {
            let current = read_opt(&subkey, name)?;
            dirty |= archive.ensure(GROUP_TCP, &subkey, name, current);
            if let Err(error) = registry::write_dword(&subkey, name, *value) {
                ok = false;
                write_errors.push(format!("write {subkey}\\{name}: {error}"));
            }
        }
        if ok {
            status.ifaces_ok += 1;
        }
    }

    if dirty {
        crate::config::save_archive(archive).map_err(|e| e.to_string())?;
    }
    if let Some(first) = write_errors.into_iter().next() {
        return Err(first);
    }
    Ok(status)
}

/// Restores the archived original values for the TCP group.
pub fn revert(archive: &mut Archive) -> Result<(), String> {
    let result = archive.restore_group(GROUP_TCP);
    crate::config::save_archive(archive).map_err(|e| e.to_string())?;
    result.map(|_| ()).map_err(|e| e.to_string())
}

/// Reads the tunables back from the registry (no side effects).
pub fn status() -> TcpStatus {
    let mut status = TcpStatus {
        global_ok: GLOBAL_VALUES.iter().all(
            |(name, value)| matches!(read_opt(GLOBAL_SUBKEY, name), Ok(Some(v)) if v == *value),
        ),
        ifaces_ok: 0,
        ifaces_total: 0,
    };
    for iface in interfaces::physical() {
        let subkey = iface_subkey(&iface.guid);
        if !registry::key_exists(&subkey) {
            continue;
        }
        status.ifaces_total += 1;
        if IFACE_VALUES
            .iter()
            .all(|(name, value)| matches!(read_opt(&subkey, name), Ok(Some(v)) if v == *value))
        {
            status.ifaces_ok += 1;
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_false_when_global_missing() {
        assert!(!TcpStatus {
            global_ok: false,
            ..TcpStatus::default()
        }
        .aligned());
    }

    #[test]
    fn aligned_true_when_no_interfaces() {
        assert!(TcpStatus {
            global_ok: true,
            ifaces_total: 0,
            ..TcpStatus::default()
        }
        .aligned());
    }

    #[test]
    fn aligned_requires_all_ifaces() {
        let status = TcpStatus {
            global_ok: true,
            ifaces_ok: 1,
            ifaces_total: 2,
        };
        assert!(!status.aligned());
        let status = TcpStatus {
            global_ok: true,
            ifaces_ok: 2,
            ifaces_total: 2,
        };
        assert!(status.aligned());
    }

    #[test]
    fn iface_subkey_shape() {
        assert_eq!(
            iface_subkey("{abc}"),
            r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\{abc}"
        );
    }

    /// End-to-end registry smoke test. Requires an elevated shell and writes
    /// real registry keys briefly; the originals are restored afterwards.
    #[test]
    #[ignore]
    fn apply_revert_roundtrip() {
        use crate::config::Archive;

        let original = GLOBAL_VALUES
            .iter()
            .map(|(name, _)| {
                (
                    name,
                    registry::read_dword(GLOBAL_SUBKEY, name).expect("read global value"),
                )
            })
            .collect::<Vec<_>>();

        let mut archive = Archive::default();
        let status = apply(&mut archive).expect("apply should succeed");
        assert!(status.aligned(), "everything must be tuned: {status:?}");
        for (name, _) in GLOBAL_VALUES {
            let restored = registry::read_dword(GLOBAL_SUBKEY, name)
                .expect("read global value")
                .expect("value must exist after apply");
            assert_eq!(
                restored,
                GLOBAL_VALUES.iter().find(|(n, _)| *n == *name).unwrap().1
            );
        }

        revert(&mut archive).expect("revert should succeed");
        assert!(
            archive.values.is_empty(),
            "archive must be drained after revert"
        );
        for (name, before) in original {
            let after = registry::read_dword(GLOBAL_SUBKEY, name).expect("read global value");
            assert_eq!(after, before, "original value of {name} must be restored");
        }
    }
}
