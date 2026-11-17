// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Phase 3: composite virtual HID device through the Virtual HID Framework.
//!
//! One VHF device exposes three top-level collections:
//!
//! * a standard **mouse** (report `0x01`) carrying optical-sensor movement and
//!   the physical buttons, so the pointer keeps working exactly as before,
//! * a **Windows Precision Touchpad** (report `0x02`) with five contacts, so
//!   Windows itself owns two-finger scrolling, pinch and 3/4-finger gestures,
//! * a **Configuration** collection (report `0x06`) carrying the input mode.
//!
//! One-finger frames are never forwarded as touch data: the optical sensor
//! stays the only pointer source for a single finger. See
//! `vhf_report::translate_apple_touch_to_ptp`.
//!
//! This is the skeleton for the separate `mmk-vhid.sys` function driver, not
//! for the `mmk.sys` lower filter. Keeping them apart is deliberate:
//!
//! * `mmk.sys` stays observation-only on the BTHENUM devnode.
//! * `mmk-vhid.sys` is a root-enumerated KMDF function driver. Its only job is
//!   to own the virtual device, so a bug here can never break the physical
//!   mouse.
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
//! 3. `submit_apple_report` from the report source, at <= DISPATCH_LEVEL.
//! 4. `stop` at PASSIVE_LEVEL, from `EvtCleanupCallback` before the device
//!    object goes away.
//!
//! The report descriptor and the PTPHQA blob are `'static`, as VHF requires:
//! it keeps the descriptor pointer for the lifetime of the virtual device.

use core::ffi::c_void;
use core::mem::{size_of, zeroed};
use core::ptr::{addr_of_mut, null_mut};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU8, Ordering};

use wdk::println;
use wdk_sys::{
    call_unsafe_wdf_function_binding,
    hid::{VhfAsyncOperationComplete, VhfCreate, VhfDelete, VhfReadReportSubmit, VhfStart},
    GUID, HID_XFER_PACKET, NTSTATUS, PDEVICE_OBJECT, VHFHANDLE, VHFOPERATIONHANDLE, VHF_CONFIG,
    WDFDEVICE,
};

use crate::vhf_report::{
    as_bytes, encode_touch_report, translate_apple_mouse, translate_apple_touch_to_ptp,
    CertificationFeature, Contact, FunctionSwitchFeature, InputModeFeature, MaxCountFeature,
    MouseReport, PtpInputReport, CONTACT_LEN, FUNCTION_SWITCH_DEFAULT, INPUT_MODE_MOUSE,
    INPUT_MODE_TOUCHPAD, MAX_CONTACTS, MOUSE_REPORT_LEN, REPORT_DESCRIPTOR,
    REPORT_ID_FUNCTION_SWITCH, REPORT_ID_INPUT_MODE, REPORT_ID_MAX_COUNT, REPORT_ID_MOUSE,
    REPORT_ID_PTPHQA, REPORT_ID_TOUCH, TOUCH_REPORT_LEN,
};

/// Bit 0 of the function-switch feature: the touch surface is enabled.
const SURFACE_SWITCH: u8 = 0x01;

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
    /// Surface (bit 0) and button (bit 1) switches from feature report `0x04`.
    function_switch: AtomicU8,
    /// `true` while Windows has touch contacts that we still need to release.
    touch_active: AtomicBool,
}

