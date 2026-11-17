// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Pure encoding for the `mmk-vhid` composite HID device.
//!
//! This module has no WDK dependency on purpose: the report layout is the part
//! that is easy to get wrong, so it is unit tested on the host:
//!
//! ```text
//! rustc --edition 2021 --test driver/src/vhf_report.rs -o mmtest && ./mmtest
//! ```
//!
//! # Device shape
//!
//! Three top-level collections share one VHF device:
//!
//! 1. **Standard mouse** (report `0x01`) — relative optical-sensor movement and
//!    the physical left/right/middle buttons. The pointer stays on the normal
//!    Windows mouse path.
//! 2. **Windows Precision Touchpad** (report `0x02`) — up to five contacts,
//!    scan time and contact count. One-finger frames are deliberately reduced
//!    to an empty report, so resting or moving one finger on the glass never
//!    moves the pointer and never scrolls.
//! 3. **Configuration** (report `0x06`) — input mode (mouse vs. touchpad).
//!
//! The touchpad collection also carries the mandatory PTP feature reports:
//! `0x03` max contacts + pad type, `0x04` surface/button switches, and `0x05`
//! the 256-byte PTPHQA blob.
//!
//! # Report layout
//!
//! ```text
//! PTP input, 30 bytes:
//!   byte 0        report id 0x02
//!   bytes 1..26   5 contacts x 5 bytes
//!   bytes 26..28  scan time, little endian, 100 us units
//!   byte 28       active contact count
//!   byte 29       buttons (kept for descriptor compatibility; the PTP path
//!                 leaves them zero because clicks use report 0x01)
//!
//! Mouse input, 6 bytes:
//!   byte 0        report id 0x01
//!   byte 1        buttons: bit 0 left, bit 1 right, bit 2 middle
//!   bytes 2..4    relative X, signed 16-bit little endian
//!   bytes 4..6    relative Y, signed 16-bit little endian
//!
//! Contact, 5 bytes (least significant bit first, as HID packs fields):
//!   bit 0      confidence (1 = valid finger; palms are filtered before encoding)
//!   bit 1      tip switch (1 = finger on the surface)
//!   bits 2..5  contact id, 0..15
//!   bits 6..7  padding
//!   bytes 1..2 X, 0..4095
//!   bytes 3..4 Y, 0..4095
//! ```
//!
//! The coordinate range is not arbitrary: the Magic Mouse reports 12-bit
//! signed positions in `-2048..=2047`, so `x + 2048` maps exactly onto
//! `0..=4095`, which is the maximum a Windows Precision Touchpad may use.

/// Mouse input report: optical-sensor movement and physical buttons.
pub const REPORT_ID_MOUSE: u8 = 0x01;
/// PTP input report with the five contacts.
pub const REPORT_ID_TOUCH: u8 = 0x02;
/// Feature report: contact count maximum (low nibble) and pad type (high).
pub const REPORT_ID_MAX_COUNT: u8 = 0x03;
/// Feature report: surface switch (bit 0) and button switch (bit 1).
pub const REPORT_ID_FUNCTION_SWITCH: u8 = 0x04;
/// Feature report: 256-byte device certification status blob (PTPHQA).
pub const REPORT_ID_PTPHQA: u8 = 0x05;
/// Feature report: input mode (0 = mouse collection, 3 = touchpad).
pub const REPORT_ID_INPUT_MODE: u8 = 0x06;

/// Apple Magic Mouse multi-touch report used over Bluetooth by `mmk.sys`.
pub const APPLE_TOUCH_REPORT_BT: u8 = 0x27;
/// The same layout is used by the USB `0x12` report.
pub const APPLE_TOUCH_REPORT_USB: u8 = 0x12;
pub const APPLE_TOUCH_DATA_OFFSET: usize = 14;
pub const APPLE_TOUCH_RECORD_LEN: usize = 8;
pub const APPLE_TOUCH_STATE_MASK: u8 = 0xf0;

/// Windows precision touchpads expose at most five contacts.
pub const MAX_CONTACTS: usize = 5;
pub const CONTACT_LEN: usize = 5;
pub const TOUCH_REPORT_LEN: usize = 1 + MAX_CONTACTS * CONTACT_LEN + 2 + 1 + 1;
pub const MOUSE_REPORT_LEN: usize = 1 + 1 + 2 + 2;

pub const PTPHQA_LEN: usize = 256;

/// Input mode values from the precision touchpad specification.
pub const INPUT_MODE_MOUSE: u8 = 0;
pub const INPUT_MODE_TOUCHPAD: u8 = 3;

/// Contacts smaller than this are the mouse's "hover"/noise records. Eight is
/// the same threshold `hid-magicmouse` userspace uses.
pub const MIN_CONTACT_SIZE: u8 = 8;

/// Pad type 0 = depressible (click-pad), which is what the Magic Mouse 2 is.
pub const PAD_TYPE_DEPRESSIBLE: u8 = 0;
pub const MAX_COUNT_PAYLOAD: u8 = (MAX_CONTACTS as u8) | (PAD_TYPE_DEPRESSIBLE << 4);

/// Surface and button switching are on by default.
pub const FUNCTION_SWITCH_DEFAULT: u8 = 0x03;

/// One finger in Magic Mouse coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Contact {
    /// Tracking id as reported by the mouse. Only the low four bits are used.
    pub id: u8,
    /// `-2048..=2047`; values outside are clamped.
    pub x: i16,
    pub y: i16,
}

/// One packed PTP contact. The byte layout is exactly the descriptor's
/// confidence + tip switch (2 bits) + contact id (4 bits) + padding (2 bits),
/// followed by little-endian X and Y.
///
/// `Copy`/`Clone` are written by hand: derived impls take references to the
/// fields, which is not allowed for a packed struct.
#[repr(C, packed)]
pub struct PtpContact {
    pub flags: u8,
    pub x: u16,
    pub y: u16,
}

impl PtpContact {
    pub const ZERO: Self = Self {
        flags: 0,
        x: 0,
        y: 0,
    };

    /// `flags`: bit 0 confidence, bit 1 tip switch, bits 2..5 contact id.
    ///
    /// Microsoft's sample descriptor lists Confidence before Tip Switch, so a
    /// real finger needs both bits: `0b11`. Confidence alone is not enough
    /// (Windows would not see a pressed contact) and a zero Confidence bit
    /// marks the contact as a palm.
    pub fn new(id: u8, x: i32, y: i32) -> Self {
        Self {
            flags: ((id & 0x0f) << 2) | 0x03,
            x: absolute(x),
            y: absolute(y),
        }
    }
}

