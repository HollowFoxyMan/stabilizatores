//! Persisted settings and the restore archive.
//!
//! Two JSON files live in `%APPDATA%\stabilizatores`:
//!
//! * `config.json` — which tweak groups are enabled and their parameters;
//! * `archive.json` — the *original* machine state replaced by the program:
//!   raw registry DWORDs for the TCP/throttling/MTU tweaks and the previous
//!   DNS servers for the DNS tweak, so reverting restores the machine exactly.
//!
//! The archive is written lazily the first time a tweak actually replaces a
//! value. Entries are keyed by `(group, subkey, value name)` and are only
//! recorded once, so re-applying a tweak never overwrites a saved original.
//! Both files are written atomically (temporary file + rename) so a crash
//! mid-write can never corrupt the only copy of the original state.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::win;

/// Which anycast resolver set the DNS feature switches to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DnsProvider {
    /// Cloudflare 1.1.1.1 / 1.0.0.1.
    #[default]
    Cloudflare,
    /// Google 8.8.8.8 / 8.8.4.4.
    Google,
    /// Pick whichever of Cloudflare / Google is faster right now.
    Auto,
}

/// Currently enabled feature groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// TCP/IP parameter tuning (Nagle, ACK frequency, timers).
    pub tcp: bool,
    /// Switch active adapters to a low-latency DNS server set.
    pub dns: bool,
    /// Disable the Windows multimedia network throttling.
    pub multimedia: bool,
    /// Re-apply enabled tweaks when something reverts them.
    pub self_heal: bool,
    /// Which DNS server set to use ([`DnsProvider::Auto`] probes both).
    #[serde(default)]
    pub dns_provider: DnsProvider,
    /// Desired IPv4 MTU when the "mtu" feature is on; `None` disables it.
    #[serde(default)]
    pub mtu: Option<u32>,
    /// Whether NICs are told not to power themselves down to save energy.
    #[serde(default)]
    pub power: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tcp: true,
            dns: false,
            multimedia: true,
            self_heal: true,
            dns_provider: DnsProvider::default(),
            mtu: None,
            power: true,
        }
    }
}

/// Tweak group of the TCP tuning feature (used by the archive).
pub const GROUP_TCP: &str = "tcp";
/// Tweak group of the multimedia throttling feature.
pub const GROUP_SYSTEM: &str = "system";
/// Tweak group of the MTU normalization feature.
pub const GROUP_MTU: &str = "mtu";
/// Tweak group of the NIC power-management feature.
pub const GROUP_POWER: &str = "power";

/// One archived original registry value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveValue {
    /// Tweak group this value belongs to, [`GROUP_TCP`] or [`GROUP_SYSTEM`].
    pub group: String,
    /// Full registry subkey under `HKEY_LOCAL_MACHINE`.
    pub subkey: String,
    /// Value name inside the subkey.
    pub name: String,
    /// The value existed before the program wrote it.
    pub present: bool,
    /// The original DWORD value (meaningful only when `present` is true).
    pub value: u32,
}

/// DNS servers that were in use on an adapter before the program switched it
/// to the Cloudflare servers. Used by the DNS revert to restore the previous
/// configuration exactly instead of falling back to defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsBackup {
    /// Friendly adapter name as returned by the enumeration and netsh.
    pub interface: String,
    /// The adapter obtained its IPv4 setting from DHCP before the switch.
    /// When true a revert puts IPv4 DNS management back on DHCP.
    pub dhcp: bool,
    /// The IPv4 DNS servers in their original order. Meaningful only when
    /// `dhcp` is false; the first server becomes the new primary.
    pub servers: Vec<String>,
    /// The IPv6 DNS servers in their original order. An empty list means the
    /// IPv6 DNS was DHCP-managed, and a revert restores that.
    pub servers6: Vec<String>,
}

/// The saved original machine state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Archive {
    /// Original registry values for the TCP and throttling tweaks.
    pub values: Vec<ArchiveValue>,
    /// Original DNS servers per adapter for the DNS tweak.
    pub dns: Vec<DnsBackup>,
}

impl Archive {
    /// Records the current value for `(group, subkey, name)` unless it was
    /// recorded already. Returns true when a new entry was added.
    pub fn ensure(&mut self, group: &str, subkey: &str, name: &str, current: Option<u32>) -> bool {
        if self
            .values
            .iter()
            .any(|entry| entry.group == group && entry.subkey == subkey && entry.name == name)
        {
            return false;
        }
        self.values.push(ArchiveValue {
            group: group.to_string(),
            subkey: subkey.to_string(),
            name: name.to_string(),
            present: current.is_some(),
            value: current.unwrap_or(0),
        });
        true
    }

