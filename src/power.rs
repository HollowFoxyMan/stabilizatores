//! Network-adapter power-management control.
//!
//! Windows lets a NIC "turn itself off to save power" while idle, which adds
//! wake-up latency and occasionally drops the link. The switch lives next to
//! the adapter's driver key under `Control\Class\{...}` as the
//! `PnPCapabilities` DWORD; bit `0x10` means "do not allow the computer to
//! turn off this device". The tweak sets that bit (plus `0x08`, the bit most
//! drivers pair with it) so the adapter stops powering down, archives the
//! original value and restores it exactly on revert.

use crate::config::{Archive, GROUP_POWER};
use crate::{config, log, registry};

const CLASS_SUBKEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4d36e972-e325-11ce-bfc1-08002be10318}";
const VALUE: &str = "PnPCapabilities";
/// Instance GUID value that marks a driver node as a NIC.
const NODE_GUID: &str = "NetCfgInstanceId";
/// Bit meaning "the computer is not allowed to turn off this device".
const POWER_BIT: u32 = 0x10;
/// `POWER_BIT` plus the companion bit most NIC drivers require together.
const APPLIED_BITS: u32 = 0x18;

/// One NIC driver node and its current power-management value.
#[derive(Clone, Debug)]
pub struct Nic {
    /// Full registry subkey of the node.
    pub subkey: String,
    /// Adapter GUID from `NetCfgInstanceId`.
    pub guid: String,
    /// Driver description (`DriverDesc`).
    pub description: String,
    /// Current `PnPCapabilities` value, if present.
    pub value: Option<u32>,
}

/// Enumerates the NIC driver nodes. Errors are skipped so a single broken
/// node cannot poison the whole group.
pub fn nics() -> Vec<Nic> {
    let mut nodes = Vec::new();
    let Ok(names) = registry::read_names(CLASS_SUBKEY) else {
        return nodes;
    };
    for name in names {
        let subkey = format!(r"{CLASS_SUBKEY}\{name}");
        let Ok(guid) = registry::read_string(&subkey, NODE_GUID) else {
            continue;
        };
        let Some(guid) = guid else { continue };
        let description = registry::read_string(&subkey, "DriverDesc")
            .ok()
            .flatten()
            .unwrap_or_default();
        let value = registry::read_dword(&subkey, VALUE).ok().flatten();
        nodes.push(Nic {
            subkey,
            guid,
            description,
            value,
        });
    }
    nodes
}

/// Applies the "no power-down" flags to every NIC, archiving each node's
/// original value first.
pub fn apply(archive: &mut Archive) -> Result<(), String> {
    let mut dirty = false;
    let mut errors = Vec::new();
    for nic in nics() {
        let current = registry::read_dword(&nic.subkey, VALUE)
            .map_err(|error| format!("read {}: {error}", nic.description))?;
        dirty |= archive.ensure(GROUP_POWER, &nic.subkey, VALUE, current);
        let target = current.unwrap_or(0) | APPLIED_BITS;
        if let Err(error) = registry::write_dword(&nic.subkey, VALUE, target) {
            errors.push(format!("{}: write {VALUE}: {error}", nic.description));
        }
        log::log(&format!(
            "power: {} ({}) {VALUE} -> {target:#x}",
            nic.description, nic.guid
        ));
    }
    if dirty {
        config::save_archive(archive).map_err(|e| e.to_string())?;
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Restores the archived original values.
pub fn revert(archive: &mut Archive) -> Result<(), String> {
    let result = archive.restore_group(GROUP_POWER);
    config::save_archive(archive).map_err(|e| e.to_string())?;
    result.map(|_| ()).map_err(|e| e.to_string())
}

/// True when every NIC already disallows power-down. A machine without NIC
/// nodes counts as aligned.
pub fn status() -> bool {
    let nodes = nics();
    if nodes.is_empty() {
        return true;
    }
    all_disallow(&nodes)
}

fn all_disallow(nodes: &[Nic]) -> bool {
    nodes
        .iter()
        .all(|nic| nic.value.is_some_and(|value| value & POWER_BIT != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic(value: Option<u32>) -> Nic {
        Nic {
            subkey: "k".into(),
            guid: "{x}".into(),
            description: "NIC".into(),
            value,
        }
    }

    #[test]
    fn all_must_disallow_power_down() {
        assert!(all_disallow(&[nic(Some(0x18)), nic(Some(0x10))]));
        assert!(!all_disallow(&[nic(Some(0x18)), nic(None)]));
        assert!(!all_disallow(&[nic(Some(0x18)), nic(Some(0x00))]));
    }

    #[test]
    fn empty_node_list_is_aligned() {
        assert!(all_disallow(&[]));
    }

    #[test]
    fn applied_bits_include_power_bit() {
        assert_ne!(APPLIED_BITS & POWER_BIT, 0);
    }

    /// End-to-end registry smoke test. Requires an elevated shell; applies the
    /// no-power-down flags to every NIC and restores the original values.
    #[test]
    #[ignore]
    fn apply_revert_roundtrip() {
        let before = nics()
            .into_iter()
            .map(|nic| (nic.subkey, nic.value))
            .collect::<Vec<_>>();
        let mut archive = Archive::default();
        apply(&mut archive).expect("apply should succeed");
        assert!(status(), "every NIC must disallow power-down after apply");

        revert(&mut archive).expect("revert should succeed");
        assert!(
            archive
                .values
                .iter()
                .all(|entry| entry.group != GROUP_POWER),
            "archive must be drained for the power group"
        );
        let after = nics()
            .into_iter()
            .map(|nic| (nic.subkey, nic.value))
            .collect::<Vec<_>>();
        assert_eq!(before, after, "values must be restored exactly");
    }
}