impl Copy for PtpContact {}

impl Clone for PtpContact {
    fn clone(&self) -> Self {
        *self
    }
}

/// Packed PTP input report. `size_of` is 30 bytes and matches the descriptor.
#[repr(C, packed)]
pub struct PtpInputReport {
    pub report_id: u8,
    pub contacts: [PtpContact; MAX_CONTACTS],
    pub scan_time: u16,
    pub contact_count: u8,
    pub buttons: u8,
}

impl PtpInputReport {
    pub const fn new() -> Self {
        Self {
            report_id: REPORT_ID_TOUCH,
            contacts: [PtpContact::ZERO; MAX_CONTACTS],
            scan_time: 0,
            contact_count: 0,
            buttons: 0,
        }
    }
}

impl Default for PtpInputReport {
    fn default() -> Self {
        Self::new()
    }
}

impl Copy for PtpInputReport {}

impl Clone for PtpInputReport {
    fn clone(&self) -> Self {
        *self
    }
}

/// Packed standard mouse report. Size is 6 bytes.
#[repr(C, packed)]
pub struct MouseReport {
    pub report_id: u8,
    pub buttons: u8,
    pub dx: i16,
    pub dy: i16,
}

impl MouseReport {
    pub const fn new() -> Self {
        Self {
            report_id: REPORT_ID_MOUSE,
            buttons: 0,
            dx: 0,
            dy: 0,
        }
    }
}

impl Copy for MouseReport {}

impl Clone for MouseReport {
    fn clone(&self) -> Self {
        *self
    }
}

/// Feature report `0x03`: low nibble max contacts, high nibble pad type.
#[repr(C, packed)]
pub struct MaxCountFeature {
    pub report_id: u8,
    pub payload: u8,
}

impl MaxCountFeature {
    pub const DEFAULT: Self = Self {
        report_id: REPORT_ID_MAX_COUNT,
        payload: MAX_COUNT_PAYLOAD,
    };
}

impl Copy for MaxCountFeature {}

impl Clone for MaxCountFeature {
    fn clone(&self) -> Self {
        *self
    }
}

/// Feature report `0x04`: bit 0 surface switch, bit 1 button switch.
#[repr(C, packed)]
pub struct FunctionSwitchFeature {
    pub report_id: u8,
    pub switches: u8,
}

impl FunctionSwitchFeature {
    pub const DEFAULT: Self = Self {
        report_id: REPORT_ID_FUNCTION_SWITCH,
        switches: FUNCTION_SWITCH_DEFAULT,
    };
}

impl Copy for FunctionSwitchFeature {}

impl Clone for FunctionSwitchFeature {
    fn clone(&self) -> Self {
        *self
    }
}

/// Feature report `0x05`: 256-byte PTPHQA certification status.
#[repr(C, packed)]
pub struct CertificationFeature {
    pub report_id: u8,
    pub blob: [u8; PTPHQA_LEN],
}

impl CertificationFeature {
    pub const DEFAULT: Self = Self {
        report_id: REPORT_ID_PTPHQA,
        blob: PTPHQA_DEFAULT,
    };
}

impl Copy for CertificationFeature {}

impl Clone for CertificationFeature {
    fn clone(&self) -> Self {
        *self
    }
}

/// Feature report `0x06`: 0 = mouse mode, 3 = touchpad mode.
#[repr(C, packed)]
pub struct InputModeFeature {
    pub report_id: u8,
    pub mode: u8,
}

impl InputModeFeature {
    pub const DEFAULT: Self = Self {
        report_id: REPORT_ID_INPUT_MODE,
        mode: INPUT_MODE_MOUSE,
    };
}

impl Copy for InputModeFeature {}

impl Clone for InputModeFeature {
    fn clone(&self) -> Self {
        *self
    }
}

/// Byte view of any packed report, for `VhfReadReportSubmit` /
/// `VhfReadReportSubmit`-style buffers.
///
/// # Safety
///
/// Only meaningful for `#[repr(C, packed)]` types made of integers, which is
/// exactly what this module defines: every byte is initialized and no field
/// ever requires alignment.
pub fn as_bytes<T>(value: &T) -> &[u8] {
    // SAFETY: see above; `T` is one of this module's packed report structs.
    unsafe {
        core::slice::from_raw_parts(value as *const T as *const u8, core::mem::size_of::<T>())
    }
}

const _: () = {
    assert!(core::mem::size_of::<PtpContact>() == CONTACT_LEN);
    assert!(core::mem::size_of::<PtpInputReport>() == TOUCH_REPORT_LEN);
    assert!(core::mem::size_of::<MouseReport>() == MOUSE_REPORT_LEN);
    assert!(core::mem::size_of::<MaxCountFeature>() == 2);
    assert!(core::mem::size_of::<FunctionSwitchFeature>() == 2);
    assert!(core::mem::size_of::<CertificationFeature>() == 1 + PTPHQA_LEN);
    assert!(core::mem::size_of::<InputModeFeature>() == 2);
};

