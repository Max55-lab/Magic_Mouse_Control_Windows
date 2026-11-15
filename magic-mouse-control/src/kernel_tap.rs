// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Optional user-mode reader for the experimental `mmk.sys` kernel tap.
//!
//! This module is **purely additive**: the app works exactly as before when the
//! driver is not installed (`try_open` returns `None`). It never blocks the UI
//! thread and never panics on a missing device.
//!
//! The on-the-wire layout must match `driver/src/ring.rs::Event` byte for byte.
//! The const assertion below guards that at compile time for our side; the
//! driver side is guarded by `#[repr(C)]` and the same field order.

use std::mem::size_of;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;

/// Must match `driver/src/lib.rs::GUID_DEVINTERFACE_MMK`.
pub const GUID_DEVINTERFACE_MMK: GUID = GUID {
    data1: 0x7f0c_3a2e,
    data2: 0x2b41,
    data3: 0x4b8f,
    data4: [0x9c, 0x1d, 0x9f, 0x0a, 0x6f, 0x3c, 0x5d, 0x21],
};

/// Must match `driver/src/control.rs::IOCTL_MMK_PING`.
pub const IOCTL_MMK_PING: u32 = 0x0022_2000;
/// Must match `driver/src/control.rs::IOCTL_MMK_READ_EVENTS`.
pub const IOCTL_MMK_READ_EVENTS: u32 = 0x0022_6004;

/// Mirror of `driver/src/ring.rs::Event`. Field order and types are part of the
/// ABI; do not reorder.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Event {
    pub ioctl: u32,
    pub status: i32,
    pub len: u16,
    pub kind: u8,
    pub first: u8,
    pub data: [u8; 32],
}

// Compile-time layout guard: 4 + 4 + 2 + 1 + 1 + 32 = 44.
const _: () = assert!(size_of::<Event>() == 44);

pub const EVENT_DATA: usize = 32;
pub const EVENT_CAP: usize = 256;

pub const KIND_INTERNAL_IOCTL: u8 = 0;
pub const KIND_READ: u8 = 1;

pub struct KernelTap {
    handle: HANDLE,
}

impl KernelTap {
    /// Try to open the device. Returns `None` when the driver is not installed
    /// or the device is not present - never an error.
    pub fn try_open() -> Option<Self> {
        let path = first_device_path()?;
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (windows::Win32::Foundation::GENERIC_READ
                    | windows::Win32::Foundation::GENERIC_WRITE)
                    .0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .ok()?;
        if handle == INVALID_HANDLE_VALUE || handle.is_invalid() {
            return None;
        }
        Some(Self { handle })
    }

    pub fn is_alive(&self) -> bool {
        let mut returned: u32 = 0;
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_MMK_PING,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            )
            .is_ok()
        }
    }

    /// Drain whatever the ring currently holds. Non-blocking.
    pub fn read_events(&self) -> Vec<Event> {
        const BATCH: usize = 64;
        let mut out = vec![Event::default(); BATCH];
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_MMK_READ_EVENTS,
                None,
                0,
                Some(out.as_mut_ptr() as *mut _),
                (BATCH * size_of::<Event>()) as u32,
                Some(&mut returned),
                None,
            )
        };
        if ok.is_err() {
            return Vec::new();
        }
        let count = (returned as usize) / size_of::<Event>();
        out.truncate(count.min(BATCH));
        out
    }

    /// Decode the first touch-bearing report in an event, if any.
    pub fn touch_report_bytes(event: &Event) -> Option<&[u8]> {
        if event.len == 0 {
            return None;
        }
        let n = (event.len as usize).min(EVENT_DATA);
        Some(&event.data[..n])
    }
}

impl Drop for KernelTap {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

fn first_device_path() -> Option<Vec<u16>> {
    let hdev = match unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_DEVINTERFACE_MMK),
            None,
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    } {
        Ok(h) => h,
        Err(_) => return None,
    };
    if hdev.0 == 0 || hdev.0 == -1 {
        return None;
    }

    let mut result = None;
    let mut index: u32 = 0;
    loop {
        let mut iface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        if unsafe {
            SetupDiEnumDeviceInterfaces(hdev, None, &GUID_DEVINTERFACE_MMK, index, &mut iface)
        }
        .is_err()
        {
            break;
        }

        // First call: get the required buffer size.
        let mut required: u32 = 0;
        let _ = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                hdev,
                &iface,
                None,
                0,
                Some(&mut required),
                None,
            )
        };
        if required == 0 {
            index += 1;
            continue;
        }

        let mut buffer = vec![0u8; required as usize];
        // cbSize for SP_DEVICE_INTERFACE_DETAIL_DATA_W is the size of the struct
        // header (4 + 2, padded). The windows crate gives the correct value.
        let header_size = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
        unsafe {
            std::ptr::write_unaligned(buffer.as_mut_ptr() as *mut u32, header_size);
        }

        if unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                hdev,
                &iface,
                Some(buffer.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W),
                required,
                Some(&mut required),
                None,
            )
        }
        .is_ok()
        {
            // DevicePath starts right after cbSize.
            let ptr = unsafe { buffer.as_ptr().add(4) as *const u16 };
            // Find the null terminator.
            let mut len = 0usize;
            while unsafe { *ptr.add(len) } != 0 {
                len += 1;
                if len > 1024 {
                    break;
                }
            }
            let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
            result = Some(slice.to_vec());
            // Append a trailing NUL for PCWSTR safety.
            result.as_mut().unwrap().push(0);
            break;
        }

        index += 1;
    }

    unsafe {
        let _ = SetupDiDestroyDeviceInfoList(hdev);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_layout_matches_driver() {
        assert_eq!(size_of::<Event>(), 44);
    }

    #[test]
    fn ioctl_constants_match_driver() {
        // PING: ctl_code(0x22, 0x800, 0, 0)
        assert_eq!(IOCTL_MMK_PING, 0x0022_2000);
        // READ_EVENTS: ctl_code(0x22, 0x801, 0, 1)
        assert_eq!(IOCTL_MMK_READ_EVENTS, 0x0022_6004);
    }

    #[test]
    fn touch_report_bytes_truncates_to_payload() {
        let mut event = Event::default();
        event.len = 5;
        event.data[..5].copy_from_slice(&[0x12, 0x00, 0x01, 0x02, 0x03]);
        let bytes = KernelTap::touch_report_bytes(&event).unwrap();
        assert_eq!(bytes, &[0x12, 0x00, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn empty_event_has_no_report() {
        let event = Event::default();
        assert!(KernelTap::touch_report_bytes(&event).is_none());
    }
}
