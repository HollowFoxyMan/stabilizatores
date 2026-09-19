# stabilizatores — architecture

`stabilizatores` is a Windows console application written in Rust with no
runtime outside of `windows-sys`. It is split into a library crate (`src/`)
and a thin binary (`src/main.rs`) so every module is unit-testable without
going through the entry point. The layout is small and layered so each
concern can be verified in isolation.

## Module map

```
src/lib.rs           public module tree (library crate)
src/main.rs          entry point; flags, elevation, startup
src/app.rs           App state machine, TUI loop, rendering
src/cli.rs           non-interactive apply/revert/status/plan/report/profile/undo
src/config.rs        Config, profiles, Archive and snapshot persistence (JSON, atomic)
src/interfaces.rs    network adapter enumeration
src/registry.rs      DWORD access to the Windows registry
src/tcp.rs           TCP parameter tuning (registry)
src/dns.rs           DNS switching, auto provider, DoH (netsh)
src/mtu.rs           MTU normalization (registry + netsh)
src/system.rs        network throttling removal (registry)
src/probe.rs         background latency (ICMP) + DNS query (UDP) probes
src/connections.rs   TCP connection counters (GetExtendedTcpTable)
src/traffic.rs       per-adapter traffic rates (GetIfTable2)
src/power.rs         NIC power-down removal (PnPCapabilities)
src/wlan.rs          Wi-Fi link diagnostics (WlanQueryInterface)
src/restore.rs       System Restore point (srclient.dll)
src/netsh.rs         shared netsh invocation helper
src/log.rs           timestamped action log (appends to %APPDATA%)
src/win.rs           console, elevation, autostart, DNS flush
```

## Control flow

`src/main.rs` parses the flags with `cli::parse` and dispatches:

1. `--help` / `--version` print and return; they never escalate.
2. `--apply`, `--revert`, `--undo`, `--profile apply`, `--restore-point` and
   the interactive mode check the token elevation. A non-elevated launch
   re-invokes the executable with `ShellExecuteW(...,"runas",...)` and
   returns. `--status`, `--plan`, `--report` and the profile bookkeeping
   commands are read-only and never escalate.
3. Interactive mode loads the config and archive, constructs `App::new`
   (applies every enabled group once, feeds the resulting status back into
   the UI) and runs the event loop:
   - polls raw console input (single-char keys) through a `win::Console`
     guard that restores the previous input/output modes and window title
     when dropped;
   - every 5 seconds runs `heal`, which verifies each enabled group against
     the live system state and re-applies drifted groups;
   - every 1 second redraws the full screen with VT sequences;
   - a background thread (probe) pings the Cloudflare and Google resolvers
     and publishes fresh latency samples for the status panel without ever
     blocking the loop.

Tests never go through the interactive constructor: `App::base` builds the
pure state, `App::new` is the live apply path and a `#[cfg(test)]` `new_view`
variant only refreshes the state.

## CLI commands

`cli.rs` reuses the exact same `apply` / `revert` / `status` functions as the
menu, so both entry points agree on what "aligned" means:

- `run_apply` snapshots the archive, resolves the DNS provider (probing when
  `auto`), applies each enabled group and prints one line per group;
- `run_revert` restores every archived group and prints `system restored.`;
- `run_undo` reverts exactly like `run_revert`, then swaps the archive back
  to the latest snapshot taken before the previous apply;
- `run_plan` re-uses the same resolution and prints what `run_apply` would
  change without touching the system;
- `run_report` gathers adapter, DNS, TCP, traffic, connection and Wi-Fi state
  into `%APPDATA%\reports\report-<timestamp>.json`;
