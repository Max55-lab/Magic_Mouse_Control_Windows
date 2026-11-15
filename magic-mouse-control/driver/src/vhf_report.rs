// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Pure encoding for the Phase 3 virtual precision touchpad.
//!
//! This module has no WDK dependency on purpose: the report layout is the part
//! that is easy to get wrong, so it is unit tested on the host:
//!
//! ```text
//! rustc --edition 2021 --test driver/src/vhf_report.rs -o mmtest && ./mmtest
//! ```
//!
//! Layout, matching the descriptor below:
//!
//! ```text
//! byte 0        report id 0x01
//! bytes 1..26   5 contacts x 5 bytes
//! bytes 26..28  scan time, little endian, 100 us units
//! byte 28       contact count
//! byte 29       buttons: bit 0 left, bit 1 right, bit 2 middle
//! ```
//!
//! Per contact (least significant bit first, as HID packs fields):
//!
//! ```text
//! bit 0      confidence (0 = a finger; we filter palms before encoding)
//! bit 1      tip switch (1 = finger on the surface)
//! bits 2..5  contact id, 0..15
//! bits 6..7  padding
//! bytes 1..2 X, 0..4095
//! bytes 3..4 Y, 0..4095
//! ```
//!
//! The coordinate range is not arbitrary: the Magic Mouse reports 12-bit
//! signed positions in `-2048..=2047`, so `x + 2048` maps exactly onto
//! `0..=4095`, which is the maximum a Windows Precision Touchpad may use.

/// Input report with the five contacts.
pub const REPORT_ID_TOUCH: u8 = 0x01;
/// Feature report: contact count maximum (low nibble) and pad type (high).
pub const REPORT_ID_MAX_COUNT: u8 = 0x02;
/// Feature report: 256-byte device certification status blob (PTPHQA).
pub const REPORT_ID_PTPHQA: u8 = 0x03;
/// Feature report: input mode (0 = mouse collection, 3 = touchpad).
pub const REPORT_ID_INPUT_MODE: u8 = 0x04;

/// Windows precision touchpads expose at most five contacts.
pub const MAX_CONTACTS: usize = 5;
pub const CONTACT_LEN: usize = 5;
pub const REPORT_LEN: usize = 1 + MAX_CONTACTS * CONTACT_LEN + 2 + 1 + 1;

/// Report payload length *including* the report id. `HID_XFER_PACKET`
/// convention: `reportBuffer[0]` is the id.
pub const TOUCH_REPORT_LEN: usize = REPORT_LEN;

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

/// One finger in Magic Mouse coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Contact {
    /// Tracking id as reported by the mouse. Only the low four bits are used.
    pub id: u8,
    /// `-2048..=2047`; values outside are clamped.
    pub x: i16,
    pub y: i16,
}

