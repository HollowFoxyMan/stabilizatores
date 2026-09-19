//! The interactive terminal application.
//!
//! Draws a lunar-adblock style TUI: a full-screen redraw of a status panel
//! with per-feature toggles bound to single keys. The loop polls the raw
//! console input, re-applies enabled tweaks on a timer (self-heal), refreshes
//! latency probes in the background and redraws every second.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{self, Archive, Config, DnsProvider};
use crate::interfaces::Interface;
use crate::{connections, dns, interfaces, mtu, power, probe, system, tcp, traffic, win, wlan};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const C_RESET: &str = "\x1b[0m";
const C_BOLD: &str = "\x1b[1m";
const C_DIM: &str = "\x1b[2m";
const C_RED: &str = "\x1b[31m";
const C_GREEN: &str = "\x1b[32m";
const C_YELLOW: &str = "\x1b[33m";
const C_CYAN: &str = "\x1b[36m";

/// Presets for the MTU `u` key cycle: off -> 1500 -> 1400 -> off.
const MTU_CYCLE: &[Option<u32>] = &[None, Some(1500), Some(1400)];

pub struct App {
    cfg: Config,
    archive: Archive,
    last_error: Option<String>,
    flash: Option<(String, Instant)>,
    tcp: tcp::TcpStatus,
    dns: dns::DnsStatus,
    throttling: bool,
    /// Whether the MTU tweak is in its target state right now.
    mtu_ok: bool,
    /// Whether NICs currently disallow power-down.
    power_ok: bool,
    physical: Vec<Interface>,
    active_ifaces: usize,
    confirming_quit: bool,
    quit_removes: bool,
    quit: bool,
    /// Resolver set currently applied, used to align the self-heal checks.
    active_dns: Option<dns::Resolvers>,
    /// Per-adapter detail pane open.
    detail: bool,
    /// Action log viewer open.
    show_log: bool,
    /// Live TCP connection statistics.
    connections: connections::TcpConnections,
    /// Previous traffic counter snapshot; rates derive from its diff.
    traffic_prev: Option<traffic::Snapshot>,
    /// `(index, down B/s, up B/s)` for the last glance window.
    traffic_rates: Vec<(u32, f64, f64)>,
    /// Wi-Fi link diagnostics per wireless interface.
    wlan: Vec<wlan::WlanLink>,
    latency: Arc<Mutex<Option<probe::Latency>>>,
    probe_stop: Arc<AtomicBool>,
}

