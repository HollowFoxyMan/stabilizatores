//! Enumerates the host's network adapters through `GetAdaptersAddresses`.
//!
//! The program deals with two views of the machine:
//!
//! * **TCP tuning** — per-adapter registry keys keyed by the adapter GUID;
//! * **DNS switching** — `netsh` needs the adapter *friendly name*.
//!
//! An [`Interface`] carries both, plus the current DNS server list and MTU.

use std::ffi::CStr;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    IP_ADAPTER_DHCP_ENABLED, IP_ADAPTER_GATEWAY_ADDRESS_LH, IP_ADAPTER_UNICAST_ADDRESS_LH,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR};

/// `IF_TYPE_SOFTWARE_LOOPBACK`.
const IF_TYPE_LOOPBACK: u32 = 24;
/// `IF_TYPE_TUNNEL` (IPv6 transition tunnels such as Teredo / 6to4).
const IF_TYPE_TUNNEL: u32 = 131;

/// A single network adapter as seen by the IP Helper API.
#[derive(Debug, Clone)]
pub struct Interface {
    /// `{GUID}` string, the registry key suffix under the Tcpip Interfaces node.
    pub guid: String,
    /// Numeric interface index (used by the IP helper counters).
    pub index: u32,
    /// Display name used by `netsh`, e.g. "Wi-Fi".
    pub friendly: String,
    /// Hardware description, e.g. "Intel(R) Wi-Fi 6 AX201".
    pub description: String,
    /// Adapter MTU in bytes (0 when unknown).
    pub mtu: u32,
    /// `IF_TYPE_*` constant.
    pub if_type: u32,
    /// Link is up.
    pub up: bool,
    /// The adapter obtains its configuration (including DNS) from DHCP.
    pub dhcp: bool,
    /// Current IPv4 DNS servers, primary first.
    pub dns: Vec<String>,
    /// Current IPv6 DNS servers, primary first.
    pub dns6: Vec<String>,
    /// Currently assigned IPv4 and IPv6 addresses.
    pub ips: Vec<String>,
    /// Currently configured IPv4 and IPv6 gateways.
    pub gateways: Vec<String>,
    /// Hardware address as `AA-BB-CC-DD-EE-FF`, empty when none is reported.
    pub mac: String,
}

impl Interface {
    /// Virtual or loopback adapters cannot be meaningfully tuned.
    pub fn is_physical(&self) -> bool {
        self.if_type != IF_TYPE_LOOPBACK && self.if_type != IF_TYPE_TUNNEL
    }
}

/// Fetches every adapter, including loopback and tunnels.
pub fn all() -> io::Result<Vec<Interface>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST;
    let mut size: u32 = 0;

    let result = unsafe {
        GetAdaptersAddresses(
            u32::from(AF_UNSPEC),
            flags,
            ptr::null(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if result != ERROR_BUFFER_OVERFLOW {
        return Err(io::Error::from_raw_os_error(result as i32));
    }

    let mut buffer = vec![0u8; size as usize];
    let pointer = buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    let result = unsafe {
        GetAdaptersAddresses(u32::from(AF_UNSPEC), flags, ptr::null(), pointer, &mut size)
    };
    if result != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(result as i32));
    }

    let mut out = Vec::new();
    let mut cursor = pointer;
    while !cursor.is_null() {
        let adapter = unsafe { &*cursor };
        let dhcp = unsafe { adapter.Anonymous2.Flags } & IP_ADAPTER_DHCP_ENABLED != 0;
        let (dns, dns6) = read_dns_servers(adapter.FirstDnsServerAddress);
        out.push(Interface {
            guid: read_cstr_utf8(adapter.AdapterName),
            index: unsafe { adapter.Anonymous1.Anonymous.IfIndex },
            friendly: read_utf16(adapter.FriendlyName),
            description: read_utf16(adapter.Description),
            mtu: adapter.Mtu,
            if_type: adapter.IfType,
            up: adapter.OperStatus == IfOperStatusUp,
            dhcp,
            dns,
            dns6,
            ips: read_unicast_addrs(adapter.FirstUnicastAddress),
            gateways: read_gateways(adapter.FirstGatewayAddress),
            mac: read_mac(&adapter.PhysicalAddress, adapter.PhysicalAddressLength),
        });
        cursor = adapter.Next;
    }
    Ok(out)
}

