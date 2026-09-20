//! Wi-Fi link diagnostics through the WLAN API (`wlanapi.dll`).
//!
//! The polling scope stays small: for every wireless interface the current
//! connection attributes (SSID, BSSID, signal quality, link rates) plus the
//! live RSSI in dBm and the channel number come from `WlanQueryInterface`.
//! All data is read-only and never triggers a scan.

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::NetworkManagement::WiFi::{
    wlan_interface_state_connected, wlan_intf_opcode_channel_number,
    wlan_intf_opcode_current_connection, wlan_intf_opcode_rssi, WlanCloseHandle,
    WlanEnumInterfaces, WlanFreeMemory, WlanOpenHandle, WlanQueryInterface, WLAN_API_VERSION,
    WLAN_ASSOCIATION_ATTRIBUTES, WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO,
    WLAN_INTERFACE_INFO_LIST,
};

/// One wireless interface at a glance.
#[derive(Clone, Debug)]
pub struct WlanLink {
    /// Interface GUID string, e.g. `{AA-BB-...}`; matches the adapter GUID.
    pub guid: String,
    /// Driver description (`strInterfaceDescription`).
    pub description: String,
    /// Whether the interface currently has an association.
    pub connected: bool,
    /// The connected network's SSID (empty when disconnected).
    pub ssid: String,
    /// BSSID of the access point, `AA:BB:CC:DD:EE:FF`.
    pub bssid: String,
    /// Link quality 0..=100 as reported by the driver.
    pub signal_quality: u32,
    /// Signal strength in dBm when the driver reports it.
    pub rssi_dbm: Option<i32>,
    /// Current radio channel when known.
    pub channel: Option<u32>,
    /// Receive link rate in kbps (0 when N/A).
    pub rx_kbps: u32,
    /// Transmit link rate in kbps (0 when N/A).
    pub tx_kbps: u32,
}

/// Opens the client handle, polls every interface and closes it again.
/// Failures fall back to an empty list so diagnostics never hard-fail.
pub fn links() -> Vec<WlanLink> {
    unsafe {
        let mut handle: HANDLE = std::ptr::null_mut();
        let mut version = 0u32;
        if WlanOpenHandle(
            WLAN_API_VERSION,
            std::ptr::null(),
            &mut version,
            &mut handle,
        ) != 0
        {
            return Vec::new();
        }
        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        let result = WlanEnumInterfaces(handle, std::ptr::null(), &mut list);
        if result != 0 || list.is_null() {
            WlanCloseHandle(handle, std::ptr::null());
            return Vec::new();
        }
        let count = (*list).dwNumberOfItems as usize;
        let entries = std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), count);
        let mut out = Vec::with_capacity(count);
        for entry in entries {
            out.push(poll_interface(handle, entry));
        }
        WlanFreeMemory(list.cast());
        WlanCloseHandle(handle, std::ptr::null());
        out
    }
}

unsafe fn poll_interface(handle: HANDLE, entry: &WLAN_INTERFACE_INFO) -> WlanLink {
    let guid = format_guid(&entry.InterfaceGuid);
    let description = utf16_trim(&entry.strInterfaceDescription);
    let rssi = query_i32(handle, &entry.InterfaceGuid, wlan_intf_opcode_rssi);
    let channel = query_u32(
        handle,
        &entry.InterfaceGuid,
        wlan_intf_opcode_channel_number,
    );

    let association = query_alloc(
        handle,
        &entry.InterfaceGuid,
        wlan_intf_opcode_current_connection,
    )
    .and_then(|data| {
        if data.len() < std::mem::size_of::<WLAN_CONNECTION_ATTRIBUTES>() {
            return None;
        }
        let attributes = unsafe { &*(data.as_ptr().cast::<WLAN_CONNECTION_ATTRIBUTES>()) };
        let attrs = &attributes.wlanAssociationAttributes;
        Some((
            attributes.isState == wlan_interface_state_connected,
            ssid_text(attrs),
            mac_text(&attrs.dot11Bssid),
            attrs.wlanSignalQuality,
            attrs.ulRxRate,
            attrs.ulTxRate,
        ))
    });

    let (connected, ssid, bssid, signal, rx, tx) = match association {
        Some(link) => link,
        None => (
            entry.isState == wlan_interface_state_connected,
            String::new(),
            String::new(),
            0,
            0,
            0,
        ),
    };

    WlanLink {
        guid,
        description,
        connected,
        ssid,
        bssid,
        signal_quality: signal,
        rssi_dbm: rssi,
        channel,
        rx_kbps: rx,
        tx_kbps: tx,
    }
}

