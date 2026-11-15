// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Non-PnP control device: the only user-mode entry point.
//!
//! A control device (instead of a symlink on the filter device) keeps the
//! user-mode surface independent of the mouse being connected, and makes it
//! obvious in the object tree what belongs to us.

use core::mem::size_of;
use core::ptr::{null, null_mut};

use wdk::{nt_success, println};
use wdk_sys::{
    call_unsafe_wdf_function_binding, _WDF_IO_QUEUE_DISPATCH_TYPE, _WDF_TRI_STATE, NTSTATUS, PVOID,
    ULONG, WDFDEVICE, WDFDRIVER, WDFQUEUE, WDFREQUEST, WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE,
    WDF_NO_OBJECT_ATTRIBUTES,
};

use crate::ring::{Event, EVENT_CAP};

const FILE_DEVICE_UNKNOWN: u32 = 0x22;
const METHOD_BUFFERED: u32 = 0;
const FILE_ANY_ACCESS: u32 = 0;
const FILE_READ_DATA: u32 = 1;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

/// Liveness check; used by the app's watchdog (3 consecutive timeouts =>
/// fall back to the user-mode HID path).
pub const IOCTL_MMK_PING: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS);

/// Drain up to `buffer_len / size_of::<Event>()` records. Non-blocking: returns
/// `STATUS_SUCCESS` with 0 bytes when there is nothing to read.
pub const IOCTL_MMK_READ_EVENTS: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_READ_DATA);

const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_INVALID_DEVICE_REQUEST: NTSTATUS = 0xC000_0010u32 as NTSTATUS;

pub unsafe fn create(driver: WDFDRIVER) -> NTSTATUS {
    // A NULL SDDL string gives the framework's default control-device
    // descriptor. Hardening note: for a distributed build, pass an explicit
    // "D:P(A;;GA;;;SY)(A;;GA;;;BA)" string so only SYSTEM/Admins can open it.
    let mut device_init =
        unsafe { call_unsafe_wdf_function_binding!(WdfControlDeviceInitAllocate, driver, null()) };
    if device_init.is_null() {
        return STATUS_INVALID_DEVICE_REQUEST;
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

    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &crate::GUID_DEVINTERFACE_MMK,
            null(),
        )
    };
    if !nt_success(status) {
        println!("[mmk] CreateDeviceInterface failed {status:#010X}");
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
                0usize,
            )
        },
        IOCTL_MMK_READ_EVENTS => unsafe { read_events(request, output_buffer_length) },
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
            copied * record,
        )
    };
}

const STATUS_BUFFER_TOO_SMALL: NTSTATUS = 0xC000_0023u32 as NTSTATUS;
