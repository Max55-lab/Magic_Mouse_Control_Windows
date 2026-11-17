// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Non-PnP control device: the only user-mode entry point.
//!
//! The device is named (`\Device\MMKControl`) and published through the
//! symbolic link `\DosDevices\MMKControl`, which user mode opens as
//! `\\.\MMKControl`. The fixed name keeps the user-mode surface independent of
//! the mouse being connected and independent of PnP enumeration.

use core::mem::size_of;
use core::ptr::null_mut;

use wdk::{nt_success, println};
use wdk_sys::{
    call_unsafe_wdf_function_binding, _WDF_IO_QUEUE_DISPATCH_TYPE, _WDF_TRI_STATE, NTSTATUS, PVOID,
    ULONG, ULONG_PTR, UNICODE_STRING, WDFDEVICE, WDFDRIVER, WDFQUEUE, WDFREQUEST,
    WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
};

use crate::ring::{Event, EVENT_CAP};

const FILE_DEVICE_UNKNOWN: u32 = 0x22;
const METHOD_BUFFERED: u32 = 0;
const FILE_ANY_ACCESS: u32 = 0;
const FILE_READ_DATA: u32 = 1;
#[cfg(feature = "vhf")]
const FILE_WRITE_DATA: u32 = 1;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

/// Encode an ASCII literal into a NUL-terminated UTF-16 buffer.
const fn wide<const N: usize>(s: &str) -> [u16; N] {
    let bytes = s.as_bytes();
    assert!(bytes.len() + 1 == N);
    let mut out = [0u16; N];
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}

/// Exact buffer length, including the trailing NUL.
const fn wide_len(s: &str) -> usize {
    s.len() + 1
}

// Names are ABI with `src/kernel_tap.rs`; change both sides together.
const DEVICE_NAME: [u16; wide_len("\\Device\\MMKControl")] = wide("\\Device\\MMKControl");
const SYMBOLIC_LINK: [u16; wide_len("\\DosDevices\\MMKControl")] = wide("\\DosDevices\\MMKControl");

/// SYSTEM and Administrators get full access; interactive users get
/// read/write so the non-elevated app can drain the ring. Everyone else gets
/// nothing. Keep in sync with the handle access in `kernel_tap.rs`.
const SDDL: [u16; wide_len("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)")] =
    wide("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)");

/// Build the `UNICODE_STRING` WDF expects. All callers pass one of the
/// `'static` buffers above, so the pointer stays valid for the WDF call.
fn unicode(value: &'static [u16]) -> UNICODE_STRING {
    let maximum = (value.len() * 2) as u16;
    UNICODE_STRING {
        Length: maximum - 2, // the trailing NUL is not part of the length
        MaximumLength: maximum,
        Buffer: value.as_ptr() as *mut u16,
    }
}

/// Liveness check; used by the app's watchdog (3 consecutive timeouts =>
/// fall back to the user-mode HID path).
pub const IOCTL_MMK_PING: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Drain up to `buffer_len / size_of::<Event>()` records. Non-blocking: returns
/// `STATUS_SUCCESS` with 0 bytes when there is nothing to read.
pub const IOCTL_MMK_READ_EVENTS: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_READ_DATA);

/// Submit one frame to the virtual precision touchpad (Phase 3 scaffold).
///
/// `IOCTL` function `0x802`, buffered write. The app sends a 30-byte
/// `MMK_VHID_FRAME`; the handler routes it to the VHF submit path and completes
/// with the resulting status. The `mmk.sys` filter itself never enters this arm
/// during Phase 0 — that is the `mmk-vhid.sys` function driver job.
#[cfg(feature = "vhf")]
pub const IOCTL_MMK_VHID_SUBMIT: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_WRITE_DATA);

const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_INVALID_DEVICE_REQUEST: NTSTATUS = 0xC000_0010u32 as NTSTATUS;