/// Composite report descriptor: standard mouse + Windows Precision Touchpad.
///
/// Report IDs:
/// * `0x01` standard mouse input (X/Y relative + three buttons),
/// * `0x02` PTP touch input (five contacts + scan time + count + buttons),
/// * `0x03` feature: contact count maximum + pad type,
/// * `0x04` feature: surface switch + button switch,
/// * `0x05` feature: 256-byte PTPHQA certification blob,
/// * `0x06` feature: input mode in the Configuration collection.
///
/// Each touch contact contributes 40 bits: confidence + tip switch (2),
/// contact id (4), padding (2), X (16), Y (16). The PTP input payload is
/// 29 bytes; `PtpInputReport` adds the report ID and is exactly 30 bytes.
pub const REPORT_DESCRIPTOR: &[u8] = &[
    // ===== Standard Mouse TLC (report 0x01) =====
    0x05,
    0x01, // Usage Page (Generic Desktop)
    0x09,
    0x02, // Usage (Mouse)
    0xa1,
    0x01, // Collection (Application)
    0x85,
    REPORT_ID_MOUSE, //   Report ID (1)
    0x09,
    0x01, //   Usage (Pointer)
    0xa1,
    0x00, //   Collection (Physical)
    0x05,
    0x09, //     Usage Page (Button)
    0x19,
    0x01, //     Usage Minimum (Button 1)
    0x29,
    0x03, //     Usage Maximum (Button 3)
    0x15,
    0x00, //     Logical Minimum (0)
    0x25,
    0x01, //     Logical Maximum (1)
    0x75,
    0x01, //     Report Size (1)
    0x95,
    0x03, //     Report Count (3)
    0x81,
    0x02, //     Input (Data,Var,Abs)
    0x95,
    0x05, //     Report Count (5)
    0x81,
    0x03, //     Input (Const,Var,Abs) padding
    0x05,
    0x01, //     Usage Page (Generic Desktop)
    0x09,
    0x30, //     Usage (X)
    0x09,
    0x31, //     Usage (Y)
    0x16,
    0x01,
    0x80, //     Logical Minimum (-32767)
    0x26,
    0xff,
    0x7f, //     Logical Maximum (32767)
    0x75,
    0x10, //     Report Size (16)
    0x95,
    0x02, //     Report Count (2)
    0x81,
    0x06, //     Input (Data,Var,Rel)
    0xc0, //   End Collection (Physical)
    0xc0, // End Collection (Mouse)
    // ===== Windows Precision Touchpad TLC (report 0x02) =====
    0x05,
    0x0d, // Usage Page (Digitizers)
    0x09,
    0x05, // Usage (Touch Pad)
    0xa1,
    0x01, // Collection (Application)
    0x85,
    REPORT_ID_TOUCH, //   Report ID (2)
    // --- Finger 1 ---
    0x05,
    0x0d, //   Usage Page (Digitizers)
    0x09,
    0x22, //   Usage (Finger)
    0xa1,
    0x02, //   Collection (Logical)
    0x15,
    0x00, //     Logical Minimum (0)
    0x25,
    0x01, //     Logical Maximum (1)
    0x09,
    0x47, //     Usage (Confidence)
    0x09,
    0x42, //     Usage (Tip Switch)
    0x95,
    0x02, //     Report Count (2)
    0x75,
    0x01, //     Report Size (1)
    0x81,
    0x02, //     Input (Data,Var,Abs)
    0x25,
    0x0f, //     Logical Maximum (15)
    0x75,
    0x04, //     Report Size (4)
    0x95,
    0x01, //     Report Count (1)
    0x09,
    0x51, //     Usage (Contact Identifier)
    0x81,
    0x02, //     Input (Data,Var,Abs)
    0x75,
    0x02, //     Report Size (2)
    0x95,
    0x01, //     Report Count (1)
    0x81,
    0x03, //     Input (Const,Var,Abs) padding to 5 bytes
    0x05,
    0x01, //     Usage Page (Generic Desktop)
    0x15,
    0x00, //     Logical Minimum (0)
    0x26,
    0xff,
    0x0f, //     Logical Maximum (4095)
    0x75,
    0x10, //     Report Size (16)
    0x55,
    0x0e, //     Unit Exponent (-2)
    0x65,
    0x13, //     Unit (Inch, English Linear)
    0x09,
    0x30, //     Usage (X)
    0x35,
    0x00, //     Physical Minimum (0)
    0x46,
    0xe0,
    0x00, //     Physical Maximum (224 = 57 mm)
    0x95,
    0x01, //     Report Count (1)
    0x81,
    0x02, //     Input (Data,Var,Abs)
    0x46,
    0xbd,
    0x01, //     Physical Maximum (445 = 113 mm)
    0x09,
    0x31, //     Usage (Y)
    0x81,
    0x02, //     Input (Data,Var,Abs)
    0xc0, //   End Collection
    // --- Fingers 2..5: identical collections, required for five contacts ---
    0x05,
    0x0d,
    0x09,
    0x22,
    0xa1,
    0x02,
    0x15,
    0x00,
    0x25,
    0x01,
    0x09,
    0x47,
    0x09,
    0x42,
    0x95,
    0x02,
    0x75,
    0x01,
    0x81,
    0x02,
    0x25,
    0x0f,
    0x75,
    0x04,
    0x95,
    0x01,
    0x09,
    0x51,
    0x81,
    0x02,
    0x75,
    0x02,
    0x95,
    0x01,
    0x81,
    0x03,
    0x05,
    0x01,
    0x15,
    0x00,
    0x26,
    0xff,
    0x0f,
    0x75,
    0x10,
    0x55,
    0x0e,
    0x65,
    0x13,
    0x09,
    0x30,
    0x35,
    0x00,
    0x46,
    0xe0,
    0x00,
    0x95,
    0x01,
    0x81,
    0x02,
    0x46,
    0xbd,
    0x01,
    0x09,
    0x31,
    0x81,
    0x02,
    0xc0,
    0x05,
    0x0d,
    0x09,
    0x22,
    0xa1,
    0x02,
    0x15,
    0x00,
    0x25,
    0x01,
    0x09,
    0x47,
    0x09,
    0x42,
    0x95,
    0x02,
    0x75,
    0x01,
    0x81,
    0x02,
    0x25,
    0x0f,
    0x75,
    0x04,
    0x95,
    0x01,
    0x09,
    0x51,
    0x81,
    0x02,
    0x75,
    0x02,
    0x95,
    0x01,
    0x81,
    0x03,
    0x05,
    0x01,
    0x15,
    0x00,
    0x26,
    0xff,
    0x0f,
    0x75,
    0x10,
    0x55,
    0x0e,
    0x65,
    0x13,
    0x09,
    0x30,
    0x35,
    0x00,
    0x46,
    0xe0,
    0x00,
    0x95,
    0x01,
    0x81,
    0x02,
    0x46,
    0xbd,
    0x01,
    0x09,
    0x31,
    0x81,
    0x02,
    0xc0,
    0x05,
    0x0d,
    0x09,
    0x22,
    0xa1,
    0x02,
    0x15,
    0x00,
    0x25,
    0x01,
    0x09,
    0x47,
    0x09,
    0x42,
    0x95,
    0x02,
    0x75,
    0x01,
    0x81,
    0x02,
    0x25,
    0x0f,
    0x75,
    0x04,
    0x95,
    0x01,
    0x09,
    0x51,
    0x81,
    0x02,
    0x75,
    0x02,
    0x95,
    0x01,
    0x81,
    0x03,
    0x05,
    0x01,
    0x15,
    0x00,
    0x26,
    0xff,
    0x0f,
    0x75,
    0x10,
    0x55,
    0x0e,
    0x65,
    0x13,
    0x09,
    0x30,
    0x35,
    0x00,
    0x46,
    0xe0,
    0x00,
    0x95,
    0x01,
    0x81,
    0x02,
    0x46,
    0xbd,
    0x01,
    0x09,
    0x31,
    0x81,
    0x02,
    0xc0,
    0x05,
    0x0d,
    0x09,
    0x22,
    0xa1,
    0x02,
    0x15,
    0x00,
    0x25,
    0x01,
    0x09,
    0x47,
    0x09,
    0x42,
    0x95,
    0x02,
    0x75,
    0x01,
    0x81,
    0x02,
    0x25,
    0x0f,
    0x75,
    0x04,
    0x95,
    0x01,
    0x09,
    0x51,
    0x81,
    0x02,
    0x75,
    0x02,
    0x95,
    0x01,
    0x81,
    0x03,
    0x05,
    0x01,
    0x15,
    0x00,
    0x26,
    0xff,
    0x0f,
    0x75,
    0x10,
    0x55,
    0x0e,
    0x65,
    0x13,
    0x09,
    0x30,
    0x35,
    0x00,
    0x46,
    0xe0,
    0x00,
    0x95,
    0x01,
    0x81,
    0x02,
    0x46,
    0xbd,
    0x01,
    0x09,
    0x31,
    0x81,
    0x02,
    0xc0,
    // Scan time and contact count.
    0x55,
    0x0c, //   Unit Exponent (-4)
    0x66,
    0x01,
    0x10, //   Unit (Seconds)
    0x47,
    0xff,
    0xff,
    0x00,
    0x00, //   Physical Maximum (65535)
    0x27,
    0xff,
    0xff,
    0x00,
    0x00, //   Logical Maximum (65535)
    0x75,
    0x10, //   Report Size (16)
    0x95,
    0x01, //   Report Count (1)
    0x05,
    0x0d, //   Usage Page (Digitizer)
    0x09,
    0x56, //   Usage (Scan Time)
    0x81,
    0x02, //   Input (Data,Var,Abs)
    0x09,
    0x54, //   Usage (Contact Count)
    0x25,
    0x7f, //   Logical Maximum (127)
    0x95,
    0x01, //   Report Count (1)
    0x75,
    0x08, //   Report Size (8)
    0x81,
    0x02, //   Input (Data,Var,Abs)
    // PTP buttons are descriptor-compatible but stay zero; physical clicks
    // travel through the mouse collection.
    0x05,
    0x09, //   Usage Page (Button)
    0x09,
    0x01, //   Usage (Button 1)
    0x09,
    0x02, //   Usage (Button 2)
    0x09,
    0x03, //   Usage (Button 3)
    0x25,
    0x01, //   Logical Maximum (1)
    0x75,
    0x01, //   Report Size (1)
    0x95,
    0x03, //   Report Count (3)
    0x81,
    0x02, //   Input (Data,Var,Abs)
    0x95,
    0x05, //   Report Count (5)
    0x81,
    0x03, //   Input (Const,Var,Abs) padding
    // Feature 0x03: contact count maximum + pad type.
    0x05,
    0x0d, //   Usage Page (Digitizer)
    0x85,
    REPORT_ID_MAX_COUNT, //   Report ID (3)
    0x09,
    0x55, //   Usage (Contact Count Maximum)
    0x09,
    0x59, //   Usage (Pad Type)
    0x75,
    0x04, //   Report Size (4)
    0x95,
    0x02, //   Report Count (2)
    0x25,
    0x0f, //   Logical Maximum (15)
    0xb1,
    0x02, //   Feature (Data,Var,Abs)
    // Feature 0x05: PTPHQA certification status.
    0x06,
    0x00,
    0xff, //   Usage Page (Vendor Defined 0xFF00)
    0x85,
    REPORT_ID_PTPHQA, //   Report ID (5)
    0x09,
    0xc5, //   Usage (Vendor Usage 0xC5)
    0x15,
    0x00, //   Logical Minimum (0)
    0x26,
    0xff,
    0x00, //   Logical Maximum (255)
    0x75,
    0x08, //   Report Size (8)
    0x96,
    0x00,
    0x01, //   Report Count (256)
    0xb1,
    0x02, //   Feature (Data,Var,Abs)
    // Feature 0x04: surface switch + button switch. Microsoft's sample keeps
    // these inside the Touch Pad collection, so Windows Settings can toggle
    // surface reporting and the integrated buttons.
    0x05,
    0x0d, //   Usage Page (Digitizers)
    0x85,
    REPORT_ID_FUNCTION_SWITCH, //   Report ID (4)
    0x09,
    0x57, //   Usage (Surface Switch)
    0x09,
    0x58, //   Usage (Button Switch)
    0x15,
    0x00, //   Logical Minimum (0)
    0x25,
    0x01, //   Logical Maximum (1)
    0x75,
    0x01, //   Report Size (1)
    0x95,
    0x02, //   Report Count (2)
    0xb1,
    0x02, //   Feature (Data,Var,Abs)
    0x95,
    0x06, //   Report Count (6)
    0xb1,
    0x03, //   Feature (Const,Var,Abs) padding
    0xc0, // End Collection (Touch Pad)
    // ===== Configuration TLC (report 0x06) =====
    0x05,
    0x0d, // Usage Page (Digitizer)
    0x09,
    0x0e, // Usage (Configuration)
    0xa1,
    0x01, // Collection (Application)
    0x85,
    REPORT_ID_INPUT_MODE, //   Report ID (6): input mode
    0x09,
    0x22, //   Usage (Finger)
    0xa1,
    0x02, //   Collection (Logical)
    0x09,
    0x52, //     Usage (Input Mode)
    0x15,
    0x00, //     Logical Minimum (0)
    0x25,
    0x0a, //     Logical Maximum (10)
    0x75,
    0x08, //     Report Size (8)
    0x95,
    0x01, //     Report Count (1)
    0xb1,
    0x02, //     Feature (Data,Var,Abs)
    0xc0, //   End Collection
    0xc0, // End Collection (Configuration)
];

