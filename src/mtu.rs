//! MTU normalization.
//!
//! Sets every physical adapter's IPv4 MTU to a uniform, safe value. A
//! fragmented or unusually large MTU (jumbo frames, strange VPN clients)
//! increases latency spikes; standardizing on a common value (typically 1500,
//! or 1400 for networks that add encapsulation overhead) removes that class
//! of jitter.
//!
//! Each adapter's MTU lives in the registry (`MTU` DWORD under the interface's
//! `Tcpip\Parameters\Interfaces\{GUID}` key) and is what netsh persists with
//! `store=persistent`. The original values are archived on the first apply
//! and restored byte-for-byte on revert, so the mechanism is identical to the
//! other registry tweaks.

use crate::config::{Archive, GROUP_MTU};
use crate::{interfaces, log, netsh, registry};

const IFACES_SUBKEY: &str = r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces";
/// Smallest MTU the program will write (IPv6's minimum is 1280).
const MIN_MTU: u32 = 1280;
/// Largest sane MTU for a physical ethernet link.
const MAX_MTU: u32 = 9000;

fn iface_subkey(guid: &str) -> String {
    format!(r"{IFACES_SUBKEY}\{guid}")
}

fn guid_from_subkey(subkey: &str) -> Option<&str> {
    subkey.rsplit('\\').next()
}

/// Applies the target MTU to every applicable physical adapter, archiving
/// each adapter's current value first.
pub fn apply(archive: &mut Archive, target: u32) -> Result<(), String> {
    if !(MIN_MTU..=MAX_MTU).contains(&target) {
        return Err(format!(
            "invalid MTU {target}: expected {MIN_MTU}..={MAX_MTU}"
        ));
    }
    let mut dirty = false;
    let mut errors = Vec::new();
    for iface in interfaces::physical() {
        let subkey = iface_subkey(&iface.guid);
        if !registry::key_exists(&subkey) {
            continue;
        }
        let current = registry::read_dword(&subkey, "MTU")
            .map_err(|error| format!("read {subkey}\\MTU: {error}"))?;
        dirty |= archive.ensure(GROUP_MTU, &subkey, "MTU", current);
        if let Err(error) = registry::write_dword(&subkey, "MTU", target) {
            errors.push(format!("{}: write MTU: {error}", iface.friendly));
            continue;
        }
        if let Err(error) = set_live_mtu(&iface, target, true) {
            errors.push(format!("{}: {error}", iface.friendly));
        }
    }
    if dirty {
        crate::config::save_archive(archive).map_err(|e| e.to_string())?;
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Restores the archived original MTU per adapter. Adapters whose original
/// DWORD existed get it written back persistently; adapters without one get
/// the default 1500 back *live only* (`store=active`) so the registry stays
/// exactly as pristine as before the program touched it.
pub fn revert(archive: &mut Archive) -> Result<(), String> {
    let entries = archive
        .values
        .iter()
        .filter(|entry| entry.group == GROUP_MTU)
        .cloned()
        .collect::<Vec<_>>();
    let result = archive.restore_group(GROUP_MTU);
    crate::config::save_archive(archive).map_err(|e| e.to_string())?;

    let ifaces = interfaces::physical();
    for entry in entries {
        let Some(guid) = guid_from_subkey(&entry.subkey) else {
            continue;
        };
        let Some(iface) = ifaces.iter().find(|iface| iface.guid == guid) else {
            continue;
        };
        let (value, persistent) = if entry.present {
            (entry.value, true)
        } else {
            (1500, false)
        };
        if let Err(error) = set_live_mtu(iface, value, persistent) {
            crate::log::log(&format!(
                "mtu: live restore of {} on {} failed: {error}",
                value, iface.friendly
            ));
        }
    }
    result.map(|_| ()).map_err(|e| e.to_string())
}

/// True when every applicable adapter currently reports `target`.
pub fn status(target: u32) -> bool {
    interfaces::physical().iter().all(|iface| {
        let subkey = iface_subkey(&iface.guid);
        if !registry::key_exists(&subkey) {
            return true;
        }
        match registry::read_dword(&subkey, "MTU") {
            Ok(Some(current)) => current == target,
            // Missing registry value means the link default, which is 1500.
            Ok(None) => target == 1500,
            Err(_) => false,
        }
    })
}

fn set_live_mtu(iface: &interfaces::Interface, mtu: u32, persistent: bool) -> Result<(), String> {
    let name_arg = netsh::name_arg(&iface.friendly)?;
    let mtu_arg = format!("mtu={mtu}");
    let store_arg = if persistent {
        "store=persistent"
    } else {
        "store=active"
    };
    log::log(&format!("mtu: setting {} to {mtu}", iface.friendly));
    netsh::run(&[
        "interface",
        "ipv4",
        "set",
        "subinterface",
        &name_arg,
        &mtu_arg,
        store_arg,
    ])
    .map_err(|error| error.to_string())
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_is_subkey_suffix() {
        assert_eq!(
            guid_from_subkey(
                r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\{abc}"
            ),
            Some("{abc}")
        );
    }

    #[test]
    fn apply_rejects_out_of_range() {
        let mut archive = Archive::default();
        assert!(apply(&mut archive, 60).is_err());
        assert!(apply(&mut archive, 20000).is_err());
    }

    /// End-to-end registry/netsh smoke test. Requires an elevated shell and
    /// briefly sets the MTU of every physical adapter; the archived original
    /// is restored right afterwards.
    #[test]
    #[ignore]
    fn apply_revert_roundtrip() {
        let mut archive = Archive::default();
        apply(&mut archive, 1400).expect("apply should succeed");
        assert!(status(1400), "every adapter must report 1400");

        revert(&mut archive).expect("revert should succeed");
        assert!(
            archive.values.iter().all(|entry| entry.group != GROUP_MTU),
            "archive must be drained for the mtu group"
        );
        assert!(status(1500), "back to a 1500-default view over the links");
    }
}
