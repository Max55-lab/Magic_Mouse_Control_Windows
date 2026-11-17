// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Decoder for the Apple Magic Mouse 2 reports, mirroring
//! `drivers/hid/hid-magicmouse.c` (GPL-2.0-or-later, Linux) for the wire format
//! only. No code is copied; the bit offsets are facts about the device.
//!
//! This module is deliberately free of any WDK dependency so it can be unit
//! tested on the host:
//!
//! ```text
//! rustc --test driver/src/magicmouse.rs -o mmtest && ./mmtest
//! ```

pub const REPORT_ID_TOUCH: u8 = 0x12;
/// The same multi-touch layout as `0x12`, but used over Bluetooth. This is the
/// report the Magic Mouse 2 sends on the BTHENUM path.
pub const REPORT_ID_TOUCH_BT: u8 = 0x27;
pub const REPORT_ID_BATTERY: u8 = 0x90;

pub const MAX_TOUCHES: usize = 15;
pub const TOUCH_RECORD_LEN: usize = 8;
/// Bytes before the first touch record (report id, buttons, cursor, timestamp).
pub const TOUCH_DATA_OFFSET: usize = 14;

pub const TOUCH_STATE_MASK: u8 = 0xf0;
pub const TOUCH_STATE_NONE: u8 = 0x00;
pub const TOUCH_STATE_START: u8 = 0x30;
pub const TOUCH_STATE_DRAG: u8 = 0x40;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Touch {
    /// Tracking id, 0..=15. Not the index in the report.
    pub id: u8,
    pub x: i16,
    pub y: i16,
    /// Contact size; values below 8 are considered "not a real touch".
    pub size: u8,
    pub orientation: i8,
    /// Raw state byte (`tdata[7] & 0xf0`).
    pub state: u8,
}

impl Touch {
    pub const ZERO: Touch = Touch {
        id: 0,
        x: 0,
        y: 0,
        size: 0,
        orientation: 0,
        state: 0,
    };

    /// `true` when the finger is on the surface.
    pub fn is_down(&self) -> bool {
        self.state != TOUCH_STATE_NONE
    }

