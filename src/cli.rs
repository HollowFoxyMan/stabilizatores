//! Non-interactive command line mode.
//!
//! `--apply` re-applies every enabled feature from the config, `--revert`
//! restores the archived machine state and removes autostart, `--status`
//! prints a read-only alignment report, `--plan` shows what `--apply` would
//! change without touching the system and `--report` writes a diagnostic
//! snapshot to `%APPDATA%`. The actions accept CLI overrides that temporarily
//! change the resolved config for one run, e.g. `--apply --mtu=1500`.

use std::io::Write;
use std::path::PathBuf;

use crate::config::{self, DnsProvider};
use crate::{
    connections, dns, interfaces, mtu, power, probe, restore, system, tcp, traffic, win, wlan,
};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";

/// Exit code for a successful apply / aligned status run.
pub const EXIT_OK: i32 = 0;
/// Exit code for a `--status` run that found drift.
pub const EXIT_DRIFT: i32 = 1;
/// Exit code for a run that hit an error.
pub const EXIT_ERROR: i32 = 2;

/// Parsed command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Show usage text.
    Help,
    /// Show the version.
    Version,
    /// Apply every enabled feature.
    Apply,
    /// Revert every archived feature and clear autostart.
    Revert,
    /// Print the alignment state read-only.
    Status,
    /// Dry-run: print what `--apply` would change, touch nothing.
    Plan,
    /// Write a diagnostic snapshot and print its path.
    Report,
    /// Create a System Restore point.
    RestorePoint,
    /// Undo the last apply: revert tweaks and restore the pre-apply archive.
    Undo,
    /// Manage named configuration profiles.
    Profile,
    /// Launch the interactive terminal menu.
    Interactive,
}

/// Named config profile operations. Saving / listing / deleting only touches
/// `%APPDATA%`; applying a profile is an apply run and needs elevation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileAction {
    /// Persist the current (override-resolved) config under a name.
    Save { name: String },
    /// Make a saved profile the active config and apply it.
    Apply { name: String },
    /// Print the saved profile names.
    List,
    /// Remove one profile.
    Delete { name: String },
}

/// The parsed arguments: one action plus optional one-run overrides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The action to run, [`Command::Interactive`] when no action was given.
    pub command: Command,
    /// Whether the DNS feature is enabled for this run.
    pub dns: Option<bool>,
    /// The MTU target for this run (`None` disables the feature).
    pub mtu: Option<Option<u32>>,
    /// Whether the NIC power feature is enabled for this run.
    pub power: Option<bool>,
    /// Profile operation when `command` is [`Command::Profile`].
    pub profile: Option<ProfileAction>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            command: Command::Interactive,
            dns: None,
            mtu: None,
            power: None,
            profile: None,
        }
    }
}

/// Interprets the argument list. `--help` / `--version` win over everything;
/// of the action flags the one listed first wins, otherwise the program runs
/// interactively. Overrides are collected wherever they appear. Unknown or
/// malformed override values are reported as errors.
pub fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    if args
        .iter()
        .any(|arg| arg == "-h" || arg == "--help" || arg == "/?")
    {
        options.command = Command::Help;
        return Ok(options);
    }
    if args.iter().any(|arg| arg == "-V" || arg == "--version") {
        options.command = Command::Version;
        return Ok(options);
    }
    let mut index = 0usize;
    while index < args.len() {
        let arg = &args[index];
        if let Some(action) = match_action(arg) {
            if options.command == Command::Interactive {
                options.command = action;
            }
            index += 1;
            continue;
        }
        if let Some(value) = override_value(args, index, "--dns") {
            options.dns = Some(parse_bool(&value)?);
            index += 1;
            continue;
        }
        if let Some(value) = override_value(args, index, "--mtu") {
            options.mtu = Some(parse_mtu(&value)?);
            index += 1;
            continue;
        }
        if let Some(value) = override_value(args, index, "--power") {
            options.power = Some(parse_bool(&value)?);
            index += 1;
            continue;
        }
        if arg == "--profile" {
            options.command = Command::Profile;
            options.profile = Some(parse_profile(&args[index + 1..])?);
            break;
        }
        index += 1;
    }
    Ok(options)
}

