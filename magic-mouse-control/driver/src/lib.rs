// Copyright (c) Magic Mouse Control contributors.
// License: MIT
//
// mmk.sys - experimental KMDF lower filter for the Apple Magic Mouse 2.
//
// Design contract (do not break it):
//   * This driver NEVER modifies, fails, delays or reorders I/O.
//   * It only *observes* requests that pass through it and copies a bounded
//     number of bytes into a pre-allocated ring buffer.
//   * Everything the driver cannot understand is forwarded untouched.
//
// That contract is what keeps the native (Apple) mouse path working even if
// the observation code is wrong.

#![no_std]
#![allow(clippy::missing_safety_doc)]

#[cfg(not(test))]
extern crate wdk_panic;

mod control;
mod device;
mod forward;
mod hidp;
mod magicmouse;
mod probe;
mod ring;
#[cfg(feature = "vhf")]
mod vhf;
mod vhf_report;

use core::mem::size_of;
use core::ptr::null_mut;

use wdk::{nt_success, println};
use wdk_sys::{
    call_unsafe_wdf_function_binding, ntddk::KeInitializeSpinLock, GUID, KSPIN_LOCK, NTSTATUS,
    PCUNICODE_STRING, PDRIVER_OBJECT, ULONG, WDFDRIVER, WDF_DRIVER_CONFIG,
    WDF_NO_OBJECT_ATTRIBUTES,
};

/// Device interface the user-mode app opens:
/// `\\?\GLOBALROOT\Device\...` via SetupAPI class enumeration.
// {7f0c3a2e-2b41-4b8f-9c1d-9f0a6f3c5d21}
pub const GUID_DEVINTERFACE_MMK: GUID = GUID {
    Data1: 0x7f0c_3a2e,
    Data2: 0x2b41,
    Data3: 0x4b8f,
    Data4: [0x9c, 0x1d, 0x9f, 0x0a, 0x6f, 0x3c, 0x5d, 0x21],
};

/// Pool tag: "MMK1". Four bytes, used only for debugging leaks.
pub const MMK_TAG: ULONG = u32::from_le_bytes(*b"MMK1");

/// Spin lock that guards the ring buffer. Initialized exactly once, in
/// `DriverEntry`, before any device (and therefore any IRP) can exist.
static mut RING_LOCK: KSPIN_LOCK = 0;

pub unsafe fn ring_lock() -> *mut KSPIN_LOCK {
    &raw mut RING_LOCK
}

/// Remaining budget for noisy per-report logging. A debug print in the report
/// path is not free: keep it bounded even during Phase 0.
pub static LOG_BUDGET: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(48);

/// DriverEntry is called by the I/O manager. It must create the framework
/// driver object and, for us, the control device that user mode talks to.
#[unsafe(export_name = "DriverEntry")] // WDF requires this exact symbol name.
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    let mut config = WDF_DRIVER_CONFIG {
        Size: size_of::<WDF_DRIVER_CONFIG>() as ULONG,
        EvtDriverDeviceAdd: Some(device::evt_device_add),
        ..WDF_DRIVER_CONFIG::default()
    };

    let mut wdf_driver: WDFDRIVER = null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &raw mut config,
            &raw mut wdf_driver
        )
    };
    if !nt_success(status) {
        println!("[mmk] WdfDriverCreate failed {status:#010X}");
        return status;
    }

    // One non-paged ring for the whole driver. Allocated here so that the
    // report path never allocates.
    unsafe {
        KeInitializeSpinLock(&raw mut RING_LOCK);
    }
    let status = unsafe { ring::init() };
    if !nt_success(status) {
        println!("[mmk] ring allocation failed {status:#010X}");
        // A driver without a ring can still forward traffic safely, so this is
        // not fatal: user mode will simply see IOCTL_MMK_READ_EVENTS fail.
    }

    let status = unsafe { control::create(wdf_driver) };
    if !nt_success(status) {
        println!("[mmk] control device failed {status:#010X}");
        return status;
    }

    println!(
        "[mmk] loaded (build {}, phase 0 probe)",
        env!("CARGO_PKG_VERSION")
    );
    0
}
