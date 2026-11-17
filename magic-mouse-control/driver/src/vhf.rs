// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Phase 3: virtual precision touchpad through the Virtual HID Framework.
//!
//! This is the skeleton for the separate `mmk-vhid.sys` function driver, not
//! for the `mmk.sys` lower filter. Keeping them apart is deliberate:
//!
//! * `mmk.sys` stays observation-only on the BTHENUM devnode.
//! * `mmk-vhid.sys` is a root-enumerated KMDF function driver. Its only job is
//!   to own a VHF touchpad, so a bug here can never break the physical mouse.
//!
//! Build it with the `vhf` feature:
//!
//! ```text
//! cargo build --features vhf
//! ```
//!
//! `vhf.sys` must be a **lower filter** of this driver:
//!
//! ```text
//! [HIDVHF_Inst.NT.AddReg]
//! HKR,,"LowerFilters",0x00010000,"vhf"
//! ```
//!
//! Lifecycle (all of it is IRQL-sensitive):
//!
//! 1. `WdfDeviceCreate` (function driver, not `WdfFdoInitSetFilter`).
//! 2. `start` at PASSIVE_LEVEL, from `EvtDeviceAdd`.
//! 3. `submit_touch` from the report source, at <= DISPATCH_LEVEL.
//! 4. `stop` at PASSIVE_LEVEL, from `EvtCleanupCallback` before the device
//!    object goes away.
//!
//! The report descriptor and the PTPHQA blob are `'static`, as VHF requires:
//! it keeps the descriptor pointer for the lifetime of the virtual device.

use core::ffi::c_void;
use core::mem::{size_of, zeroed};
use core::ptr::{addr_of_mut, null_mut};
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU8, Ordering};

use wdk::println;
use wdk_sys::{
    call_unsafe_wdf_function_binding,
    hid::{VhfAsyncOperationComplete, VhfCreate, VhfDelete, VhfReadReportSubmit, VhfStart},
    GUID, HID_XFER_PACKET, NTSTATUS, PDEVICE_OBJECT, VHFHANDLE, VHFOPERATIONHANDLE, VHF_CONFIG,
    WDFDEVICE,
};

use crate::vhf_report::{
    encode_touch_report, Contact, INPUT_MODE_MOUSE, INPUT_MODE_TOUCHPAD, MAX_COUNT_PAYLOAD,
    PTPHQA_DEFAULT, PTPHQA_LEN, REPORT_DESCRIPTOR, REPORT_ID_INPUT_MODE, REPORT_ID_MAX_COUNT,
    REPORT_ID_PTPHQA, REPORT_ID_TOUCH, REPORT_LEN,
};

const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000_000Du32 as NTSTATUS;
const STATUS_BUFFER_TOO_SMALL: NTSTATUS = 0xC000_0023u32 as NTSTATUS;
const STATUS_NOT_SUPPORTED: NTSTATUS = 0xC000_00BBu32 as NTSTATUS;
const STATUS_INVALID_DEVICE_STATE: NTSTATUS = 0xC000_0184u32 as NTSTATUS;

/// Placeholder VID/PID from the open-source `pid.codes` range. Replace with a
/// registered pair before distribution; never reuse Apple's `0x05AC/0x0269`,
/// or hidapi-based applications will enumerate the virtual pad as a mouse.
const VENDOR_ID: u16 = 0x1209;
const PRODUCT_ID: u16 = 0x0001;
const VERSION_NUMBER: u16 = 0x0001;

// {c1e4b7a2-6f3d-4e59-9b8a-2d7c5e1f0a43}
const CONTAINER_ID: GUID = GUID {
    Data1: 0xc1e4_b7a2,
    Data2: 0x6f3d,
    Data3: 0x4e59,
    Data4: [0x9b, 0x8a, 0x2d, 0x7c, 0x5e, 0x1f, 0x0a, 0x43],
};

/// Number of failed `VhfReadReportSubmit` calls that are still logged. The
/// report path runs at up to 100 Hz; an unlimited debug print would itself
/// become the problem.
static SUBMIT_LOG_BUDGET: AtomicU32 = AtomicU32::new(8);

/// Per-device state. Embed this in your WDF device context and pass a pointer
/// to `start`; VHF hands the same pointer back to every callback.
///
/// `handle` is atomic because the input path may submit while teardown clears
/// it. A NULL handle means "not started" or "being deleted".
pub struct Context {
    handle: AtomicPtr<c_void>,
    input_mode: AtomicU8,
}

