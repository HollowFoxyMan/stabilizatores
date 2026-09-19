//! Live TCP connection statistics.
//!
//! Reads the kernel's TCP connection table through `GetExtendedTcpTable` and
//! counts the states the tuning cares about: total connections and how many
//! sit in `TIME_WAIT`. With `TcpTimedWaitDelay` shortened these should shrink
//! quickly after bursty transfers, which the menu shows live.

use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, MIB_TCP_STATE_ESTAB,
    MIB_TCP_STATE_TIME_WAIT, TCP_TABLE_OWNER_PID_ALL,
};

/// Snapshot of the IPv4 TCP connection table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpConnections {
    /// Connections in every state.
    pub total: u32,
    /// Established connections.
    pub established: u32,
    /// Sockets waiting out their TIME_WAIT period.
    pub time_wait: u32,
}

const AF_INET: u32 = 2;
const ERROR_SUCCESS: u32 = 0;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

/// Counts the current IPv4 TCP connections. Failures (or an empty table) are
/// reported as a default snapshot.
pub fn read() -> TcpConnections {
    unsafe {
        let mut size: u32 = 0;
        let first = GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            AF_INET,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
        if first != ERROR_INSUFFICIENT_BUFFER as u32 {
            return TcpConnections::default();
        }
        let mut buffer = vec![0u8; size as usize];
        let table = buffer.as_mut_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
        if GetExtendedTcpTable(
            table.cast(),
            &mut size,
            0,
            AF_INET,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        ) != ERROR_SUCCESS
        {
            return TcpConnections::default();
        }
        let count = (*table).dwNumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).table.as_ptr(), count);
        count_rows(rows)
    }
}

fn count_rows(rows: &[MIB_TCPROW_OWNER_PID]) -> TcpConnections {
    let mut status = TcpConnections {
        total: rows.len() as u32,
        ..TcpConnections::default()
    };
    for row in rows {
        match row.dwState as i32 {
            MIB_TCP_STATE_ESTAB => status.established += 1,
            MIB_TCP_STATE_TIME_WAIT => status.time_wait += 1,
            _ => {}
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_states() {
        let rows = [
            MIB_TCPROW_OWNER_PID {
                dwState: MIB_TCP_STATE_ESTAB as u32,
                dwLocalAddr: 0,
                dwLocalPort: 1,
                dwRemoteAddr: 0,
                dwRemotePort: 2,
                dwOwningPid: 0,
            },
            MIB_TCPROW_OWNER_PID {
                dwState: MIB_TCP_STATE_TIME_WAIT as u32,
                dwLocalAddr: 0,
                dwLocalPort: 1,
                dwRemoteAddr: 0,
                dwRemotePort: 2,
                dwOwningPid: 0,
            },
            MIB_TCPROW_OWNER_PID {
                dwState: 2,
                dwLocalAddr: 0,
                dwLocalPort: 1,
                dwRemoteAddr: 0,
                dwRemotePort: 2,
                dwOwningPid: 0,
            },
        ];
        let status = count_rows(&rows);
        assert_eq!(status.total, 3);
        assert_eq!(status.established, 1);
        assert_eq!(status.time_wait, 1);
    }
}
