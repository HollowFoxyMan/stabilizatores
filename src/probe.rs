//! Background latency probes.
//!
//! Measures round-trip time to signalling gateways via ICMP echo using the
//! Windows `IcmpSendEcho` API (iphlpapi) and, separately, the real DNS
//! resolution latency by sending a minimal DNS query over UDP. Runs on its
//! own thread so the menu never blocks while a probe is in flight.
//!
//! Each endpoint keeps a sliding window of the last probes. The window feeds
//! three statistics: the latest RTT, the peak-to-peak jitter of the
//! successful samples and the packet-loss ratio (failed probes over all
//! probes). The same window is what the menu draws a sparkline from.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY, IP_OPTION_INFORMATION,
    IP_SUCCESS,
};

/// Result of a single ICMP probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Round-trip time in milliseconds.
    pub rtt_ms: u32,
    /// IP status code returned by the stack, 0 (IP_SUCCESS) on success.
    pub status: u32,
}

/// Sliding-window statistics of one endpoint.
#[derive(Clone, Debug, Default)]
pub struct Endpoint {
    /// Most recent successful sample.
    pub last: Option<Sample>,
    /// Last [`HISTORY`] probe outcomes, oldest first. `None` is a lost probe.
    history: VecDeque<Option<u32>>,
}

/// How many probe outcomes each endpoint remembers.
const HISTORY: usize = 60;

impl Endpoint {
    /// Test convenience: an endpoint whose only probe answered in `rtt_ms`.
    pub fn with_rtt(rtt_ms: Option<u32>) -> Endpoint {
        let mut endpoint = Endpoint::default();
        endpoint.record(rtt_ms.map(|rtt_ms| Sample { rtt_ms, status: 0 }));
        endpoint
    }

    /// Stores one probe outcome and keeps the window bounded.
    fn record(&mut self, sample: Option<Sample>) {
        if let Some(sample) = sample {
            self.last = Some(sample);
        }
        self.history.push_back(sample.map(|s| s.rtt_ms));
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
    }

    /// RTT of the most recent successful probe.
    pub fn rtt_ms(&self) -> Option<u32> {
        self.last.map(|sample| sample.rtt_ms)
    }

    /// Failed probes in the window.
    pub fn loss(&self) -> u32 {
        self.history.iter().filter(|entry| entry.is_none()).count() as u32
    }

    /// Total probes in the window.
    pub fn total(&self) -> u32 {
        self.history.len() as u32
    }

    /// Peak-to-peak jitter (max minus min) of the successful window samples.
    pub fn jitter_ms(&self) -> Option<u32> {
        let rtts = self.history.iter().flatten().copied().collect::<Vec<_>>();
        if rtts.is_empty() {
            return None;
        }
        let min = rtts.iter().copied().min().unwrap();
        let max = rtts.iter().copied().max().unwrap();
        Some(max.saturating_sub(min))
    }

    /// One-character-per-probe sparkline: RTT mapped to seven brightness
    /// levels, lost probes as spaces. The scale follows the window maximum
    /// (never smaller than 50 ms for a meaningful early graph). U+2587 is
    /// avoided because several console fonts render it as a box artifact.
    pub fn sparkline(&self) -> String {
        const LEVELS: [char; 7] = [
            '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}', '\u{2588}',
        ];
        let max = self
            .history
            .iter()
            .flatten()
            .copied()
            .max()
            .unwrap_or(0)
            .max(50);
        self.history
            .iter()
            .map(|entry| match entry {
                None => ' ',
                Some(rtt) => {
                    let index = ((*rtt as usize) * 6 / max as usize).min(6);
                    LEVELS[index]
                }
            })
            .collect()
    }
}

/// Latency snapshot for the endpoints the program cares about.
#[derive(Clone, Debug, Default)]
pub struct Latency {
    /// Cloudflare 1.1.1.1 — the DNS servers this program prefers.
    pub cloudflare: Endpoint,
    /// Google 8.8.8.8 — kept for comparison.
    pub google: Endpoint,
    /// Latest real DNS resolution time through 1.1.1.1, milliseconds.
    pub cf_dns_ms: Option<u32>,
    /// Latest real DNS resolution time through 8.8.8.8, milliseconds.
    pub google_dns_ms: Option<u32>,
}