/// Interprets the tokens after `--profile`.
fn parse_profile(tokens: &[String]) -> Result<ProfileAction, String> {
    let op = tokens.first().map(String::as_str).unwrap_or_default();
    let name = |pos: usize| -> Result<String, String> {
        tokens
            .get(pos)
            .filter(|name| !name.starts_with("--"))
            .cloned()
            .ok_or_else(|| format!("'{op}' needs a profile name: --profile {op} <name>"))
            .and_then(|name| {
                config::validate_profile_name(&name)?;
                Ok(name)
            })
    };
    match op {
        "save" => Ok(ProfileAction::Save { name: name(1)? }),
        "apply" => Ok(ProfileAction::Apply { name: name(1)? }),
        "list" => Ok(ProfileAction::List),
        "delete" => Ok(ProfileAction::Delete { name: name(1)? }),
        other => Err(format!(
            "unknown profile operation '{other}', expected save | apply | list | delete"
        )),
    }
}

fn match_action(arg: &str) -> Option<Command> {
    match arg {
        "--apply" => Some(Command::Apply),
        "--revert" => Some(Command::Revert),
        "--status" => Some(Command::Status),
        "--plan" => Some(Command::Plan),
        "--report" => Some(Command::Report),
        "--restore-point" => Some(Command::RestorePoint),
        "--undo" => Some(Command::Undo),
        _ => None,
    }
}

/// Reads an override in either `--key=value` or `--key value` form. The value
/// ends up in the returned string; the caller advances once more.
fn override_value(args: &[String], index: usize, key: &str) -> Option<String> {
    let arg = &args[index];
    if let Some(value) = arg.strip_prefix(&format!("{key}=")) {
        return Some(value.to_string());
    }
    if arg == key {
        let value = args.get(index + 1)?;
        if !value.starts_with("--") {
            return Some(value.clone());
        }
    }
    None
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        other => Err(format!(
            "invalid boolean override '{other}', expected on/off"
        )),
    }
}

fn parse_mtu(value: &str) -> Result<Option<u32>, String> {
    match value {
        "off" | "default" | "0" => Ok(None),
        other => match other.parse::<u32>() {
            Ok(mtu) if (1280..=9000).contains(&mtu) => Ok(Some(mtu)),
            _ => Err(format!(
                "invalid mtu override '{other}', expected off or 1280..=9000"
            )),
        },
    }
}

/// The config this run acts on: the persisted one with the CLI overrides
/// folded in.
fn resolved_cfg(options: &Options) -> config::Config {
    let mut cfg = config::load();
    if let Some(dns) = options.dns {
        cfg.dns = dns;
    }
    if let Some(mtu) = options.mtu {
        cfg.mtu = mtu;
    }
    if let Some(power) = options.power {
        cfg.power = power;
    }
    cfg
}

/// Resolves the configured provider to a concrete resolver set, probing the
/// endpoints when the provider is `Auto`.
fn resolvers(cfg: &config::Config) -> dns::Resolvers {
    let latency = if cfg.dns_provider == DnsProvider::Auto {
        Some(probe::latency_now())
    } else {
        None
    };
    dns::resolve_provider(cfg.dns_provider, latency.as_ref())
}

/// Creates a System Restore point named after the current scenario.
pub fn run_restore_point() -> i32 {
    let description = format!(
        "stabilizatores before tweaks v{}",
        env!("CARGO_PKG_VERSION")
    );
    match restore::create(&description) {
        Ok(()) => {
            println!("restore point created.");
            EXIT_OK
        }
        Err(error) => {
            println!("{YELLOW}warning{RESET}: {error}");
            EXIT_ERROR
        }
    }
}

fn nothing_enabled(cfg: &config::Config) -> bool {
    !cfg.tcp && !cfg.dns && !cfg.multimedia && cfg.mtu.is_none() && !cfg.power
}