- `run_profile` saves / applies / lists / deletes named configs under
  `%APPDATA%\profiles\`; applying a profile persists it as the active config
  and then runs `run_apply`;
- `run_status` evaluates alignment group by group and exits `0` (aligned),
  `1` (drift detected) or `2` (could not evaluate).

## The tweak interface

Each group module exposes the same three-shaped API:

- `apply(...) -> Result<_, String>` — bring the system into the group's
  target state and archive the previous state;
- `revert(...) -> Result<_, String>` — restore the archived state exactly;
- `status() -> ...` — read-only report, `aligned()` when nothing is left to
  do.

`heal` and the overall PROTECTED/ACTIVE/PENDING/ERROR status simply combine
the three `aligned*` answers.

The DNS group is parameterized: `dns::Resolvers` bundles a provider and its
IPv4/IPv6 server sets, and `resolve_provider(DnsProvider, Option<&Latency>)`
selects the concrete set — `Auto` compares the freshest ICMP samples and
falls back to Cloudflare on ties or missing data. The active set is tracked
by the UI (`active_dns`) so re-applies and reverts stay symmetric, including
DoH cleanup.

The MTU group (`mtu.rs`) applies through `netsh` but archives through the
registry: each adapter's `MTU` DWORD under
`Tcpip\Parameters\Interfaces\{GUID}` is the persistent origin. Revert deletes
the DWORD when it was originally absent and sets the link live-only
(`store=active`), so the registry stays byte-identical to the pre-tool
machine.

## Shared netsh helper

`netsh.rs` wraps `netsh.exe` — the DNS, DoH and MTU features all shell out
through it. One process per command keeps calls honest: no shell parsing, the
exit status and stderr are captured, and interface-identifying arguments are
built by `name_arg`/`interface_arg`, which reject quotes so no argument can
ever be smuggled into a second netsh verb.

## Archive & exact restore

`config::Archive` persists two kinds of entries. Registry values:

```json
{
  "group": "tcp",
  "subkey": "SYSTEM\\...\\Tcpip\\Parameters",
  "name": "TcpTimedWaitDelay",
  "present": true,
  "value": 30
}
```

`group` is one of `tcp`, `system` or `mtu`; `present: false` records a value
that did not exist, so revert knows to delete it instead of writing back.
MTU entries use the interface GUID as their subkey.

and per-adapter DNS state:

```json
{
  "interface": "Wi-Fi",
  "dhcp": true,
  "servers": ["192.168.1.1"],
  "servers6": []
}
```

- `ensure` records the *current* value the first time a tweak replaces it and
  never overwrites an existing entry, so re-applies cannot destroy the
  original.
- `restore_group` writes the original back (or deletes the value when it was
  absent) and drains the entry. A failed restore stays in the archive and is
  retried on the next revert.
- DNS revert is faithful per adapter: DHCP adapters go back to DHCP, static
  adapters get their exact prior server list back (IPv4 and IPv6). Adapters
  that disappeared keep their backup.
- Both `config.json` and `archive.json` are written atomically
  (`write_atomic`: temp file + `fs::rename`), so a crash mid-write can never
  corrupt the only copy of the original state.
- `x` / `--revert` runs every group's revert plus autostart removal.

## Why these Windows APIs

| Concern | API | Reason |
|---|---|---|
| elevation | `OpenProcessToken` + `TokenElevation` | cheap, no UAC prompt when already elevated |
| relaunch | `ShellExecuteW("runas")` | canonical self-elevation |
| key input | `ReadConsoleInputW` + VT mode | same style of terminal UI; no external crates |
| latency | `IcmpCreateFile` + `IcmpSendEcho` | locale-independent ICMP RTT measurement, no spawned processes |
| DNS query probe | raw UDP :53 `WSASendTo`/`RecvFrom` | measures real resolution time; used by `auto` selection too |
| adapters | `GetAdaptersAddresses` | authoritative GUIDs (registry keys), friendly names (netsh), DHCP flag, MTU, DNS list, IPs, gateways and MAC in one pass |
| connections | `GetExtendedTcpTable` (owner PID) | total / established / TIME-WAIT counters for the status line |
| traffic | `GetIfTable2` + `FreeMibTable` | sampled deltas produce per-adapter down/up rates |
| power | registry `PnPCapabilities` | the documented "don't power down" control bit with no extra APIs |
| Wi-Fi | `WlanQueryInterface` | SSID, BSSID, RSSI, channel and link speeds per entry |
| restore point | `srclient.dll` `SRSetRestorePointW` | dynamic load; works on clients, guarded by System Restore |
| DNS | `netsh interface ip set dns` | handles DHCP and static adapters uniformly |
| DoH | `netsh dns add encryption` | Win11-only; presence probed once and cached |
| DNS flush | `DnsFlushResolverCache` (`dnsapi`) | equals `ipconfig /flushdns` |
| log stamps | `GetLocalTime` | local-time timestamps without dependencies |

`GetAdaptersAddresses` returns the adapter GUID as the `{GUID}` string used
verbatim under `...\Tcpip\Parameters\Interfaces`, and the friendly name used
by `netsh`; one enumeration feeds the TCP, DNS and MTU features plus the
detail view.

## DNS parsing detail

`SOCKADDR` in `windows-sys` only carries 16 bytes, but a `sockaddr_in6` is
28 bytes and `sa_data` does not cover its address field. `interfaces.rs`
therefore reads raw bytes through the pointer using the real socket length
reported by the API (`iSockaddrLength`) and validates every offset against it —
a single macro-driven walker feeds the unicast address, gateway and DNS lists:

- IPv4 address lives at byte offset 4 (family 2 + port 2);
- IPv6 address lives at byte offset 8 (family 2 + port 2 + flow info 4).

Each adapter splits its DNS list into IPv4 and IPv6, so the DNS feature can
set (and restore) both families independently, and adapters on DHCP are
recognized through the `IP_ADAPTER_DHCP_ENABLED` flag before the switch.

## MTU detail

MTU state is normalized across physical adapters because Windows assigns
different link MTUs (1500 on Ethernet, 1400 on some Wi-Fi drivers).
`mtu::apply` writes the target into each applicable adapter's registry `MTU`
DWORD (`store=persistent`) and corrects the live link; `mtu::status(target)`
verifies both the registry value and the live subinterface, and an absent
value counts as the 1500 default.

## Action log

`log.rs` appends one line per registry write / netsh run with a
`GetLocalTime` timestamp to `%APPDATA%\stabilizatores\logs\stabilizatores.log`,
rotating the file when it exceeds 1 MiB. Logging never fails a caller.

## Testing strategy

- Pure logic (config parsing, archive semantics, sockaddr formatting,
  `resolve_provider` selection, netsh argument quoting, MTU range validation,
  status composition, latency rendering) is covered by normal unit tests.
- The `apply_revert_roundtrip` tests in `tcp.rs`, `system.rs`, `dns.rs`,
  `mtu.rs` and `power.rs` are `#[ignore]`d: they require elevation and briefly
  write real registry keys / move adapters to a resolver set / change link
  MTUs / toggle the power bit, then restore the exact original state. Run
  them with `cargo test -- --ignored --test-threads=1`.
- The `enumerate_adapters` and `autostart_roundtrip` tests are `#[ignore]`d
  as well; the former is read-only and prints the adapter table.
- The default `cargo test` run never modifies the machine.

## CI

`.github/workflows/ci.yml` runs build (debug + release), `cargo test`,
`cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` on
`windows-latest`.