pub const CLOUDFLARE: [u8; 4] = [1, 1, 1, 1];
pub const GOOGLE: [u8; 4] = [8, 8, 8, 8];
const PROBE_INTERVAL: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT_MS: u32 = 1000;
const DNS_TIMEOUT: Duration = Duration::from_millis(1200);
const PROBE_PAYLOAD: &[u8] = b"stabilizatores";

/// Spawns the probe thread. The caller keeps `stop` and must set it (and
/// join) before exiting; the thread writes fresh results into `state`.
pub fn spawn(state: Arc<Mutex<Option<Latency>>>, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let mut measured = Latency::default();
            measured
                .cloudflare
                .record(ping_once(CLOUDFLARE, PROBE_TIMEOUT_MS));
            measured.google.record(ping_once(GOOGLE, PROBE_TIMEOUT_MS));
            measured.cf_dns_ms = dns_query_rtt(Ipv4Addr::from(CLOUDFLARE), DNS_TIMEOUT);
            measured.google_dns_ms = dns_query_rtt(Ipv4Addr::from(GOOGLE), DNS_TIMEOUT);
            match state.lock() {
                Ok(mut slot) => *slot = Some(measured),
                Err(_) => return,
            }
            thread::sleep(PROBE_INTERVAL);
        }
    });
}

fn ping_once(address: [u8; 4], timeout_ms: u32) -> Option<Sample> {
    unsafe {
        let handle = IcmpCreateFile();
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut reply: ICMP_ECHO_REPLY = std::mem::zeroed();
        let destination = u32::from_be_bytes(address);
        let sent = IcmpSendEcho(
            handle,
            destination,
            PROBE_PAYLOAD.as_ptr().cast(),
            PROBE_PAYLOAD.len() as u16,
            std::ptr::null::<IP_OPTION_INFORMATION>(),
            (&mut reply as *mut ICMP_ECHO_REPLY).cast(),
            std::mem::size_of::<ICMP_ECHO_REPLY>() as u32,
            timeout_ms,
        );
        IcmpCloseHandle(handle);
        if sent == 0 {
            return None;
        }
        let sample = Sample {
            rtt_ms: reply.RoundTripTime,
            status: reply.Status,
        };
        if sample.status == IP_SUCCESS {
            Some(sample)
        } else {
            None
        }
    }
}

