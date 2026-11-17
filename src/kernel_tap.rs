// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Optional user-mode reader for the experimental `mmk.sys` kernel tap.
//!
//! # What this module does
//!
//! When the optional kernel filter driver (`mmk.sys`) is installed, it sits at
//! the bottom of the Bluetooth HID device stack and copies every completed
//! HID request into a pre-allocated ring buffer in non-paged kernel memory.
//! This module opens a handle to that ring, drains it on demand, and hands raw
//! HIDP frames to the rest of the application.
//!
//! Without the driver the module is completely inert: `try_open` returns `None`
//! and nothing else is ever called.  The app never panics or errors on the
//! absence of `mmk.sys`.
//!
//! # Why a fixed device path
//!
//! `mmk.sys` creates a named control device and publishes the symbolic link
//! `\DosDevices\MMKControl`, which user mode opens as `\\.\MMKControl`.  The
//! name is stable across reboots and driver updates, and it exists even when
//! the mouse is disconnected — a control device cannot register a PnP device
//! interface in the first place.
//!
//! # ABI contract with the kernel
//!
//! The [`Event`] struct below must be **byte-for-byte identical** to
//! `driver/src/ring.rs::Event`.  Both sides are `#[repr(C)]`, the field order
//! is locked, and the compile-time assert at the bottom of this file catches
//! any accidental drift during development.

use std::mem::size_of;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

// ---------------------------------------------------------------------------
// Device path and IOCTL constants — must stay in sync with
// driver/src/control.rs.  Any mismatch means the call silently fails.
// ---------------------------------------------------------------------------

/// Symbolic link published by `mmk.sys`. Must match
/// `driver/src/control.rs::SYMBOLIC_LINK`.
const CONTROL_DEVICE_PATH: &str = r"\\.\MMKControl";

/// Watchdog IOCTL: zero-byte input, zero-byte output.  Returns STATUS_SUCCESS
/// when the driver is loaded and responsive.
/// CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, function=0x800, METHOD_BUFFERED=0, FILE_ANY_ACCESS=0)
/// Must match `driver/src/control.rs::IOCTL_MMK_PING`.
pub const IOCTL_MMK_PING: u32 = 0x0022_2000;

/// Ring-drain IOCTL: zero-byte input, output is a packed array of [`Event`].
/// The driver copies up to N completed records and returns the byte count.
/// CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, function=0x801, METHOD_BUFFERED=0, FILE_READ_DATA=1)
/// Must match `driver/src/control.rs::IOCTL_MMK_READ_EVENTS`.
pub const IOCTL_MMK_READ_EVENTS: u32 = 0x0022_6004;

// ---------------------------------------------------------------------------
// Shared data layout (kernel <-> user-mode ABI)
// ---------------------------------------------------------------------------

/// Number of data bytes stored in each ring record.
/// Sized to hold a full 6-finger Magic Mouse touch report (approx 60 bytes)
/// with a few bytes of headroom.  Must match `driver/src/ring.rs::EVENT_DATA`.
pub const EVENT_DATA: usize = 64;

/// Maximum number of events the ring holds at once.
/// Must match `driver/src/ring.rs::EVENT_CAP`.
#[allow(dead_code)]
pub const EVENT_CAP: usize = 256;

/// One ring record, shared between the kernel and user-mode.
///
/// **Field order and types are ABI** — the kernel writes these bytes and the
/// user reads them directly via `DeviceIoControl`.  `#[repr(C)]` removes any
/// Rust-specific padding or reordering.
///
/// Layout: 4 + 4 + 2 + 1 + 1 + 64 = 76 bytes (confirmed by the assert below).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// IOCTL code if this record came from an internal IOCTL, or 0 for a plain
    /// `IRP_MJ_READ`.  Used for filtering in tests; the gesture engine ignores it.
    pub ioctl: u32,
    /// `NTSTATUS` from the lower driver.  Negative values indicate failure;
    /// those records are discarded by `decode_tap_event` in touch_source.rs.
    pub status: i32,
    /// Number of bytes actually returned by the lower driver (before truncation
    /// to `EVENT_DATA`).  Use this to slice `data`, not `EVENT_DATA`.
    pub len: u16,
    /// Source kind: `KIND_INTERNAL_IOCTL` (0) or `KIND_READ` (1).
    pub kind: u8,
    /// First byte of `data`, copied here for quick report-ID inspection without
    /// a pointer indirection.
    pub first: u8,
    /// Raw payload bytes from the HID request buffer.  Only the first `len`
    /// bytes (clamped to `EVENT_DATA`) are meaningful.
    pub data: [u8; EVENT_DATA],
}