impl Context {
    pub const fn new() -> Self {
        Self {
            handle: AtomicPtr::new(null_mut()),
            input_mode: AtomicU8::new(INPUT_MODE_MOUSE),
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// Create and start the virtual touchpad.
///
/// * `device` is the WDF device created by your function driver.
/// * `context` must outlive the VHF device: allocate it in the device context,
///   not on the stack.
///
/// PASSIVE_LEVEL. The caller must already have called `WdfDeviceCreate`.
pub unsafe fn start(device: WDFDEVICE, context: *mut Context) -> NTSTATUS {
    if context.is_null() {
        return STATUS_INVALID_PARAMETER;
    }

    let device_object: PDEVICE_OBJECT =
        unsafe { call_unsafe_wdf_function_binding!(WdfDeviceWdmGetDeviceObject, device) };
    if device_object.is_null() {
        return STATUS_INVALID_DEVICE_STATE;
    }

    // VHF_CONFIG_INIT is a C macro, so build the equivalent by hand. The
    // struct is plain old data; zeroing it is what the macro does first.
    let mut config: VHF_CONFIG = unsafe { zeroed() };
    config.Size = size_of::<VHF_CONFIG>() as u32;
    config.VhfClientContext = context.cast();
    config.OperationContextSize = 0;
    config.DeviceObject = device_object;
    config.VendorID = VENDOR_ID;
    config.ProductID = PRODUCT_ID;
    config.VersionNumber = VERSION_NUMBER;
    config.ContainerID = CONTAINER_ID;
    config.InstanceIDLength = 0;
    config.InstanceID = null_mut();
    config.ReportDescriptorLength = REPORT_DESCRIPTOR.len() as u16;
    // VHF reads this buffer, it does not take ownership. `'static` is exactly
    // what the lifetime requirement asks for.
    config.ReportDescriptor = REPORT_DESCRIPTOR.as_ptr().cast_mut();
    config.EvtVhfAsyncOperationGetFeature = Some(on_get_feature);
    config.EvtVhfAsyncOperationSetFeature = Some(on_set_feature);
    config.EvtVhfAsyncOperationGetInputReport = None;
    config.EvtVhfAsyncOperationWriteReport = None;
    config.EvtVhfReadyForNextReadReport = None;
    config.EvtVhfCleanup = Some(on_cleanup);
    config.HardwareIDsLength = 0;
    config.HardwareIDs = null_mut();

    let mut handle: VHFHANDLE = null_mut();
    let status = unsafe { VhfCreate(addr_of_mut!(config), addr_of_mut!(handle)) };
    if status < 0 {
        println!("[mmk-vhid] VhfCreate failed {status:#010X}");
        return status;
    }

    let status = unsafe { VhfStart(handle) };
    if status < 0 {
        println!("[mmk-vhid] VhfStart failed {status:#010X}");
        // The handle is valid even when Start fails; deleting it is required
        // or the VHF device leaks until the system reboots.
        unsafe { VhfDelete(handle, 1u8) };
        return status;
    }

    unsafe { &*context }.handle.store(handle, Ordering::Release);
    println!("[mmk-vhid] virtual touchpad started");
    STATUS_SUCCESS
}

/// Submit one frame. Call at <= DISPATCH_LEVEL.
///
/// Returns `STATUS_SUCCESS` when VHF accepted (buffered) the report. A
/// non-negative status is always success; a negative one is a real error and
/// the caller should drop the frame, not retry in a loop.
pub unsafe fn submit_touch(
    context: &Context,
    contacts: &[Contact],
    buttons: u8,
    scan_time: u16,
) -> NTSTATUS {
    let handle = context.handle.load(Ordering::Acquire);
    if handle.is_null() {
        return STATUS_INVALID_DEVICE_STATE;
    }

    // The specification: only report through the collection the host selected.
    // Until Windows sends input mode 3, stay silent.
    if context.input_mode.load(Ordering::Relaxed) != INPUT_MODE_TOUCHPAD {
        return STATUS_SUCCESS;
    }

    let mut buffer = [0u8; REPORT_LEN];
    encode_touch_report(contacts, buttons, scan_time, &mut buffer);

    let mut packet = HID_XFER_PACKET {
        reportBuffer: buffer.as_mut_ptr(),
        reportBufferLen: REPORT_LEN as u32,
        reportId: REPORT_ID_TOUCH,
    };

    // With the default (no EvtVhfReadyForNextReadReport) VHF buffers the
    // report and returns immediately, so a stack buffer is correct here.
    let status = unsafe { VhfReadReportSubmit(handle, addr_of_mut!(packet)) };
    if status < 0 && take_submit_budget() {
        println!("[mmk-vhid] VhfReadReportSubmit failed {status:#010X}");
    }
    status
}

/// Delete the VHF device. PASSIVE_LEVEL only, and only after producers stop.
pub unsafe fn stop(context: &Context) {
    let handle = context.handle.swap(null_mut(), Ordering::AcqRel);
    if !handle.is_null() {
        unsafe { VhfDelete(handle, 1u8) };
    }
}

fn take_submit_budget() -> bool {
    let left = SUBMIT_LOG_BUDGET.load(Ordering::Relaxed);
    if left == 0 {
        return false;
    }
    SUBMIT_LOG_BUDGET.store(left - 1, Ordering::Relaxed);
    true
}

unsafe fn context_ref<'a>(client_context: *mut c_void) -> Option<&'a Context> {
    if client_context.is_null() {
        None
    } else {
        // The pointer comes from `VhfClientContext`, which we set to a valid
        // `Context` that outlives the VHF device.
        Some(unsafe { &*client_context.cast::<Context>() })
    }
}

/// GET_FEATURE: the report ID is repeated in `reportBuffer[0]` by the HID
/// class, so payload bytes start at index 1 and `reportBufferLen` includes the
/// ID byte. VHF calls this at <= DISPATCH_LEVEL.
unsafe extern "C" fn on_get_feature(
    client_context: *mut c_void,
    operation: VHFOPERATIONHANDLE,
    _operation_context: *mut c_void,
    packet: *mut HID_XFER_PACKET,
) {
    let status = unsafe { fill_feature(client_context, packet) };
    if !operation.is_null() {
        let _ = unsafe { VhfAsyncOperationComplete(operation, status) };
    }
}

unsafe fn fill_feature(client_context: *mut c_void, packet: *mut HID_XFER_PACKET) -> NTSTATUS {
    if packet.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let packet = unsafe { &mut *packet };
    let length = packet.reportBufferLen as usize;
    if packet.reportBuffer.is_null() || length == 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let buffer = unsafe { core::slice::from_raw_parts_mut(packet.reportBuffer, length) };

    match packet.reportId {
        REPORT_ID_MAX_COUNT => {
            if length < 2 {
                return STATUS_BUFFER_TOO_SMALL;
            }
            buffer[0] = REPORT_ID_MAX_COUNT;
            buffer[1] = MAX_COUNT_PAYLOAD;
            packet.reportBufferLen = 2;
            STATUS_SUCCESS
        }
        REPORT_ID_PTPHQA => {
            if length < 1 + PTPHQA_LEN {
                return STATUS_BUFFER_TOO_SMALL;
            }
            buffer[0] = REPORT_ID_PTPHQA;
            buffer[1..=PTPHQA_LEN].copy_from_slice(&PTPHQA_DEFAULT);
            packet.reportBufferLen = (1 + PTPHQA_LEN) as u32;
            STATUS_SUCCESS
        }
        REPORT_ID_INPUT_MODE => {
            if length < 2 {
                return STATUS_BUFFER_TOO_SMALL;
            }
            buffer[0] = REPORT_ID_INPUT_MODE;
            buffer[1] = unsafe { context_ref(client_context) }
                .map_or(INPUT_MODE_MOUSE, |context| {
                    context.input_mode.load(Ordering::Acquire)
                });
            packet.reportBufferLen = 2;
            STATUS_SUCCESS
        }
        _ => STATUS_NOT_SUPPORTED,
    }
}

/// SET_FEATURE: only the input mode report is writable. Per the precision
/// touchpad specification, any value other than 3 means mouse mode.
unsafe extern "C" fn on_set_feature(
    client_context: *mut c_void,
    operation: VHFOPERATIONHANDLE,
    _operation_context: *mut c_void,
    packet: *mut HID_XFER_PACKET,
) {
    let status = unsafe { store_feature(client_context, packet) };
    if !operation.is_null() {
        let _ = unsafe { VhfAsyncOperationComplete(operation, status) };
    }
}

unsafe fn store_feature(client_context: *mut c_void, packet: *mut HID_XFER_PACKET) -> NTSTATUS {
    if packet.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let packet = unsafe { &*packet };
    let length = packet.reportBufferLen as usize;
    if packet.reportBuffer.is_null() || length < 2 {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let buffer = unsafe { core::slice::from_raw_parts(packet.reportBuffer, length) };

    match packet.reportId {
        REPORT_ID_INPUT_MODE => {
            let requested = buffer[1];
            let mode = if requested == INPUT_MODE_TOUCHPAD {
                INPUT_MODE_TOUCHPAD
            } else {
                INPUT_MODE_MOUSE
            };
            if let Some(context) = unsafe { context_ref(client_context) } {
                // Leaving touchpad mode must first lift every contact, or the
                // last frame stays "down" in the PTP stack. `submit_touch`
                // still sees the old mode, so this frame really is sent.
                if mode == INPUT_MODE_MOUSE
                    && context.input_mode.load(Ordering::Acquire) == INPUT_MODE_TOUCHPAD
                {
                    let _ = unsafe { submit_touch(context, &[], 0, 0) };
                }
                context.input_mode.store(mode, Ordering::Release);
            }
            STATUS_SUCCESS
        }
        _ => STATUS_NOT_SUPPORTED,
    }
}

/// VHF is already deleting the device. Only clear our handle; calling any VHF
/// API from here would recurse into the deletion that is in progress.
unsafe extern "C" fn on_cleanup(client_context: *mut c_void) {
    if let Some(context) = unsafe { context_ref(client_context) } {
        context.handle.store(null_mut(), Ordering::Release);
    }
}
