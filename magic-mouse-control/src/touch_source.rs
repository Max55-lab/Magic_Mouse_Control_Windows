// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! The single input abstraction used by the gesture engine.
//!
//! The application can receive the same Magic Mouse reports from two very
//! different places:
//!
//! * `mmk.sys` (a lower filter on the BTHENUM devnode) exposes raw HIDP frames
//!   through its ring buffer. This is the only path that sees the `0x12` touch
//!   reports over Bluetooth while Windows keeps the native mouse collection.
//! * hidapi sees the normal HID collections. That works for USB and, on some
//!   systems, for Bluetooth.
//!
//! `TouchSource` keeps that choice out of the daemon and the gesture engine.
//! A source only has to do two things: return raw input reports (first byte =
//! report ID) and, when it can, a fresh battery percentage. Decoding stays in
//! `hid_daemon` so every source produces exactly the same `TouchReport`.
//!
//! This module is compiled and tested, but it is not wired into `hid_daemon`
//! yet. Wiring means replacing `Connection`/`read_timeout` in the daemon with
//! a `Box<dyn TouchSource>` created once per session; the rest of the daemon
//! (parsers, simulator, event channel) does not change.

use std::time::Duration;

use hidapi::HidDevice;

use crate::hid_daemon::{
    is_access_denied, parse_battery_input, read_feature_battery, request_input_battery, Transport,
};
use crate::kernel_tap::{Event, KernelTap};

/// Where the bytes came from. The UI uses this for a small status label, not
/// for behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Raw HIDP traffic captured by `mmk.sys`.
    KernelTap,
    /// A normal HID collection opened with hidapi.
    Hid,
    /// No usable device.
    None,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::KernelTap => "kernel tap",
            Self::Hid => "HID",
            Self::None => "unavailable",
        }
    }
}

/// Static capabilities of a source. Capabilities can change after a `poll`
/// (for example when Windows withdraws raw HID access), so read this again
/// instead of caching it forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceStatus {
    pub kind: SourceKind,
    pub transport: Option<Transport>,
    pub touch_capable: bool,
    /// Windows refused raw input reads. Battery access can still work.
    pub input_blocked: bool,
}

/// One unit of work produced by a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceEvent {
    /// One HID input report, first byte is the report ID (`0x12`, `0x27`,
    /// `0x29`, `0xF7`, ...). The consumer runs `parse_touch_reports` on it.
    Report(Vec<u8>),
    /// A decoded battery percentage (`0x90`, byte 2).
    Battery(u8),
    /// Windows withdrew raw input access; the daemon should switch to its
    /// limited session instead of tearing the device down.
    AccessDenied,
    /// A non-fatal read error. The consumer decides whether to retry.
    Error(String),
}

/// Common interface for every input path.
///
/// `poll` blocks for at most `timeout` and returns every event that is already
/// available. `Duration::ZERO` means "do not block". Implementations must
/// never panic on a missing device or a malformed report.
pub trait TouchSource {
    fn status(&self) -> SourceStatus;

    fn poll(&mut self, timeout: Duration) -> Vec<SourceEvent>;

    /// Ask the device for a fresh battery reading. Best effort: the kernel tap
    /// cannot actively request one, so it returns `None`.
    fn request_battery(&mut self) -> Option<u8> {
        None
    }

    /// Cheap liveness check used by the watchdog. HID handles cannot be
    /// pinged, so the HID source is considered alive until it reports an error.
    fn is_alive(&self) -> bool {
        true
    }
}

/// Raw reports captured by the optional `mmk.sys` lower filter.
///
/// The ring contains completed requests. For HIDP traffic the payload starts
/// with the transaction header (`0xA1` = DATA Input); bare reports are accepted
/// too because the ring is also fed from paths that have no HIDP header.
pub struct KernelTapSource {
    tap: KernelTap,
    touch_capable: bool,
}

impl KernelTapSource {
    /// Open the control device. Returns `None` when the driver is not
    /// installed or does not answer the ping - exactly the old behaviour of
    /// the app when the optional driver is absent.
    pub fn open() -> Option<Self> {
        let tap = KernelTap::try_open()?;
        if !tap.is_alive() {
            return None;
        }
        Some(Self {
            tap,
            touch_capable: true,
        })
    }
}

impl TouchSource for KernelTapSource {
    fn status(&self) -> SourceStatus {
        SourceStatus {
            kind: SourceKind::KernelTap,
            transport: Some(Transport::Bluetooth),
            touch_capable: self.touch_capable,
            input_blocked: false,
        }
    }