/// Manual `Default` impl because Rust only auto-derives `Default` for arrays
/// up to length 32; `[u8; 64]` requires this explicit implementation.
impl Default for Event {
    fn default() -> Self {
        Self {
            ioctl: 0,
            status: 0,
            len: 0,
            kind: 0,
            first: 0,
            data: [0u8; EVENT_DATA],
        }
    }
}

/// Compile-time ABI guard: if either side accidentally changes a field type or
/// adds padding the size changes and this line refuses to compile.
/// 4 (ioctl) + 4 (status) + 2 (len) + 1 (kind) + 1 (first) + 64 (data) = 76.
const _: () = assert!(size_of::<Event>() == 76);

/// Record came from an `IRP_MJ_INTERNAL_DEVICE_CONTROL` completion.
#[allow(dead_code)]
pub const KIND_INTERNAL_IOCTL: u8 = 0;
/// Record came from an `IRP_MJ_READ` completion.
#[allow(dead_code)]
pub const KIND_READ: u8 = 1;

// ---------------------------------------------------------------------------
// KernelTap — the user-mode handle to the driver's ring buffer
// ---------------------------------------------------------------------------

/// A live connection to the `mmk.sys` ring buffer.
///
/// Obtain one with [`KernelTap::try_open`].  The handle is released on drop.
pub struct KernelTap {
    /// Win32 file handle to the driver's control device object.
    handle: HANDLE,
}

