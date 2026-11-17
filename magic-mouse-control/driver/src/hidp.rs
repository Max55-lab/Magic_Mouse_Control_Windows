// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Minimal HIDP (HID over Bluetooth) framing helpers.
//!
//! At the BTHENUM layer there is no "report buffer": the payload is a stream of
//! HIDP transactions carried in L2CAP frames. The transaction header is one
//! byte:
//!
//! ```text
//!   0xA0 | param  DATA            param: 0 Other, 1 Input, 2 Output, 3 Feature
//!   0x40 | param  GET_REPORT      param: report type
//!   0x50 | param  SET_REPORT
//!   0x10 | param  HID_CONTROL
//!   0x00 | param  HANDSHAKE
//! ```
//!
//! Phase 0 only needs to recognise headers so the log can tell "this is HIDP"
//! from "this is some BthPort framing we do not understand yet". Full
//! reassembly (a report split over several IRPs) is intentionally NOT
//! implemented until the probe shows whether it is needed.

pub const TRANS_HANDSHAKE: u8 = 0x00;
pub const TRANS_HID_CONTROL: u8 = 0x10;
pub const TRANS_GET_REPORT: u8 = 0x40;
pub const TRANS_SET_REPORT: u8 = 0x50;
pub const TRANS_GET_IDLE: u8 = 0x60;
pub const TRANS_SET_IDLE: u8 = 0x70;
pub const TRANS_DATA: u8 = 0xA0;
pub const TRANS_DATC: u8 = 0xB0;

pub const DATA_INPUT: u8 = 0x01;

pub fn transaction_name(header: u8) -> &'static str {
    match header & 0xF0 {
        TRANS_HANDSHAKE => "handshake",
        TRANS_HID_CONTROL => "hid_control",
        TRANS_GET_REPORT => "get_report",
        TRANS_SET_REPORT => "set_report",
        TRANS_GET_IDLE => "get_idle",
        TRANS_SET_IDLE => "set_idle",
        TRANS_DATA => "data",
        TRANS_DATC => "datc",
        _ => "unknown",
    }
}

/// `true` when this byte starts a DATA(Input) transaction, i.e. the frame that
/// carries touch/battery reports on the wire.
pub fn is_data_input(header: u8) -> bool {
    header & 0xF0 == TRANS_DATA && header & 0x03 == DATA_INPUT
}

/// Return the report payload (starting with the report id) of a
/// DATA(Input) frame, or `None` for every other transaction.
pub fn data_input_payload(frame: &[u8]) -> Option<&[u8]> {
    match frame.split_first() {
        Some((header, payload)) if is_data_input(*header) && !payload.is_empty() => Some(payload),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_data_input() {
        assert!(is_data_input(0xA1));
        assert!(!is_data_input(0xA2));
        assert!(!is_data_input(0xA3));
        assert!(!is_data_input(0x40));
    }

    #[test]
    fn extracts_payload_only_for_data_input() {
        let frame = [0xA1u8, 0x12, 0x00, 0x01];
        assert_eq!(data_input_payload(&frame), Some(&frame[1..]));

        let set_report = [0x53u8, 0x00, 0x01];
        assert_eq!(data_input_payload(&set_report), None);
        assert_eq!(data_input_payload(&[0xA1]), None);
        assert_eq!(data_input_payload(&[]), None);
    }

    #[test]
    fn names_transactions() {
        assert_eq!(transaction_name(0xA1), "data");
        assert_eq!(transaction_name(0x43), "get_report");
        assert_eq!(transaction_name(0x10), "hid_control");
    }
}