/// The five-contact precision touchpad report descriptor.
///
/// This follows Microsoft's sample layout, with three deliberate changes:
/// * the finger collection is repeated five times, because a precision
///   touchpad report carries up to five contacts,
/// * the contact identifier is four bits instead of two, so five simultaneous
///   contacts can have unique ids,
/// * the physical maxima are the measured Magic Mouse surface (57 mm x 113 mm),
/// * no mouse collection: the physical mouse keeps providing the pointer.
///
/// Each finger contributes 40 bits: confidence + tip switch (2), contact id
/// (4), padding (2), X (16), Y (16). Five fingers plus scan time, contact
/// count and buttons are exactly the 29 payload bytes encoded below.
pub const REPORT_DESCRIPTOR: &[u8] = &[
    0x05, 0x0d, // Usage Page (Digitizers)
    0x09, 0x05, // Usage (Touch Pad)
    0xa1, 0x01, // Collection (Application)
    0x85, 0x01, //   Report ID (1)
    // --- Finger 1 ---
    0x05, 0x0d, //   Usage Page (Digitizers)
    0x09, 0x22, //   Usage (Finger)
    0xa1, 0x02, //   Collection (Logical)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x09, 0x47, //     Usage (Confidence)
    0x09, 0x42, //     Usage (Tip Switch)
    0x95, 0x02, //     Report Count (2)
    0x75, 0x01, //     Report Size (1)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x25, 0x0f, //     Logical Maximum (15)
    0x75, 0x04, //     Report Size (4)
    0x95, 0x01, //     Report Count (1)
    0x09, 0x51, //     Usage (Contact Identifier)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x75, 0x02, //     Report Size (2)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x03, //     Input (Const,Var,Abs)  padding to 5 bytes
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x15, 0x00, //     Logical Minimum (0)
    0x26, 0xff, 0x0f, //     Logical Maximum (4095)
    0x75, 0x10, //     Report Size (16)
    0x55, 0x0e, //     Unit Exponent (-2)
    0x65, 0x13, //     Unit (Inch, English Linear)
    0x09, 0x30, //     Usage (X)
    0x35, 0x00, //     Physical Minimum (0)
    0x46, 0xe0, 0x00, //     Physical Maximum (224 = 2.24 in / 57 mm)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0x46, 0xbd, 0x01, //     Physical Maximum (445 = 4.45 in / 113 mm)
    0x09, 0x31, //     Usage (Y)
    0x81, 0x02, //     Input (Data,Var,Abs)
    0xc0, //   End Collection
    // --- Fingers 2..5: identical collections, required by the five-contact
    // report. Every block re-declares the globals it depends on, so the order
    // can be read without tracking state across comments. ---
    0x05, 0x0d, 0x09, 0x22, 0xa1, 0x02, 0x15, 0x00, 0x25, 0x01, 0x09, 0x47, 0x09, 0x42, 0x95, 0x02,
    0x75, 0x01, 0x81, 0x02, 0x25, 0x0f, 0x75, 0x04, 0x95, 0x01, 0x09, 0x51, 0x81, 0x02, 0x75, 0x02,
    0x95, 0x01, 0x81, 0x03, 0x05, 0x01, 0x15, 0x00, 0x26, 0xff, 0x0f, 0x75, 0x10, 0x55, 0x0e, 0x65,
    0x13, 0x09, 0x30, 0x35, 0x00, 0x46, 0xe0, 0x00, 0x95, 0x01, 0x81, 0x02, 0x46, 0xbd, 0x01, 0x09,
    0x31, 0x81, 0x02, 0xc0, 0x05, 0x0d, 0x09, 0x22, 0xa1, 0x02, 0x15, 0x00, 0x25, 0x01, 0x09, 0x47,
    0x09, 0x42, 0x95, 0x02, 0x75, 0x01, 0x81, 0x02, 0x25, 0x0f, 0x75, 0x04, 0x95, 0x01, 0x09, 0x51,
    0x81, 0x02, 0x75, 0x02, 0x95, 0x01, 0x81, 0x03, 0x05, 0x01, 0x15, 0x00, 0x26, 0xff, 0x0f, 0x75,
    0x10, 0x55, 0x0e, 0x65, 0x13, 0x09, 0x30, 0x35, 0x00, 0x46, 0xe0, 0x00, 0x95, 0x01, 0x81, 0x02,
    0x46, 0xbd, 0x01, 0x09, 0x31, 0x81, 0x02, 0xc0, 0x05, 0x0d, 0x09, 0x22, 0xa1, 0x02, 0x15, 0x00,
    0x25, 0x01, 0x09, 0x47, 0x09, 0x42, 0x95, 0x02, 0x75, 0x01, 0x81, 0x02, 0x25, 0x0f, 0x75, 0x04,
    0x95, 0x01, 0x09, 0x51, 0x81, 0x02, 0x75, 0x02, 0x95, 0x01, 0x81, 0x03, 0x05, 0x01, 0x15, 0x00,
    0x26, 0xff, 0x0f, 0x75, 0x10, 0x55, 0x0e, 0x65, 0x13, 0x09, 0x30, 0x35, 0x00, 0x46, 0xe0, 0x00,
    0x95, 0x01, 0x81, 0x02, 0x46, 0xbd, 0x01, 0x09, 0x31, 0x81, 0x02, 0xc0, 0x05, 0x0d, 0x09, 0x22,
    0xa1, 0x02, 0x15, 0x00, 0x25, 0x01, 0x09, 0x47, 0x09, 0x42, 0x95, 0x02, 0x75, 0x01, 0x81, 0x02,
    0x25, 0x0f, 0x75, 0x04, 0x95, 0x01, 0x09, 0x51, 0x81, 0x02, 0x75, 0x02, 0x95, 0x01, 0x81, 0x03,
    0x05, 0x01, 0x15, 0x00, 0x26, 0xff, 0x0f, 0x75, 0x10, 0x55, 0x0e, 0x65, 0x13, 0x09, 0x30, 0x35,
    0x00, 0x46, 0xe0, 0x00, 0x95, 0x01, 0x81, 0x02, 0x46, 0xbd, 0x01, 0x09, 0x31, 0x81, 0x02, 0xc0,
    0x55, 0x0c, //   Unit Exponent (-4)
    0x66, 0x01, 0x10, //   Unit (Seconds)
    0x47, 0xff, 0xff, 0x00, 0x00, //   Physical Maximum (65535)
    0x27, 0xff, 0xff, 0x00, 0x00, //   Logical Maximum (65535)
    0x75, 0x10, //   Report Size (16)
    0x95, 0x01, //   Report Count (1)
    0x05, 0x0d, //   Usage Page (Digitizer)
    0x09, 0x56, //   Usage (Scan Time)
    0x81, 0x02, //   Input (Data,Var,Abs)
    0x09, 0x54, //   Usage (Contact Count)
    0x25, 0x7f, //   Logical Maximum (127)
    0x95, 0x01, //   Report Count (1)
    0x75, 0x08, //   Report Size (8)
    0x81, 0x02, //   Input (Data,Var,Abs)
    0x05, 0x09, //   Usage Page (Button)
    0x09, 0x01, //   Usage (Button 1)
    0x09, 0x02, //   Usage (Button 2)
    0x09, 0x03, //   Usage (Button 3)
    0x25, 0x01, //   Logical Maximum (1)
    0x75, 0x01, //   Report Size (1)
    0x95, 0x03, //   Report Count (3)
    0x81, 0x02, //   Input (Data,Var,Abs)
    0x95, 0x05, //   Report Count (5)
    0x81, 0x03, //   Input (Const,Var,Abs)
    0x05, 0x0d, //   Usage Page (Digitizer)
    0x85, 0x02, //   Report ID (2): contact count maximum + pad type
    0x09, 0x55, //   Usage (Contact Count Maximum)
    0x09, 0x59, //   Usage (Pad Type)
    0x75, 0x04, //   Report Size (4)
    0x95, 0x02, //   Report Count (2)
    0x25, 0x0f, //   Logical Maximum (15)
    0xb1, 0x02, //   Feature (Data,Var,Abs)
    0x06, 0x00, 0xff, //   Usage Page (Vendor Defined 0xFF00)
    0x85, 0x03, //   Report ID (3): PTPHQA
    0x09, 0xc5, //   Usage (Vendor Usage 0xC5)
    0x15, 0x00, //   Logical Minimum (0)
    0x26, 0xff, 0x00, //   Logical Maximum (255)
    0x75, 0x08, //   Report Size (8)
    0x96, 0x00, 0x01, //   Report Count (256)
    0xb1, 0x02, //   Feature (Data,Var,Abs)
    0xc0, // End Collection (Touch Pad)
    0x05, 0x0d, // Usage Page (Digitizer)
    0x09, 0x0e, // Usage (Configuration)
    0xa1, 0x01, // Collection (Application)
    0x85, 0x04, //   Report ID (4): input mode
    0x09, 0x22, //   Usage (Finger)
    0xa1, 0x02, //   Collection (Logical)
    0x09, 0x52, //     Usage (Input Mode)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x0a, //     Logical Maximum (10)
    0x75, 0x08, //     Report Size (8)
    0x95, 0x01, //     Report Count (1)
    0xb1, 0x02, //     Feature (Data,Var,Abs)
    0xc0, //   End Collection
    0xc0, // End Collection
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

/// Encode one frame. `contacts` must already be filtered to fingers that are
/// down; this function enforces the five-contact limit and unique ids.
///
/// Returns the number of contacts written. `out` is always fully initialized,
/// so unused slots contain zero (tip switch clear, id 0).
pub fn encode_touch_report(
    contacts: &[Contact],
    buttons: u8,
    scan_time: u16,
    out: &mut [u8; REPORT_LEN],
) -> u8 {
    out.fill(0);
    out[0] = REPORT_ID_TOUCH;

    let mut used_ids: u16 = 0;
    let mut count = 0usize;

    for contact in contacts.iter().take(MAX_CONTACTS) {
        let mut id = contact.id & 0x0f;
        if used_ids & (1 << id) != 0 {
            // Windows requires ids to be unique within a frame. The mouse
            // should already do this, but a duplicated id is worse than a
            // different one, so pick the first free value.
            match (0..16).find(|candidate| used_ids & (1 << candidate) == 0) {
                Some(free) => id = free,
                // Impossible with at most five contacts, but never panic in a
                // driver.
                None => break,
            }
        }
        used_ids |= 1 << id;

        let offset = 1 + count * CONTACT_LEN;
        out[offset] = (id << 2) | 0x02; // tip switch set, confidence clear
        out[offset + 1..offset + 3].copy_from_slice(&absolute(contact.x).to_le_bytes());
        out[offset + 3..offset + 5].copy_from_slice(&absolute(contact.y).to_le_bytes());
        count += 1;
    }

    let scan_offset = 1 + MAX_CONTACTS * CONTACT_LEN;
    out[scan_offset..scan_offset + 2].copy_from_slice(&scan_time.to_le_bytes());
    out[scan_offset + 2] = count as u8;
    out[scan_offset + 3] = buttons & 0x07;
    count as u8
}

/// `-2048..=2047` to the PTP range `0..=4095`.
fn absolute(value: i16) -> u16 {
    let shifted = value as i32 + 2048;
    shifted.clamp(0, 4095) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(id: u8, x: i16, y: i16) -> Contact {
        Contact { id, x, y }
    }

    fn x_at(report: &[u8; REPORT_LEN], index: usize) -> u16 {
        u16::from_le_bytes([
            report[1 + index * CONTACT_LEN + 1],
            report[1 + index * CONTACT_LEN + 2],
        ])
    }

    fn y_at(report: &[u8; REPORT_LEN], index: usize) -> u16 {
        u16::from_le_bytes([
            report[1 + index * CONTACT_LEN + 3],
            report[1 + index * CONTACT_LEN + 4],
        ])
    }

    fn count_at(report: &[u8; REPORT_LEN]) -> u8 {
        report[1 + MAX_CONTACTS * CONTACT_LEN + 2]
    }

    #[test]
    fn report_layout_is_thirty_bytes() {
        assert_eq!(REPORT_LEN, 30);
        assert_eq!(TOUCH_REPORT_LEN, REPORT_LEN);
        assert_eq!(PTPHQA_DEFAULT.len(), PTPHQA_LEN);
        assert_eq!(PTPHQA_DEFAULT[0], 0xfc);
        assert_eq!(PTPHQA_DEFAULT[255], 0xc2);
        assert_eq!(MAX_COUNT_PAYLOAD, 0x05);
    }

    #[test]
    fn descriptor_starts_and_ends_with_an_application_collection() {
        assert_eq!(
            &REPORT_DESCRIPTOR[..6],
            &[0x05, 0x0d, 0x09, 0x05, 0xa1, 0x01]
        );
        assert_eq!(REPORT_DESCRIPTOR.last(), Some(&0xc0));
        for id in [
            REPORT_ID_TOUCH,
            REPORT_ID_MAX_COUNT,
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

    #[test]
    fn encodes_id_position_and_tip_switch() {
        let mut report = [0u8; REPORT_LEN];
        let written = encode_touch_report(&[contact(4, 0, 0)], 0, 0, &mut report);

        assert_eq!(written, 1);
        assert_eq!(report[0], REPORT_ID_TOUCH);
        assert_eq!(report[1], 0x12); // id 4 << 2, tip switch bit 1
        assert_eq!(x_at(&report, 0), 2048);
        assert_eq!(y_at(&report, 0), 2048);
        assert_eq!(count_at(&report), 1);
    }

    #[test]
    fn maps_the_full_signed_range_to_0_4095() {
        let mut report = [0u8; REPORT_LEN];
        encode_touch_report(
            &[contact(0, -2048, -2048), contact(1, 2047, 2047)],
            0,
            0,
            &mut report,
        );

        assert_eq!(x_at(&report, 0), 0);
        assert_eq!(y_at(&report, 0), 0);
        assert_eq!(x_at(&report, 1), 4095);
        assert_eq!(y_at(&report, 1), 4095);
    }

    #[test]
    fn clamps_out_of_range_values() {
        let mut report = [0u8; REPORT_LEN];
        encode_touch_report(&[contact(0, i16::MIN, i16::MAX)], 0, 0, &mut report);
        assert_eq!(x_at(&report, 0), 0);
        assert_eq!(y_at(&report, 0), 4095);
    }

    #[test]
    fn keeps_at_most_five_contacts() {
        let contacts: Vec<Contact> = (0..6).map(|id| contact(id, id as i16, 0)).collect();
        let mut report = [0u8; REPORT_LEN];
        let written = encode_touch_report(&contacts, 0, 0, &mut report);

        assert_eq!(written, 5);
        assert_eq!(count_at(&report), 5);
        for index in 0..MAX_CONTACTS {
            let id = report[1 + index * CONTACT_LEN] >> 2 & 0x0f;
            assert_eq!(id, index as u8);
        }
    }

    #[test]
    fn makes_duplicate_ids_unique() {
        let mut report = [0u8; REPORT_LEN];
        encode_touch_report(&[contact(3, 0, 0), contact(3, 10, 10)], 0, 0, &mut report);

        let first = report[1] >> 2 & 0x0f;
        let second = report[1 + CONTACT_LEN] >> 2 & 0x0f;
        assert_eq!(first, 3);
        assert_ne!(first, second);
        assert_eq!(second, 0);
    }

    #[test]
    fn writes_buttons_and_scan_time() {
        let mut report = [0u8; REPORT_LEN];
        encode_touch_report(&[], 0b101, 0x1234, &mut report);

        let scan_offset = 1 + MAX_CONTACTS * CONTACT_LEN;
        assert_eq!(&report[scan_offset..scan_offset + 2], &[0x34, 0x12]);
        assert_eq!(report[scan_offset + 3], 0b101);
        assert_eq!(count_at(&report), 0);
    }

    #[test]
    fn empty_frame_is_all_zero_contacts() {
        let mut report = [0xaa; REPORT_LEN];
        let written = encode_touch_report(&[], 0, 0, &mut report);

        assert_eq!(written, 0);
        assert!(report[1..1 + MAX_CONTACTS * CONTACT_LEN]
            .iter()
            .all(|byte| *byte == 0));
    }
}