    /// Restores every archived value of `group` and removes the restored
    /// entries. Failed restores stay in the archive so they can be retried.
    /// Returns the number of restored entries.
    pub fn restore_group(&mut self, group: &str) -> io::Result<usize> {
        let mut first_error: Option<io::Error> = None;
        let mut restored = 0usize;

        self.values.retain(|entry| {
            if entry.group != group {
                return true;
            }
            let result = if entry.present {
                crate::registry::write_dword(&entry.subkey, &entry.name, entry.value)
            } else {
                crate::registry::delete_value(&entry.subkey, &entry.name)
            };
            match result {
                Ok(()) => {
                    restored += 1;
                    false
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    true
                }
            }
        });

        match first_error {
            Some(error) => Err(error),
            None => Ok(restored),
        }
    }

    /// Records the DNS state of `interface` unless it was recorded already.
    /// Returns true when a new entry was added.
    pub fn dns_ensure(
        &mut self,
        interface: &str,
        dhcp: bool,
        servers: Vec<String>,
        servers6: Vec<String>,
    ) -> bool {
        if self.dns.iter().any(|entry| entry.interface == interface) {
            return false;
        }
        self.dns.push(DnsBackup {
            interface: interface.to_string(),
            dhcp,
            servers,
            servers6,
        });
        true
    }
}

fn dir() -> io::Result<PathBuf> {
    win::app_data_dir()
}

/// The `%APPDATA%\stabilizatores` directory. Its parent is created on demand
/// by the save functions; reports are written under it as well.
pub fn data_dir() -> PathBuf {
    dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn config_path() -> io::Result<PathBuf> {
    Ok(dir()?.join("config.json"))
}

fn archive_path() -> io::Result<PathBuf> {
    Ok(dir()?.join("archive.json"))
}

/// Loads the config, falling back to defaults on any error.
pub fn load() -> Config {
    let path = config_path();
    let Ok(path) = path else {
        return Config::default();
    };
    fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persists the config. The containing directory is created on demand.
pub fn save(cfg: &Config) -> io::Result<()> {
    let path = config_path()?;
    write_atomic(&path, &serde_json::to_vec_pretty(cfg)?)
}

/// Loads the archive, defaulting to empty on any error.
pub fn load_archive() -> Archive {
    let path = archive_path();
    let Ok(path) = path else {
        return Archive::default();
    };
    fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persists the archive.
pub fn save_archive(archive: &Archive) -> io::Result<()> {
    let path = archive_path()?;
    write_atomic(&path, &serde_json::to_vec_pretty(archive)?)
}

/// Overwrites the archive with raw JSON bytes (e.g. a snapshot restore).
pub fn save_raw_archive(bytes: &[u8]) -> io::Result<()> {
    let path = archive_path()?;
    write_atomic(&path, bytes)
}

/// Directory holding named config profiles.
pub fn profiles_dir() -> PathBuf {
    data_dir().join("profiles")
}

/// Number of old archive snapshots kept around.
const MAX_SNAPSHOTS: usize = 8;

/// Copies the current archive to a dated snapshot before a tweak run changes
/// it, trimming to the last [`MAX_SNAPSHOTS`]. Missing archive files are fine.
pub fn take_archive_snapshot() {
    let Ok(path) = archive_path() else {
        return;
    };
    let Ok(bytes) = fs::read(&path) else {
        return;
    };
    let dir = data_dir().join("archive-snapshots");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = fs::write(
        dir.join(format!("{}.json", crate::win::timestamp_compact())),
        bytes,
    );
    prune(&dir, MAX_SNAPSHOTS);
}

fn prune(dir: &Path, keep: usize) {
    let mut files = match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    files.sort();
    let excess = files.len().saturating_sub(keep);
    for file in files.iter().take(excess) {
        let _ = fs::remove_file(file);
    }
}

/// Returns the most recent archive snapshot path, if any.
pub fn latest_archive_snapshot() -> Option<PathBuf> {
    let dir = data_dir().join("archive-snapshots");
    let mut files = fs::read_dir(&dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    files.sort();
    files.pop()
}

/// Writes `bytes` to `path` through a sibling temporary file so a process
/// that dies mid-write cannot truncate the target. `std::fs::rename` replaces
/// the destination on Windows.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// Validates a profile name: non-empty, short, printable, no path tricks.
pub fn validate_profile_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.chars().count() <= 40
        && name
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid profile name '{name}': use letters, digits, '-', '_' or '.'"
        ))
    }
}

fn profile_path(name: &str) -> io::Result<PathBuf> {
    validate_profile_name(name).map_err(io::Error::other)?;
    Ok(profiles_dir().join(format!("{name}.json")))
}

/// Saves the current config as a named profile.
pub fn save_profile(name: &str, cfg: &Config) -> io::Result<()> {
    let path = profile_path(name)?;
    write_atomic(&path, &serde_json::to_vec_pretty(cfg)?)
}

/// Loads a named profile.
pub fn load_profile(name: &str) -> Option<Config> {
    let path = profile_path(name).ok()?;
    let bytes = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&bytes).ok()
}

