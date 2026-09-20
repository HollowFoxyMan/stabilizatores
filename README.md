# stabilizatores

Optimizes the Windows network stack for lower latency and more consistent
throughput: TCP parameter tuning, a low-latency DNS resolver, removal of the
Windows network throttling and MTU normalization. Runs as an interactive
Terminal program with self-healing state and a one-key revert path that
restores the machine exactly, or as a non-interactive command line (`--apply`,
`--revert`, `--status`, `--plan`, `--report`, `--profile`, `--undo`).

Inspired by the console menu style of
[lunar-adblock](https://github.com/HollowFoxyMan/lunar-adblock).

## Features

Four tweak groups can be toggled at runtime and are re-applied automatically
while the program runs:

- **TCP tuning** — written into the registry (per-interface and machine-wide)
  so new TCP connections use the tuned parameters:

  | Value | Default | Target | Effect |
  |---|---|---|---|
  | `TCPNoDelay` (per interface) | off | `1` | disables Nagle's algorithm, small packets leave immediately |
  | `TcpAckFrequency` (per interface) | `2` | `1` | acknowledges every segment, shortens the RTT for small exchanges |
  | `TcpDelAckTicks` (per interface) | `2` | `0` | disables delayed ACK scheduling |
  | `TcpTimedWaitDelay` (global) | `240` s | `30` s | frees TIME-WAIT sockets faster, more ports available sooner |
  | `MaxUserPort` (global) | `16384` | `65534` | raises the ephemeral port ceiling |

- **DNS optimization** — switches every *active* physical adapter to a
  low-latency anycast resolver set. The provider is configurable:

  | Provider | IPv4 | IPv6 |
  |---|---|---|
  | `cloudflare` (default) | `1.1.1.1` / `1.0.0.1` | `2606:4700:4700::1111` / `::1001` |
  | `google` | `8.8.8.8` / `8.8.4.4` | `2001:4860:4860::8888` / `::8844` |
  | `auto` | whichever answers faster right now | same set |

  The previous adapter configuration — DHCP or static, IPv4 and IPv6 servers —
  is archived first, so reverting restores it exactly instead of forcing
  everything back to DHCP. On Windows 11 the feature additionally configures
  DNS-over-HTTPS (`netsh dns add encryption`) when the `netsh dns` context
  supports it; on Windows 10 DoH is unavailable and ignored gracefully.

- **Network throttling removal** — sets
  `HKLM\...\Multimedia\SystemProfile\NetworkThrottlingIndex` to `0xFFFFFFFF`,
  removing the bandwidth quota the Multimedia Class Scheduler applies to
  non-multimedia traffic.

- **MTU normalization** — cycles the IPv4 MTU of every physical adapter to a
  target (`1500` or `1400`) or back to the system default. The original links
  are archived per GUID and restored exactly on revert. Values outside
  `1280..=9000` are rejected.

Additionally:

- **Self-heal** — every five seconds each enabled group is verified against
  the registry / adapter state and re-applied if something reverted it.
- **Archive & revert** — the first time a value is replaced, the original
  machine state is saved to a JSON archive. `x` restores every archived
  value and quits. Writes are atomic (temp file + rename), so a crash can
  never corrupt the only copy of the original state.
- **Latency probe** — a background thread measures round-trip time to the
  Cloudflare (1.1.1.1) and Google (8.8.8.8) resolvers via ICMP and shows a
  live comparison with peak-to-peak jitter, packet loss and a sparkline in
  the menu; `auto` DNS picks the faster one.
- **DNS probe** — a parallel thread sends real DNS queries (UDP 53) to both
  resolvers and the menu shows their actual resolution times; when set to
  `auto` the resolvers pick the fastest working set.
- **TCP connections** — the menu shows total, established and TIME-WAIT
  connection counts (`GetExtendedTcpTable`).
- **Traffic meter** — the detail view shows each adapter's download/upload
  rate in bytes per second (`GetIfTable2`, sampled every refresh).
- **NIC power-down removal** — `p` toggles a policy that keeps network
  adapters from powering down at idle (sets the `PnPCapabilities` bit).
- **Wi-Fi diagnostics** — the detail view lists SSID, BSSID, signal strength
  bar, RSSI in dBm, channel and link speeds for connected Wi-Fi links
  (`WlanQueryInterface`).
- **Restore point** — `s` (or `--restore-point`) creates a System Restore
  point through `srclient.dll` before tweaking.
- **Action log viewer** — `l` shows the last logged actions inside the app.
- **Adapter details** — the `i` key opens a per-adapter view: up/down,
  DHCP/static, MTU, MAC address, IPs, gateways, both DNS families, traffic
  and Wi-Fi state.
- **DNS cache flush** — manual `f` key action (`ipconfig /flushdns`).
- **Autostart** — optional "run on login" entry in the user registry, so the
  tweaks are re-applied (and self-healed) after every reboot.
- **Action log** — every registry write and `netsh` run is timestamped to
  `%APPDATA%\stabilizatores\logs\stabilizatores.log` (1 MiB, rotated once).

## Requirements

- Windows 10/11.
- Run as administrator — the program requests elevation itself on start.
- The DNS and MTU features use `netsh.exe`, which is part of Windows.

## Usage

```
stabilizatores.exe            interactive menu
stabilizatores.exe --apply    apply every enabled tweak and exit
stabilizatores.exe --revert   restore the archived machine state and exit
stabilizatores.exe --undo     revert the last apply, restore its archive snapshot
stabilizatores.exe --status   print alignment state, exit 0/1/2
stabilizatores.exe --plan     dry-run: print what --apply would change (read-only)
stabilizatores.exe --report   write a diagnostic JSON snapshot, print its path
stabilizatores.exe --restore-point   create a System Restore point
stabilizatores.exe --profile save lan     save current config as profile 'lan'
stabilizatores.exe --profile apply lan    apply profile 'lan' (elevated)
stabilizatores.exe --profile list         list saved profiles
stabilizatores.exe --profile delete lan   delete profile 'lan'
stabilizatores.exe --help     usage text (never triggers UAC)
stabilizatores.exe --version  version (never triggers UAC)
```

One-run overrides change the resolved config without persisting it:
`stabilizatores.exe --status --dns=off --mtu=1400 --power=on` (each accepts
`--key=value` or `--key value`).

Exit codes: `0` all enabled tweaks aligned, `1` drift detected, `2` error
while evaluating. `--apply` / `--revert` / `--undo` / `--profile apply` /
`--restore-point` self-elevate; the rest are read-only and never show a UAC
prompt.

Console menu (`q` exits and keeps tweaks, `x` reverts everything and exits):

```
  STABILIZATORES  v0.3.0
  -------------------------------------------------
  status      PROTECTED
  interfaces  3 physical, 2 active
  adapters    Wi-Fi (MTU 1500)  ...
  tcp         [x]  tuned 1/1 ifaces  global [x]
  dns         [x]  cloudflare 1/2 aligned  (1.1.1.1 / 1.0.0.1)
  dns probe   1.1.1.1 9 ms   8.8.8.8 10 ms
  throttling  [x]  removed
  mtu         [x]  1500 set
  power       [x]  no power-down
  latency     cloudflare 9 ms (±2, ▂▄▆█▄▅▃▄, 0% loss)
  connections 184 total, 12 established, 92 time-wait
  -------------------------------------------------
  t tcp [x]    d dns [ ]    m throttling [x]    h self-heal [x]
  r cloudflare    u 1500    p power [x]    s restore point
  a autostart [x]    f flush dns    i details    l log
  q quit    x revert + quit
```

| Key | Action |
|---|---|
| `t` | toggle TCP tuning |
| `d` | toggle DNS optimization |
| `m` | toggle network throttling removal |
| `r` | cycle DNS provider: cloudflare → google → auto |
| `u` | cycle MTU: off → 1500 → 1400 → off |
| `p` | toggle NIC power-down removal |
| `s` | create a System Restore point |
| `i` | toggle the per-adapter detail view |
| `l` | toggle the action log viewer |
| `h` | toggle self-heal |
| `a` | toggle start on login |
| `f` | flush the DNS cache now |
| `q` | quit, **tweaks stay active** |
| `x` | revert every tweak, remove autostart, quit |

The status line reads `PROTECTED` (all enabled groups aligned, self-heal on),
`ACTIVE` (aligned, self-heal off), `PENDING` (some group needs re-application)
or `ERROR` (the last operation failed; the message is shown below).

## Files

| Path | Purpose |
|---|---|
| `%APPDATA%\stabilizatores\config.json` | which groups are enabled (`tcp`, `dns`, `multimedia`, `self_heal`, `dns_provider`, `mtu`, `power`) |
| `%APPDATA%\stabilizatores\archive.json` | original registry values and prior DNS configuration, used by the revert path |
| `%APPDATA%\stabilizatores\archive-snapshots\*.json` | dated archive copies taken before each apply (last 8 kept) |
| `%APPDATA%\stabilizatores\profiles\*.json` | named config profiles |
| `%APPDATA%\stabilizatores\reports\report-*.json` | diagnostic snapshots from `--report` |
| `%APPDATA%\stabilizatores\logs\stabilizatores.log` | timestamped action log (rotated after 1 MiB) |

All files are plain JSON / text, safe to delete (settings fall back to
defaults).

## Safety

- Registry writes are limited to the documented keys. Every replaced value is
  archived before writing; `x` restores the machine to its prior state.
- Reverting DNS restores each adapter's archived configuration: DHCP adapters
  go back to DHCP, static adapters get their exact previous server list back
  (IPv4 and IPv6).
- Reverting MTU puts each adapter back on its archived value, or on the system
  default if the original link had none.
- The console is restored to its previous mode and window title on exit, so a
  parent shell is left untouched.
- Live write tests are `#[ignore]`d and only run on an explicit request; the
  rest of the suite is pure logic and never modifies the system.
- If anything looks wrong, press `x` in the menu or run `--revert`.

## Build & test

```
cargo build --release
cargo test
cargo test -- --ignored    # live registry/DNS/MTU smoke tests, requires elevation
```

Binary: `target\release\stabilizatores.exe`.

## Project layout

```
src/
  lib.rs         public module tree (library crate)
  main.rs        entry point: flags, elevation, startup
  app.rs         terminal UI loop and rendering
  cli.rs         non-interactive commands (apply/revert/status/plan/report/profile/undo)
  config.rs      settings, profiles + machine-state archive (JSON)
  interfaces.rs  adapter enumeration (GetAdaptersAddresses)
  registry.rs    safe DWORD registry access
  tcp.rs         TCP parameter tuning
  dns.rs         DNS switching + auto provider + DoH through netsh
  mtu.rs         MTU normalization (registry + netsh)
  system.rs      network throttling removal
  probe.rs       background latency + DNS query probes
  connections.rs TCP connection counters (GetExtendedTcpTable)
  traffic.rs     per-adapter traffic rates (GetIfTable2)
  power.rs       NIC power-down removal (PnPCapabilities)
  wlan.rs        Wi-Fi link diagnostics (WlanQueryInterface)
  restore.rs     System Restore point creation (srclient.dll)
  netsh.rs       shared netsh invocation helper
  log.rs         timestamped action log
  win.rs         OS wrappers (console, elevation, autostart)
```

See `docs/ARCHITECTURE.md` for design details and
[`CHANGELOG.md`](CHANGELOG.md) for release history.

## Notes & limitations

- TCP tunables apply to connections established after the change; existing
  sockets keep their previous parameters. A reboot is not required.
- Hotspot/APN adapters that report a non-physical type are ignored by the
  DNS, TCP and MTU scanning.
- Interface names containing a double quote cannot be passed to `netsh`;
  such adapters are reported as errors rather than half-applied.
- ICMP latency probes can be blocked by firewalls; the menu then shows
  `n/a` for the affected resolver. The DNS query probe falls back to it.
- DNS-over-HTTPS is configured only when `netsh dns add encryption` exists
  (Windows 11); Windows 10 keeps plain resolvers.
- The restore point command needs System Restore enabled (`vssadmin list
  shadows`); on some clean VMs every create fails gracefully with a message.
- `--undo` restores the archive snapshot taken before the apply; it does not
  undo a restore point.

## CI

GitHub Actions (`workflows/ci.yml`) builds, tests, lints and formats the
crate on `windows-latest`.

## License

GPL-3.0, see [LICENSE](LICENSE). This is not affiliated with Cloudflare.