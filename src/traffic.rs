//! Per-adapter traffic rate measurement.
//!
//! Reads the cumulative octet counters of every physical adapter through
//! `GetIfTable2`, and a [`sample`] snapshot can be diffed against an earlier
//! one with [`rate`] to report the average transfer speed in both directions.
//! The menu keeps the previous snapshot to render `↓ ↑` speeds.

use std::time::Instant;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2,
};

/// One adapter's counters at a point in time.
#[derive(Clone, Copy, Debug, Default)]
pub struct Row {
    /// Adapter index (interface index).
    pub index: u32,
    /// Bytes received since the driver loaded.
    pub in_octets: u64,
    /// Bytes sent since the driver loaded.
    pub out_octets: u64,
}

/// The counter snapshot with its timestamp.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// When the counters were read.
    pub taken_at: Instant,
    /// One row per physical adapter.
    pub rows: Vec<Row>,
}

/// Reads the current counters of every physical adapter.
pub fn read() -> Vec<Row> {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table) != 0 {
            return Vec::new();
        }
        let count = (*table).NumEntries as usize;
        let entries = std::slice::from_raw_parts((*table).Table.as_ptr(), count);
        let mut rows = Vec::with_capacity(count);
        for entry in entries {
            if is_physical(entry) {
                rows.push(Row {
                    index: entry.InterfaceIndex,
                    in_octets: entry.InOctets,
                    out_octets: entry.OutOctets,
                });
            }
        }
        FreeMibTable(table.cast());
        rows
    }
}

/// Captures a snapshot of the current counters.
pub fn sample() -> Snapshot {
    Snapshot {
        taken_at: Instant::now(),
        rows: read(),
    }
}

/// Transfer rate between `before` and `after` in bytes per second, matched by
/// adapter index. Returns an empty vector when the interval is unusable.
pub fn rate(before: &Snapshot, after: &Snapshot) -> Vec<(u32, f64, f64)> {
    let seconds = after.taken_at.duration_since(before.taken_at).as_secs_f64();
    if seconds <= 0.0 {
        return Vec::new();
    }
    let mut own_before = before.rows.clone();
    own_before.retain(|first| after.rows.iter().any(|second| second.index == first.index));
    own_before
        .iter()
        .filter_map(|first| {
            let second = after
                .rows
                .iter()
                .find(|second| second.index == first.index)?;
            let down = second.in_octets.saturating_sub(first.in_octets) as f64 / seconds;
            let up = second.out_octets.saturating_sub(first.out_octets) as f64 / seconds;
            Some((first.index, down, up))
        })
        .collect()
}

const IF_TYPE_LOOPBACK: u32 = 24;
const IF_TYPE_TUNNEL: u32 = 131;

fn is_physical(entry: &MIB_IF_ROW2) -> bool {
    entry.Type != IF_TYPE_LOOPBACK && entry.Type != IF_TYPE_TUNNEL
}

/// Renders a byte-rate as a compact human string, e.g. "1.2 MB/s".
pub fn format_bytes_per_second(bytes: f64) -> String {
    if bytes >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bytes / (1024.0 * 1024.0))
    } else if bytes >= 1024.0 {
        format!("{:.0} KB/s", bytes / 1024.0)
    } else {
        format!("{bytes:.0} B/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_rates() {
        assert_eq!(format_bytes_per_second(500.0), "500 B/s");
        assert_eq!(format_bytes_per_second(2048.0), "2 KB/s");
        assert_eq!(format_bytes_per_second(1536.0 * 1024.0), "1.5 MB/s");
    }

    #[test]
    fn rate_requires_matching_indexes() {
        let before = Snapshot {
            taken_at: Instant::now() - std::time::Duration::from_secs(2),
            rows: vec![Row {
                index: 1,
                in_octets: 1000,
                out_octets: 500,
            }],
        };
        let after = Snapshot {
            taken_at: Instant::now(),
            rows: vec![
                Row {
                    index: 1,
                    in_octets: 3_000,
                    out_octets: 900,
                },
                Row {
                    index: 9,
                    in_octets: 0,
                    out_octets: 0,
                },
            ],
        };
        let rates = rate(&before, &after);
        assert_eq!(rates.len(), 1, "index 9 has no baseline");
        assert_eq!(rates[0].0, 1);
        assert!(
            (rates[0].1 - 1000.0).abs() < 100.0,
            "down about 2000 B / 2 s, got {}",
            rates[0].1
        );
        assert!(
            (rates[0].2 - 200.0).abs() < 20.0,
            "up about 400 B / 2 s, got {}",
            rates[0].2
        );
    }
}