/// Microsoft's documented default (uncertified) certification blob.
///
/// The certification feature report itself is mandatory; Microsoft states that
/// on Windows 10 a valid signed blob is optional and this default is
/// acceptable. Windows 8.1 requires a certified blob, which is a Partner
/// Center/HLK task, not a driver-code task.
pub const PTPHQA_DEFAULT: [u8; PTPHQA_LEN] = [
    0xfc, 0x28, 0xfe, 0x84, 0x40, 0xcb, 0x9a, 0x87, 0x0d, 0xbe, 0x57, 0x3c, 0xb6, 0x70, 0x09, 0x88,
    0x07, 0x97, 0x2d, 0x2b, 0xe3, 0x38, 0x34, 0xb6, 0x6c, 0xed, 0xb0, 0xf7, 0xe5, 0x9c, 0xf6, 0xc2,
    0x2e, 0x84, 0x1b, 0xe8, 0xb4, 0x51, 0x78, 0x43, 0x1f, 0x28, 0x4b, 0x7c, 0x2d, 0x53, 0xaf, 0xfc,
    0x47, 0x70, 0x1b, 0x59, 0x6f, 0x74, 0x43, 0xc4, 0xf3, 0x47, 0x18, 0x53, 0x1a, 0xa2, 0xa1, 0x71,
    0xc7, 0x95, 0x0e, 0x31, 0x55, 0x21, 0xd3, 0xb5, 0x1e, 0xe9, 0x0c, 0xba, 0xec, 0xb8, 0x89, 0x19,
    0x3e, 0xb3, 0xaf, 0x75, 0x81, 0x9d, 0x53, 0xb9, 0x41, 0x57, 0xf4, 0x6d, 0x39, 0x25, 0x29, 0x7c,
    0x87, 0xd9, 0xb4, 0x98, 0x45, 0x7d, 0xa7, 0x26, 0x9c, 0x65, 0x3b, 0x85, 0x68, 0x89, 0xd7, 0x3b,
    0xbd, 0xff, 0x14, 0x67, 0xf2, 0x2b, 0xf0, 0x2a, 0x41, 0x54, 0xf0, 0xfd, 0x2c, 0x66, 0x7c, 0xf8,
    0xc0, 0x8f, 0x33, 0x13, 0x03, 0xf1, 0xd3, 0xc1, 0x0b, 0x89, 0xd9, 0x1b, 0x62, 0xcd, 0x51, 0xb7,
    0x80, 0xb8, 0xaf, 0x3a, 0x10, 0xc1, 0x8a, 0x5b, 0xe8, 0x8a, 0x56, 0xf0, 0x8c, 0xaa, 0xfa, 0x35,
    0xe9, 0x42, 0xc4, 0xd8, 0x55, 0xc3, 0x38, 0xcc, 0x2b, 0x53, 0x5c, 0x69, 0x52, 0xd5, 0xc8, 0x73,
    0x02, 0x38, 0x7c, 0x73, 0xb6, 0x41, 0xe7, 0xff, 0x05, 0xd8, 0x2b, 0x79, 0x9a, 0xe2, 0x34, 0x60,
    0x8f, 0xa3, 0x32, 0x1f, 0x09, 0x78, 0x62, 0xbc, 0x80, 0xe3, 0x0f, 0xbd, 0x65, 0x20, 0x08, 0x13,
    0xc1, 0xe2, 0xee, 0x53, 0x2d, 0x86, 0x7e, 0xa7, 0x5a, 0xc5, 0xd3, 0x7d, 0x98, 0xbe, 0x31, 0x48,
    0x1f, 0xfb, 0xda, 0xaf, 0xa2, 0xa8, 0x6a, 0x89, 0xd6, 0xbf, 0xf2, 0xd3, 0x32, 0x2a, 0x9a, 0xe4,
    0xcf, 0x17, 0xb7, 0xb8, 0xf4, 0xe1, 0x33, 0x08, 0x24, 0x8b, 0xc4, 0x43, 0xa5, 0xe5, 0x24, 0xc2,
];

