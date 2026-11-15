// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! `EvtDeviceAdd` for the BTHENUM filter device.

use core::mem::size_of;
use core::ptr::addr_of_mut;

use wdk::{nt_success, println};
use wdk_sys::{
    call_unsafe_wdf_function_binding, _WDF_IO_QUEUE_DISPATCH_TYPE, _WDF_REQUEST_TYPE,
    _WDF_TRI_STATE, NTSTATUS, PWDFDEVICE_INIT, ULONG, WDFDEVICE, WDFDRIVER, WDFQUEUE,
    WDF_IO_QUEUE_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
};

/// Set to `true` only when probing a HID-class devnode (option A in
/// `docs/kernel-driver.md`). At the BTHENUM/L2CAP layer there are no
/// `IRP_MJ_READ` requests, so leaving this `false` keeps the log clean.
const WATCH_READS: bool = false;

pub unsafe extern "C" fn evt_device_add(
    _driver: WDFDRIVER,
    device_init: PWDFDEVICE_INIT,
) -> NTSTATUS {
    let mut device_init = device_init;

    // Step 1 (mandatory, before WdfDeviceCreate): declare that this driver
    // attaches as a filter, so WDF keeps forwarding every IRP we do not claim
    // and no PnP/power request is swallowed by us.
    unsafe { call_unsafe_wdf_function_binding!(WdfFdoInitSetFilter, device_init) };

    let mut device: WDFDEVICE = WDF_NO_HANDLE as WDFDEVICE;
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            addr_of_mut!(device_init).cast(),
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut device,
        )
    };
    if !nt_success(status) {
        println!("[mmk] WdfDeviceCreate failed {status:#010X}");
        return status;
    }

    let status = unsafe { create_observation_queue(device) };
    if !nt_success(status) {
        return status;
    }

    println!("[mmk] filter attached");
    0
}

unsafe fn create_observation_queue(device: WDFDEVICE) -> NTSTATUS {
    let mut queue: WDFQUEUE = WDF_NO_HANDLE as WDFQUEUE;

    // Parallel: internal device control requests must not serialize behind each
    // other, otherwise one pending request blocks unrelated traffic.
    let mut config = WDF_IO_QUEUE_CONFIG {
        Size: size_of::<WDF_IO_QUEUE_CONFIG>() as ULONG,
        PowerManaged: _WDF_TRI_STATE::WdfUseDefault,
        DefaultQueue: u8::from(false),
        DispatchType: _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchParallel,
        EvtIoInternalDeviceControl: Some(crate::forward::evt_io_internal_device_control),
        EvtIoStop: Some(crate::forward::evt_io_stop),
        EvtIoResume: Some(crate::forward::evt_io_resume),
        ..WDF_IO_QUEUE_CONFIG::default()
    };
    if WATCH_READS {
        config.EvtIoRead = Some(crate::forward::evt_io_read);
    }

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
        println!("[mmk] WdfIoQueueCreate failed {status:#010X}");
        return status;
    }

    // Step 2 (mandatory for filters): without this the callback is never
    // invoked and WDF silently forwards everything.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceConfigureRequestDispatching,
            device,
            queue,
            _WDF_REQUEST_TYPE::WdfRequestTypeDeviceControlInternal,
        )
    };
    if !nt_success(status) {
        println!("[mmk] dispatching (internal ioctl) failed {status:#010X}");
        return status;
    }

    if WATCH_READS {
        let status = unsafe {
            call_unsafe_wdf_function_binding!(
                WdfDeviceConfigureRequestDispatching,
                device,
                queue,
                _WDF_REQUEST_TYPE::WdfRequestTypeRead,
            )
        };
        if !nt_success(status) {
            println!("[mmk] dispatching (read) failed {status:#010X}");
            return status;
        }
    }

    0
}