/// Applies every enabled feature. Returns `EXIT_OK` when everything aligned,
/// `EXIT_ERROR` when any step logged a failure.
pub fn run_apply(options: &Options) -> i32 {
    let cfg = resolved_cfg(options);
    config::take_archive_snapshot();
    let mut archive = config::load_archive();
    let mut errors = Vec::new();
    let mut enabled = 0usize;

    if cfg.tcp {
        enabled += 1;
        match tcp::apply(&mut archive) {
            Ok(status) => println!(
                "tcp: tuned {}/{} ifaces, global ok",
                status.ifaces_ok, status.ifaces_total
            ),
            Err(error) => errors.push(format!("tcp: {error}")),
        }
    }
    if cfg.dns {
        enabled += 1;
        let resolvers = resolvers(&cfg);
        println!(
            "dns: switching to {} {} / {}",
            provider_label(resolvers.provider),
            resolvers.primary,
            resolvers.secondary
        );
        if let Err(error) = dns::apply(&mut archive, &resolvers) {
            errors.push(format!("dns: {error}"));
        }
    }
    if cfg.multimedia {
        enabled += 1;
        if let Err(error) = system::apply(&mut archive) {
            errors.push(format!("throttling: {error}"));
        }
    }
    if let Some(target) = cfg.mtu {
        enabled += 1;
        match mtu::apply(&mut archive, target) {
            Ok(()) => {
                println!("mtu: {target} applied to all applicable adapters");
            }
            Err(error) => errors.push(format!("mtu: {error}")),
        }
    }
    if cfg.power {
        enabled += 1;
        match power::apply(&mut archive) {
            Ok(()) => println!("power: nics told to stay on"),
            Err(error) => errors.push(format!("power: {error}")),
        }
    }

    if enabled == 0 {
        println!("nothing enabled in the config; use the interactive menu to opt in.");
        return EXIT_OK;
    }
    finish(errors)
}

/// Restores everything the program has applied and clears autostart.
pub fn run_revert() -> i32 {
    let cfg = config::load();
    let mut archive = config::load_archive();
    let mut errors = Vec::new();

    if let Err(error) = mtu::revert(&mut archive) {
        errors.push(format!("mtu: {error}"));
    }
    if let Err(error) = system::revert(&mut archive) {
        errors.push(format!("throttling: {error}"));
    }
    if let Err(error) = power::revert(&mut archive) {
        errors.push(format!("power: {error}"));
    }
    if let Err(error) = tcp::revert(&mut archive) {
        errors.push(format!("tcp: {error}"));
    }
    // The resolvers the DNS feature may have mapped to DoH; best guess.
    if let Err(error) = dns::revert(&mut archive, Some(&resolvers(&cfg))) {
        errors.push(format!("dns: {error}"));
    }
    if win::autostart_enabled() {
        if let Err(error) = win::set_autostart(false) {
            errors.push(format!("autostart: {error}"));
        }
    }

    if archive.dns.is_empty() && archive.values.is_empty() {
        let _ = config::save_archive(&archive);
    }

    println!("system restored.");
    finish(errors)
}

/// Undoes the last apply: reverts the tweaks (exactly like `--revert`) and
/// then swaps the archive back to the snapshot taken before that apply, so a
/// later `--revert` stays harmless.
pub fn run_undo() -> i32 {
    let code = run_revert();
    match config::latest_archive_snapshot() {
        Some(snapshot)
            if std::fs::read(&snapshot)
                .is_ok_and(|bytes| config::save_raw_archive(&bytes).is_ok()) =>
        {
            println!(
                "archive restored from {}",
                snapshot.file_name().unwrap_or_default().to_string_lossy()
            );
        }
        _ => println!("no archive snapshot to restore; tweaks were reverted."),
    }
    code
}

/// Applies a named profile: makes it the active config, then runs `--apply`.
pub fn run_profile(options: &Options) -> i32 {
    let ProfileAction::Apply { name } = options.profile.clone().unwrap_or(ProfileAction::List)
    else {
        return run_profile_bookkeeping(options);
    };
    let Some(profile) = config::load_profile(&name) else {
        eprintln!("profile '{name}' does not exist; use --profile list");
        return EXIT_ERROR;
    };
    if let Err(error) = config::save(&profile) {
        eprintln!("error saving config: {error}");
        return EXIT_ERROR;
    }
    println!("profile '{name}' is now the active config.");
    run_apply(options)
}