/// Encode one frame from already-filtered fingers.
///
/// `contacts` are fingers that are down; the five-contact limit and unique
/// contact identifiers are enforced here. Unused slots stay zero.
pub fn encode_touch_report(contacts: &[Contact], scan_time: u16) -> PtpInputReport {
    let mut report = PtpInputReport::new();
    report.scan_time = scan_time;

    let mut used_ids: u16 = 0;

    for contact in contacts.iter().take(MAX_CONTACTS) {
        let mut id = contact.id & 0x0f;
        if used_ids & (1u16 << id) != 0 {
            // Windows requires ids to be unique within a frame. The mouse
            // should already do this, but a duplicated id is worse than a
            // different one, so pick the first free value.
            match (0..16u8).find(|candidate| used_ids & (1u16 << *candidate) == 0) {
                Some(free) => id = free,
                // Impossible with at most five contacts, but never panic in a
                // driver.
                None => break,
            }
        }
        used_ids |= 1u16 << id;

        let slot = report.contact_count as usize;
        report.contacts[slot] = PtpContact::new(id, contact.x as i32, contact.y as i32);
        report.contact_count = slot as u8 + 1;
    }

    report
}

/// `-2048..=2047` to the PTP range `0..=4095`.
fn absolute(value: i32) -> u16 {
    (value + 2048).clamp(0, 4095) as u16
}

/// `true` for the two report ids that carry Apple touch data: `0x27` over
/// Bluetooth and `0x12` over USB.
pub fn is_apple_touch_report(report: &[u8]) -> bool {
    matches!(
        report.first(),
        Some(&APPLE_TOUCH_REPORT_BT) | Some(&APPLE_TOUCH_REPORT_USB)
    )
}

#[derive(Clone, Copy)]
struct Record {
    id: u8,
    x: i32,
    y: i32,
}

/// Decode one 8-byte Apple touch record.
///
/// The byte offsets mirror `magicmouse::parse_touch`, which is the authority
/// used by the observation filter. Keep the two in sync.
fn parse_record(record: &[u8]) -> Option<Record> {
    // record[7] & 0xf0 is the contact state: 0 means no finger on the glass,
    // 0x30/0x40 are start and drag.
    if record[7] & APPLE_TOUCH_STATE_MASK == 0 {
        return None;
    }

    // Low six bits of record[5] hold the contact size; tiny contacts are
    // palms and hover noise, not fingers.
    if record[5] & 0x3f < MIN_CONTACT_SIZE {
        return None;
    }

    // X: bits 20..32 of the little-endian word at record[0..2], signed 12-bit.
    // Y: bits 20..32 of the word at record[1..3], also 12-bit, but the device
    // measures upwards while the PTP origin is top-left.
    let x = (((record[1] as i32) << 28) | ((record[0] as i32) << 20)) >> 20;
    let y = -((((record[2] as i32) << 24) | ((record[1] as i32) << 16)) >> 20);
    // Contact id: low two bits from record[5] bits 6..8, high two from
    // record[6] bits 0..2.
    let id = ((((record[6] as u32) << 2) | ((record[5] as u32) >> 6)) & 0x0f) as u8;

    Some(Record { id, x, y })
}