impl KernelTap {
    /// Try to open the driver's control device.
    ///
    /// Returns `None` without an error when:
    /// * the driver is not installed or not loaded,
    /// * the symbolic link is not present,
    /// * `CreateFileW` fails (e.g. access denied).
    ///
    /// The caller should retry when the driver becomes available rather than
    /// treating `None` as a permanent failure.
    pub fn try_open() -> Option<Self> {
        // CreateFileW wants a NUL-terminated wide string. Opening happens only
        // on driver (re)connect, so the small allocation does not matter.
        let path: Vec<u16> = CONTROL_DEVICE_PATH.encode_utf16().chain([0]).collect();
        // SAFETY: `path` is a valid, null-terminated UTF-16 string.  The ACL
        // installed by `control.rs` grants read/write to interactive users.
        // FILE_SHARE_READ|WRITE lets multiple processes probe for the driver
        // without competing for exclusive access.  OPEN_EXISTING is correct:
        // the device object is created by the driver, not by us.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (windows::Win32::Foundation::GENERIC_READ
                    | windows::Win32::Foundation::GENERIC_WRITE)
                    .0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,          // default security descriptor
                OPEN_EXISTING, // the device must already exist
                FILE_ATTRIBUTE_NORMAL,
                None, // no template file
            )
        }
        .ok()?;
        if handle == INVALID_HANDLE_VALUE || handle.is_invalid() {
            return None;
        }
        Some(Self { handle })
    }

    /// Send a zero-byte ping IOCTL and return whether the driver answered.
    ///
    /// Used as a watchdog: if the driver unloads or the device disappears while
    /// we hold the handle, the kernel returns an error and this returns `false`,
    /// triggering a reconnect attempt.
    pub fn is_alive(&self) -> bool {
        let mut returned: u32 = 0;
        // SAFETY: Both input and output buffers are None/zero-length, which is
        // exactly what IOCTL_MMK_PING expects (no payload in either direction).
        // `self.handle` is valid as long as `self` is alive (enforced by Drop).
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_MMK_PING,
                None, // no input buffer
                0,
                None, // no output buffer
                0,
                Some(&mut returned),
                None, // synchronous call, no OVERLAPPED
            )
            .is_ok()
        }
    }

    /// Drain whatever the ring currently holds into `out`.  Non-blocking.
    ///
    /// `out` is cleared and filled with up to `BATCH` events.  Using a
    /// caller-supplied buffer avoids a heap allocation on every 100 Hz tick.
    ///
    /// Returns the number of events written into `out`.
    pub fn read_events_into(&self, out: &mut Vec<Event>) -> usize {
        // 64 records per call is generous: at 100 Hz the ring accumulates at
        // most a handful of records between polls.
        const BATCH: usize = 64;
        out.clear();
        out.resize(BATCH, Event::default());
        let mut returned: u32 = 0;
        // SAFETY: `out` is a Vec<Event> with exactly BATCH initialised elements.
        // Casting to `*mut _` is safe because Event is #[repr(C)] and
        // DeviceIoControl writes raw bytes that map directly onto the struct
        // fields — the kernel side produces the same layout.  `returned`
        // receives the byte count written, used below to truncate `out`.
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_MMK_READ_EVENTS,
                None, // no input
                0,
                Some(out.as_mut_ptr() as *mut _), // output: packed Event array
                (BATCH * size_of::<Event>()) as u32,
                Some(&mut returned),
                None, // synchronous
            )
        };
        if ok.is_err() {
            out.clear();
            return 0;
        }
        // Divide the byte count by struct size to get the number of complete
        // records, then shrink the Vec to exactly that many.
        let count = (returned as usize) / size_of::<Event>();
        out.truncate(count.min(BATCH));
        out.len()
    }

    /// Drain whatever the ring currently holds and return a fresh `Vec`.
    ///
    /// Convenience wrapper around [`read_events_into`] for call sites that do
    /// not need to reuse a buffer (e.g. one-shot queries in tests).
    #[allow(dead_code)]
    pub fn read_events(&self) -> Vec<Event> {
        let mut out = Vec::new();
        self.read_events_into(&mut out);
        out
    }

    /// Return a slice of the meaningful payload bytes in `event`, or `None`
    /// if the event carries no data.
    ///
    /// `event.len` is the authoritative length; `EVENT_DATA` is an upper bound
    /// on what was actually copied into the buffer.
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
        // SAFETY: `self.handle` was obtained from a successful `CreateFileW`
        // and has not been closed anywhere else.  Leaking a kernel handle would
        // prevent the driver from unloading cleanly, so we always close even
        // on a panicking path.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the struct size matches the kernel definition.
    /// If this fails, update EVENT_DATA or the field types on both sides.
    #[test]
    fn event_layout_matches_driver() {
        assert_eq!(size_of::<Event>(), 76);
    }

    /// Verify the IOCTL codes match the CTL_CODE macro output documented above.
    #[test]
    fn ioctl_constants_match_driver() {
        // CTL_CODE(0x22, 0x800, METHOD_BUFFERED=0, FILE_ANY_ACCESS=0)
        assert_eq!(IOCTL_MMK_PING, 0x0022_2000);
        // CTL_CODE(0x22, 0x801, METHOD_BUFFERED=0, FILE_READ_DATA=1)
        assert_eq!(IOCTL_MMK_READ_EVENTS, 0x0022_6004);
    }

    /// `touch_report_bytes` must slice to exactly `len` bytes, not the full array.
    #[test]
    fn touch_report_bytes_truncates_to_payload() {
        let mut event = Event::default();
        event.len = 5;
        event.data[..5].copy_from_slice(&[0x12, 0x00, 0x01, 0x02, 0x03]);
        let bytes = KernelTap::touch_report_bytes(&event).unwrap();
        assert_eq!(bytes, &[0x12, 0x00, 0x01, 0x02, 0x03]);
    }

    /// An event with `len = 0` carries no report and must return `None`.
    #[test]
    fn empty_event_has_no_report() {
        let event = Event::default();
        assert!(KernelTap::touch_report_bytes(&event).is_none());
    }
}