/// Lists the saved profile names sorted.
pub fn list_profiles() -> Vec<String> {
    let dir = profiles_dir();
    let mut names = fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(".json")
                        .map(str::to_owned)
                        .filter(|name| !name.is_empty())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Deletes a named profile.
pub fn delete_profile(name: &str) -> io::Result<()> {
    let path = profile_path(name)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let cfg = Config::default();
        assert!(cfg.tcp);
        assert!(!cfg.dns);
        assert!(cfg.multimedia);
        assert!(cfg.self_heal);
        assert_eq!(cfg.dns_provider, DnsProvider::Cloudflare);
        assert_eq!(cfg.mtu, None);
        assert!(cfg.power);
    }

    #[test]
    fn config_json_roundtrip() {
        let cfg = Config {
            tcp: false,
            dns: true,
            multimedia: false,
            self_heal: false,
            dns_provider: DnsProvider::Google,
            mtu: Some(1400),
            power: true,
        };
        let bytes = serde_json::to_vec(&cfg).unwrap();
        let back: Config = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn config_new_fields_have_serde_defaults() {
        let back: Config =
            serde_json::from_str(r#"{"tcp":true,"dns":false,"multimedia":true,"self_heal":false}"#)
                .unwrap();
        assert_eq!(back.dns_provider, DnsProvider::Cloudflare);
        assert_eq!(back.mtu, None);
    }

    #[test]
    fn provider_variants_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&DnsProvider::Auto).unwrap(),
            "\"auto\""
        );
        assert_eq!(
            serde_json::from_str::<DnsProvider>("\"google\"").unwrap(),
            DnsProvider::Google
        );
    }

    #[test]
    fn archive_ensure_is_idempotent() {
        let mut archive = Archive::default();
        assert_eq!(archive.values.len(), 0);
        assert!(archive.ensure(GROUP_TCP, "k", "v", Some(9)));
        assert!(!archive.ensure(GROUP_TCP, "k", "v", Some(9)));
        assert_eq!(archive.values.len(), 1);
        assert_eq!(archive.values[0].value, 9);
        assert!(archive.values[0].present);
    }

    #[test]
    fn archive_ensure_records_absence() {
        let mut archive = Archive::default();
        archive.ensure(GROUP_SYSTEM, "k", "missing", None);
        assert!(!archive.values[0].present);
        assert_eq!(archive.values[0].value, 0);
    }

    #[test]
    fn archive_roundtrip() {
        let mut archive = Archive::default();
        archive.ensure(GROUP_TCP, "s", "n", Some(7));
        let bytes = serde_json::to_vec(&archive).unwrap();
        drop(archive);
        let back: Archive = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.values.len(), 1);
        assert_eq!(back.values[0].name, "n");
    }

    #[test]
    fn archive_groups_are_independent() {
        let mut archive = Archive::default();
        archive.ensure(GROUP_TCP, "t", "a", Some(1));
        archive.ensure(GROUP_SYSTEM, "s", "b", Some(2));
        archive.ensure(GROUP_TCP, "t", "c", Some(3));
        assert_eq!(archive.values.len(), 3);
    }

    #[test]
    fn dns_ensure_is_idempotent_per_interface() {
        let mut archive = Archive::default();
        assert_eq!(archive.dns.len(), 0);
        assert!(archive.dns_ensure(
            "Wi-Fi",
            false,
            vec!["1.1.1.1".into(), "8.8.8.8".into()],
            vec!["2606:4700:4700::1111".into()]
        ));
        assert!(!archive.dns_ensure("Wi-Fi", false, vec!["9.9.9.9".into()], vec![]));
        assert_eq!(archive.dns.len(), 1);
        assert_eq!(
            archive.dns[0].servers,
            vec!["1.1.1.1".to_string(), "8.8.8.8".to_string()]
        );
        assert_eq!(
            archive.dns[0].servers6,
            vec!["2606:4700:4700::1111".to_string()]
        );
        assert!(!archive.dns[0].dhcp);
        assert!(archive.dns_ensure("Ethernet", true, vec![], vec![]));
        assert_eq!(archive.dns.len(), 2);
        assert!(archive.dns[1].dhcp);
    }

    #[test]
    fn dns_backup_roundtrip() {
        let mut archive = Archive::default();
        archive.dns_ensure(
            "Ethernet",
            true,
            vec!["192.168.1.1".into()],
            vec!["2606:4700:4700::1001".into()],
        );
        let bytes = serde_json::to_vec(&archive).unwrap();
        drop(archive);
        let back: Archive = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.dns.len(), 1);
        assert!(back.dns[0].dhcp);
        assert_eq!(back.dns[0].servers[0], "192.168.1.1");
        assert_eq!(back.dns[0].servers6[0], "2606:4700:4700::1001");
    }
}