    fn poll(&mut self, _timeout: Duration) -> Vec<SourceEvent> {
        // The kernel ring is non-blocking by design: the daemon owns the
        // cadence, and blocking a kernel IOCTL would buy nothing.
        self.tap
            .read_events()
            .iter()
            .filter_map(decode_tap_event)
            .collect()
    }

    fn request_battery(&mut self) -> Option<u8> {
        // Battery reports are passive on this path: the tap observes `0x90`
        // when Apple's filter (or the firmware) sends one. There is no request
        // to send, so the next `poll` will deliver it as `Battery`.
        None
    }

    fn is_alive(&self) -> bool {
        self.tap.is_alive()
    }
}

/// A normal hidapi collection. `battery` is the optional vendor collection
/// (`0x90` input report) discovered by the daemon.
pub struct HidTouchSource {
    primary: HidDevice,
    battery: Option<HidDevice>,
    battery_has_input: bool,
    transport: Transport,
    touch_capable: bool,
    input_blocked: bool,
}

impl HidTouchSource {
    pub fn new(
        primary: HidDevice,
        battery: Option<HidDevice>,
        battery_has_input: bool,
        transport: Transport,
        touch_capable: bool,
        input_blocked: bool,
    ) -> Self {
        Self {
            primary,
            battery,
            battery_has_input,
            transport,
            touch_capable,
            input_blocked,
        }
    }
}

impl TouchSource for HidTouchSource {
    fn status(&self) -> SourceStatus {
        SourceStatus {
            kind: SourceKind::Hid,
            transport: Some(self.transport),
            touch_capable: self.touch_capable,
            input_blocked: self.input_blocked,
        }
    }

    fn poll(&mut self, timeout: Duration) -> Vec<SourceEvent> {
        let mut events = Vec::new();
        let mut buffer = [0u8; 512];

        if !self.input_blocked {
            match self.primary.read_timeout(&mut buffer, timeout_ms(timeout)) {
                Ok(0) => {}
                Ok(length) => push_hid_bytes(&mut events, &buffer[..length]),
                Err(error) if is_access_denied(&error) => {
                    // Bluetooth mice are reserved for the Windows input stack.
                    // Keep the handle (battery may still work) but stop reading.
                    self.input_blocked = true;
                    self.touch_capable = false;
                    events.push(SourceEvent::AccessDenied);
                }
                Err(error) => events.push(SourceEvent::Error(error.to_string())),
            }
        }

        // Battery reports arrive on their own schedule; never wait for them.
        if self.battery_has_input {
            if let Some(battery) = self.battery.as_ref() {
                match battery.read_timeout(&mut buffer, 0) {
                    Ok(length) if length > 0 => push_hid_bytes(&mut events, &buffer[..length]),
                    Ok(_) => {}
                    Err(error) => events.push(SourceEvent::Error(error.to_string())),
                }
            }
        }

        events
    }

    fn request_battery(&mut self) -> Option<u8> {
        // Same order as the existing daemon: the vendor input report works on
        // Bluetooth and USB; the feature report is a USB-only fallback.
        if let Some(battery) = self.battery.as_ref() {
            if let Some(percent) = request_input_battery(battery) {
                return Some(percent);
            }
        }
        if self.transport != Transport::Usb {
            return None;
        }
        if let Some(battery) = self.battery.as_ref() {
            if let Some(percent) = read_feature_battery(battery) {
                return Some(percent);
            }
        }
        read_feature_battery(&self.primary)
    }
}

/// Fallback when neither path is available. It keeps the daemon loop uniform
/// instead of special-casing "no source" everywhere.
pub struct NullSource;

impl TouchSource for NullSource {
    fn status(&self) -> SourceStatus {
        SourceStatus {
            kind: SourceKind::None,
            transport: None,
            touch_capable: false,
            input_blocked: true,
        }
    }

    fn poll(&mut self, _timeout: Duration) -> Vec<SourceEvent> {
        Vec::new()
    }
}

/// Pick the first source that works. The kernel tap is opt-in: `prefer_kernel`
/// is true only when the user installed the experimental driver.
///
/// The order is deliberately boring:
/// 1. `KernelTapSource` when requested and alive,
/// 2. the hidapi source supplied by the caller,
/// 3. `NullSource`.
pub fn select_source<F>(prefer_kernel: bool, hid: F) -> Box<dyn TouchSource>
where
    F: FnOnce() -> Option<HidTouchSource>,
{
    if prefer_kernel {
        if let Some(source) = KernelTapSource::open() {
            return Box::new(source);
        }
    }
    match hid() {
        Some(source) => Box::new(source),
        None => Box::new(NullSource),
    }
}

/// Decode one ring record. Only completed, successful requests carry data;
/// dispatch records have no buffer and are ignored.
fn decode_tap_event(event: &Event) -> Option<SourceEvent> {
    if event.status < 0 {
        return None;
    }
    let bytes = KernelTap::touch_report_bytes(event)?;
    classify_bytes(bytes)
}

