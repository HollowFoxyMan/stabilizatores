//! Small safe wrappers around the Win32 registry for DWORD values.
//!
//! Every function opens the requested key from `HKEY_LOCAL_MACHINE` with the
//! minimal access right, performs one operation and closes the handle again.
//! A missing key or value is reported as `Ok(None)` by [`read_dword`] so the
//! callers can treat "not configured" as a plain result instead of an error.

use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_SZ,
};

use crate::win::wide;

const KEY_READ_ACCESS_QUERY: u32 = KEY_QUERY_VALUE;
const KEY_WRITE_ACCESS: u32 = KEY_SET_VALUE;
/// Max registry subkey name length, matches Windows' `MAX_KEY_LENGTH`.
const MAX_NAME: usize = 255;

fn open(subkey: &str, access: u32, create: bool) -> io::Result<HKEY> {
    let mut key: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            wide(subkey).as_ptr(),
            0,
            access,
            &mut key,
        )
    };
    if status == 0 {
        return Ok(key);
    }
    if create && status == ERROR_FILE_NOT_FOUND {
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_LOCAL_MACHINE,
                wide(subkey).as_ptr(),
                0,
                ptr::null(),
                0,
                access,
                ptr::null(),
                &mut key,
                ptr::null_mut(),
            )
        };
        if status == 0 {
            return Ok(key);
        }
    }
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(std::ptr::null_mut());
    }
    Err(io::Error::from_raw_os_error(status as i32))
}

/// Reads a DWORD value. Missing key or value yields `Ok(None)`.
pub fn read_dword(subkey: &str, name: &str) -> io::Result<Option<u32>> {
    let key = open(subkey, KEY_READ_ACCESS_QUERY, false)?;
    if key.is_null() {
        return Ok(None);
    }
    let mut data: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let mut value_type: u32 = 0;
    let status = unsafe {
        RegQueryValueExW(
            key,
            wide(name).as_ptr(),
            std::ptr::null(),
            &mut value_type,
            &mut data as *mut u32 as *mut u8,
            &mut size,
        )
    };
    unsafe { RegCloseKey(key) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if value_type != REG_DWORD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("value '{name}' is not a DWORD"),
        ));
    }
    Ok(Some(data))
}

/// Writes a DWORD value, creating the key when it does not exist yet.
pub fn write_dword(subkey: &str, name: &str, value: u32) -> io::Result<()> {
    let key = open(subkey, KEY_WRITE_ACCESS, true)?;
    if key.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("cannot open '{subkey}' for writing"),
        ));
    }
    let status = unsafe {
        RegSetValueExW(
            key,
            wide(name).as_ptr(),
            0,
            REG_DWORD,
            (&value as *const u32).cast::<u8>(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    unsafe { RegCloseKey(key) };
    if status != 0 {
        Err(io::Error::from_raw_os_error(status as i32))
    } else {
        Ok(())
    }
}

/// Returns true when the key exists and is readable.
pub fn key_exists(subkey: &str) -> bool {
    match open(subkey, KEY_READ_ACCESS_QUERY, false) {
        Ok(key) if !key.is_null() => {
            unsafe { RegCloseKey(key) };
            true
        }
        _ => false,
    }
}

/// Lists the immediate sub-key names of `subkey`. A missing key yields an
/// empty list.
pub fn read_names(subkey: &str) -> io::Result<Vec<String>> {
    let key = open(subkey, KEY_READ_ACCESS_QUERY, false)?;
    if key.is_null() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    let mut index: u32 = 0;
    loop {
        let mut buffer = vec![0u16; MAX_NAME + 1];
        let mut length = buffer.len() as u32;
        let status = unsafe {
            RegEnumKeyExW(
                key,
                index,
                buffer.as_mut_ptr(),
                &mut length,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            break;
        }
        if status != 0 {
            unsafe { RegCloseKey(key) };
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        buffer.truncate(length as usize);
        names.push(String::from_utf16_lossy(&buffer));
        index += 1;
    }
    unsafe { RegCloseKey(key) };
    Ok(names)
}

/// Reads a REG_SZ string value. Missing key or value yields `Ok(None)`.
pub fn read_string(subkey: &str, name: &str) -> io::Result<Option<String>> {
    let key = open(subkey, KEY_READ_ACCESS_QUERY, false)?;
    if key.is_null() {
        return Ok(None);
    }
    let mut size: u32 = 0;
    let mut value_type: u32 = 0;
    let status = unsafe {
        RegQueryValueExW(
            key,
            wide(name).as_ptr(),
            std::ptr::null(),
            &mut value_type,
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if status == ERROR_FILE_NOT_FOUND || value_type != REG_SZ {
        unsafe { RegCloseKey(key) };
        return Ok(None);
    }
    if status != 0 {
        unsafe { RegCloseKey(key) };
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let mut buffer = vec![0u16; size as usize / 2 + 1];
    let status = unsafe {
        RegQueryValueExW(
            key,
            wide(name).as_ptr(),
            std::ptr::null(),
            &mut value_type,
            buffer.as_mut_ptr().cast::<u8>(),
            &mut size,
        )
    };
    unsafe { RegCloseKey(key) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let end = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    buffer.truncate(end);
    Ok(Some(String::from_utf16_lossy(&buffer)))
}

/// Deletes a value. A value that is already absent is treated as success, so
/// reverting an unapplied tweak stays a no-op.
pub fn delete_value(subkey: &str, name: &str) -> io::Result<()> {
    let key = open(subkey, KEY_WRITE_ACCESS, false)?;
    if key.is_null() {
        return Ok(());
    }
    let status = unsafe { RegDeleteValueW(key, wide(name).as_ptr()) };
    unsafe { RegCloseKey(key) };
    if status == 0 || status == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_missing_key_is_none() {
        let value = read_dword(
            r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces",
            "TCPNoDelay",
        )
        .expect("reading should not fail");
        assert_eq!(value, None);
    }
}