pub unsafe fn create(driver: WDFDRIVER) -> NTSTATUS {
    // Explicit SDDL: the non-elevated app must be able to open the device.
    // A NULL descriptor would leave the ACL to the framework defaults.
    let sddl = unicode(&SDDL);
    let mut device_init =
        unsafe { call_unsafe_wdf_function_binding!(WdfControlDeviceInitAllocate, driver, &sddl) };
    if device_init.is_null() {
        return STATUS_INVALID_DEVICE_REQUEST;
    }

    // A control device cannot register a PnP device interface, so name it and
    // publish the stable user-mode path with a symbolic link instead.
    let name = unicode(&DEVICE_NAME);
    let status =
        unsafe { call_unsafe_wdf_function_binding!(WdfDeviceInitAssignName, device_init, &name) };
    if !nt_success(status) {
        println!("[mmk] WdfDeviceInitAssignName failed {status:#010X}");
        return status;
    }

    let mut device: WDFDEVICE = WDF_NO_HANDLE as WDFDEVICE;
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &raw mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut device,
        )
    };
    if !nt_success(status) {
        println!("[mmk] control WdfDeviceCreate failed {status:#010X}");
        return status;
    }

    let link = unicode(&SYMBOLIC_LINK);
    let status =
        unsafe { call_unsafe_wdf_function_binding!(WdfDeviceCreateSymbolicLink, device, &link) };
    if !nt_success(status) {
        println!("[mmk] WdfDeviceCreateSymbolicLink failed {status:#010X}");
        return status;
    }

    let mut config = WDF_IO_QUEUE_CONFIG {
        Size: size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG,
        PowerManaged: _WDF_TRI_STATE::WdfUseDefault,
        DefaultQueue: u8::from(true),
        DispatchType: _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchParallel,
        EvtIoDeviceControl: Some(evt_io_device_control),
        ..WDF_IO_QUEUE_CONFIG::default()
    };

    let mut queue: WDFQUEUE = WDF_NO_HANDLE as WDFQUEUE;
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            &raw mut config,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut queue,
        )
    };
    if !nt_success(status) {
        println!("[mmk] control WdfIoQueueCreate failed {status:#010X}");
        return status;
    }

    // Required by KMDF for control devices before returning, so user mode can open handles.
    unsafe {
        call_unsafe_wdf_function_binding!(WdfControlFinishInitializing, device);
    }

    0
}

unsafe extern "C" fn evt_io_device_control(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    output_buffer_length: usize,
    _input_buffer_length: usize,
    io_control_code: ULONG,
) {
    match io_control_code {
        IOCTL_MMK_PING => unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestCompleteWithInformation,
                request,
                STATUS_SUCCESS,
                0 as ULONG_PTR,
            )
        },
        IOCTL_MMK_READ_EVENTS => unsafe { read_events(request, output_buffer_length) },
        #[cfg(feature = "vhf")]
        IOCTL_MMK_VHID_SUBMIT => unsafe {
            // Phase 3 scaffold: app -> frame -> VHF. The mmk.sys filter itself
            // never enters this arm during Phase 0; that is the mmk-vhid function
            // driver job.
            let context = crate::vhf::control_context();
            let mut buffer: PVOID = core::ptr::null_mut();
            let mut length: usize = 0;
            let status = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestRetrieveInputBuffer,
                    request,
                    _input_buffer_length,
                    &raw mut buffer,
                    &raw mut length
                )
            };
            if !nt_success(status) {
                call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status);
                return;
            }
            let frame_status =
                unsafe { crate::vhf::submit_frame(context, buffer as *const u8, length) };
            call_unsafe_wdf_function_binding!(
                WdfRequestCompleteWithInformation,
                request,
                frame_status,
                0 as ULONG_PTR
            );
        },
        _ => unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestComplete,
                request,
                STATUS_INVALID_DEVICE_REQUEST
            )
        },
    }
}

unsafe fn read_events(request: WDFREQUEST, output_buffer_length: usize) {
    let record = size_of::<Event>();
    if output_buffer_length < record {
        unsafe {
            call_unsafe_wdf_function_binding!(WdfRequestComplete, request, STATUS_BUFFER_TOO_SMALL)
        };
        return;
    }

    let mut buffer: PVOID = null_mut();
    let mut length: usize = 0;
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            record,
            &raw mut buffer,
            &raw mut length,
        )
    };
    if !nt_success(status) {
        // Typically STATUS_BUFFER_TOO_SMALL; pass it through so user mode can
        // report the required size instead of guessing.
        unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status) };
        return;
    }

    // Never trust the advertised length blindly; cap it by the caller's buffer
    // and by the ring capacity.
    let max = core::cmp::min(length / record, EVENT_CAP);
    let copied = unsafe { crate::ring::pop_into(buffer as *mut Event, max) };

    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCompleteWithInformation,
            request,
            STATUS_SUCCESS,
            (copied * record) as ULONG_PTR,
        )
    };
}

const STATUS_BUFFER_TOO_SMALL: NTSTATUS = 0xC000_0023u32 as NTSTATUS;
