// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Observation-only forwarding path.
//!
//! Every request that reaches these callbacks is forwarded to the next lower
//! driver with its original parameters and completed with the original status
//! and information. The only extra work is a bounded copy of the buffers into
//! the ring buffer, plus a rate-limited debug print.

use core::ptr::null_mut;

use wdk::println;
use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, ULONG, WDFDEVICE, WDFIOTARGET, WDFQUEUE,
    WDFREQUEST, WDF_REQUEST_COMPLETION_PARAMS,
};

use crate::probe;

/// Which callback produced the record. Kept short so it fits a ring record.
pub const KIND_INTERNAL_IOCTL: u8 = 0;
pub const KIND_READ: u8 = 1;

const STATUS_INVALID_DEVICE_REQUEST: NTSTATUS = 0xC000_0010u32 as NTSTATUS;

// Values from `wdfio.h`. Kept numeric on purpose: how bindgen names the
// `WDF_REQUEST_STOP_ACTION_FLAGS` enum is a build-time detail, while the values
// are part of the WDF ABI.
const STOP_ACTION_MASK: ULONG = 0x7FFF_FFFF;
const STOP_ACTION_SUSPEND: ULONG = 1;
const STOP_ACTION_PURGE: ULONG = 2;

pub unsafe extern "C" fn evt_io_internal_device_control(
    queue: WDFQUEUE,
    request: WDFREQUEST,
    output_buffer_length: usize,
    input_buffer_length: usize,
    io_control_code: ULONG,
) {
    unsafe {
        probe::log_dispatch(
            io_control_code,
            KIND_INTERNAL_IOCTL,
            output_buffer_length,
            input_buffer_length,
        );
        forward(queue, request);
    }
}

pub unsafe extern "C" fn evt_io_read(queue: WDFQUEUE, request: WDFREQUEST, length: usize) {
    unsafe {
        probe::log_dispatch(0, KIND_READ, length, 0);
        forward(queue, request);
    }
}

/// The queue's device is leaving D0. WDF calls this once for **every** request
/// that is still outstanding, including requests we already forwarded to
/// `BthEnum`. Ignoring those requests is a classic cause of bugcheck `0x9F`:
/// the framework waits for them before the power transition can complete.
///
/// Our request lifecycle is simple and bounded:
/// * a request is either forwarded and owned by `on_complete`, or
/// * `WdfRequestSend` failed and the request was completed immediately.
///
/// Therefore a request seen here is always an in-flight forwarded request. The
/// lower Bluetooth stack drains its outstanding I/O on the Dx IRP, and our
/// completion routine only touches non-paged memory and `WdfRequestComplete`,
/// which is legal while the device is in a low-power state.
pub unsafe extern "C" fn evt_io_stop(_queue: WDFQUEUE, request: WDFREQUEST, action_flags: ULONG) {
    let action = action_flags & STOP_ACTION_MASK;
    unsafe { probe::log_power("stop", action) };

    match action {
        STOP_ACTION_SUSPEND => unsafe {
            // Requeue = FALSE: the request stays with the lower target. When
            // it completes, `on_complete` completes it exactly once. WDF calls
            // EvtIoResume after the device returns to D0, but there is nothing
            // for us to restart.
            call_unsafe_wdf_function_binding!(WdfRequestStopAcknowledge, request, 0u8);
        },
        STOP_ACTION_PURGE => unsafe {
            // Device removal / queue purge. Ask the target to cancel. If the
            // target cannot cancel, its eventual completion still reaches
            // `on_complete`; never complete the request here, because the
            // lower driver still owns it.
            let _ = call_unsafe_wdf_function_binding!(WdfRequestCancelSentRequest, request);
        },
        _ => {
            // Validity-checked by WDF; never panic or complete an unknown
            // request in a power callback.
        }
    }
}

/// Called when the device returns to D0. Forwarded requests acknowledged with
/// `Requeue = FALSE` are completed by their completion routine, not restarted
/// here, so this callback is informational.
pub unsafe extern "C" fn evt_io_resume(_queue: WDFQUEUE, _request: WDFREQUEST) {
    unsafe { probe::log_power("resume", 0) };
}

unsafe fn forward(queue: WDFQUEUE, request: WDFREQUEST) {
    let device: WDFDEVICE =
        unsafe { call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, queue) };
    let target: WDFIOTARGET =
        unsafe { call_unsafe_wdf_function_binding!(WdfDeviceGetIoTarget, device) };

    // A filter always has a lower target. If it somehow does not, complete the
    // request instead of leaking it: a leaked request hangs the caller forever.
    if target.is_null() {
        unsafe {
            call_unsafe_wdf_function_binding!(
                WdfRequestComplete,
                request,
                STATUS_INVALID_DEVICE_REQUEST
            )
        };
        return;
    }

    unsafe {
        // Keep the original buffer type (buffered/direct/neither) and the
        // original IOCTL code; never rebuild the request ourselves.
        call_unsafe_wdf_function_binding!(WdfRequestFormatRequestUsingCurrentType, request);
        call_unsafe_wdf_function_binding!(
            WdfRequestSetCompletionRoutine,
            request,
            Some(on_complete),
            null_mut::<core::ffi::c_void>(),
        );

        // A null WDF_REQUEST_SEND_OPTIONS == WDF_NO_SEND_OPTIONS. Passing the
        // null pointer explicitly avoids depending on a macro binding.
        let sent = call_unsafe_wdf_function_binding!(
            WdfRequestSend,
            request,
            target,
            null_mut::<wdk_sys::WDF_REQUEST_SEND_OPTIONS>()
        );
        if sent == 0 {
            // Not sent => we still own it. Complete it so the caller does not
            // wait forever. This is the single most common filter-driver hang.
            let status = call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request);
            let status = if status < 0 {
                status
            } else {
                STATUS_INVALID_DEVICE_REQUEST
            };
            call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status);
        }
    }
}

unsafe extern "C" fn on_complete(
    request: WDFREQUEST,
    params: *mut WDF_REQUEST_COMPLETION_PARAMS,
    _context: *mut core::ffi::c_void,
) {
    // The request is still ours here. It must be completed exactly once, with
    // exactly the status and information the lower driver produced.
    if params.is_null() {
        unsafe {
            let status = call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request);
            call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status);
        }
        return;
    }

    let status = unsafe { call_unsafe_wdf_function_binding!(WdfRequestGetStatus, request) };
    let completion = unsafe { &*params };

    // Never panic, never unwrap: a malformed buffer just means less logging.
    unsafe { probe::log_completion(completion, status) };

    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCompleteWithInformation,
            request,
            status,
            completion.IoStatus.Information as usize,
        )
    };
}