/// Translate one raw Apple multi-touch frame into a PTP input report.
///
/// Hot path: no allocation, no floating point, no I/O. Returns `true` when
/// `out` must be submitted to VHF; `false` means the frame had fewer than two
/// active fingers and must not produce touch input.
///
/// ```text
/// byte 0        report id 0x27 (Bluetooth) or 0x12 (USB)
/// byte 1        clicks: bit 0 left, bit 1 right
/// bytes 2..4    cursor dx, little-endian i16
/// bytes 4..6    cursor dy, little-endian i16
/// bytes 6..14   timestamp and unused bytes
/// bytes 14..    up to 15 touch records, 8 bytes each
/// ```
pub fn translate_apple_touch_to_ptp(
    report: &[u8],
    scan_time: u16,
    out: &mut PtpInputReport,
) -> bool {
    *out = PtpInputReport::new();
    out.scan_time = scan_time;

    if !is_apple_touch_report(report) || report.len() < APPLE_TOUCH_DATA_OFFSET {
        return false;
    }

    let records = (report.len() - APPLE_TOUCH_DATA_OFFSET) / APPLE_TOUCH_RECORD_LEN;
    let mut used_ids: u16 = 0;

    for index in 0..records {
        let slot = out.contact_count as usize;
        if slot == MAX_CONTACTS {
            break;
        }

        let start = APPLE_TOUCH_DATA_OFFSET + index * APPLE_TOUCH_RECORD_LEN;
        let record = &report[start..start + APPLE_TOUCH_RECORD_LEN];
        let Some(record) = parse_record(record) else {
            continue;
        };

        let mut id = record.id & 0x0f;
        if used_ids & (1u16 << id) != 0 {
            match (0..16u8).find(|candidate| used_ids & (1u16 << *candidate) == 0) {
                Some(free) => id = free,
                None => break,
            }
        }
        used_ids |= 1u16 << id;

        out.contacts[slot] = PtpContact::new(id, record.x, record.y);
        out.contact_count = slot as u8 + 1;
    }

    // CRITICAL Magic Mouse adaptation: the optical sensor owns one-finger
    // pointer movement, so only frames with at least two fingers become touch
    // input. The zeroed report above releases contacts that were forwarded on
    // the previous frame.
    if out.contact_count < 2 {
        *out = PtpInputReport::new();
        out.scan_time = scan_time;
        return false;
    }

    true
}

