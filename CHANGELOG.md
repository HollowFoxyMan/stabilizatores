# Changelog

All notable changes to `stabilizatores` are documented in this file.

## [0.2.0] - 2026-09-19

### Added

- Non-interactive commands: `--apply`, `--revert`, `--status`, `--plan`
  (dry-run), `--report` (diagnostic JSON snapshot), `--undo` (revert the last
  apply and restore its archive snapshot), `--restore-point` and
  `--profile save|apply|list|delete`. Exit codes `0` (aligned), `1` (drift),
  `2` (error). Applying/reverting/undoing self-elevate; the rest are read-only.
- One-run CLI overrides for the interactive config: `--dns=on|off`,
  `--mtu=off|1280..9000`, `--power=on|off` (also as `--key value`), applied
  without persisting.
- DNS provider selection: `cloudflare` (default), `google` and `auto` (picks
  the faster of the two from the live probes). Persisted as `dns_provider`.
- DNS-over-HTTPS on Windows 11: configures `netsh dns add encryption` when
  the `netsh dns` context supports it, detected once and cached; Windows 10
  ignores it gracefully.
- MTU normalization: cycles the IPv4 MTU of every physical adapter through
  off / 1500 / 1400 (`u` key), archived per adapter GUID in the registry and
  reverted exactly (absent links stay absent).
- NIC power-down removal (`p` key, `--power`): sets the `PnPCapabilities` bit
  that stops network adapters from powering down at idle.
- Latency probe now reports peak-to-peak jitter, packet loss and a sparkline.
- Real DNS query probe (UDP 53) per resolver via BEEF-transaction UDP; used
  by `auto` DNS selection as well.
- TCP connection counters in the menu (`GetExtendedTcpTable`): total,
  established, TIME-WAIT.
- Per-adapter traffic meter in the detail view (`GetIfTable2`), download and
  upload rates.
- Wi-Fi diagnostics per adapter (`WlanQueryInterface`): SSID, BSSID, signal
  bar, RSSI, channel and link speeds.
- System Restore point creation (`s` key / `--restore-point`) via
  `srclient.dll` before tweaking.
- Config profiles under `%APPDATA%\stabilizatores\profiles\`; archive
  snapshots taken before each apply (last 8 kept) so `--undo` can restore
  the pre-apply archive.
- Action log viewer (`l` key) showing the last log lines in the menu.
- Per-adapter detail view (`i` key): MTU, MAC, IPs, gateways, DNS and DNS6.
- Timestamped action log at
  `%APPDATA%\stabilizatores\logs\stabilizatores.log` (1 MiB, rotated once).
- Atomic (temp file + rename) writes for `config.json` and `archive.json`.
- Adapter enumeration now also reports IPs, gateways and MAC addresses.

## [0.1.0] - 2026-09-19

Initial release.

### Added

- Interactive VT terminal menu in the style of lunar-adblock with per-feature
  toggles and status reporting.
- TCP/IP parameter tuning (registry): `TCPNoDelay`, `TcpAckFrequency`,
  `TcpDelAckTicks` per interface; `TcpTimedWaitDelay`, `MaxUserPort` global.
- DNS optimization through `netsh`: switches active adapters to the
  Cloudflare resolvers `1.1.1.1` / `1.0.0.1` (IPv6 `2606:4700:4700::1111`
  and `::1001`) and restores the prior configuration exactly on revert.
- Network throttling removal (`NetworkThrottlingIndex = 0xFFFFFFFF`).
- Self-heal loop that re-applies enabled tweaks every five seconds.
- JSON archive of the original machine state (`%APPDATA%\stabilizatores`) and
  a one-key `x` revert path.
- Background ICMP latency probes for Cloudflare and Google resolvers.
- DNS cache flush (via `dnsapi`), optional autostart, elevation prompt and
  `--help` / `--version` flags.
- Console mode and window title are restored on exit.
- CI workflow (build, test, clippy, rustfmt) on GitHub Actions.