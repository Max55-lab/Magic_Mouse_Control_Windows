// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Pre-allocated, non-paged ring buffer.
//!
//! Allocated once in `DriverEntry`; never resized, never freed, never touched
//! with an allocator on the report path. A single global spin lock guards it,
//! so the same buffer is safe from a completion routine (DISPATCH_LEVEL) and
//! from a user-mode IOCTL (PASSIVE_LEVEL).

use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, Ordering};

use wdk_sys::{
    ntddk::{ExAllocatePool2, KeAcquireSpinLockRaiseToDpc, KeReleaseSpinLock},
    KIRQL, NTSTATUS, POOL_FLAG_NON_PAGED, SIZE_T, STATUS_INSUFFICIENT_RESOURCES,
};

pub const EVENT_CAP: usize = 256;
pub const EVENT_DATA: usize = 32;

/// One observed request. `#[repr(C)]` because user mode reads this memory
/// directly (fixed layout, no Rust-specific layout guarantees).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Event {
    /// `IOCTL` code, or 0 for `IRP_MJ_READ`.
    pub ioctl: u32,
    pub status: i32,
    /// Full buffer length as reported by the lower driver.
    pub len: u16,
    /// `KIND_INTERNAL_IOCTL` / `KIND_READ`.
    pub kind: u8,
    /// First byte of the payload, handy as a HIDP transaction hint.
    pub first: u8,
    pub data: [u8; EVENT_DATA],
}

impl Event {
    pub const ZERO: Event = Event {
        ioctl: 0,
        status: 0,
        len: 0,
        kind: 0,
        first: 0,
        data: [0; EVENT_DATA],
    };

    /// Copy at most `EVENT_DATA` bytes from `src` into a record.
    ///
    /// # Safety
    /// `src` must be valid for `len` bytes, or null (then nothing is copied).
    pub unsafe fn from_buffer(
        ioctl: u32,
        status: NTSTATUS,
        kind: u8,
        len: usize,
        src: *const u8,
    ) -> Event {
        let mut event = Event::ZERO;
        event.ioctl = ioctl;
        event.status = status;
        event.kind = kind;
        event.len = if len > u16::MAX as usize {
            u16::MAX
        } else {
            len as u16
        };

        if !src.is_null() && len > 0 {
            let n = core::cmp::min(len, EVENT_DATA);
            unsafe { core::ptr::copy_nonoverlapping(src, event.data.as_mut_ptr(), n) };
            event.first = event.data[0];
        }

        event
    }
}

#[repr(C)]
pub struct Ring {
    /// Write index (monotonic, wraps).
    pub head: u32,
    /// Read index (monotonic, wraps).
    pub tail: u32,
    /// Records dropped because the ring was full.
    pub dropped: u32,
    pub _reserved: u32,
    pub events: [Event; EVENT_CAP],
}

impl Ring {
    pub const fn new() -> Ring {
        Ring {
            head: 0,
            tail: 0,
            dropped: 0,
            _reserved: 0,
            events: [Event::ZERO; EVENT_CAP],
        }
    }

    fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    fn is_full(&self) -> bool {
        self.head.wrapping_sub(self.tail) as usize >= EVENT_CAP
    }

    fn push(&mut self, event: Event) {
        if self.is_full() {
            // Overwrite the oldest record: for a live tap, recent data matters
            // more than a perfect history. Never block, never allocate.
            self.tail = self.tail.wrapping_add(1);
            self.dropped = self.dropped.saturating_add(1);
        }
        let index = (self.head as usize) % EVENT_CAP;
        self.events[index] = event;
        self.head = self.head.wrapping_add(1);
    }

    fn pop(&mut self) -> Option<Event> {
        if self.is_empty() {
            return None;
        }
        let index = (self.tail as usize) % EVENT_CAP;
        self.tail = self.tail.wrapping_add(1);
        Some(self.events[index])
    }
}

/// Published to user mode through `IOCTL_MMK_READ_EVENTS`. Kept in an `AtomicPtr`
/// so the publication in `DriverEntry` is well-defined even if a device is added
/// concurrently on another CPU.
static RING: AtomicPtr<Ring> = AtomicPtr::new(null_mut());

pub unsafe fn init() -> NTSTATUS {
    let size = core::mem::size_of::<Ring>();
    let memory = unsafe { ExAllocatePool2(POOL_FLAG_NON_PAGED, size as SIZE_T, crate::MMK_TAG) }
        as *mut Ring;

    if memory.is_null() {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    unsafe { core::ptr::write(memory, Ring::new()) };
    RING.store(memory, Ordering::Release);
    0
}

/// Push one record. Safe to call at DISPATCH_LEVEL.
pub unsafe fn push_event(event: Event) {
    let ring = RING.load(Ordering::Acquire);
    if ring.is_null() {
        return;
    }

    unsafe {
        let lock = crate::ring_lock();
        let irql: KIRQL = KeAcquireSpinLockRaiseToDpc(lock);
        (*ring).push(event);
        KeReleaseSpinLock(lock, irql);
    }
}

/// Copy up to `max` records into `dst`; returns how many were copied.
///
/// # Safety
/// `dst` must be valid for `max` writes of `Event`.
pub unsafe fn pop_into(dst: *mut Event, max: usize) -> usize {
    let ring = RING.load(Ordering::Acquire);
    if ring.is_null() || dst.is_null() || max == 0 {
        return 0;
    }

    let mut copied = 0usize;
    unsafe {
        let lock = crate::ring_lock();
        let irql: KIRQL = KeAcquireSpinLockRaiseToDpc(lock);
        while copied < max {
            match (*ring).pop() {
                Some(event) => {
                    core::ptr::write(dst.add(copied), event);
                    copied += 1;
                }
                None => break,
            }
        }
        KeReleaseSpinLock(lock, irql);
    }
    copied
}