/// Calls `WlanQueryInterface` with the given opcode and returns the raw
/// callback buffer (an allocation the API handed to us).
unsafe fn query_alloc(handle: HANDLE, guid: &GUID, opcode: i32) -> Option<Vec<u8>> {
    let mut size = 0u32;
    let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut value_type = 0i32;
    let result = WlanQueryInterface(
        handle,
        guid,
        opcode,
        std::ptr::null(),
        &mut size,
        &mut data,
        &mut value_type,
    );
    if result != 0 || data.is_null() || size == 0 {
        if !data.is_null() {
            WlanFreeMemory(data);
        }
        return None;
    }
    let bytes = std::slice::from_raw_parts(data.cast::<u8>(), size as usize).to_vec();
    WlanFreeMemory(data);
    Some(bytes)
}

unsafe fn query_i32(handle: HANDLE, guid: &GUID, opcode: i32) -> Option<i32> {
    query_alloc(handle, guid, opcode).and_then(|data| {
        if data.len() >= 4 {
            Some(i32::from_le_bytes(data[0..4].try_into().ok()?))
        } else {
            None
        }
    })
}

unsafe fn query_u32(handle: HANDLE, guid: &GUID, opcode: i32) -> Option<u32> {
    query_alloc(handle, guid, opcode).and_then(|data| {
        if data.len() >= 4 {
            Some(u32::from_le_bytes(data[0..4].try_into().ok()?))
        } else {
            None
        }
    })
}

fn ssid_text(attrs: &WLAN_ASSOCIATION_ATTRIBUTES) -> String {
    let length = attrs.dot11Ssid.uSSIDLength as usize;
    let bytes = attrs.dot11Ssid.ucSSID;
    String::from_utf8_lossy(bytes.split_at(length.min(bytes.len())).0).into_owned()
}

fn mac_text(bytes: &[u8; 6]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn format_guid(guid: &GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7],
    )
}

fn utf16_trim(text: &[u16]) -> String {
    let end = text
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
        .trim_end()
        .to_string()
}

/// Renders the signal strength as a plain percentage. Block glyphs are a
/// font lottery in consoles, so the strength is shown without decoration.
pub fn signal_bar(quality: u32) -> String {
    format!("{quality:3}%")
}

/// Renders a link rate in kbps as a compact human string.
pub fn format_kbps(kbps: u32) -> String {
    if kbps >= 1000 {
        format!("{:.1} Mbps", f64::from(kbps) / 1000.0)
    } else {
        format!("{kbps} kbps")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_mac_address() {
        assert_eq!(
            mac_text(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]),
            "00:11:22:33:44:55"
        );
    }

    #[test]
    fn formats_guid() {
        let guid = GUID {
            data1: 0x00112233,
            data2: 0x4455,
            data3: 0x6677,
            data4: [0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        };
        assert_eq!(format_guid(&guid), "{00112233-4455-6677-8899-AABBCCDDEEFF}");
    }

    #[test]
    fn signal_bar_bounds() {
        assert!(signal_bar(0).ends_with("0%"));
        assert!(signal_bar(100).ends_with("100%"));
        assert!(signal_bar(1000).ends_with("1000%"), "never panics");
    }

    #[test]
    fn trims_null_terminated_utf16() {
        let mut text = [0u16; 256];
        let bytes = "Wireless".encode_utf16().collect::<Vec<_>>();
        text[..bytes.len()].copy_from_slice(&bytes);
        assert_eq!(utf16_trim(&text), "Wireless");
    }
}
