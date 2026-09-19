//! DNS optimization.
//!
//! Switches every *active* physical adapter to a low-latency anycast resolver
//! set. The sets are pairs: Cloudflare (1.1.1.1 / 1.0.0.1, IPv6
//! 2606:4700:4700::1111 / ::1001) and Google (8.8.8.8 / 8.8.4.4, IPv6
//! 2001:4860:4860::8888 / ::8844). With [`config::DnsProvider::Auto`] the
//! program measures both with an ICMP probe and picks the current fastest.
//!
//! The change goes through `netsh`, which handles both DHCP-managed and
//! statically configured adapters. The previous DNS servers are archived per
//! adapter so a revert restores the exact prior configuration; adapters
//! without any recorded DNS servers fall back to DHCP (automatic).
//!
//! On Windows 11 the resolver is additionally upgraded to DNS-over-HTTPS via
//! `netsh dns add encryption` (best effort; probing that context once decides
//! whether the command exists at all, so Windows 10 never spams errors).

use crate::config::{Archive, DnsProvider};
use crate::interfaces::{self, Interface};
use crate::{log, netsh, probe};

/// IPv4/IPv6 resolver pair plus the DoH endpoint that serves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolvers {
    /// Which provider this set came from.
    pub provider: DnsProvider,
    /// Primary IPv4 resolver.
    pub primary: &'static str,
    /// Secondary IPv4 resolver.
    pub secondary: &'static str,
    /// Primary IPv6 resolver.
    pub primary_v6: &'static str,
    /// Secondary IPv6 resolver.
    pub secondary_v6: &'static str,
    /// DNS-over-HTTPS endpoint template for the primary/secondary pair.
    pub doh_template: &'static str,
}

/// Cloudflare anycast resolvers.
pub const CLOUDFLARE: Resolvers = Resolvers {
    provider: DnsProvider::Cloudflare,
    primary: "1.1.1.1",
    secondary: "1.0.0.1",
    primary_v6: "2606:4700:4700::1111",
    secondary_v6: "2606:4700:4700::1001",
    doh_template: "https://cloudflare-dns.com/dns-query",
};

/// Google public resolvers.
pub const GOOGLE: Resolvers = Resolvers {
    provider: DnsProvider::Google,
    primary: "8.8.8.8",
    secondary: "8.8.4.4",
    primary_v6: "2001:4860:4860::8888",
    secondary_v6: "2001:4860:4860::8844",
    doh_template: "https://dns.google/dns-query",
};

/// The resolver set `provider` asks for. [`DnsProvider::Auto`] compares the
/// two endpoints — the real DNS resolution time when measured, the ICMP RTT
/// otherwise — and prefers the faster pair; without any measurement (no
/// network yet) it falls back to Cloudflare.
pub fn resolve_provider(provider: DnsProvider, latency: Option<&probe::Latency>) -> Resolvers {
    match provider {
        DnsProvider::Cloudflare => CLOUDFLARE,
        DnsProvider::Google => GOOGLE,
        DnsProvider::Auto => {
            let Some(latency) = latency else {
                return CLOUDFLARE;
            };
            let cloudflare_ms = latency.cf_dns_ms.or_else(|| latency.cloudflare.rtt_ms());
            let google_ms = latency.google_dns_ms.or_else(|| latency.google.rtt_ms());
            let cloudflare_faster = match (cloudflare_ms, google_ms) {
                (Some(cloudflare), Some(google)) => cloudflare <= google,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => true,
            };
            if cloudflare_faster {
                CLOUDFLARE
            } else {
                GOOGLE
            }
        }
    }
}

/// Result of a DNS state scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DnsStatus {
    /// Active interfaces already pointing at the requested resolvers.
    pub aligned: usize,
    /// Active physical interfaces.
    pub total: usize,
}

impl DnsStatus {
    /// True when there is nothing left to tune.
    pub fn aligned_all(&self) -> bool {
        self.total == 0 || self.aligned == self.total
    }
}

