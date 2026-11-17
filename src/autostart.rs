//! "Start with Windows" support through the per-user Run registry key.
//!
//! No scheduled task and no elevation are required: Windows launches the app
//! from `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` when the user logs
//! in. The app is started with `--minimized`, so it goes straight to the tray.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE_NAME: &str = "Magic Mouse Control";
const ERROR_FILE_NOT_FOUND: u32 = 2;

struct KeyGuard(HKEY);

impl Drop for KeyGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn open_run_key() -> Result<KeyGuard, String> {
    let mut key = HKEY(ptr::null_mut());
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut key,
        )
    };
    if status.0 != 0 {
        return Err(format!(
            "Could not open the Windows startup registry key (error {}).",
            status.0
        ));
    }
    Ok(KeyGuard(key))
}

fn quote_exe(path: &std::path::Path) -> String {
    format!("\"{}\" --minimized", path.display())
}

/// True only when the Run value points at *this* executable. A stale value
/// from a moved executable is reported as disabled, so the user can repair it
/// by toggling the option.
pub fn is_enabled() -> bool {
    let Some(current_exe) = std::env::current_exe().ok() else {
        return false;
    };
    let expected = current_exe.to_string_lossy().to_lowercase();

    let Ok(key) = open_run_key() else {
        return false;
    };

    let name = wide(VALUE_NAME);
    let mut buffer = [0u16; 2048];
    let mut bytes = (buffer.len() * std::mem::size_of::<u16>()) as u32;
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            PCWSTR::from_raw(name.as_ptr()),
            None,
            None,
            Some(buffer.as_mut_ptr().cast::<u8>()),
            Some(&mut bytes),
        )
    };
    if status.0 != 0 {
        return false;
    }

    let units = (bytes as usize / 2).min(buffer.len());
    let end = buffer[..units]
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units);
    let value = String::from_utf16_lossy(&buffer[..end]);
    value.to_lowercase().contains(&expected)
}

/// Add or remove the per-user startup entry. The current executable path is
/// always written when enabling, which also repairs stale entries.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let key = open_run_key()?;
    let name = wide(VALUE_NAME);

    let status: WIN32_ERROR = if enabled {
        let exe = std::env::current_exe()
            .map_err(|error| format!("Could not locate the application executable: {error}"))?;
        let command = quote_exe(&exe);
        let data: Vec<u8> = command
            .encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        unsafe {
            RegSetValueExW(
                key.0,
                PCWSTR::from_raw(name.as_ptr()),
                0,
                REG_SZ,
                Some(data.as_slice()),
            )
        }
    } else {
        unsafe { RegDeleteValueW(key.0, PCWSTR::from_raw(name.as_ptr())) }
    };

    if status.0 == 0 || (!enabled && status.0 == ERROR_FILE_NOT_FOUND) {
        Ok(())
    } else {
        Err(format!(
            "Could not update the Windows startup entry (error {}).",
            status.0
        ))
    }
}