/// Translate the pointer part of one Apple report into a standard mouse
/// report: optical-sensor movement and the physical left/right buttons.
///
/// This runs for every Apple input report, including frames that also carry
/// touch records, because the sensor stays the only pointer source.
pub fn translate_apple_mouse(report: &[u8], out: &mut MouseReport) -> bool {
    *out = MouseReport::new();

    if !is_apple_touch_report(report) || report.len() < 6 {
        return false;
    }

    // byte 1: bit 0 left, bit 1 right. The Magic Mouse has no middle button;
    // Windows mouse reports use the same order for buttons 1 and 2.
    out.buttons = report[1] & 0x03;
    // bytes 2..4 and 4..6: relative movement from the optical sensor, little
    // endian. This is what actually moves the pointer.
    out.dx = i16::from_le_bytes([report[2], report[3]]);
    out.dy = i16::from_le_bytes([report[4], report[5]]);

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raw state byte for "finger is down" (`TOUCH_STATE_START`).
    const DOWN: u8 = 0x30;

    /// Build a synthetic Apple touch frame. Coordinates are in device
    /// (-2048..=2047) space; the helper applies the inverse of the translator.
    fn frame(report_id: u8, touches: &[(u8, i32, i32)]) -> Vec<u8> {
        let mut raw = vec![0u8; APPLE_TOUCH_DATA_OFFSET + touches.len() * APPLE_TOUCH_RECORD_LEN];
        raw[0] = report_id;
        raw[1] = 0x01; // left button down
        raw[2] = 0xfe; // dx = -2
        raw[3] = 0xff;
        raw[4] = 0x03; // dy = 3
        raw[5] = 0x00;

        for (index, &(id, x, y)) in touches.iter().enumerate() {
            let start = APPLE_TOUCH_DATA_OFFSET + index * APPLE_TOUCH_RECORD_LEN;
            let record = &mut raw[start..start + APPLE_TOUCH_RECORD_LEN];

            let x_raw = (x & 0x0fff) as u16;
            // The device stores Y upwards, the translator negates it.
            let y_raw = (-y & 0x0fff) as u16;

            record[0] = x_raw as u8;
            record[1] = ((x_raw >> 8) as u8 & 0x0f) | (((y_raw & 0x0f) as u8) << 4);
            record[2] = (y_raw >> 4) as u8;
            record[5] = (id & 0x03) << 6 | 0x20; // size 32, id bits 0..2
            record[6] = ((id >> 2) & 0x03) | 0x80; // id bits 2..4, orientation 0
            record[7] = DOWN;
        }

        raw
    }

    // Packed fields must be read by value: taking a reference to one is
    // rejected by the compiler (E0793).
    fn slot(report: &PtpInputReport, index: usize) -> PtpContact {
        let contacts = report.contacts;
        contacts[index]
    }

    fn flags_at(report: &PtpInputReport, index: usize) -> u8 {
        slot(report, index).flags
    }

    fn x_at(report: &PtpInputReport, index: usize) -> u16 {
        slot(report, index).x
    }

    fn y_at(report: &PtpInputReport, index: usize) -> u16 {
        slot(report, index).y
    }

    fn count(report: &PtpInputReport) -> u8 {
        report.contact_count
    }

    fn scan(report: &PtpInputReport) -> u16 {
        report.scan_time
    }

    fn mouse_buttons(report: &MouseReport) -> u8 {
        report.buttons
    }

    fn mouse_dx(report: &MouseReport) -> i16 {
        report.dx
    }

    fn mouse_dy(report: &MouseReport) -> i16 {
        report.dy
    }

    #[test]
    fn report_layout_is_thirty_bytes() {
        assert_eq!(core::mem::size_of::<PtpContact>(), CONTACT_LEN);
        assert_eq!(core::mem::size_of::<PtpInputReport>(), 30);
        assert_eq!(core::mem::size_of::<MouseReport>(), MOUSE_REPORT_LEN);
        assert_eq!(PTPHQA_DEFAULT.len(), PTPHQA_LEN);
        assert_eq!(PTPHQA_DEFAULT[0], 0xfc);
        assert_eq!(PTPHQA_DEFAULT[255], 0xc2);
        assert_eq!(MAX_COUNT_PAYLOAD, 0x05);
    }

    #[test]
    fn one_finger_never_reaches_the_touchpad() {
        let raw = frame(APPLE_TOUCH_REPORT_BT, &[(3, 100, 200)]);
        let mut report = PtpInputReport::new();

        assert!(!translate_apple_touch_to_ptp(&raw, 7, &mut report));
        assert_eq!(count(&report), 0);
        assert_eq!(scan(&report), 7);
        assert_eq!(flags_at(&report, 0), 0);
    }

    #[test]
    fn empty_frame_releases_contacts() {
        let raw = frame(APPLE_TOUCH_REPORT_BT, &[]);
        let mut report = PtpInputReport::new();
        report.contacts[0] = PtpContact::new(9, 0, 0); // stale previous frame
        report.contact_count = 2;

        assert!(!translate_apple_touch_to_ptp(&raw, 5, &mut report));
        assert_eq!(count(&report), 0);
        assert_eq!(flags_at(&report, 0), 0);
        assert_eq!(scan(&report), 5);
    }

    #[test]
    fn dropping_to_one_finger_releases_the_second_contact() {
        let two = frame(APPLE_TOUCH_REPORT_BT, &[(0, 0, 0), (1, 10, 10)]);
        let mut report = PtpInputReport::new();
        assert!(translate_apple_touch_to_ptp(&two, 0, &mut report));
        assert_eq!(count(&report), 2);

        let one = frame(APPLE_TOUCH_REPORT_BT, &[(0, 0, 0)]);
        assert!(!translate_apple_touch_to_ptp(&one, 0, &mut report));
        assert_eq!(count(&report), 0);
    }

    #[test]
    fn two_fingers_are_mapped_into_the_ptp_range() {
        // X is signed 12-bit and maps -2048..=2047 onto 0..=4095. Y is
        // negated while decoding, so its reachable range is -2047..=2048.
        let raw = frame(APPLE_TOUCH_REPORT_BT, &[(0, -2048, -2047), (1, 2047, 2048)]);
        let mut report = PtpInputReport::new();

        assert!(translate_apple_touch_to_ptp(&raw, 0x1234, &mut report));
        assert_eq!(count(&report), 2);
        assert_eq!(scan(&report), 0x1234);
        assert_eq!(flags_at(&report, 0), 0x03); // confidence + tip switch, id 0
        assert_eq!(x_at(&report, 0), 0);
        assert_eq!(y_at(&report, 0), 1);
        assert_eq!(flags_at(&report, 1), (1 << 2) | 0x03);
        assert_eq!(x_at(&report, 1), 4095);
        assert_eq!(y_at(&report, 1), 4095);
    }

    #[test]
    fn more_than_five_fingers_are_clamped() {
        let raw = frame(
            APPLE_TOUCH_REPORT_BT,
            &[
                (0, 0, 0),
                (1, 10, 10),
                (2, 20, 20),
                (3, 30, 30),
                (4, 40, 40),
                (5, 50, 50),
            ],
        );
        let mut report = PtpInputReport::new();

        assert!(translate_apple_touch_to_ptp(&raw, 0, &mut report));
        assert_eq!(count(&report), 5);
    }

    #[test]
    fn duplicate_contact_ids_are_made_unique() {
        let raw = frame(APPLE_TOUCH_REPORT_BT, &[(3, 0, 0), (3, 10, 10)]);
        let mut report = PtpInputReport::new();

        assert!(translate_apple_touch_to_ptp(&raw, 0, &mut report));
        let first = flags_at(&report, 0) >> 2 & 0x0f;
        let second = flags_at(&report, 1) >> 2 & 0x0f;
        assert_eq!(first, 3);
        assert_ne!(first, second);
        assert_eq!(second, 0);
    }

    #[test]
    fn usb_report_id_is_accepted() {
        let raw = frame(APPLE_TOUCH_REPORT_USB, &[(0, 10, 10), (1, 20, 20)]);
        let mut report = PtpInputReport::new();

        assert!(is_apple_touch_report(&raw));
        assert!(translate_apple_touch_to_ptp(&raw, 0, &mut report));
        assert_eq!(count(&report), 2);
    }

    #[test]
    fn mouse_part_keeps_sensor_and_buttons() {
        let raw = frame(APPLE_TOUCH_REPORT_BT, &[(0, 0, 0)]);
        let mut mouse = MouseReport::new();

        assert!(translate_apple_mouse(&raw, &mut mouse));
        assert_eq!(mouse_buttons(&mouse), 0x01);
        assert_eq!(mouse_dx(&mouse), -2);
        assert_eq!(mouse_dy(&mouse), 3);
    }

    #[test]
    fn mouse_rejects_foreign_reports() {
        let mut mouse = MouseReport::new();
        assert!(!translate_apple_mouse(&[0x90, 0x01, 0x64], &mut mouse));
        assert_eq!(mouse_buttons(&mouse), 0);
        assert!(!is_apple_touch_report(&[]));
        assert!(!is_apple_touch_report(&[0x90, 0x01, 0x64]));

        // A valid id with a truncated body is still not translatable.
        assert!(is_apple_touch_report(&[0x12]));
        let mut report = PtpInputReport::new();
        assert!(!translate_apple_touch_to_ptp(&[0x12], 0, &mut report));
        assert_eq!(count(&report), 0);
    }

    #[test]
    fn encoder_sets_count_scan_time_and_contact_bytes() {
        let report = encode_touch_report(&[Contact { id: 4, x: 0, y: 0 }], 0x1234);

        assert_eq!(count(&report), 1);
        assert_eq!(scan(&report), 0x1234);
        assert_eq!(flags_at(&report, 0), 0x13); // id 4 << 2, confidence + tip switch
        assert_eq!(x_at(&report, 0), 2048);
        assert_eq!(y_at(&report, 0), 2048);
    }

    #[test]
    fn as_bytes_matches_the_declared_layout() {
        let report = encode_touch_report(&[Contact { id: 2, x: 0, y: 0 }], 0x1234);
        let bytes = as_bytes(&report);

        assert_eq!(bytes.len(), 30);
        assert_eq!(bytes[0], REPORT_ID_TOUCH);
        assert_eq!(&bytes[26..28], &[0x34, 0x12]); // scan time
        assert_eq!(bytes[28], 1); // contact count
        assert_eq!(bytes[29], 0); // buttons stay zero on the PTP path
    }

    #[test]
    fn feature_payloads_match_their_declared_sizes() {
        assert_eq!(
            as_bytes(&MaxCountFeature::DEFAULT),
            &[REPORT_ID_MAX_COUNT, MAX_COUNT_PAYLOAD]
        );
        assert_eq!(
            as_bytes(&FunctionSwitchFeature::DEFAULT),
            &[REPORT_ID_FUNCTION_SWITCH, FUNCTION_SWITCH_DEFAULT]
        );
        assert_eq!(
            as_bytes(&InputModeFeature::DEFAULT),
            &[REPORT_ID_INPUT_MODE, INPUT_MODE_MOUSE]
        );

        let cert = as_bytes(&CertificationFeature::DEFAULT);
        assert_eq!(cert.len(), 1 + PTPHQA_LEN);
        assert_eq!(cert[0], REPORT_ID_PTPHQA);
        assert_eq!(cert[256], 0xc2);
    }

    #[test]
    fn descriptor_declares_every_collection_and_report_id() {
        assert_eq!(&REPORT_DESCRIPTOR[..4], &[0x05, 0x01, 0x09, 0x02]); // mouse
        assert!(REPORT_DESCRIPTOR
            .windows(4)
            .any(|window| window == [0x05, 0x0d, 0x09, 0x05])); // touch pad
        assert!(REPORT_DESCRIPTOR
            .windows(4)
            .any(|window| window == [0x05, 0x0d, 0x09, 0x0e])); // configuration

        for id in [
            REPORT_ID_MOUSE,
            REPORT_ID_TOUCH,
            REPORT_ID_MAX_COUNT,
            REPORT_ID_FUNCTION_SWITCH,
            REPORT_ID_PTPHQA,
            REPORT_ID_INPUT_MODE,
        ] {
            assert!(
                REPORT_DESCRIPTOR
                    .windows(2)
                    .any(|window| window == [0x85, id]),
                "descriptor does not declare report id {id}"
            );
        }
    }

    #[test]
    fn descriptor_carries_every_required_ptp_usage() {
        // Tip Switch, Confidence, Contact Identifier, X, Y, Scan Time,
        // Contact Count, Contact Count Maximum, Pad Type, Surface Switch,
        // Button Switch and the PTPHQA vendor usage.
        for usage in [
            0x42u8, 0x47, 0x51, 0x30, 0x31, 0x56, 0x54, 0x55, 0x59, 0x57, 0x58, 0xc5,
        ] {
            assert!(
                REPORT_DESCRIPTOR
                    .windows(2)
                    .any(|window| window[0] == 0x09 && window[1] == usage),
                "descriptor is missing usage 0x{usage:02x}"
            );
        }
    }

    #[test]
    fn descriptor_declares_exactly_five_finger_collections() {
        // The full prefix matters: the configuration collection also contains
        // `0x09, 0x22, 0xa1, 0x02`, but it is preceded by the Configuration
        // usage, not by `Usage Page (Digitizers)`.
        const FINGER_COLLECTION: [u8; 6] = [0x05, 0x0d, 0x09, 0x22, 0xa1, 0x02];
        let count = REPORT_DESCRIPTOR
            .windows(FINGER_COLLECTION.len())
            .filter(|window| window.iter().eq(FINGER_COLLECTION.iter()))
            .count();

        assert_eq!(count, MAX_CONTACTS, "PTP must expose exactly five fingers");
    }

    /// Total bit length of the input (`0x81`) or feature (`0xb1`) reports per
    /// report id, computed from the descriptor itself.
    fn report_bits(kind: u8) -> Vec<(u8, usize)> {
        let mut totals: Vec<(u8, usize)> = Vec::new();
        let mut report_id = 0u8;
        let mut report_size = 0usize;
        let mut report_count = 0usize;
        let mut index = 0;

        while index < REPORT_DESCRIPTOR.len() {
            let prefix = REPORT_DESCRIPTOR[index];
            if prefix == 0xfe {
                let size = REPORT_DESCRIPTOR.get(index + 1).copied().unwrap_or(0) as usize;
                index += 2 + size;
                continue;
            }
            let size = match prefix & 0x03 {
                3 => 4,
                other => other as usize,
            };
            let item_type = (prefix >> 2) & 0x03;
            let tag = prefix >> 4;
            let data = &REPORT_DESCRIPTOR[index + 1..index + 1 + size];
            let value = data
                .iter()
                .rev()
                .fold(0usize, |acc, byte| (acc << 8) | *byte as usize);

            if item_type == 1 {
                match tag {
                    0x07 => report_size = value,
                    0x08 => report_id = value as u8,
                    0x09 => report_count = value,
                    _ => {}
                }
            } else if item_type == 0 && tag == kind {
                let bits = report_size * report_count;
                match totals.iter_mut().find(|(id, _)| *id == report_id) {
                    Some((_, total)) => *total += bits,
                    None => totals.push((report_id, bits)),
                }
            }

            index += 1 + size;
        }

        totals
    }

    #[test]
    fn descriptor_report_sizes_match_the_packed_structs() {
        // 8 button bits + 16 + 16; 5 x 40 contact bits + 16 scan + 8 count
        // + 8 buttons.
        assert_eq!(
            report_bits(0x08), // Input items
            vec![(REPORT_ID_MOUSE, 40), (REPORT_ID_TOUCH, 232)]
        );
        assert_eq!(
            report_bits(0x0b), // Feature items
            vec![
                (REPORT_ID_MAX_COUNT, 8),
                (REPORT_ID_PTPHQA, PTPHQA_LEN * 8),
                (REPORT_ID_FUNCTION_SWITCH, 8),
                (REPORT_ID_INPUT_MODE, 8),
            ]
        );
    }

    #[test]
    fn descriptor_collections_are_balanced() {
        let mut depth = 0i32;
        let mut index = 0;

        while index < REPORT_DESCRIPTOR.len() {
            let prefix = REPORT_DESCRIPTOR[index];
            if prefix == 0xfe {
                // Long item: the next byte is the payload size.
                let size = REPORT_DESCRIPTOR.get(index + 1).copied().unwrap_or(0) as usize;
                index += 2 + size;
                continue;
            }

            let size = match prefix & 0x03 {
                3 => 4,
                other => other as usize,
            };
            let item_type = (prefix >> 2) & 0x03;
            let tag = prefix >> 4;

            if item_type == 0 {
                match tag {
                    0x0a => depth += 1, // Collection
                    0x0c => {
                        depth -= 1; // End Collection
                        assert!(depth >= 0, "end collection without a matching start");
                    }
                    _ => {}
                }
            }
            index += 1 + size;
        }

        assert_eq!(depth, 0, "unbalanced collections");
    }
}