/// `--profile save/list/delete`: only `%APPDATA%` is touched, no elevation.
fn run_profile_bookkeeping(options: &Options) -> i32 {
    let action = options.profile.clone().unwrap_or(ProfileAction::List);
    match action {
        ProfileAction::Save { name } => {
            let cfg = resolved_cfg(options);
            match config::save_profile(&name, &cfg) {
                Ok(()) => {
                    println!("profile '{name}' saved.");
                    EXIT_OK
                }
                Err(error) => {
                    eprintln!("error saving profile: {error}");
                    EXIT_ERROR
                }
            }
        }
        ProfileAction::List => {
            let names = config::list_profiles();
            if names.is_empty() {
                println!("no profiles saved.");
            } else {
                println!(
                    "{}",
                    names
                        .iter()
                        .map(|name| format!("  {name}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            EXIT_OK
        }
        ProfileAction::Delete { name } => match config::delete_profile(&name) {
            Ok(()) => {
                println!("profile '{name}' deleted.");
                EXIT_OK
            }
            Err(error) => {
                eprintln!("error deleting profile: {error}");
                EXIT_ERROR
            }
        },
        ProfileAction::Apply { name } => {
            // Unreachable: handled in run_profile, kept only for exhaustiveness.
            let _ = name;
            EXIT_ERROR
        }
    }
}

/// Prints the current alignment report. `EXIT_OK` when everything is aligned,
/// `EXIT_DRIFT` when something drifted, `EXIT_ERROR` when a probe failed.
pub fn run_status(options: &Options) -> i32 {
    let cfg = resolved_cfg(options);
    let resolvers = dns::resolve_provider(cfg.dns_provider, None);
    let tcp_status = tcp::status();
    let dns_status = dns::status(&resolvers);
    let throttling = system::status();
    let mut errors = Vec::new();

    let tcp_text = if !cfg.tcp {
        dim("off")
    } else if tcp_status.aligned() {
        green(&format!(
            "tuned {}/{} ifaces, global ok",
            tcp_status.ifaces_ok, tcp_status.ifaces_total
        ))
    } else if tcp_status.ifaces_total == 0 {
        yellow("no applicable interfaces")
    } else {
        yellow(&format!(
            "pending ({}/{} ifaces)",
            tcp_status.ifaces_ok, tcp_status.ifaces_total
        ))
    };
    let dns_text = if !cfg.dns {
        dim("off")
    } else if dns_status.aligned_all() {
        green(&format!(
            "{}/{} aligned",
            dns_status.aligned, dns_status.total
        ))
    } else {
        yellow(&format!(
            "{}/{} aligned",
            dns_status.aligned, dns_status.total
        ))
    };
    let throttle_text = if !cfg.multimedia {
        dim("off")
    } else if throttling {
        green("removed")
    } else {
        yellow("pending")
    };
    let mtu_text = match cfg.mtu {
        None => dim("off"),
        Some(target) if mtu::status(target) => green(&format!("{target} aligned")),
        Some(target) => {
            errors.push(format!("mtu: adapters not at {target}"));
            yellow(&format!("{target} pending"))
        }
    };
    let power_text = if !cfg.power {
        dim("off")
    } else if power::status() {
        green("no power-down")
    } else {
        errors.push("power: some NICs may still power down".into());
        yellow("pending")
    };
    let conns = connections::read();

    println!(
        "{DIM}interfaces{RESET}  {} physical, {} active",
        interfaces::physical().len(),
        interfaces::active().len()
    );
    println!("{DIM}tcp{RESET}         {tcp_text}");
    println!(
        "{DIM}dns{RESET}         {} ({}/{}): {dns_text}",
        provider_label(resolvers.provider),
        resolvers.primary,
        resolvers.secondary
    );
    println!("{DIM}throttling{RESET}  {throttle_text}");
    println!("{DIM}mtu{RESET}         {mtu_text}");
    println!("{DIM}power{RESET}       {power_text}");
    println!(
        "{DIM}connections{RESET} {} total, {} established, {} time-wait",
        conns.total, conns.established, conns.time_wait
    );

    if nothing_enabled(&cfg) {
        println!("{DIM}overall{RESET}      nothing enabled");
        return EXIT_OK;
    }

    let aligned = (!cfg.tcp || tcp_status.aligned())
        && (!cfg.dns || dns_status.aligned_all())
        && (!cfg.multimedia || throttling)
        && cfg.mtu.is_none_or(mtu::status)
        && (!cfg.power || power::status());

    if !errors.is_empty() {
        for error in &errors {
            eprintln!("{YELLOW}warning{RESET}: {error}");
        }
        return EXIT_ERROR;
    }

    let label = if aligned {
        "all aligned"
    } else {
        "drift detected"
    };
    println!("{DIM}overall{RESET}      {label}");
    if aligned {
        EXIT_OK
    } else {
        EXIT_DRIFT
    }
}

/// Dry-run: prints exactly what [`run_apply`] would do without changing the
/// system. Uses the same override resolution, so `--plan --mtu=1500` shows the
/// plans for a temporary config.
pub fn run_plan(options: &Options) -> i32 {
    let cfg = resolved_cfg(options);
    let adapters = interfaces::physical();
    let resolvers = resolvers(&cfg);

    let mut lines = Vec::new();
    lines.push(format!(
        "tcp:         {}\n  (tune {} physical adapters: Nagle, ACK frequency, TCP timers)",
        on_off(cfg.tcp),
        adapters.len()
    ));
    lines.push(format!(
        "dns:         {}\n  (switch adapters to {} {} / {})",
        on_off(cfg.dns),
        provider_label(resolvers.provider),
        resolvers.primary,
        resolvers.secondary
    ));
    lines.push(format!(
        "throttling:  {}\n  (remove the Windows multimedia network throttling)",
        on_off(cfg.multimedia)
    ));
    lines.push(format!(
        "mtu:         {}\n  (set adapter MTUs to {}, normalized)",
        match cfg.mtu {
            Some(target) => format!("{target}"),
            None => "off".into(),
        },
        cfg.mtu
            .map(|target| format!("{target}"))
            .unwrap_or_else(|| "unchanged".into())
    ));
    lines.push(format!(
        "power:       {}\n  (tell NICs to stay on, PnPCapabilities 0x18)",
        on_off(cfg.power)
    ));

    for line in &lines {
        println!("{GREEN}would set{RESET}");
        for part in line.split('\n') {
            println!("           {part}");
        }
    }

    if nothing_enabled(&cfg) {
        println!(
            "\nnothing enabled: --apply would do nothing. use the interactive menu to opt in."
        );
    } else {
        println!(
            "\nrun `stabilizatores --apply` to apply{}.",
            override_suffix(options)
        );
    }
    EXIT_OK
}

fn override_suffix(options: &Options) -> String {
    let mut parts = Vec::new();
    if let Some(dns) = options.dns {
        parts.push(format!("--dns={}", on_off(dns)));
    }
    if let Some(mtu) = options.mtu {
        parts.push(match mtu {
            Some(target) => format!("--mtu={target}"),
            None => "--mtu=off".into(),
        });
    }
    if let Some(power) = options.power {
        parts.push(format!("--power={}", on_off(power)));
    }
    if parts.is_empty() {
        "".into()
    } else {
        format!(" (overrides: {})", parts.join(" "))
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

/// Writes a diagnostic snapshot (version, config, live feature state, NICs and
/// a latency probe) as JSON next to the config, printing the file path.
pub fn run_report() -> i32 {
    let report = collect_report();
    let dir = config::data_dir().join("reports");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "{YELLOW}warning{RESET}: cannot create {}: {error}",
            dir.display()
        );
        return EXIT_ERROR;
    }
    let filename = format!("report-{}.json", crate::win::timestamp_compact());
    let path = dir.join(&filename);
    let bytes = match serde_json::to_vec_pretty(&report) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            bytes
        }
        Err(error) => {
            eprintln!("{YELLOW}warning{RESET}: report serialization failed: {error}");
            return EXIT_ERROR;
        }
    };
    let work = dir.join(format!("{filename}.tmp"));
    if let Err(error) = write_replace(&work, &path, &bytes) {
        eprintln!(
            "{YELLOW}warning{RESET}: cannot write {}: {error}",
            path.display()
        );
        return EXIT_ERROR;
    }
    println!("report written to {}", path.display());
    EXIT_OK
}

fn collect_report() -> serde_json::Value {
    let cfg = config::load();
    let archive = config::load_archive();
    let resolvers = dns::resolve_provider(cfg.dns_provider, None);
    let adapters = interfaces::physical();
    let tcp_status = tcp::status();
    let conns = connections::read();
    let before = traffic::sample();
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let traffic = traffic::rate(&before, &traffic::sample());
    serde_json::json!({
        "program": "stabilizatores",
        "version": env!("CARGO_PKG_VERSION"),
        "time": win::timestamp_compact(),
        "config": {
            "tcp": cfg.tcp,
            "dns": cfg.dns,
            "multimedia": cfg.multimedia,
            "self_heal": cfg.self_heal,
            "dns_provider": provider_label(cfg.dns_provider),
            "mtu": cfg.mtu,
            "power": cfg.power,
        },
        "archive": {
            "registry_values": archive.values.len(),
            "dns_backups": archive.dns.len(),
        },
        "tcp": {
            "global_ok": tcp_status.global_ok,
            "ifaces_ok": tcp_status.ifaces_ok,
            "ifaces_total": tcp_status.ifaces_total,
        },
        "dns": {
            "provider": provider_label(resolvers.provider),
            "primary": resolvers.primary,
            "secondary": resolvers.secondary,
        },
        "throttling_removed": system::status(),
        "mtu_aligned": cfg.mtu.is_none_or(mtu::status),
        "power_no_power_down": power::status(),
        "connections": {
            "total": conns.total,
            "established": conns.established,
            "time_wait": conns.time_wait,
        },
        "interfaces": adapters
            .iter()
            .map(|iface| serde_json::json!({
                "friendly": iface.friendly,
                "guid": iface.guid,
                "up": iface.up,
                "dhcp": iface.dhcp,
                "mtu": iface.mtu,
                "dns": iface.dns,
                "dns6": iface.dns6,
                "ips": iface.ips,
                "gateways": iface.gateways,
            }))
            .collect::<Vec<_>>(),
        "wlan": wlan::links()
            .iter()
            .map(|link| serde_json::json!({
                "guid": link.guid,
                "ssid": link.ssid,
                "bssid": link.bssid,
                "signal_quality": link.signal_quality,
                "rssi_dbm": link.rssi_dbm,
                "channel": link.channel,
                "rx_kbps": link.rx_kbps,
                "tx_kbps": link.tx_kbps,
            }))
            .collect::<Vec<_>>(),
        "traffic": traffic
            .iter()
            .map(|(index, down, up)| serde_json::json!({
                "index": index,
                "down_bps": down,
                "up_bps": up,
            }))
            .collect::<Vec<_>>(),
        "latency_ms": probe::latency_now().cloudflare.rtt_ms(),
    })
}

/// Writes `bytes` to `path` via a temporary file in the same directory so a
/// crash never leaves a half-written report.
fn write_replace(work: &PathBuf, path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(work)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(work, path)?;
    Ok(())
}

fn finish(errors: Vec<String>) -> i32 {
    if errors.is_empty() {
        EXIT_OK
    } else {
        for error in errors {
            eprintln!("{YELLOW}warning{RESET}: {error}");
        }
        EXIT_ERROR
    }
}

fn provider_label(provider: DnsProvider) -> &'static str {
    match provider {
        DnsProvider::Cloudflare => "cloudflare",
        DnsProvider::Google => "google",
        DnsProvider::Auto => "auto (cloudflare)",
    }
}

fn green(text: &str) -> String {
    format!("{GREEN}{text}{RESET}")
}

fn yellow(text: &str) -> String {
    format!("{YELLOW}{text}{RESET}")
}

fn dim(text: &str) -> String {
    format!("{DIM}{text}{RESET}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(args: &[&str]) -> Options {
        let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        parse(&args).expect("arguments parse")
    }

    #[test]
    fn parse_recognizes_flags() {
        assert_eq!(parse(&[]).unwrap().command, Command::Interactive);
        assert_eq!(parse_one(&["--status"]).command, Command::Status);
        assert_eq!(parse_one(&["--apply"]).command, Command::Apply);
        assert_eq!(parse_one(&["--revert"]).command, Command::Revert);
        assert_eq!(parse_one(&["--plan"]).command, Command::Plan);
        assert_eq!(parse_one(&["--report"]).command, Command::Report);
        assert_eq!(
            parse_one(&["--restore-point"]).command,
            Command::RestorePoint
        );
        assert_eq!(parse_one(&["-h"]).command, Command::Help);
        assert_eq!(parse_one(&["--version"]).command, Command::Version);
    }

    #[test]
    fn parse_first_action_wins() {
        assert_eq!(parse_one(&["--revert", "--apply"]).command, Command::Revert);
        assert_eq!(parse_one(&["--apply", "--status"]).command, Command::Apply);
    }

    #[test]
    fn parse_help_wins_over_actions() {
        assert_eq!(parse_one(&["--apply", "--help"]).command, Command::Help);
    }

    #[test]
    fn overrides_parse_both_forms() {
        let options = parse_one(&["--status", "--dns=on", "--mtu=off", "--power", "off"]);
        assert_eq!(options.dns, Some(true));
        assert_eq!(options.mtu, Some(None));
        assert_eq!(options.power, Some(false));

        let options = parse_one(&["--status", "--dns", "off", "--mtu=1400"]);
        assert_eq!(options.dns, Some(false));
        assert_eq!(options.mtu, Some(Some(1400)));
    }

    #[test]
    fn invalid_overrides_are_errors() {
        assert!(parse(&["--dns=yes-please".into()]).is_err());
        assert!(parse(&["--mtu=999".into()]).is_err());
        assert!(parse(&["--power".into()]).is_ok(), "bare flag is ignored");
    }

    #[test]
    fn exit_codes_are_distinct() {
        assert_ne!(EXIT_OK, EXIT_DRIFT);
        assert_ne!(EXIT_OK, EXIT_ERROR);
        assert_ne!(EXIT_DRIFT, EXIT_ERROR);
    }

    #[test]
    fn profile_operations_parse() {
        let save = parse_one(&["--profile", "save", "lan"]);
        assert_eq!(save.command, Command::Profile);
        assert_eq!(
            save.profile,
            Some(ProfileAction::Save { name: "lan".into() })
        );

        let apply = parse_one(&["--profile", "apply", "gaming"]);
        assert_eq!(
            apply.profile,
            Some(ProfileAction::Apply {
                name: "gaming".into()
            })
        );

        let list = parse_one(&["--profile", "list"]);
        assert_eq!(list.profile, Some(ProfileAction::List));

        let delete = parse_one(&["--profile", "delete", "lan"]);
        assert_eq!(
            delete.profile,
            Some(ProfileAction::Delete { name: "lan".into() })
        );
    }

    #[test]
    fn profile_names_with_path_tricks_are_rejected() {
        assert!(parse(&["--profile".into(), "save".into(), "../evil".into()]).is_err());
        assert!(parse(&["--profile".into(), "apply".into(), "bad".into()]).is_ok());
    }

    #[test]
    fn undo_parses_as_action() {
        assert_eq!(parse_one(&["--undo"]).command, Command::Undo);
        assert!(
            parse(&["--profile".into()]).is_err(),
            "bare --profile needs an operation"
        );
    }
}
