// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Decoding + logging for Phase 0.
//!
//! Everything here is best-effort and side-effect free: a decode failure only
//! means a shorter log line, never a failed request.
//!
//! The point of Phase 0 is to answer, on real hardware:
//!   * which `IOCTL` device type / function / method reaches BthEnum,
//!   * whether input data arrives in a completion buffer at all,
//!   * whether those bytes start with a HIDP transaction header (0xA1 = DATA
//!     Input).

use core::sync::atomic::Ordering;

use wdk::println;
use wdk_sys::{
    call_unsafe_wdf_function_binding, _WDF_REQUEST_TYPE, NTSTATUS, ULONG,
    WDF_REQUEST_COMPLETION_PARAMS,
};

use crate::forward::KIND_READ;
use crate::ring::{Event, EVENT_DATA};
use crate::LOG_BUDGET;

/// `IOCTL` method names, from `CTL_CODE` bit 0..1.
pub fn method_name(method: u32) -> &'static str {
    match method {
        0 => "buffered",
        1 => "in_direct",
        2 => "out_direct",
        _ => "neither",
    }
}

/// Split a control code the way `CTL_CODE` packs it. Device type tells us which
/// subsystem is talking (`0x0B` = HID, others likely BthPort/L2CAP).
pub fn ctl_parts(code: u32) -> (u32, u32, u32, u32) {
    let device_type = (code >> 16) & 0xFFFF;
    let access = (code >> 14) & 0x3;
    let function = (code >> 2) & 0xFFF;
    let method = code & 0x3;
    (device_type, function, method, access)
}

fn take_budget() -> bool {
    let left = LOG_BUDGET.load(Ordering::Relaxed);
    if left == 0 {
        return false;
    }
    LOG_BUDGET.store(left - 1, Ordering::Relaxed);
    true
}

/// Power transitions are rare, so they get their own small budget instead of
/// competing with the noisy Phase 0 I/O log.
static POWER_LOG_BUDGET: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(24);

fn take_power_budget() -> bool {
    let left = POWER_LOG_BUDGET.load(Ordering::Relaxed);
    if left == 0 {
        return false;
    }
    POWER_LOG_BUDGET.store(left - 1, Ordering::Relaxed);
    true
}

/// Log one EvtIoStop/EvtIoResume event. Safe at DISPATCH_LEVEL.
pub unsafe fn log_power(event: &str, action: u32) {
    if !take_power_budget() {
        return;
    }
    println!("[mmk] power {event} action={action}");
}

pub unsafe fn log_dispatch(ioctl: ULONG, kind: u8, out_len: usize, in_len: usize) {
    if !take_budget() {
        return;
    }
    if kind == KIND_READ {
        println!("[mmk] -> READ out={out_len}");
        return;
    }
    let (device_type, function, method, access) = ctl_parts(ioctl);
    println!(
        "[mmk] -> ioctl={ioctl:#010X} dev=0x{device_type:04X} fn=0x{function:03X} \
         method={} access={access} in={in_len} out={out_len}",
        method_name(method),
    );
}

pub unsafe fn log_completion(params: &WDF_REQUEST_COMPLETION_PARAMS, status: NTSTATUS) {
    let information = params.IoStatus.Information;

    match params.Type {
        _WDF_REQUEST_TYPE::WdfRequestTypeDeviceControlInternal
        | _WDF_REQUEST_TYPE::WdfRequestTypeDeviceControl => {
            let dio = &params.Parameters.Ioctl;
            let code = dio.IoControlCode;

            let mut out_len: usize = 0;
            let out_ptr = if dio.Output.Buffer.is_null() {
                core::ptr::null()
            } else {
                call_unsafe_wdf_function_binding!(
                    WdfMemoryGetBuffer,
                    dio.Output.Buffer,
                    &raw mut out_len
                ) as *const u8
            };

            let mut in_len: usize = 0;
            let in_ptr = if dio.Input.Buffer.is_null() {
                core::ptr::null()
            } else {
                call_unsafe_wdf_function_binding!(
                    WdfMemoryGetBuffer,
                    dio.Input.Buffer,
                    &raw mut in_len
                ) as *const u8
            };

            // Record even when we do not print: the ring is the useful artifact.
            let (len, src) = if !out_ptr.is_null() && out_len > 0 {
                (out_len, out_ptr)
            } else if !in_ptr.is_null() && in_len > 0 {
                (in_len, in_ptr)
            } else {
                (0, core::ptr::null())
            };
            unsafe {
                crate::ring::push_event(Event::from_buffer(
                    code,
                    status,
                    crate::forward::KIND_INTERNAL_IOCTL,
                    len,
                    src,
                ))
            };

            if !take_budget() {
                return;
            }

            let mut hex = HexBuf::new();
            if !src.is_null() {
                hex.write_prefix(src, len, EVENT_DATA);
            }
            println!(
                "[mmk] <- ioctl={code:#010X} st={status:#010X} info={information} n={len} data={}",
                hex.as_str(),
            );
        }
        _WDF_REQUEST_TYPE::WdfRequestTypeRead => {
            let mut len: usize = 0;
            let ptr = if params.Parameters.Read.Buffer.is_null() {
                core::ptr::null()
            } else {
                call_unsafe_wdf_function_binding!(
                    WdfMemoryGetBuffer,
                    params.Parameters.Read.Buffer,
                    &raw mut len
                ) as *const u8
            };

            unsafe { crate::ring::push_event(Event::from_buffer(0, status, KIND_READ, len, ptr)) };

            if !take_budget() {
                return;
            }

            let mut hex = HexBuf::new();
            if !ptr.is_null() {
                hex.write_prefix(ptr, len, EVENT_DATA);
            }
            println!(
                "[mmk] <- READ st={status:#010X} info={information} n={len} data={}",
                hex.as_str()
            );
        }
        _ => {}
    }
}

/// Tiny fixed-size hex formatter; no allocation, no panic.
struct HexBuf {
    buf: [u8; 64],
    len: usize,
}

impl HexBuf {
    fn new() -> Self {
        Self {
            buf: [0; 64],
            len: 0,
        }
    }

    /// Copy at most `max` leading bytes of `src` as lowercase hex.
    ///
    /// # Safety
    /// `src` must be valid for `len` bytes.
    unsafe fn write_prefix(&mut self, src: *const u8, len: usize, max: usize) {
        let n = core::cmp::min(core::cmp::min(len, max), 24);
        for i in 0..n {
            let byte = unsafe { core::ptr::read_volatile(src.add(i)) };
            let hi = HEX[(byte >> 4) as usize];
            let lo = HEX[(byte & 0x0F) as usize];
            if self.len + 2 > self.buf.len() {
                break;
            }
            self.buf[self.len] = hi;
            self.buf[self.len + 1] = lo;
            self.len += 2;
        }
    }

    fn as_str(&self) -> &str {
        // Only ASCII hex digits are ever stored.
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }
}

const HEX: [u8; 16] = *b"0123456789abcdef";