impl App {
    /// Creates the struct without touching the system.
    fn base(cfg: Config, archive: Archive) -> Self {
        Self {
            cfg,
            archive,
            last_error: None,
            flash: None,
            tcp: tcp::TcpStatus::default(),
            dns: dns::DnsStatus::default(),
            throttling: false,
            mtu_ok: true,
            power_ok: true,
            physical: Vec::new(),
            active_ifaces: 0,
            confirming_quit: false,
            quit_removes: false,
            quit: false,
            active_dns: None,
            detail: false,
            show_log: false,
            connections: connections::TcpConnections::default(),
            traffic_prev: None,
            traffic_rates: Vec::new(),
            wlan: Vec::new(),
            latency: Arc::new(Mutex::new(None)),
            probe_stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Real entry point: applies every enabled feature, then reads the state.
    pub fn new(cfg: Config, archive: Archive) -> Self {
        let mut app = Self::base(cfg, archive);
        app.apply_enabled();
        app.refresh();
        app
    }

    /// Test view only: skips the side effects, still reflects live status.
    #[cfg(test)]
    fn new_view(cfg: Config, archive: Archive) -> Self {
        let mut app = Self::base(cfg, archive);
        app.refresh();
        app
    }

    pub fn run(&mut self) {
        let console = win::Console::setup();
        let vt = console.as_ref().map(|console| console.vt).unwrap_or(false);
        win::install_ctrl_handler();
        win::set_title(&format!("stabilizatores {VERSION}"));

        let probe_state = Arc::clone(&self.latency);
        let probe_stop = Arc::clone(&self.probe_stop);
        probe::spawn(probe_state, probe_stop);

        let mut stdout = std::io::stdout();
        self.render(&mut stdout, vt);

        if !vt {
            let _ = writeln!(
                stdout,
                "stabilizatores running, tweaks active. close this window or press q to stop."
            );
        }

        let mut last_tick = Instant::now();
        let mut last_heal = Instant::now();
        let mut last_render = Instant::now();

        while !self.quit && !win::exit_requested() {
            if let Some(console) = &console {
                while let Some(key) = console.poll_key() {
                    self.key(key);
                    if self.quit {
                        break;
                    }
                }
            }

            let now = Instant::now();
            if now.duration_since(last_tick) >= Duration::from_millis(250) {
                last_tick = now;
                self.expire_flash();
                if now.duration_since(last_heal) >= Duration::from_secs(5) {
                    last_heal = now;
                    self.heal();
                }
                if now.duration_since(last_render) >= Duration::from_secs(1) {
                    last_render = now;
                    self.render(&mut stdout, vt);
                }
            }
            std::thread::sleep(Duration::from_millis(40));
        }

        self.probe_stop.store(true, Ordering::SeqCst);

        let _ = if self.quit_removes {
            self.revert_all();
            writeln!(stdout, "\ntweaks removed, system restored.")
        } else {
            writeln!(stdout, "\nbye, tweaks kept.")
        };
        if self.quit_removes {
            if let Some(error) = &self.last_error {
                let _ = writeln!(stdout, "{C_YELLOW}warning{C_RESET}: {error}");
            }
        }
        let _ = writeln!(stdout, "press enter to close...");
        if let Some(console) = &console {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if console.poll_key().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        } else {
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn key(&mut self, key: char) {
        if self.confirming_quit {
            match key.to_ascii_lowercase() {
                'y' => self.quit = true,
                'n' | '\x1b' => self.confirming_quit = false,
                _ => {}
            }
            return;
        }
        match key.to_ascii_lowercase() {
            't' => self.toggle_tcp(),
            'd' => self.toggle_dns(),
            'm' => self.toggle_multimedia(),
            'u' => self.cycle_mtu(),
            'p' => self.toggle_power(),
            'r' => self.cycle_provider(),
            'i' => self.detail = !self.detail,
            'l' => self.show_log = !self.show_log,
            'h' => {
                self.cfg.self_heal = !self.cfg.self_heal;
                self.save_cfg();
            }
            'a' => {
                let next = !win::autostart_enabled();
                match win::set_autostart(next) {
                    Ok(()) => self.last_error = None,
                    Err(error) => self.last_error = Some(format!("autostart: {error}")),
                }
            }
            'f' => {
                win::flush_dns_cache();
                self.flash = Some(("dns cache flushed".to_string(), Instant::now()));
            }
            's' => {
                let label = format!("stabilizatores before tweaks v{VERSION}");
                match crate::restore::create(&label) {
                    Ok(()) => {
                        self.flash = Some(("restore point created".to_string(), Instant::now()));
                    }
                    Err(error) => self.last_error = Some(format!("restore point: {error}")),
                }
            }
            'q' => {
                self.quit_removes = false;
                self.confirming_quit = true;
            }
            'x' => {
                self.quit_removes = true;
                self.confirming_quit = true;
            }
            _ => {}
        }
    }

    fn toggle_tcp(&mut self) {
        self.cfg.tcp = !self.cfg.tcp;
        let save = config::save(&self.cfg);
        let result = if self.cfg.tcp {
            tcp::apply(&mut self.archive).map(|_| ())
        } else {
            tcp::revert(&mut self.archive)
        };
        self.apply_result("tcp", result);
        self.report_save(save);
        self.refresh();
    }

    fn toggle_dns(&mut self) {
        self.cfg.dns = !self.cfg.dns;
        let save = config::save(&self.cfg);
        let result = if self.cfg.dns {
            let resolvers = self.resolve_dns();
            dns::apply(&mut self.archive, &resolvers).map(|()| self.active_dns = Some(resolvers))
        } else {
            let last = self.active_dns;
            self.active_dns = None;
            dns::revert(&mut self.archive, last.as_ref())
        };
        self.apply_result("dns", result);
        self.report_save(save);
        self.refresh();
    }

    fn toggle_multimedia(&mut self) {
        self.cfg.multimedia = !self.cfg.multimedia;
        let save = config::save(&self.cfg);
        let result = if self.cfg.multimedia {
            system::apply(&mut self.archive)
        } else {
            system::revert(&mut self.archive)
        };
        self.apply_result("throttling", result);
        self.report_save(save);
        self.refresh();
    }

    fn cycle_mtu(&mut self) {
        let next = next_in_cycle(&self.cfg.mtu, MTU_CYCLE);
        self.cfg.mtu = next;
        let save = config::save(&self.cfg);
        let result = match next {
            Some(target) => mtu::apply(&mut self.archive, target),
            None => mtu::revert(&mut self.archive),
        };
        self.apply_result("mtu", result);
        self.report_save(save);
        self.refresh();
    }

    fn toggle_power(&mut self) {
        self.cfg.power = !self.cfg.power;
        let save = config::save(&self.cfg);
        let result = if self.cfg.power {
            power::apply(&mut self.archive)
        } else {
            power::revert(&mut self.archive)
        };
        self.apply_result("power", result);
        self.report_save(save);
        self.refresh();
    }

    fn cycle_provider(&mut self) {
        let next = next_provider(self.cfg.dns_provider);
        self.cfg.dns_provider = next;
        let save = config::save(&self.cfg);
        let result = if self.cfg.dns {
            let resolvers = self.resolve_dns();
            dns::apply(&mut self.archive, &resolvers).map(|()| self.active_dns = Some(resolvers))
        } else {
            Ok(())
        };
        match result {
            Ok(()) => self.last_error = None,
            Err(error) => self.last_error = Some(format!("dns: {error}")),
        }
        self.report_save(save);
        self.refresh();
    }

    fn save_cfg(&mut self) {
        self.report_save(config::save(&self.cfg));
    }

    fn report_save(&mut self, result: Result<(), std::io::Error>) {
        if let Err(error) = result {
            self.last_error = Some(format!("save config: {error}"));
        }
    }

    fn apply_result(&mut self, label: &str, result: Result<(), String>) {
        match result {
            Ok(()) => self.last_error = None,
            Err(error) => self.last_error = Some(format!("{label}: {error}")),
        }
    }

    /// The resolver set the DNS feature should use right now. For `Auto` the
    /// freshest latency measurement decides; without one Cloudflare wins.
    fn resolve_dns(&self) -> dns::Resolvers {
        dns::resolve_provider(self.cfg.dns_provider, self.latency().as_ref())
    }

    fn latency(&self) -> Option<probe::Latency> {
        self.latency.lock().ok().and_then(|slot| slot.clone())
    }

    /// Applies every enabled tweak group at startup.
    fn apply_enabled(&mut self) {
        let anything = self.cfg.tcp
            || self.cfg.dns
            || self.cfg.multimedia
            || self.cfg.mtu.is_some()
            || self.cfg.power;
        if anything {
            config::take_archive_snapshot();
        }
        if self.cfg.tcp {
            match tcp::apply(&mut self.archive) {
                Ok(status) => self.tcp = status,
                Err(error) => self.last_error = Some(format!("tcp: {error}")),
            }
        }
        if self.cfg.dns {
            let resolvers = self.resolve_dns();
            match dns::apply(&mut self.archive, &resolvers) {
                Ok(()) => self.active_dns = Some(resolvers),
                Err(error) => self.last_error = Some(format!("dns: {error}")),
            }
        }
        if self.cfg.multimedia {
            if let Err(error) = system::apply(&mut self.archive) {
                self.last_error = Some(format!("throttling: {error}"));
            }
        }
        if let Some(target) = self.cfg.mtu {
            if let Err(error) = mtu::apply(&mut self.archive, target) {
                self.last_error = Some(format!("mtu: {error}"));
            }
        }
        if self.cfg.power {
            if let Err(error) = power::apply(&mut self.archive) {
                self.last_error = Some(format!("power: {error}"));
            }
        }
    }

    /// Re-applies enabled tweak groups that have drifted out of state.
    fn heal(&mut self) {
        if !self.cfg.self_heal {
            return;
        }
        let mut errors: Vec<String> = Vec::new();
        if self.cfg.tcp && !self.tcp.aligned() {
            if let Err(error) = tcp::apply(&mut self.archive).map(|_| ()) {
                errors.push(format!("tcp: {error}"));
            }
        }
        if self.cfg.dns {
            let resolvers = self.resolve_dns();
            match self.active_dns {
                // With Auto a faster provider may have emerged; adopt it.
                Some(active) if active.provider != resolvers.provider => {
                    match dns::apply(&mut self.archive, &resolvers) {
                        Ok(()) => self.active_dns = Some(resolvers),
                        Err(error) => errors.push(format!("dns: {error}")),
                    }
                }
                _ if !self.dns.aligned_all() => match dns::apply(&mut self.archive, &resolvers) {
                    Ok(()) => self.active_dns = Some(resolvers),
                    Err(error) => errors.push(format!("dns: {error}")),
                },
                _ => {}
            }
        }
        if self.cfg.multimedia && !self.throttling {
            if let Err(error) = system::apply(&mut self.archive) {
                errors.push(format!("throttling: {error}"));
            }
        }
        if let Some(target) = self.cfg.mtu {
            if !self.mtu_ok {
                if let Err(error) = mtu::apply(&mut self.archive, target) {
                    errors.push(format!("mtu: {error}"));
                }
            }
        }
        if self.cfg.power && !self.power_ok {
            if let Err(error) = power::apply(&mut self.archive) {
                errors.push(format!("power: {error}"));
            }
        }
        if errors.is_empty() {
            self.last_error = None;
        } else {
            self.last_error = Some(errors.join(" | "));
        }
        self.refresh();
    }

    /// Reads the current state of every feature from the system.
    fn refresh(&mut self) {
        let resolvers = self.resolve_dns();
        self.tcp = tcp::status();
        self.dns = dns::status(&resolvers);
        self.throttling = system::status();
        self.mtu_ok = self.cfg.mtu.is_none_or(mtu::status);
        self.power_ok = power::status();
        self.physical = interfaces::physical();
        self.active_ifaces = interfaces::active().len();
        self.connections = connections::read();
        let now = traffic::sample();
        if let Some(prev) = &self.traffic_prev {
            if now.taken_at.duration_since(prev.taken_at) >= Duration::from_secs(1) {
                self.traffic_rates = traffic::rate(prev, &now);
            }
        }
        self.traffic_prev = Some(now);
        self.wlan = wlan::links();
    }

    /// Reverts every feature that was applied and clears autostart.
    fn revert_all(&mut self) {
        let mut errors: Vec<String> = Vec::new();
        if let Err(error) = mtu::revert(&mut self.archive) {
            errors.push(format!("mtu: {error}"));
        }
        if self.cfg.tcp {
            if let Err(error) = tcp::revert(&mut self.archive) {
                errors.push(format!("tcp: {error}"));
            }
        }
        if self.cfg.dns {
            let last = self.active_dns;
            if let Err(error) = dns::revert(&mut self.archive, last.as_ref()) {
                errors.push(format!("dns: {error}"));
            }
        }
        if self.cfg.multimedia {
            if let Err(error) = system::revert(&mut self.archive) {
                errors.push(format!("throttling: {error}"));
            }
        }
        if self.cfg.power {
            if let Err(error) = power::revert(&mut self.archive) {
                errors.push(format!("power: {error}"));
            }
        }
        if win::autostart_enabled() {
            let _ = win::set_autostart(false);
        }
        self.last_error = if errors.is_empty() {
            None
        } else {
            Some(errors.join(" | "))
        };
    }

    fn all_aligned(&self) -> bool {
        (!self.cfg.tcp || self.tcp.aligned())
            && (!self.cfg.dns || self.dns.aligned_all())
            && (!self.cfg.multimedia || self.throttling)
            && self.cfg.mtu.is_none_or(mtu::status)
            && (!self.cfg.power || self.power_ok)
    }

    fn overall_state(&self) -> (&'static str, &'static str) {
        if self.last_error.is_some() {
            ("ERROR", C_RED)
        } else if !self.all_aligned() {
            ("PENDING", C_YELLOW)
        } else if !self.cfg.tcp
            && !self.cfg.dns
            && !self.cfg.multimedia
            && self.cfg.mtu.is_none()
            && !self.cfg.power
        {
            ("READY", C_YELLOW)
        } else if self.cfg.self_heal {
            ("PROTECTED", C_GREEN)
        } else {
            ("ACTIVE", C_GREEN)
        }
    }

    fn expire_flash(&mut self) {
        if let Some((_, at)) = &self.flash {
            if at.elapsed() >= Duration::from_secs(3) {
                self.flash = None;
            }
        }
    }

    fn render(&self, stdout: &mut impl Write, vt: bool) {
        if !vt {
            return;
        }

        let (state, state_color) = self.overall_state();
        let tcp_status = if !self.cfg.tcp {
            format!("{C_DIM}tuning off{C_RESET}")
        } else if self.tcp.aligned() {
            format!(
                "{C_GREEN}tuned {}/{} ifaces{C_RESET}",
                self.tcp.ifaces_ok, self.tcp.ifaces_total
            )
        } else {
            format!(
                "{C_YELLOW}restoring {}/{} ifaces{C_RESET}",
                self.tcp.ifaces_ok, self.tcp.ifaces_total
            )
        };
        let tcp_global = if self.cfg.tcp && self.tcp.global_ok {
            format!("{C_GREEN}{C_BOLD}[x]{C_RESET}")
        } else {
            format!("{C_DIM}{C_BOLD}[ ]{C_RESET}")
        };
        let resolvers = self.resolve_dns();
        let dns_status = if !self.cfg.dns {
            format!("{C_DIM}off{C_RESET}")
        } else if self.dns.aligned_all() {
            format!(
                "{C_GREEN}{}/{} aligned{C_RESET}",
                self.dns.aligned, self.dns.total
            )
        } else {
            format!(
                "{C_YELLOW}{}/{} aligned{C_RESET}",
                self.dns.aligned, self.dns.total
            )
        };
        let throttle_status = if !self.cfg.multimedia {
            format!("{C_DIM}left as is{C_RESET}")
        } else if self.throttling {
            format!("{C_GREEN}removed{C_RESET}")
        } else {
            format!("{C_YELLOW}pending{C_RESET}")
        };
        let mtu_status = match self.cfg.mtu {
            None => format!("{C_DIM}off{C_RESET}"),
            Some(target) if self.mtu_ok => format!("{C_GREEN}{target} set{C_RESET}"),
            Some(target) => format!("{C_YELLOW}{target} pending{C_RESET}"),
        };
        let power_status = if !self.cfg.power {
            format!("{C_DIM}left as is{C_RESET}")
        } else if self.power_ok {
            format!("{C_GREEN}no power-down{C_RESET}")
        } else {
            format!("{C_YELLOW}pending{C_RESET}")
        };

        let mut out = String::new();
        out.push_str("\x1b[2J\x1b[H");
        out.push_str(&format!(
            "{C_BOLD}{C_CYAN}  STABILIZATORES{C_RESET}  {C_DIM}v{VERSION}{C_RESET}\n"
        ));
        out.push_str("  -------------------------------------------------\n");
        out.push_str(&format!(
            "  {C_DIM}status{C_RESET}      {state_color}{C_BOLD}{state}{C_RESET}\n"
        ));
        out.push_str(&format!(
            "  {C_DIM}interfaces{C_RESET}  {} physical, {} active\n",
            self.physical.len(),
            self.active_ifaces
        ));
        if !self.physical.is_empty() {
            let labels = self
                .physical
                .iter()
                .take(4)
                .map(|iface| {
                    let name = if !iface.friendly.is_empty() {
                        &iface.friendly
                    } else if !iface.description.is_empty() {
                        &iface.description
                    } else {
                        &iface.guid
                    };
                    let mtu = if iface.mtu > 0 {
                        format!("{C_DIM} (MTU {}){C_RESET}", iface.mtu)
                    } else {
                        String::new()
                    };
                    format!("{C_CYAN}{name}{C_RESET}{mtu}")
                })
                .collect::<Vec<_>>()
                .join("  ");
            out.push_str(&format!("  {C_DIM}adapters{C_RESET}    {labels}\n"));
        }
        out.push_str(&format!(
            "  {C_DIM}tcp{C_RESET}         {}  {tcp_status}  global {tcp_global}\n",
            check(self.cfg.tcp)
        ));
        out.push_str(&format!(
            "  {C_DIM}dns{C_RESET}         {}  {} {dns_status}   {C_DIM}({C_RESET}{C_CYAN}{}{C_RESET} / {C_CYAN}{}{C_RESET}{C_DIM}){C_RESET}\n",
            check(self.cfg.dns),
            provider_label(resolvers.provider),
            resolvers.primary,
            resolvers.secondary,
        ));
        out.push_str(&format!(
            "  {C_DIM}throttling{C_RESET}  {}  {throttle_status}\n",
            check(self.cfg.multimedia)
        ));
        out.push_str(&format!(
            "  {C_DIM}mtu{C_RESET}         {}  {mtu_status}\n",
            check(self.cfg.mtu.is_some())
        ));
        out.push_str(&format!(
            "  {C_DIM}power{C_RESET}       {}  {power_status}\n",
            check(self.cfg.power)
        ));
        out.push_str(&format!(
            "  {C_DIM}latency{C_RESET}     {}\n",
            latency_line(&self.latency)
        ));
        let dns_probe = dns_probe_line(&self.latency);
        if !dns_probe.is_empty() {
            out.push_str(&dns_probe);
        }
        out.push_str(&format!(
            "  {C_DIM}connections{C_RESET} {} total  {} est  {} tw\n",
            self.connections.total, self.connections.established, self.connections.time_wait
        ));
        if let Some(wifi) = self.wlan.iter().find(|link| link.connected) {
            out.push_str(&format!(
                "  {C_DIM}wi-fi{C_RESET}      {}  {}  ch {}{}  {} / {}",
                wifi.ssid,
                wlan::signal_bar(wifi.signal_quality),
                wifi.channel
                    .map(|ch| ch.to_string())
                    .unwrap_or_else(|| "?".into()),
                wifi.rssi_dbm
                    .map(|rssi| format!("  {rssi} dBm"))
                    .unwrap_or_default(),
                wlan::format_kbps(wifi.rx_kbps),
                wlan::format_kbps(wifi.tx_kbps),
            ));
            out.push('\n');
        }
        if let Some(error) = &self.last_error {
            out.push_str(&format!("  {C_YELLOW}error{C_RESET}       {error}\n"));
        }
        if let Some((text, _)) = &self.flash {
            out.push_str(&format!("  {C_GREEN}{text}{C_RESET}\n"));
        }
        out.push_str("  -------------------------------------------------\n");
        out.push_str(&format!(
            "  {C_DIM}t{C_RESET} tcp {}    {C_DIM}d{C_RESET} dns {}    {C_DIM}m{C_RESET} throttling {}    {C_DIM}h{C_RESET} self-heal {}\n",
            check(self.cfg.tcp),
            check(self.cfg.dns),
            check(self.cfg.multimedia),
            check(self.cfg.self_heal),
        ));
        out.push_str(&format!(
            "  {C_DIM}r{C_RESET} dns {}    {C_DIM}u{C_RESET} mtu {}    {C_DIM}p{C_RESET} power {}    {C_DIM}s{C_RESET} restore point\n",
            provider_label(self.cfg.dns_provider),
            mtu_label(self.cfg.mtu),
            check(self.cfg.power),
        ));
        out.push_str(&format!(
            "  {C_DIM}a{C_RESET} autostart {}    {C_DIM}f{C_RESET} flush dns    {C_DIM}i{C_RESET} details    {C_DIM}l{C_RESET} log\n",
            check(win::autostart_enabled())
        ));
        out.push_str(&format!(
            "  {C_DIM}q{C_RESET} quit    {C_DIM}x{C_RESET} revert + quit\n",
        ));
        if self.detail {
            self.render_detail(&mut out);
        }
        if self.show_log {
            self.render_log(&mut out);
        }
        if self.confirming_quit {
            let text = if self.quit_removes {
                "revert every tweak and exit? (y/n)"
            } else {
                "exit and keep tweaks? (y/n)"
            };
            out.push_str(&format!("  {C_YELLOW}{text}{C_RESET}\n"));
        }
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }

    /// The last action-log lines when the log view is open.
    fn render_log(&self, out: &mut String) {
        out.push_str(&format!(
            "  {C_CYAN}{C_BOLD}action log{C_RESET}{C_DIM} (last 12){C_RESET}\n"
        ));
        let lines = crate::log::read(12);
        if lines.is_empty() {
            out.push_str(&format!("  {C_DIM}nothing logged yet{C_RESET}\n"));
        } else {
            for line in lines {
                out.push_str(&format!("  {C_DIM}>{C_RESET} {line}\n"));
            }
        }
    }

    /// One paragraph per physical adapter when the detail view is open.
    fn render_detail(&self, out: &mut String) {
        for iface in &self.physical {
            let name = if !iface.friendly.is_empty() {
                &iface.friendly
            } else if !iface.description.is_empty() {
                &iface.description
            } else {
                &iface.guid
            };
            out.push_str(&format!("  {C_CYAN}{name}{C_RESET}\n"));
            let mut flags = Vec::new();
            flags.push(if iface.up { "up" } else { "down" }.to_string());
            flags.push(if iface.dhcp {
                "dhcp".to_string()
            } else {
                "static".to_string()
            });
            if iface.mtu > 0 {
                flags.push(format!("mtu {}", iface.mtu));
            }
            if !iface.mac.is_empty() {
                flags.push(iface.mac.clone());
            }
            out.push_str(&format!("    {} {}\n", iface.description, flags.join("  ")));
            let rates = self
                .traffic_rates
                .iter()
                .find(|rate| rate.0 == iface.index)
                .copied();
            if let Some((_, down, up)) = rates {
                out.push_str(&format!(
                    "    {C_DIM}traffic{C_RESET}    {}{}  {}{}\n",
                    "\u{2193}",
                    traffic::format_bytes_per_second(down),
                    "\u{2191}",
                    traffic::format_bytes_per_second(up)
                ));
            }
            if let Some(link) = self
                .wlan
                .iter()
                .find(|link| link.guid.eq_ignore_ascii_case(&iface.guid))
            {
                out.push_str(&format!(
                    "    {C_DIM}wi-fi{C_RESET}     {}  {}  {}{}  rx {}  tx {}\n",
                    link.ssid,
                    wlan::signal_bar(link.signal_quality),
                    link.channel
                        .map(|ch| format!("ch {ch}"))
                        .unwrap_or_else(|| "ch ?".into()),
                    link.rssi_dbm
                        .map(|rssi| format!("  {rssi} dBm"))
                        .unwrap_or_default(),
                    wlan::format_kbps(link.rx_kbps),
                    wlan::format_kbps(link.tx_kbps),
                ));
            }
            if !iface.ips.is_empty() {
                out.push_str(&format!(
                    "    {C_DIM}ip{C_RESET}       {}\n",
                    iface.ips.join(", ")
                ));
            }
            if !iface.gateways.is_empty() {
                out.push_str(&format!(
                    "    {C_DIM}gateway{C_RESET}   {}\n",
                    iface.gateways.join(", ")
                ));
            }
            if !iface.dns.is_empty() {
                out.push_str(&format!(
                    "    {C_DIM}dns{C_RESET}       {}\n",
                    iface.dns.join(", ")
                ));
            }
            if !iface.dns6.is_empty() {
                out.push_str(&format!(
                    "    {C_DIM}dns6{C_RESET}      {}\n",
                    iface.dns6.join(", ")
                ));
            }
        }
    }
}

fn check(on: bool) -> String {
    if on {
        format!("{C_GREEN}[x]{C_RESET}")
    } else {
        format!("{C_DIM}[ ]{C_RESET}")
    }
}

fn provider_label(provider: DnsProvider) -> &'static str {
    match provider {
        DnsProvider::Cloudflare => "cloudflare",
        DnsProvider::Google => "google",
        DnsProvider::Auto => "auto",
    }
}

fn mtu_label(mtu: Option<u32>) -> String {
    match mtu {
        Some(target) => format!("{target}"),
        None => "off".to_string(),
    }
}

/// Steps the DNS provider forward: cloudflare -> google -> auto -> cloudflare.
fn next_provider(provider: DnsProvider) -> DnsProvider {
    match provider {
        DnsProvider::Cloudflare => DnsProvider::Google,
        DnsProvider::Google => DnsProvider::Auto,
        DnsProvider::Auto => DnsProvider::Cloudflare,
    }
}

/// Steps `value` through `cycle`, wrapping around at both ends. `value` must
/// be one of the cycle's members (which holds for anything the menu sets).
fn next_in_cycle(value: &Option<u32>, cycle: &[Option<u32>]) -> Option<u32> {
    let index = cycle.iter().position(|entry| entry == value).unwrap_or(0);
    cycle[(index + 1) % cycle.len()]
}

fn latency_line(latency: &Mutex<Option<probe::Latency>>) -> String {
    fn one(endpoint: &probe::Endpoint, label: &str) -> String {
        let mut text = format!("{C_DIM}{label}{C_RESET} ");
        match endpoint.rtt_ms() {
            Some(rtt) if rtt > 0 => {
                text.push_str(&format!("{C_GREEN}{rtt} ms{C_RESET}"));
                if let Some(jitter) = endpoint.jitter_ms() {
                    text.push_str(&format!(" {C_DIM}\u{b1}{jitter}{C_RESET}"));
                }
            }
            _ => text.push_str(&format!("{C_DIM}n/a{C_RESET}")),
        }
        if endpoint.total() > 0 {
            text.push_str(&format!(
                " {C_DIM}{}%{C_RESET}",
                endpoint.loss() * 100 / endpoint.total()
            ));
        }
        let sparkline = endpoint.sparkline();
        if !sparkline.is_empty() {
            text.push_str(&format!(" {C_CYAN}{sparkline}{C_RESET}"));
        }
        text
    }
    let Ok(slot) = latency.lock() else {
        return format!("{C_DIM}n/a{C_RESET}");
    };
    let Some(latency) = slot.clone() else {
        return format!("{C_DIM}measuring...{C_RESET}");
    };
    format!(
        "{}   {}",
        one(&latency.cloudflare, "cloudflare"),
        one(&latency.google, "google")
    )
}

fn dns_probe_line(latency: &Mutex<Option<probe::Latency>>) -> String {
    let Ok(slot) = latency.lock() else {
        return String::new();
    };
    let Some(latency) = slot.clone() else {
        return String::new();
    };
    let cloudflare = latency.cf_dns_ms.map_or_else(
        || format!("{C_DIM}n/a{C_RESET}"),
        |ms| format!("{C_GREEN}{ms} ms{C_RESET}"),
    );
    let google = latency.google_dns_ms.map_or_else(
        || format!("{C_DIM}n/a{C_RESET}"),
        |ms| format!("{C_GREEN}{ms} ms{C_RESET}"),
    );
    if latency.cf_dns_ms.is_none() && latency.google_dns_ms.is_none() {
        return String::new();
    }
    format!(
        "  {C_DIM}resolution{C_RESET} 1.1.1.1 {}   8.8.8.8 {}\n",
        cloudflare, google
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_with_all_features_disabled() {
        let cfg = Config {
            tcp: false,
            dns: false,
            multimedia: false,
            self_heal: false,
            dns_provider: DnsProvider::Cloudflare,
            mtu: None,
            power: false,
        };
        let app = App::new_view(cfg, Archive::default());
        let mut buffer = Vec::new();
        app.render(&mut buffer, true);
        let text = String::from_utf8(buffer).expect("render output is utf-8");
        assert!(text.contains("STABILIZATORES"), "title is rendered");
        assert!(text.contains("tuning off"), "disabled tcp shows off state");
        assert!(
            text.contains("READY"),
            "no enabled feature shows ready state"
        );
    }

    #[test]
    fn render_shows_mtu_and_provider() {
        let cfg = Config {
            tcp: false,
            dns: true,
            multimedia: false,
            self_heal: false,
            dns_provider: DnsProvider::Cloudflare,
            mtu: Some(1500),
            power: false,
        };
        let app = App::new_view(cfg, Archive::default());
        let mut buffer = Vec::new();
        app.render(&mut buffer, true);
        let text = String::from_utf8(buffer).expect("render output is utf-8");
        assert!(
            text.contains("1.1.1.1") && text.contains("1.0.0.1"),
            "the active resolver set is shown"
        );
        assert!(text.contains("cloudflare"), "provider label is shown");
        assert!(text.contains("1500"), "mtu target is shown");
    }

    #[test]
    fn overall_state_with_no_tweaks() {
        let cfg = Config {
            tcp: false,
            dns: false,
            multimedia: false,
            self_heal: false,
            dns_provider: DnsProvider::Cloudflare,
            mtu: None,
            power: false,
        };
        let app = App::new_view(cfg, Archive::default());
        assert!(app.all_aligned(), "disabled features are always aligned");
    }

    #[test]
    fn mtu_cycle_steps_and_wraps() {
        assert_eq!(next_in_cycle(&None, MTU_CYCLE), Some(1500));
        assert_eq!(next_in_cycle(&Some(1500), MTU_CYCLE), Some(1400));
        assert_eq!(next_in_cycle(&Some(1400), MTU_CYCLE), None);
    }

    #[test]
    fn provider_cycle_matches_docs_order() {
        let mut provider = DnsProvider::Cloudflare;
        let mut order = Vec::new();
        for _ in 0..6 {
            order.push(provider);
            provider = next_provider(provider);
        }
        assert_eq!(
            order,
            vec![
                DnsProvider::Cloudflare,
                DnsProvider::Google,
                DnsProvider::Auto,
                DnsProvider::Cloudflare,
                DnsProvider::Google,
                DnsProvider::Auto,
            ]
        );
    }

    #[test]
    fn latency_line_measures() {
        let latency = Mutex::new(Some(probe::Latency {
            cloudflare: probe::Endpoint::with_rtt(Some(12)),
            google: probe::Endpoint::with_rtt(None),
            ..probe::Latency::default()
        }));
        let text = latency_line(&latency);
        assert!(text.contains("12 ms"), "reported rtt is shown");
        assert!(text.contains("n/a"), "missing probe is shown as n/a");
    }

    #[test]
    fn dns_probe_line_hidden_without_results() {
        let latency = Mutex::new(Some(probe::Latency::default()));
        assert_eq!(dns_probe_line(&latency), "");
    }

    #[test]
    fn dns_probe_line_shows_results() {
        let latency = Mutex::new(Some(probe::Latency {
            cf_dns_ms: Some(14),
            google_dns_ms: None,
            ..probe::Latency::default()
        }));
        let text = dns_probe_line(&latency);
        assert!(text.contains("14 ms"), "dns resolution rtt is shown");
        assert!(text.contains("1.1.1.1"));
    }
}