/// Sets the requested resolvers on every active physical adapter, archiving
/// the previous DNS servers first.
pub fn apply(archive: &mut Archive, resolvers: &Resolvers) -> Result<(), String> {
    let mut errors = Vec::new();
    let mut dirty = false;
    for iface in interfaces::active() {
        if is_set(&iface, resolvers) {
            continue;
        }
        if archive.dns_ensure(
            &iface.friendly,
            iface.dhcp,
            iface.dns.clone(),
            iface.dns6.clone(),
        ) {
            dirty = true;
        }
        if let Err(error) = set_iface(&iface, resolvers).map(|()| set_doh(&iface, resolvers)) {
            errors.push(format!("{}: {error}", iface.friendly));
        }
    }
    if dirty {
        crate::config::save_archive(archive).map_err(|error| error.to_string())?;
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Restores the archived DNS configuration on every adapter it was applied
/// to, optionally removing the DoH mapping for `resolvers`. Restored entries
/// are drained; adapters that cannot be reached keep their backup so the
/// restore can be retried.
pub fn revert(archive: &mut Archive, resolvers: Option<&Resolvers>) -> Result<(), String> {
    let mut errors = Vec::new();
    let ifaces = interfaces::physical();
    let backups = std::mem::take(&mut archive.dns);
    let total = backups.len();
    let mut kept = Vec::new();

    for backup in backups {
        let outcome = restore_backup(&backup, &ifaces).map(|()| {
            if let Some(resolvers) = resolvers {
                if let Some(iface) = ifaces.iter().find(|i| i.friendly == backup.interface) {
                    clear_doh(iface, resolvers);
                }
            }
        });
        match outcome {
            Ok(()) => {}
            Err(error) => {
                errors.push(format!("{}: {error}", backup.interface));
                kept.push(backup);
            }
        }
    }
    archive.dns = kept;
    if archive.dns.len() != total {
        crate::config::save_archive(archive).map_err(|error| error.to_string())?;
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Scans the current DNS state of all active physical adapters.
pub fn status(resolvers: &Resolvers) -> DnsStatus {
    let ifaces = interfaces::active();
    DnsStatus {
        aligned: ifaces
            .iter()
            .filter(|iface| is_set(iface, resolvers))
            .count(),
        total: ifaces.len(),
    }
}

/// True when the adapter already uses both resolvers of the set.
pub fn is_set(iface: &Interface, resolvers: &Resolvers) -> bool {
    iface.dns.len() >= 2
        && iface.dns.iter().any(|d| d == resolvers.primary)
        && iface.dns.iter().any(|d| d == resolvers.secondary)
}

fn restore_backup(backup: &crate::config::DnsBackup, ifaces: &[Interface]) -> Result<(), String> {
    let iface = ifaces
        .iter()
        .find(|iface| iface.friendly == backup.interface)
        .ok_or_else(|| "adapter no longer present".to_string())?;
    let name_arg = netsh::name_arg(&iface.friendly)?;

    // The adapter managed its IPv4 DNS through DHCP (or had no DNS to
    // restore): hand IPv4 DNS management back to DHCP.
    if backup.dhcp || backup.servers.is_empty() {
        netsh::run(&["interface", "ip", "set", "dns", &name_arg, "dhcp"])
            .map_err(|error| error.to_string())?;
    } else {
        let primary = &backup.servers[0];
        netsh::run(&[
            "interface",
            "ip",
            "set",
            "dns",
            &name_arg,
            "static",
            primary,
        ])
        .map_err(|error| error.to_string())?;
        for (index, server) in backup.servers.iter().enumerate().skip(1) {
            let index_arg = format!("index={}", index + 1);
            netsh::run(&[
                "interface",
                "ip",
                "add",
                "dns",
                &name_arg,
                server,
                &index_arg,
            ])
            .map_err(|error| error.to_string())?;
        }
    }

    // IPv6: undo exactly what `set_iface` did. Best effort, mirroring apply.
    if backup.servers6.is_empty() {
        let _ = netsh::run(&["interface", "ipv6", "set", "dns", &name_arg, "dhcp"]);
    } else {
        let primary_v6 = &backup.servers6[0];
        let _ = netsh::run(&[
            "interface",
            "ipv6",
            "set",
            "dns",
            &name_arg,
            "static",
            primary_v6,
        ]);
        for (index, server) in backup.servers6.iter().enumerate().skip(1) {
            let index_arg = format!("index={}", index + 1);
            let _ = netsh::run(&[
                "interface",
                "ipv6",
                "add",
                "dns",
                &name_arg,
                server,
                &index_arg,
            ]);
        }
    }
    Ok(())
}

fn set_iface(iface: &Interface, resolvers: &Resolvers) -> Result<(), String> {
    let name_arg = netsh::name_arg(&iface.friendly)?;
    log::log(&format!(
        "dns: setting {} to {}",
        iface.friendly, resolvers.primary
    ));
    netsh::run(&[
        "interface",
        "ip",
        "set",
        "dns",
        &name_arg,
        "static",
        resolvers.primary,
    ])
    .map_err(|error| error.to_string())?;
    // The secondary may already exist after a previous run; netsh then
    // errors, which is fine because the primary replacement reset the list.
    let _ = netsh::run(&[
        "interface",
        "ip",
        "add",
        "dns",
        &name_arg,
        resolvers.secondary,
        "index=2",
    ]);
    // IPv6: set on adapters that carry it, silently skip the rest.
    let _ = netsh::run(&[
        "interface",
        "ipv6",
        "set",
        "dns",
        &name_arg,
        "static",
        resolvers.primary_v6,
    ]);
    let _ = netsh::run(&[
        "interface",
        "ipv6",
        "add",
        "dns",
        &name_arg,
        resolvers.secondary_v6,
        "index=2",
    ]);
    Ok(())
}

/// Configures DNS-over-HTTPS for the resolver set on `iface`. Windows 11
/// only; the command is probed once per process and skipped entirely when
/// the `netsh dns` context does not advertise it.
fn set_doh(iface: &Interface, resolvers: &Resolvers) {
    if !doh_supported() {
        log::log("dns: DoH unavailable on this system, skipping");
        return;
    }
    let Ok(arg) = netsh::interface_arg(&iface.friendly) else {
        return;
    };
    let template = format!("dohtemplate={}", resolvers.doh_template);
    let mut failed = false;
    for server in [resolvers.primary, resolvers.secondary] {
        let server_arg = format!("server={server}");
        if let Err(error) = netsh::run(&["dns", "add", "encryption", &server_arg, &template, &arg])
        {
            failed = true;
            log::log(&format!(
                "dns: DoH add {server} on {}: {error}",
                iface.friendly
            ));
        }
    }
    if !failed {
        log::log(&format!(
            "dns: DoH enabled for {} on {}",
            resolvers.primary, iface.friendly
        ));
    }
}

/// Removes the DoH mapping for `resolvers` on `iface`.
fn clear_doh(iface: &Interface, resolvers: &Resolvers) {
    if !doh_supported() {
        return;
    }
    let Ok(arg) = netsh::interface_arg(&iface.friendly) else {
        return;
    };
    for server in [resolvers.primary, resolvers.secondary] {
        let server_arg = format!("server={server}");
        if let Err(error) = netsh::run(&["dns", "delete", "encryption", &server_arg, &arg]) {
            log::log(&format!(
                "dns: DoH delete {server} on {}: {error}",
                iface.friendly
            ));
        }
    }
}

/// Whether `netsh dns add encryption` exists on this system. Tri-state cache:
/// -1 unknown, 0 unsupported, 1 supported.
fn doh_supported() -> bool {
    use std::sync::atomic::{AtomicI8, Ordering};
    static STATE: AtomicI8 = AtomicI8::new(-1);
    let cached = STATE.load(Ordering::Relaxed);
    if cached >= 0 {
        return cached == 1;
    }
    let supported =
        matches!(netsh::run(&["dns", "add", "?"]), Ok(text) if text.contains("encryption"));
    log::log(&format!(
        "dns: DoH {}supported",
        if supported { "" } else { "not " }
    ));
    STATE.store(i8::from(supported), Ordering::Relaxed);
    supported
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_provider_returns_requested_set() {
        assert_eq!(resolve_provider(DnsProvider::Cloudflare, None), CLOUDFLARE);
        assert_eq!(resolve_provider(DnsProvider::Google, None), GOOGLE);
    }

    #[test]
    fn resolve_auto_prefers_fastest() {
        let latency = probe::Latency {
            cloudflare: probe::Endpoint::with_rtt(Some(40)),
            google: probe::Endpoint::with_rtt(Some(10)),
            ..probe::Latency::default()
        };
        assert_eq!(resolve_provider(DnsProvider::Auto, Some(&latency)), GOOGLE);
        let latency = probe::Latency {
            cloudflare: probe::Endpoint::with_rtt(Some(10)),
            google: probe::Endpoint::with_rtt(Some(40)),
            ..probe::Latency::default()
        };
        assert_eq!(
            resolve_provider(DnsProvider::Auto, Some(&latency)),
            CLOUDFLARE
        );
    }

    #[test]
    fn resolve_auto_prefers_real_dns_resolution() {
        let latency = probe::Latency {
            cloudflare: probe::Endpoint::with_rtt(Some(5)),
            google: probe::Endpoint::with_rtt(Some(5)),
            cf_dns_ms: Some(30),
            google_dns_ms: Some(12),
        };
        assert_eq!(resolve_provider(DnsProvider::Auto, Some(&latency)), GOOGLE);
    }

    #[test]
    fn resolve_auto_ties_break_to_cloudflare() {
        let latency = probe::Latency {
            cloudflare: probe::Endpoint::with_rtt(Some(10)),
            google: probe::Endpoint::with_rtt(Some(10)),
            ..probe::Latency::default()
        };
        assert_eq!(
            resolve_provider(DnsProvider::Auto, Some(&latency)),
            CLOUDFLARE
        );
    }

    #[test]
    fn resolve_auto_falls_back_without_measurement() {
        assert_eq!(resolve_provider(DnsProvider::Auto, None), CLOUDFLARE);
    }

    #[test]
    fn resolvers_have_valid_ips() {
        for set in [CLOUDFLARE, GOOGLE] {
            assert!(set.primary.parse::<std::net::Ipv4Addr>().is_ok());
            assert!(set.secondary.parse::<std::net::Ipv4Addr>().is_ok());
            assert!(set.primary_v6.parse::<std::net::Ipv6Addr>().is_ok());
            assert!(set.secondary_v6.parse::<std::net::Ipv6Addr>().is_ok());
            assert!(set.doh_template.starts_with("https://"));
        }
    }

    #[test]
    fn is_set_ignores_order() {
        let mut iface = Interface {
            guid: String::new(),
            index: 1,
            friendly: String::new(),
            description: String::new(),
            mtu: 0,
            if_type: 6,
            up: true,
            dhcp: false,
            dns: vec!["1.1.1.1".into(), "1.0.0.1".into()],
            dns6: vec![],
            ips: vec![],
            gateways: vec![],
            mac: String::new(),
        };
        assert!(is_set(&iface, &CLOUDFLARE));

        iface.dns = vec!["1.0.0.1".into(), "8.8.8.8".into(), "1.1.1.1".into()];
        assert!(is_set(&iface, &CLOUDFLARE));

        iface.dns = vec![CLOUDFLARE.primary.into()];
        assert!(!is_set(&iface, &CLOUDFLARE));

        // Google's set is a different feature, even on the same adapter.
        assert!(!is_set(&iface, &GOOGLE));
    }

    #[test]
    fn status_empty_when_no_ifaces() {
        let status = DnsStatus::default();
        assert!(status.aligned_all());
        assert_eq!(status.total, 0);
    }

    #[test]
    fn revert_keeps_backup_for_missing_adapter() {
        let mut archive = Archive::default();
        archive.dns_ensure("Ghost", false, vec!["1.1.1.1".into()], vec![]);
        let result = revert(&mut archive, None);
        assert!(result.is_err(), "missing adapter must fail the revert");
        assert_eq!(archive.dns.len(), 1, "failed backup stays archived");
    }

    #[test]
    fn revert_drains_empty_archive() {
        let mut archive = Archive::default();
        assert!(revert(&mut archive, None).is_ok());
        assert!(archive.dns.is_empty());
    }

    /// End-to-end DNS smoke test. Requires an elevated shell and briefly
    /// moves every active adapter to the Cloudflare resolvers; the archived
    /// configuration is restored right afterwards.
    #[test]
    #[ignore]
    fn apply_revert_roundtrip() {
        let mut archive = Archive::default();
        apply(&mut archive, &CLOUDFLARE).expect("apply should succeed");
        assert!(
            status(&CLOUDFLARE).aligned_all(),
            "every active adapter must be aligned: {:?}",
            status(&CLOUDFLARE)
        );
        let backups = archive.dns.clone();
        assert!(!backups.is_empty(), "at least one adapter was backed up");

        std::thread::sleep(std::time::Duration::from_millis(500));
        revert(&mut archive, Some(&CLOUDFLARE)).expect("revert should succeed");
        assert!(archive.dns.is_empty(), "archive must be drained");

        let current = interfaces::active();
        for backup in backups {
            let iface = current
                .iter()
                .find(|iface| iface.friendly == backup.interface)
                .unwrap_or_else(|| panic!("adapter {} must still exist", backup.interface));
            assert!(
                !is_set(iface, &CLOUDFLARE),
                "cloudflare must be gone from {} after revert",
                backup.interface
            );
            if !backup.dhcp {
                let want = if backup.servers.is_empty() {
                    "dhcp".to_string()
                } else {
                    backup.servers[0].clone()
                };
                let own = if iface.dns.is_empty() {
                    "dhcp".to_string()
                } else {
                    iface.dns[0].clone()
                };
                assert_eq!(
                    want, own,
                    "primary DNS of {} must be restored",
                    backup.interface
                );
            }
            // IPv6 must also be back under DHCP / original servers.
            assert!(
                !iface.dns6.iter().any(|d| d.contains("2606:4700:4700::")),
                "cloudflare IPv6 must be gone from {} after revert",
                backup.interface
            );
        }
    }
}
