// Copyright (c) Magic Mouse Control contributors.
// License: MIT

//! Single input abstraction used by the HID daemon and gesture engine.
//!
//! # The problem
//!
//! Magic Mouse reports can arrive from two very different places depending on
//! how the mouse is connected and whether the optional kernel driver is loaded:
//!
//! * **`mmk.sys` (KernelTap)** — a lower filter driver on the BTHENUM device
//!   node.  It sits below `applewirelessmouse.sys` in the device stack and
//!   copies every completed HID request (including `0x12` multitouch reports)
//!   into a ring buffer.  User-mode reads the ring via IOCTL.  This is the
//!   *only* path that sees touch reports over Bluetooth, because Windows
//!   reserves the native Bluetooth mouse collection for the system input stack.
//!
//! * **hidapi (HidTouchSource)** — the standard HID collection opened with
//!   `hidapi::HidDevice`.  Works for USB connections and, in some cases, for
//!   Bluetooth when Windows allows raw reads (rare in practice, hence
//!   `experimental_input` gating).
//!
//! # The solution
//!
//! `TouchSource` is a trait that both paths implement.  The daemon holds a
//! `Box<dyn TouchSource>` and calls `poll` on every tick.  Switching sources
//! (e.g. kernel tap becomes available mid-session) is a one-line swap of the
//! boxed value.
//!
//! Decoding stays in `hid_daemon` so every source produces exactly the same
//! `TouchReport` regardless of which path delivered the bytes.

use std::time::Duration;

use hidapi::HidDevice;

use crate::hid_daemon::{
    is_access_denied, parse_battery_input, read_feature_battery, request_input_battery, Transport,
};
use crate::kernel_tap::{Event, KernelTap};

// ---------------------------------------------------------------------------
// Source metadata
// ---------------------------------------------------------------------------

/// Which physical or virtual path produced the input data.
/// The UI uses this for a small status label; it never changes behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Raw HIDP traffic captured by `mmk.sys`.
    KernelTap,
    /// A normal HID collection opened with hidapi.
    Hid,
}

impl SourceKind {
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Self::KernelTap => "kernel tap",
            Self::Hid => "HID",
        }
    }
}

/// Point-in-time capability snapshot of a source.
///
/// Capabilities can change between polls (e.g. Windows can withdraw raw HID
/// access on a live Bluetooth handle), so the daemon reads this after each
/// poll rather than caching it forever.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceStatus {
    pub kind: SourceKind,
    pub transport: Option<Transport>,
    /// True when the source is expected to deliver multitouch (`0x12`) reports.
    pub touch_capable: bool,
    /// True when Windows refused raw input reads.
    /// Battery access can still work through feature reports.
    pub input_blocked: bool,
}

// ---------------------------------------------------------------------------
// SourceEvent — the output of a poll call
// ---------------------------------------------------------------------------

/// One unit of work produced by a [`TouchSource::poll`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceEvent {
    /// One raw HID input report.  The first byte is the report ID:
    /// - `0x12` / `0x27` / `0x29` — multitouch (decoded by `parse_touch_reports`)
    /// - `0xF7` — double-report wrapper (also decoded by `parse_touch_reports`)
    /// - `0x90` — battery (extracted by `parse_battery_input` before it reaches here)
    Report(Vec<u8>),
    /// A decoded battery percentage extracted from a `0x90` report.
    /// Range: 1–100 (0 and >100 are rejected upstream).
    Battery(u8),
    /// Windows withdrew raw input access on a live handle (error 5, Access Denied).
    /// The daemon should switch to `limited_session` instead of tearing down.
    AccessDenied,
    /// A non-fatal read error.  The daemon decides whether to retry or disconnect.
    Error(String),
}

// ---------------------------------------------------------------------------
// TouchSource trait
// ---------------------------------------------------------------------------