/// Real adapters only (no loopback, no tunnel), regardless of link state.
pub fn physical() -> Vec<Interface> {
    all()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.is_physical())
        .collect()
}

/// Real adapters with an established connection (used for DNS switching).
pub fn active() -> Vec<Interface> {
    all()
        .unwrap_or_default()
        .into_iter()
        .filter(|iface| iface.is_physical() && iface.up)
        .collect()
}

fn read_cstr_utf8(ptr: *const u8) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe {
        CStr::from_ptr(ptr.cast::<i8>())
            .to_string_lossy()
            .into_owned()
    }
}

fn read_utf16(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut length = 0usize;
    while unsafe { *ptr.add(length) } != 0 {
        length += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, length) })
}

fn read_dns_servers(
    first: *mut windows_sys::Win32::NetworkManagement::IpHelper::IP_ADAPTER_DNS_SERVER_ADDRESS_XP,
) -> (Vec<String>, Vec<String>) {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    let mut cursor = first;
    while !cursor.is_null() {
        let entry = unsafe { &*cursor };
        if let Some((family, text)) =
            format_sockaddr(entry.Address.lpSockaddr, entry.Address.iSockaddrLength)
        {
            if family == AF_INET {
                v4.push(text);
            } else if family == AF_INET6 {
                v6.push(text);
            }
        }
        cursor = entry.Next;
    }
    (v4, v6)
}

fn read_mac(bytes: &[u8; 8], length: u32) -> String {
    let length = (length as usize).min(bytes.len());
    if length == 0 {
        return String::new();
    }
    bytes[..length]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join("-")
}

/// Compiles into a function that walks a linked sockaddr list and formats
/// every entry as an IP string. All IP Helper list nodes share the same
/// layout: an `Address` and a `Next` pointer.
macro_rules! collect_sockaddr_list {
    ($first:expr) => {{
        let mut out = Vec::new();
        let mut cursor = $first;
        while !cursor.is_null() {
            let entry = unsafe { &*cursor };
            if let Some((_, text)) =
                format_sockaddr(entry.Address.lpSockaddr, entry.Address.iSockaddrLength)
            {
                out.push(text);
            }
            cursor = entry.Next;
        }
        out
    }};
}

fn read_unicast_addrs(first: *mut IP_ADAPTER_UNICAST_ADDRESS_LH) -> Vec<String> {
    collect_sockaddr_list!(first)
}

fn read_gateways(first: *mut IP_ADAPTER_GATEWAY_ADDRESS_LH) -> Vec<String> {
    collect_sockaddr_list!(first)
}

/// Formats a `SOCKADDR` as a numeric IP address string.
///
/// The underlying buffer may be longer than the 16-byte `SOCKADDR` view (a
/// `sockaddr_in6` is 28 bytes); `length` is the real size reported by the
/// API and every byte read is checked against it. Returns the address family
/// together with the human-readable address.
fn format_sockaddr(ptr: *const SOCKADDR, length: i32) -> Option<(u16, String)> {
    if ptr.is_null() {
        return None;
    }
    let sockaddr = unsafe { &*ptr };
    let bytes = ptr.cast::<u8>();
    match sockaddr.sa_family {
        AF_INET if length as usize >= 8 => {
            // family(2) + port(2), then the 4 address bytes.
            let ip = [
                unsafe { *bytes.add(4) },
                unsafe { *bytes.add(5) },
                unsafe { *bytes.add(6) },
                unsafe { *bytes.add(7) },
            ];
            Some((AF_INET, Ipv4Addr::from(ip).to_string()))
        }
        AF_INET6 if length as usize >= sockaddr_in6_addr_end() => {
            // family(2) + port(2) + flow info(4), then the 16 address bytes.
            let mut ip = [0u8; 16];
            for (index, slot) in ip.iter_mut().enumerate() {
                *slot = unsafe { *bytes.add(index + 8) };
            }
            Some((AF_INET6, Ipv6Addr::from(ip).to_string()))
        }
        _ => None,
    }
}