impl Context {
    pub const fn new() -> Self {
        Self {
            handle: AtomicPtr::new(null_mut()),
            input_mode: AtomicU8::new(INPUT_MODE_MOUSE),
            function_switch: AtomicU8::new(FUNCTION_SWITCH_DEFAULT),
            touch_active: AtomicBool::new(false),
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// Context for the app-facing control device submit path (Phase 3 scaffold).
///
/// The real `mmk-vhid` function driver will pass its own device context into
/// `start`; this shared context is enough to exercise the frame path until the
/// separate driver package lands.
static CONTROL_CONTEXT: Context = Context::new();

/// Context used by the control-device submit path. See [`CONTROL_CONTEXT`].
pub fn control_context() -> &'static Context {
    &CONTROL_CONTEXT
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

/// Submit one packed report to VHF. Call at <= DISPATCH_LEVEL.
///
/// Returns `STATUS_SUCCESS` when VHF accepted (buffered) the report. A
/// non-negative status is always success; a negative one is a real error and
/// the caller should drop the frame, not retry in a loop.
unsafe fn submit_packet(
    context: &Context,
    report_id: u8,
    buffer: *mut u8,
    length: usize,
) -> NTSTATUS {
    let handle = context.handle.load(Ordering::Acquire);
    if handle.is_null() {
        return STATUS_INVALID_DEVICE_STATE;
    }

    let mut packet = HID_XFER_PACKET {
        reportBuffer: buffer,
        reportBufferLen: length as u32,
        reportId: report_id,
    };

    // With the default (no EvtVhfReadyForNextReadReport) VHF buffers the
    // report and returns immediately, so a stack buffer is correct here.
    let status = unsafe { VhfReadReportSubmit(handle, addr_of_mut!(packet)) };
    if status < 0 && take_submit_budget() {
        println!("[mmk-vhid] VhfReadReportSubmit failed {status:#010X}");
    }
    status
}

/// Submit a PTP frame on the touchpad collection.
///
/// Silently succeeds when Windows has not selected touchpad input mode or has
/// switched the touch surface off, because writing touch data then would be a
/// specification violation, not an error.
pub unsafe fn submit_ptp(context: &Context, report: &PtpInputReport) -> NTSTATUS {
    // Only report through the collection the host selected. Until Windows
    // sends input mode 3, stay silent.
    if context.input_mode.load(Ordering::Relaxed) != INPUT_MODE_TOUCHPAD {
        return STATUS_SUCCESS;
    }
    if context.function_switch.load(Ordering::Relaxed) & SURFACE_SWITCH == 0 {
        return STATUS_SUCCESS;
    }

    let mut copy = *report;
    unsafe {
        submit_packet(
            context,
            REPORT_ID_TOUCH,
            addr_of_mut!(copy).cast::<u8>(),
            TOUCH_REPORT_LEN,
        )
    }
}

/// Submit a relative mouse report on the mouse collection: optical-sensor
/// movement and the physical buttons. Independent of the touchpad input mode.
pub unsafe fn submit_mouse(context: &Context, report: &MouseReport) -> NTSTATUS {
    let mut copy = *report;
    unsafe {
        submit_packet(
            context,
            REPORT_ID_MOUSE,
            addr_of_mut!(copy).cast::<u8>(),
            MOUSE_REPORT_LEN,
        )
    }
}

/// Translate one raw Apple report and submit both collections.
///
/// `scan_time` is the app's monotonic 100 µs clock: the kernel has no reason
/// to own a gesture timer. The mouse collection always gets the frame; the
/// touchpad collection only follows frames with two or more fingers, plus one
/// empty report when the fingers lift so Windows does not keep contacts down.
pub unsafe fn submit_apple_report(context: &Context, report: &[u8], scan_time: u16) -> NTSTATUS {
    let mut mouse = MouseReport::new();
    if translate_apple_mouse(report, &mut mouse) {
        let _ = unsafe { submit_mouse(context, &mouse) };
    }

    let mut ptp = PtpInputReport::new();
    let forward = translate_apple_touch_to_ptp(report, scan_time, &mut ptp);
    if forward {
        context.touch_active.store(true, Ordering::Relaxed);
        let _ = unsafe { submit_ptp(context, &ptp) };
    } else if context.touch_active.swap(false, Ordering::Relaxed) {
        // Two fingers became one (or none): one empty report lifts the
        // contacts Windows still thinks are down.
        let _ = unsafe { submit_ptp(context, &ptp) };
    }

    STATUS_SUCCESS
}

/// Minimum frame size accepted on `IOCTL_MMK_VHID_SUBMIT`: report id (1) +
/// five contacts (5 bytes each = 25) + scan time (2) + contact count (1) +
/// buttons (1) = 30 bytes, matching `TOUCH_REPORT_LEN`.
pub const MMK_VHID_FRAME_LEN: usize = 30;

/// Parse and submit one app-supplied frame from the virtual touchpad control
/// device. Called at <= DISPATCH_LEVEL; it never blocks and never allocates
/// (the frame is copied into a stack array before parsing).
///
/// Wire layout (`MMK_VHID_FRAME`: id + signed device coordinates, the kernel
/// owns the HID bit packing):
///
/// ```text
/// byte 0        report id (0x02; informational)
/// bytes 1..25  five contacts: id, x i16, y i16 (device space, -2048..=2047)
/// bytes 26..27 scan time, 100 us, little endian
/// byte 28      contact count
/// byte 29      buttons (ignored: clicks travel on report 0x01)
/// ```
///
/// A count below two is reduced to an empty frame, so a caller cannot bypass
/// the single-finger suppression rule.
pub unsafe fn submit_frame(context: &Context, input: *const u8, input_length: usize) -> NTSTATUS {
    if context.handle.load(Ordering::Acquire).is_null() {
        return STATUS_INVALID_DEVICE_STATE;
    }
    if input.is_null() || input_length < MMK_VHID_FRAME_LEN {
        return STATUS_BUFFER_TOO_SMALL;
    }

    let mut frame = [0u8; MMK_VHID_FRAME_LEN];
    unsafe { core::ptr::copy_nonoverlapping(input, frame.as_mut_ptr(), MMK_VHID_FRAME_LEN) };

    let count = (frame[28] as usize).min(MAX_CONTACTS);
    let mut contacts = [Contact { id: 0, x: 0, y: 0 }; MAX_CONTACTS];

    for (index, contact) in contacts.iter_mut().enumerate().take(count) {
        let offset = 1 + index * CONTACT_LEN;
        *contact = Contact {
            id: frame[offset] & 0x0f,
            x: i16::from_le_bytes([frame[offset + 1], frame[offset + 2]]),
            y: i16::from_le_bytes([frame[offset + 3], frame[offset + 4]]),
        };
    }

    let scan_time = u16::from_le_bytes([frame[26], frame[27]]);
    let mut report = encode_touch_report(&contacts[..count], scan_time);
    if report.contact_count < 2 {
        report = PtpInputReport::new();
        report.scan_time = scan_time;
    }

    unsafe { submit_ptp(context, &report) }
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

/// Copy a packed feature payload into the caller's buffer and fix up the
/// length. `reportBufferLen` includes the report ID byte.
fn write_feature(packet: &mut HID_XFER_PACKET, buffer: &mut [u8], payload: &[u8]) -> NTSTATUS {
    if buffer.len() < payload.len() {
        return STATUS_BUFFER_TOO_SMALL;
    }
    buffer[..payload.len()].copy_from_slice(payload);
    packet.reportBufferLen = payload.len() as u32;
    STATUS_SUCCESS
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
        // Contact count maximum (low nibble) + pad type (high nibble).
        REPORT_ID_MAX_COUNT => write_feature(packet, buffer, as_bytes(&MaxCountFeature::DEFAULT)),
        // Surface switch (bit 0) + button switch (bit 1), read back as stored.
        REPORT_ID_FUNCTION_SWITCH => {
            let switches = unsafe { context_ref(client_context) }
                .map_or(FUNCTION_SWITCH_DEFAULT, |context| {
                    context.function_switch.load(Ordering::Acquire)
                });
            let feature = FunctionSwitchFeature {
                report_id: REPORT_ID_FUNCTION_SWITCH,
                switches,
            };
            write_feature(packet, buffer, as_bytes(&feature))
        }
        // 256-byte PTPHQA certification blob.
        REPORT_ID_PTPHQA => write_feature(packet, buffer, as_bytes(&CertificationFeature::DEFAULT)),
        // Input mode: 0 = mouse collection, 3 = touchpad collection.
        REPORT_ID_INPUT_MODE => {
            let mode = unsafe { context_ref(client_context) }.map_or(INPUT_MODE_MOUSE, |context| {
                context.input_mode.load(Ordering::Acquire)
            });
            let feature = InputModeFeature {
                report_id: REPORT_ID_INPUT_MODE,
                mode,
            };
            write_feature(packet, buffer, as_bytes(&feature))
        }
        _ => STATUS_NOT_SUPPORTED,
    }
}

/// SET_FEATURE: the input mode and the function switches are writable. Per
/// the precision touchpad specification, any input mode other than 3 means
/// mouse mode, and the upper bits of the switch byte are reserved.
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
    let Some(context) = (unsafe { context_ref(client_context) }) else {
        return STATUS_INVALID_DEVICE_STATE;
    };

    match packet.reportId {
        REPORT_ID_INPUT_MODE => {
            let mode = if buffer[1] == INPUT_MODE_TOUCHPAD {
                INPUT_MODE_TOUCHPAD
            } else {
                INPUT_MODE_MOUSE
            };
            // Leaving touchpad mode must first lift every contact, or the last
            // frame stays "down" in the PTP stack. `submit_ptp` still sees the
            // old mode, so this empty frame really is sent.
            if mode == INPUT_MODE_MOUSE
                && context.input_mode.load(Ordering::Acquire) == INPUT_MODE_TOUCHPAD
            {
                let _ = unsafe { submit_ptp(context, &PtpInputReport::new()) };
                context.touch_active.store(false, Ordering::Relaxed);
            }
            context.input_mode.store(mode, Ordering::Release);
            STATUS_SUCCESS
        }
        REPORT_ID_FUNCTION_SWITCH => {
            let requested = buffer[1] & 0x03;
            let previous = context.function_switch.load(Ordering::Acquire);
            // Switching the surface off must release the contacts first; the
            // button switch only matters to the PTP stack, because physical
            // clicks travel on the mouse collection.
            if previous & SURFACE_SWITCH != 0 && requested & SURFACE_SWITCH == 0 {
                let _ = unsafe { submit_ptp(context, &PtpInputReport::new()) };
                context.touch_active.store(false, Ordering::Relaxed);
            }
            context.function_switch.store(requested, Ordering::Release);
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