/// Common interface for every input path.
///
/// # Contract
///
/// * `poll` blocks for at most `timeout` and returns all events that are
///   already ready.  `Duration::ZERO` means "do not block at all".
/// * Implementations must **never panic** on a missing device or malformed data.
/// * `request_battery` is best-effort: return `None` rather than an error.
/// * `is_alive` is a cheap liveness check that must not block.
pub trait TouchSource {
    /// Current capability snapshot.  Called after each `poll`.
    fn status(&self) -> SourceStatus;

    /// Block for at most `timeout` and return all available events.
    fn poll(&mut self, timeout: Duration) -> Vec<SourceEvent>;

    /// Ask the device for a fresh battery reading.  Best effort: the kernel
    /// tap cannot actively request one (it only observes passive `0x90`
    /// reports), so it always returns `None`.
    fn request_battery(&mut self) -> Option<u8> {
        None
    }

    /// Cheap liveness check used by the watchdog.  HID handles cannot be
    /// pinged, so `HidTouchSource` is considered alive until it reports an
    /// error.  `KernelTapSource` delegates to `KernelTap::is_alive`.
    #[allow(dead_code)]
    fn is_alive(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// KernelTapSource — wraps KernelTap for the TouchSource interface
// ---------------------------------------------------------------------------

/// Input source backed by the optional `mmk.sys` lower filter driver.
///
/// The ring buffer is non-blocking by design: the daemon controls the polling
/// cadence (100 Hz), and blocking a kernel IOCTL would gain nothing here.
/// Each `poll` call drains the entire ring into `events_cache` (a reusable
/// heap buffer) and converts each record to a `SourceEvent`.
pub struct KernelTapSource {
    tap: KernelTap,
    touch_capable: bool,
    /// Reusable buffer for `KernelTap::read_events_into`.
    /// Pre-allocated once and reused every poll to avoid per-tick allocations.
    events_cache: Vec<Event>,
}

impl KernelTapSource {
    /// Open the control device and verify it is alive.
    ///
    /// Returns `None` when the driver is not installed, the device is not
    /// present, or the ping IOCTL fails — same semantics as `KernelTap::try_open`.
    pub fn open() -> Option<Self> {
        let tap = KernelTap::try_open()?;
        // A ping confirms the driver is not just registered but actually
        // responding.  A stale handle from a previous driver load would fail here.
        if !tap.is_alive() {
            return None;
        }
        Some(Self {
            tap,
            touch_capable: true,
            events_cache: Vec::with_capacity(64),
        })
    }
}

impl TouchSource for KernelTapSource {
    fn status(&self) -> SourceStatus {
        SourceStatus {
            kind: SourceKind::KernelTap,
            transport: Some(Transport::Bluetooth),
            touch_capable: self.touch_capable,
            input_blocked: false, // kernel tap is never "access denied"
        }
    }

    fn poll(&mut self, _timeout: Duration) -> Vec<SourceEvent> {
        // The ring is non-blocking: drain whatever is there right now.
        // The `_timeout` parameter is ignored; the daemon's tick rate is the
        // effective poll interval.
        self.tap.read_events_into(&mut self.events_cache);
        self.events_cache
            .iter()
            .filter_map(decode_tap_event)
            .collect()
    }

    fn request_battery(&mut self) -> Option<u8> {
        // Battery reporting is *passive* on this path: the kernel tap observes
        // `0x90` reports when Apple's filter or the firmware sends one.  There
        // is no request to send; the next `poll` will deliver it as `Battery`.
        None
    }

    fn is_alive(&self) -> bool {
        self.tap.is_alive()
    }
}

// ---------------------------------------------------------------------------
// HidTouchSource — wraps a hidapi device for the TouchSource interface
// ---------------------------------------------------------------------------

/// Input source backed by a normal hidapi HID collection.
///
/// `primary` is the top-level collection chosen by `open_connection` (the one
/// with the highest `primary_score`).  `battery` is an optional separate
/// vendor collection (usage page `0xFF00`, report ID `0x90`) that the daemon
/// opens when it finds a better battery interface than the primary.
pub struct HidTouchSource {
    primary: HidDevice,
    battery: Option<HidDevice>,
    /// True when the battery collection advertises an *input* report (`0x90`).
    /// Some firmware versions only expose a *feature* report (`0x47`) instead.
    battery_has_input: bool,
    transport: Transport,
    touch_capable: bool,
    /// Set to true when Windows returns error 5 on `ReadFile`.
    /// The handle stays open for battery requests, but input reads stop.
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

        // --- Primary collection (touch + buttons) ---
        if !self.input_blocked {
            match self.primary.read_timeout(&mut buffer, timeout_ms(timeout)) {
                Ok(0) => {}
                Ok(length) => push_hid_bytes(&mut events, &buffer[..length]),
                Err(error) if is_access_denied(&error) => {
                    // Windows Bluetooth stack has revoked our read access.
                    // Keep the handle open (battery feature reports may still
                    // work) but stop issuing reads.
                    self.input_blocked = true;
                    self.touch_capable = false;
                    events.push(SourceEvent::AccessDenied);
                }
                Err(error) => events.push(SourceEvent::Error(error.to_string())),
            }
        }

        // --- Battery collection (never wait — it fires on its own schedule) ---
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
        // Order mirrors the existing daemon logic:
        // 1. Try the vendor input report (`0x90`) — works on Bluetooth and USB.
        // 2. Fall back to the feature report (`0x47`) — USB-only on most firmware.
        // 3. Try the feature report on the primary collection as a last resort.
        if let Some(battery) = self.battery.as_ref() {
            if let Some(percent) = request_input_battery(battery) {
                return Some(percent);
            }
        }
        if self.transport != Transport::Usb {
            // Feature reports require a USB connection on Apple firmware.
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

// ---------------------------------------------------------------------------
// Decoding helpers (private)
// ---------------------------------------------------------------------------

/// Convert one ring record to a `SourceEvent`, or discard it.
///
/// Records with a negative `status` (NTSTATUS error) are silently dropped —
/// they represent failed requests and carry no useful payload.
fn decode_tap_event(event: &Event) -> Option<SourceEvent> {
    if event.status < 0 {
        // Negative NTSTATUS = failed request; no payload is meaningful.
        return None;
    }
    let bytes = KernelTap::touch_report_bytes(event)?;
    classify_bytes(bytes)
}

/// Classify a raw byte slice into a `SourceEvent`.
///
/// The kernel tap sees requests at the HIDP layer, so the buffer often starts
/// with the HIDP DATA(Input) transaction header byte `0xA1`.  We strip it and
/// treat the remainder as a bare HID report.  Bare reports (no `0xA1` prefix)
/// are also accepted because some paths in the driver skip the HIDP layer.
///
/// Report IDs handled:
/// - `0x90` — battery: extract the percentage from byte 2 and return `Battery`.
/// - `0x12`, `0x27`, `0x29`, `0xF7` — touch/button reports: return `Report`.
/// - Everything else: ignored (feature report echoes, internal IOCTL ACKs, ...).
fn classify_bytes(bytes: &[u8]) -> Option<SourceEvent> {
    // Strip the HIDP DATA(Input) header byte if present.
    let payload = match bytes.split_first() {
        Some((0xA1, rest)) if !rest.is_empty() => rest,
        _ => bytes,
    };
    let report_id = *payload.first()?;

    if report_id == 0x90 {
        // Battery report layout: [0x90, reserved, percent, ...].
        // Reject 0 and values above 100 (firmware artefacts).
        let percent = *payload.get(2)?;
        return (1..=100)
            .contains(&percent)
            .then_some(SourceEvent::Battery(percent));
    }

    match report_id {
        // Known touch/button report IDs from Apple's HID descriptor.
        0x12 | 0x27 | 0x29 | 0xF7 => Some(SourceEvent::Report(payload.to_vec())),
        _ => None,
    }
}

/// Classify a byte slice from a hidapi read and append to `events`.
///
/// A `0x90` battery report is decoded immediately; everything else is passed
/// through as a raw `Report` for the touch parser downstream.
fn push_hid_bytes(events: &mut Vec<SourceEvent>, bytes: &[u8]) {
    if let Some(percent) = parse_battery_input(bytes) {
        events.push(SourceEvent::Battery(percent));
    } else {
        events.push(SourceEvent::Report(bytes.to_vec()));
    }
}

/// Clamp a `Duration` to an `i32` millisecond timeout for hidapi.
fn timeout_ms(timeout: Duration) -> i32 {
    timeout.as_millis().min(i32::MAX as u128) as i32
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel_tap::{KIND_INTERNAL_IOCTL, KIND_READ};

    /// Build a synthetic ring record for use in decode tests.
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

    /// The `0xA1` HIDP header must be stripped so the report ID is exposed.
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

    /// Bare reports (no HIDP header) must also be accepted.
    #[test]
    fn accepts_a_bare_touch_report() {
        let event = tap_event(0, KIND_READ, &[0x12, 0x01]);
        assert_eq!(
            decode_tap_event(&event),
            Some(SourceEvent::Report(vec![0x12, 0x01]))
        );
    }

    /// Report `0x27` is the multi-touch frame `mmk.sys` forwards over Bluetooth.
    #[test]
    fn kernel_tap_ingests_0x27_multitouch_frames() {
        let frame = [0xA1, 0x27, 0x01, 0x02, 0x03, 0x04];
        let event = tap_event(0, KIND_INTERNAL_IOCTL, &frame);
        assert_eq!(
            decode_tap_event(&event),
            Some(SourceEvent::Report(vec![0x27, 0x01, 0x02, 0x03, 0x04]))
        );
    }

    /// Battery reports arrive as `0x90` and byte 2 carries the percentage.
    #[test]
    fn decodes_battery_from_the_tap() {
        let event = tap_event(0, KIND_INTERNAL_IOCTL, &[0xA1, 0x90, 0x00, 92]);
        assert_eq!(decode_tap_event(&event), Some(SourceEvent::Battery(92)));

        // Percent == 0 must be rejected (firmware artefact).
        let invalid = tap_event(0, KIND_INTERNAL_IOCTL, &[0x90, 0x00, 0]);
        assert_eq!(decode_tap_event(&invalid), None);
    }

    /// Failed requests (negative NTSTATUS) and unrelated report IDs must be dropped.
    #[test]
    fn ignores_failed_requests_and_unrelated_ioctls() {
        let failed = tap_event(-1, KIND_INTERNAL_IOCTL, &[0xA1, 0x12, 0x01]);
        assert_eq!(decode_tap_event(&failed), None);

        // 0x53 is a feature report set command — not a touch report.
        let feature = tap_event(0, KIND_INTERNAL_IOCTL, &[0x53, 0xD7, 0x01]);
        assert_eq!(decode_tap_event(&feature), None);
    }

    /// Battery bytes from hidapi must become `Battery`, touch bytes `Report`.
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

    /// `timeout_ms` must clamp large durations to `i32::MAX`.
    #[test]
    fn timeout_is_clamped_to_i32() {
        assert_eq!(timeout_ms(Duration::ZERO), 0);
        assert_eq!(timeout_ms(Duration::from_millis(18)), 18);
        assert_eq!(timeout_ms(Duration::MAX), i32::MAX);
    }

    /// Source labels must be stable strings (the UI may show them verbatim).
    #[test]
    fn source_labels_are_stable() {
        assert_eq!(SourceKind::KernelTap.label(), "kernel tap");
        assert_eq!(SourceKind::Hid.label(), "HID");
    }
}