/// Measures the resolution time of a real DNS query through `server`. A
/// response (even an empty NXDOMAIN one) counts as a success; timeout or an
/// ICMP port-unreachable reply yields `None`.
pub fn dns_query_rtt(server: Ipv4Addr, timeout: Duration) -> Option<u32> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.set_read_timeout(Some(timeout)).ok()?;
    socket.connect((server, 53)).ok()?;

    let mut packet = Vec::with_capacity(40);
    packet.extend_from_slice(&0xBEEF_u16.to_be_bytes()); // transaction id
    packet.extend_from_slice(&0x0100_u16.to_be_bytes()); // RD flag
    packet.extend_from_slice(&1_u16.to_be_bytes()); // QDCOUNT
    packet.extend_from_slice(&0_u16.to_be_bytes()); // ANCOUNT
    packet.extend_from_slice(&0_u16.to_be_bytes()); // NSCOUNT
    packet.extend_from_slice(&0_u16.to_be_bytes()); // ARCOUNT
    for label in b"stabilizatores.probe".split(|byte| *byte == b'.') {
        packet.push(label.len() as u8);
        packet.extend_from_slice(label);
    }
    packet.push(0);
    packet.extend_from_slice(&1_u16.to_be_bytes()); // QTYPE: A
    packet.extend_from_slice(&1_u16.to_be_bytes()); // QCLASS: IN

    socket.send(&packet).ok()?;
    let start = Instant::now();
    let mut buffer = [0u8; 512];
    loop {
        match socket.recv(&mut buffer) {
            Ok(received) if received >= 12 && buffer[0] == 0xBE && buffer[1] == 0xEF => {
                return Some(start.elapsed().as_millis() as u32);
            }
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Convenience for tests: runs a single in-process probe.
pub fn ping(address: [u8; 4]) -> Option<Sample> {
    ping_once(address, PROBE_TIMEOUT_MS)
}

/// Blocks until both endpoints answered (or timed out) and returns a fresh
/// snapshot. Used by the non-interactive mode for the "auto" resolver pick.
pub fn latency_now() -> Latency {
    let mut latency = Latency::default();
    latency
        .cloudflare
        .record(ping_once(CLOUDFLARE, PROBE_TIMEOUT_MS));
    latency.google.record(ping_once(GOOGLE, PROBE_TIMEOUT_MS));
    latency.cf_dns_ms = dns_query_rtt(Ipv4Addr::from(CLOUDFLARE), DNS_TIMEOUT);
    latency.google_dns_ms = dns_query_rtt(Ipv4Addr::from(GOOGLE), DNS_TIMEOUT);
    latency
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_network_byte_order_destination() {
        assert_eq!(u32::from_be_bytes([1, 1, 1, 1]), 0x0101_0101);
        assert_eq!(u32::from_be_bytes([8, 8, 8, 8]), 0x0808_0808);
    }

    #[test]
    fn constants_are_valid_ipv4() {
        assert!(CLOUDFLARE[0] < 224 && GOOGLE[0] < 224);
    }

    #[test]
    fn window_counts_loss_and_jitter() {
        let mut endpoint = Endpoint::default();
        endpoint.record(Some(Sample {
            rtt_ms: 10,
            status: 0,
        }));
        endpoint.record(Some(Sample {
            rtt_ms: 20,
            status: 0,
        }));
        endpoint.record(None);
        endpoint.record(Some(Sample {
            rtt_ms: 15,
            status: 0,
        }));
        assert_eq!(endpoint.total(), 4);
        assert_eq!(endpoint.loss(), 1);
        assert_eq!(endpoint.jitter_ms(), Some(10));
        assert_eq!(endpoint.rtt_ms(), Some(15));
    }

    #[test]
    fn empty_window_has_no_stats() {
        let endpoint = Endpoint::default();
        assert_eq!(endpoint.rtt_ms(), None);
        assert_eq!(endpoint.jitter_ms(), None);
        assert_eq!(endpoint.loss(), 0);
        assert_eq!(endpoint.total(), 0);
        assert_eq!(endpoint.sparkline(), "");
    }

    #[test]
    fn window_is_bounded() {
        let mut endpoint = Endpoint::default();
        for index in 0..(HISTORY + 10) {
            endpoint.record(Some(Sample {
                rtt_ms: index as u32,
                status: 0,
            }));
        }
        assert_eq!(endpoint.total(), HISTORY as u32);
        assert_eq!(endpoint.sparkline().chars().count(), HISTORY);
    }

    #[test]
    fn sparkline_has_seven_levels_and_spaces_for_loss() {
        let mut endpoint = Endpoint::default();
        endpoint.record(Some(Sample {
            rtt_ms: 1,
            status: 0,
        }));
        endpoint.record(None);
        endpoint.record(Some(Sample {
            rtt_ms: 50,
            status: 0,
        }));
        let line = endpoint.sparkline();
        let mut chars = line.chars();
        let first = chars.next();
        assert_eq!(chars.next(), Some(' '), "lost probe renders blank");
        let last = chars.next();
        assert_ne!(first, last);
    }

    #[test]
    fn dns_packet_marks_transaction_id() {
        let mut packet = Vec::new();
        packet.extend_from_slice(&0xBEEF_u16.to_be_bytes());
        assert_eq!(packet, vec![0xBE, 0xEF]);
    }
}