/// 8-byte header + 16 address bytes of a `sockaddr_in6`.
const fn sockaddr_in6_addr_end() -> usize {
    24
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_not_physical() {
        let iface = Interface {
            guid: "{00000000-0000-0000-0000-000000000000}".into(),
            index: 1,
            friendly: "Loopback".into(),
            description: String::new(),
            mtu: 1500,
            if_type: IF_TYPE_LOOPBACK,
            up: true,
            dhcp: false,
            dns: vec!["127.0.0.1".into()],
            dns6: vec!["::1".into()],
            ips: vec!["127.0.0.1".into(), "::1".into()],
            gateways: vec![],
            mac: "00-00-00-00-00-00".into(),
        };
        assert!(!iface.is_physical());
    }

    #[test]
    fn ethernet_is_physical() {
        let iface = Interface {
            guid: "{11111111-1111-1111-1111-111111111111}".into(),
            index: 2,
            friendly: "Ethernet".into(),
            description: String::new(),
            mtu: 1500,
            if_type: 6, // IF_TYPE_ETHERNET_CSMACD
            up: false,
            dhcp: true,
            dns: vec![],
            dns6: vec![],
            ips: vec![],
            gateways: vec![],
            mac: String::new(),
        };
        assert!(iface.is_physical());
    }

    #[test]
    fn format_ipv4_sockaddr() {
        let mut raw = [0u8; 16];
        raw[0] = 2; // AF_INET low byte
        raw[4] = 1;
        raw[5] = 1;
        raw[6] = 1;
        raw[7] = 1;
        let (family, formatted) =
            format_sockaddr(raw.as_ptr().cast::<SOCKADDR>(), raw.len() as i32).unwrap();
        assert_eq!(family, AF_INET);
        assert_eq!(formatted, "1.1.1.1");
    }

    #[test]
    fn format_ipv6_sockaddr() {
        let mut raw = [0u8; 28];
        raw[0] = 23; // AF_INET6 low byte
        raw[8] = 0x26;
        raw[9] = 0x06;
        raw[10] = 0x47;
        raw[11] = 0x00;
        raw[12] = 0x47;
        raw[13] = 0x00;
        raw[14] = 0x00;
        raw[15] = 0x00;
        raw[16] = 0x00;
        raw[17] = 0x00;
        raw[18] = 0x00;
        raw[19] = 0x00;
        raw[20] = 0x00;
        raw[21] = 0x00;
        raw[22] = 0x11;
        raw[23] = 0x11;
        let (family, formatted) =
            format_sockaddr(raw.as_ptr().cast::<SOCKADDR>(), raw.len() as i32).unwrap();
        assert_eq!(family, AF_INET6);
        assert_eq!(formatted, "2606:4700:4700::1111");
    }

    #[test]
    fn format_sockaddr_rejects_short_buffer() {
        let raw = [0u8; 4];
        assert_eq!(
            format_sockaddr(raw.as_ptr().cast::<SOCKADDR>(), raw.len() as i32),
            None
        );
    }

    #[test]
    #[ignore]
    fn enumerate_adapters() {
        let all = all().expect("GetAdaptersAddresses should succeed");
        assert!(!all.is_empty(), "there must be at least one adapter");
        for iface in all {
            println!(
                "{} | up={} | dhcp={} | type={} | mtu={} | dns={:?} | dns6={:?} | ips={:?} | gw={:?} | mac={}",
                iface.friendly,
                iface.up,
                iface.dhcp,
                iface.if_type,
                iface.mtu,
                iface.dns,
                iface.dns6,
                iface.ips,
                iface.gateways,
                iface.mac
            );
        }
        assert!(physical().iter().any(|i| i.is_physical()));
    }
}