    /// `true` for the first report of a new contact.
    pub fn is_start(&self) -> bool {
        self.state == TOUCH_STATE_START
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TouchFrame {
    /// Bit 0 = left, bit 1 = right (as reported by the mouse itself).
    pub clicks: u8,
    /// Cursor movement carried by the same report.
    pub cursor_x: i16,
    pub cursor_y: i16,
    pub count: u8,
    pub touches: [Touch; MAX_TOUCHES],
}

impl TouchFrame {
    pub const EMPTY: TouchFrame = TouchFrame {
        clicks: 0,
        cursor_x: 0,
        cursor_y: 0,
        count: 0,
        touches: [Touch::ZERO; MAX_TOUCHES],
    };

    pub fn left_click(&self) -> bool {
        self.clicks & 0x01 != 0
    }

    pub fn right_click(&self) -> bool {
        self.clicks & 0x02 != 0
    }

    /// Touches that actually have a finger on the surface.
    pub fn active(&self) -> impl Iterator<Item = &Touch> {
        self.touches[..self.count as usize]
            .iter()
            .filter(|touch| touch.is_down())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Parsed {
    Touch(TouchFrame),
    Battery { percent: u8 },
    Unknown,
}

pub fn is_touch_report(report: &[u8]) -> bool {
    matches!(
        report.first(),
        Some(&REPORT_ID_TOUCH) | Some(&REPORT_ID_TOUCH_BT)
    )
}

pub fn is_battery_report(report: &[u8]) -> bool {
    report.first() == Some(&REPORT_ID_BATTERY)
}

/// Decode one input report that already starts with its report id.
pub fn parse(report: &[u8]) -> Parsed {
    match report.first() {
        Some(&REPORT_ID_TOUCH) | Some(&REPORT_ID_TOUCH_BT) => parse_touch(report),
        Some(&REPORT_ID_BATTERY) => parse_battery(report),
        _ => Parsed::Unknown,
    }
}

pub fn parse_battery(report: &[u8]) -> Parsed {
    // Battery strength is exposed as an ordinary input report:
    //   [0] = 0x90, [1] = flags, [2] = percentage.
    if report.len() >= 3 {
        let percent = report[2];
        if (1..=100).contains(&percent) {
            return Parsed::Battery { percent };
        }
    }
    Parsed::Unknown
}

pub fn parse_touch(report: &[u8]) -> Parsed {
    let len = report.len();

    // Either the short form (buttons + cursor only) or
    // 14 + 8 * N for N touch records.
    if len != TOUCH_RECORD_LEN
        && (len < TOUCH_DATA_OFFSET || (len - TOUCH_DATA_OFFSET) % TOUCH_RECORD_LEN != 0)
    {
        return Parsed::Unknown;
    }

    let count = if len >= TOUCH_DATA_OFFSET {
        (len - TOUCH_DATA_OFFSET) / TOUCH_RECORD_LEN
    } else {
        0
    };
    if count > MAX_TOUCHES {
        return Parsed::Unknown;
    }

    let mut frame = TouchFrame::EMPTY;
    frame.clicks = report[1];
    frame.cursor_x = sign_extend_16((report[3] as i32) << 8 | report[2] as i32);
    frame.cursor_y = sign_extend_16((report[5] as i32) << 8 | report[4] as i32);
    frame.count = count as u8;

    for index in 0..count {
        let offset = TOUCH_DATA_OFFSET + index * TOUCH_RECORD_LEN;
        let record = &report[offset..offset + TOUCH_RECORD_LEN];

        // Same expressions as hid-magicmouse.c, but written on u32/i32 so that
        // the shifts are well-defined in Rust.
        let id = (((record[6] as u32) << 2) | ((record[5] as u32) >> 6)) & 0x0f;
        let x = (((record[1] as i32) << 28) | ((record[0] as i32) << 20)) >> 20;
        let y = -((((record[2] as i32) << 24) | ((record[1] as i32) << 16)) >> 20);
        let size = record[5] & 0x3f;
        let orientation = ((record[6] >> 2) as i32 - 32) as i8;
        let state = record[7] & TOUCH_STATE_MASK;

        frame.touches[index] = Touch {
            id: id as u8,
            x: clamp_i16(x),
            y: clamp_i16(y),
            size,
            orientation,
            state,
        };
    }

    Parsed::Touch(frame)
}

fn sign_extend_16(value: i32) -> i16 {
    clamp_i16(value as i16 as i32)
}

fn clamp_i16(value: i32) -> i16 {
    if value > i16::MAX as i32 {
        i16::MAX
    } else if value < i16::MIN as i32 {
        i16::MIN
    } else {
        value as i16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a well-formed touch report for a single contact.
    fn synth_touch(t0: u8, t1: u8, t2: u8, t5: u8, t6: u8, t7: u8, clicks: u8) -> [u8; 22] {
        let mut report = [0u8; 22];
        report[0] = REPORT_ID_TOUCH;
        report[1] = clicks;
        report[2] = 0x10; // cursor x = 0x0010
        report[3] = 0x00;
        report[4] = 0x20; // cursor y = 0x0020
        report[5] = 0x00;
        report[14] = t0;
        report[15] = t1;
        report[16] = t2;
        report[19] = t5;
        report[20] = t6;
        report[21] = t7;
        report
    }

    #[test]
    fn decodes_position_id_size_and_state() {
        // t0 = 100, t1 = 0  => x = 100
        // t2 = 13,  t1 = 0  => y = -208
        // t5 = 10           => size = 10
        // t6 = 1            => id = (1 << 2) & 0xf = 4, orientation = -32
        // t7 = 0x40         => drag
        let report = synth_touch(100, 0, 13, 10, 1, 0x40, 0x02);
        let Parsed::Touch(frame) = parse(&report) else {
            panic!("expected touch frame");
        };

        assert_eq!(frame.count, 1);
        assert_eq!(frame.cursor_x, 0x0010);
        assert_eq!(frame.cursor_y, 0x0020);
        assert!(frame.right_click());
        assert!(!frame.left_click());

        let touch = frame.touches[0];
        assert_eq!(touch.x, 100);
        assert_eq!(touch.y, -208);
        assert_eq!(touch.size, 10);
        assert_eq!(touch.id, 4);
        assert_eq!(touch.orientation, -32);
        assert_eq!(touch.state, TOUCH_STATE_DRAG);
        assert!(touch.is_down());
    }

    #[test]
    fn negative_x_is_sign_extended() {
        // t0 = 0x00, t1 = 0x08 => 12 bit value 0x800 => -2048
        let report = synth_touch(0, 0x08, 0, 1, 0, 0x30, 0);
        let Parsed::Touch(frame) = parse(&report) else {
            panic!("expected touch frame");
        };
        assert_eq!(frame.touches[0].x, -2048);
        assert!(frame.touches[0].is_start());
    }

    #[test]
    fn short_report_has_buttons_but_no_touches() {
        let mut report = [0u8; 8];
        report[0] = REPORT_ID_TOUCH;
        report[1] = 0x01;
        let Parsed::Touch(frame) = parse(&report) else {
            panic!("expected touch frame");
        };
        assert_eq!(frame.count, 0);
        assert!(frame.left_click());
        assert_eq!(frame.active().count(), 0);
    }

    #[test]
    fn bluetooth_touch_report_uses_the_same_layout() {
        let mut report = synth_touch(100, 0, 13, 10, 1, 0x40, 0x02);
        report[0] = REPORT_ID_TOUCH_BT;

        assert!(is_touch_report(&report));
        let Parsed::Touch(frame) = parse(&report) else {
            panic!("expected touch frame");
        };
        assert_eq!(frame.count, 1);
        assert_eq!(frame.touches[0].x, 100);
        assert_eq!(frame.touches[0].y, -208);
        assert!(frame.touches[0].is_down());
        assert!(frame.right_click());
    }

    #[test]
    fn rejects_malformed_lengths() {
        assert_eq!(parse(&[0x12, 0, 0]), Parsed::Unknown);
        assert_eq!(parse(&[0x12; 15]), Parsed::Unknown);
        assert_eq!(parse(&[0x27; 15]), Parsed::Unknown);
        assert_eq!(parse(&[]), Parsed::Unknown);
    }

    #[test]
    fn battery_needs_a_plausible_percentage() {
        assert_eq!(parse(&[0x90, 0x00, 92]), Parsed::Battery { percent: 92 });
        assert_eq!(parse(&[0x90, 0x00, 0]), Parsed::Unknown);
        assert_eq!(parse(&[0x90, 0x00, 101]), Parsed::Unknown);
        assert_eq!(parse(&[0x90, 0x00]), Parsed::Unknown);
    }
}