fn classify_bytes(bytes: &[u8]) -> Option<SourceEvent> {
    // The tap sees HIDP DATA(Input) with the 0xA1 transaction header in front
    // of the report. Some records are bare reports; accept both.
    let payload = match bytes.split_first() {
        Some((0xA1, rest)) if !rest.is_empty() => rest,
        _ => bytes,
    };
    let report_id = *payload.first()?;

    if report_id == 0x90 {
        let percent = *payload.get(2)?;
        return (1..=100)
            .contains(&percent)
            .then_some(SourceEvent::Battery(percent));
    }

    match report_id {
        0x12 | 0x27 | 0x29 | 0xF7 => Some(SourceEvent::Report(payload.to_vec())),
        _ => None,
    }
}

fn push_hid_bytes(events: &mut Vec<SourceEvent>, bytes: &[u8]) {
    if let Some(percent) = parse_battery_input(bytes) {
        events.push(SourceEvent::Battery(percent));
    } else {
        events.push(SourceEvent::Report(bytes.to_vec()));
    }
}

fn timeout_ms(timeout: Duration) -> i32 {
    timeout.as_millis().min(i32::MAX as u128) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_tap::{KIND_INTERNAL_IOCTL, KIND_READ};

    fn tap_event(status: i32, kind: u8, bytes: &[u8]) -> Event {
        let mut event = Event {
            status,
            kind,
            len: bytes.len() as u16,
            ..Event::default()
        };
        event.data[..bytes.len()].copy_from_slice(bytes);
        event.first = bytes.first().copied().unwrap_or(0);
        event
    }

    #[test]
    fn strips_the_hidp_data_input_header() {
        let event = tap_event(
            0,
            KIND_INTERNAL_IOCTL,
            &[0xA1, 0x12, 0x00, 0x01, 0x02, 0x03],
        );
        assert_eq!(
            decode_tap_event(&event),
            Some(SourceEvent::Report(vec![0x12, 0x00, 0x01, 0x02, 0x03]))
        );
    }

    #[test]
    fn accepts_a_bare_touch_report() {
        let event = tap_event(0, KIND_READ, &[0x12, 0x01]);
        assert_eq!(
            decode_tap_event(&event),
            Some(SourceEvent::Report(vec![0x12, 0x01]))
        );
    }

    #[test]
    fn decodes_battery_from_the_tap() {
        let event = tap_event(0, KIND_INTERNAL_IOCTL, &[0xA1, 0x90, 0x00, 92]);
        assert_eq!(decode_tap_event(&event), Some(SourceEvent::Battery(92)));

        let invalid = tap_event(0, KIND_INTERNAL_IOCTL, &[0x90, 0x00, 0]);
        assert_eq!(decode_tap_event(&invalid), None);
    }

    #[test]
    fn ignores_failed_requests_and_unrelated_ioctls() {
        let failed = tap_event(-1, KIND_INTERNAL_IOCTL, &[0xA1, 0x12, 0x01]);
        assert_eq!(decode_tap_event(&failed), None);

        let feature = tap_event(0, KIND_INTERNAL_IOCTL, &[0x53, 0xD7, 0x01]);
        assert_eq!(decode_tap_event(&feature), None);
    }

    #[test]
    fn hid_reports_are_classified_without_parsing_touch() {
        let mut events = Vec::new();
        push_hid_bytes(&mut events, &[0x90, 0x00, 50]);
        push_hid_bytes(&mut events, &[0x12, 0x01, 0x02]);
        assert_eq!(
            events,
            vec![
                SourceEvent::Battery(50),
                SourceEvent::Report(vec![0x12, 0x01, 0x02]),
            ]
        );
    }

    #[test]
    fn null_source_never_produces_events() {
        let mut source = select_source(false, || None);
        assert_eq!(source.status().kind, SourceKind::None);
        assert!(source.poll(Duration::ZERO).is_empty());
        assert!(source.is_alive());
        assert_eq!(source.request_battery(), None);
    }

    #[test]
    fn timeout_is_clamped_to_i32() {
        assert_eq!(timeout_ms(Duration::ZERO), 0);
        assert_eq!(timeout_ms(Duration::from_millis(18)), 18);
        assert_eq!(timeout_ms(Duration::MAX), i32::MAX);
    }

    #[test]
    fn source_labels_are_stable() {
        assert_eq!(SourceKind::KernelTap.label(), "kernel tap");
        assert_eq!(SourceKind::Hid.label(), "HID");
        assert_eq!(SourceKind::None.label(), "unavailable");
    }
}
