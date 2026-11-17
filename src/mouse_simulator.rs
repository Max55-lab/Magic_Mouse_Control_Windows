//! Turns Magic Mouse touch reports into smooth wheel events, middle clicks
//! and two-finger back/forward gestures.
//!
//! Windows can already understand the mouse's built-in single-finger scrolling
//! and physical clicks. Adding a signed HID filter driver is out of scope for a
//! free portable app, so this module *adds* gestures on top of the native
//! behaviour. Smooth wheel deltas are sent as partial `WHEEL_DELTA` values,
//! which modern applications accumulate; the result feels continuous rather
//! than stepped. See the README for the known limitations.

use std::time::{Duration, Instant};

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY, VK_CONTROL,
};

use crate::config::Config;
use crate::hid_daemon::TouchReport;

const RAW_UNITS_PER_NOTCH: f32 = 224.0;
const WHEEL_DELTA_UNITS: f32 = 120.0;
const MAX_UNITS_PER_EVENT: f32 = 480.0;
const EMIT_INTERVAL: Duration = Duration::from_millis(8);
const AXIS_LOCK_THRESHOLD: f32 = 24.0;
const SWIPE_THRESHOLD: f32 = 270.0;
const MIDDLE_ZONE: i32 = 350;
const FIRM_TOUCH_SIZE: u8 = 8;
/// Finger-distance change before a two-finger gesture becomes a pinch.
const PINCH_START_DISTANCE: f32 = 18.0;
/// Raw finger-distance units that translate to one wheel notch.
const PINCH_UNITS_PER_NOTCH: f32 = 60.0;
/// `MOUSEINPUT.mouseData` values for the native Back/Forward buttons.
const XBUTTON_BACK: u32 = 0x0001;
const XBUTTON_FORWARD: u32 = 0x0002;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug, Default)]
struct AxisState {
    /// Pending wheel units. A full detent is 120 units.
    pending: f32,
    /// Inertia velocity in wheel units per second.
    velocity: f32,
}

impl AxisState {
    fn clear_inertia(&mut self) {
        self.velocity = 0.0;
    }
}

#[derive(Clone, Copy, Debug)]
struct Swipe {
    start_x: f32,
    start_y: f32,
    fired: bool,
    cancelled: bool,
}

#[derive(Clone, Copy, Debug)]
struct Pinch {
    start_distance: f32,
    last_distance: f32,
    active: bool,
}

pub struct MouseSimulator {
    config: Config,
    left_was_down: bool,
    right_was_down: bool,
    active_touches: usize,
    last_two_center: Option<(i32, i32)>,
    last_report_at: Option<Instant>,
    gesture_dx: f32,
    gesture_dy: f32,
    axis: Option<Axis>,
    vertical: AxisState,
    horizontal: AxisState,
    swipe: Option<Swipe>,
    pinch: Option<Pinch>,
    pinching: bool,
    zoom_pending: f32,
    last_scroll_motion: Option<Instant>,
    last_tick: Instant,
    last_emit: Instant,
}

impl MouseSimulator {
    pub fn new(config: Config) -> Self {
        let now = Instant::now();
        Self {
            config,
            left_was_down: false,
            right_was_down: false,
            active_touches: 0,
            last_two_center: None,
            last_report_at: None,
            gesture_dx: 0.0,
            gesture_dy: 0.0,
            axis: None,
            vertical: AxisState::default(),
            horizontal: AxisState::default(),
            swipe: None,
            pinch: None,
            pinching: false,
            zoom_pending: 0.0,
            last_scroll_motion: None,
            last_tick: now,
            last_emit: now - EMIT_INTERVAL,
        }
    }

    pub fn update_config(&mut self, config: Config) {
        if config.natural_scrolling != self.config.natural_scrolling || !config.smooth_scrolling {
            self.stop_inertia();
        }
        if !config.vertical_scroll {
            self.vertical = AxisState::default();
        }
        if !config.horizontal_scroll {
            self.horizontal = AxisState::default();
        }
        if !config.pinch_zoom {
            self.pinch = None;
            self.pinching = false;
            self.zoom_pending = 0.0;
        }
        self.config = config;
    }

    /// Feed one decoded touch report. This runs on the HID worker thread and
    /// may inject input into Windows.
    pub fn handle_report(&mut self, report: &TouchReport, now: Instant) {
        let delta_time = self
            .last_report_at
            .map(|last| now.saturating_duration_since(last).as_secs_f32())
            .unwrap_or(0.012)
            .clamp(0.002, 0.100);
        self.last_report_at = Some(now);

        let count = report.points.len();
        let clicking = report.left || report.right;

        if report.left && !self.left_was_down {
            self.stop_inertia();
            if self.should_middle_click(report) {
                inject_middle_click();
            }
        }
        if report.right && !self.right_was_down {
            self.stop_inertia();
        }
        self.left_was_down = report.left;
        self.right_was_down = report.right;

        if count != self.active_touches {
            if self.active_touches == 2 && count == 0 {
                self.finish_scroll_gesture(now);
            } else {
                self.cancel_scroll_gesture();
            }
            self.swipe = None;
            self.pinch = None;
            self.pinching = false;
        }

        let pinch_active = self.update_pinch(report);

        if clicking {
            self.cancel_scroll_gesture();
            self.swipe = None;
        } else if !pinch_active {
            self.update_swipe(report, now);
        } else {
            self.swipe = None;
        }

        if !clicking && count == 2 && !pinch_active {
            let a = &report.points[0];
            let b = &report.points[1];
            let center = ((a.x + b.x) / 2, (a.y + b.y) / 2);
            if let Some((previous_x, previous_y)) = self.last_two_center {
                self.accumulate_scroll(
                    (center.0 - previous_x) as f32,
                    (center.1 - previous_y) as f32,
                    delta_time,
                    now,
                );
            }
            self.last_two_center = Some(center);
        } else {
            self.last_two_center = None;
            self.gesture_dx = 0.0;
            self.gesture_dy = 0.0;
        }

        self.active_touches = count;
    }

    /// Emit accumulated wheel units and apply decaying inertia. Called from
    /// the HID loop even when no report arrived, which is what keeps inertia
    /// moving after the finger has lifted.
    pub fn tick(&mut self, now: Instant) {
        let dt = now
            .saturating_duration_since(self.last_tick)
            .as_secs_f32()
            .clamp(0.0, 0.050);
        self.last_tick = now;

        if self.axis.is_none()
            && self.config.smooth_scrolling
            && self.config.inertia
            && self.active_touches == 0
        {
            let friction = (-5.2 * dt).exp();
            self.vertical.pending += self.vertical.velocity * dt;
            self.horizontal.pending += self.horizontal.velocity * dt;
            self.vertical.velocity *= friction;
            self.horizontal.velocity *= friction;
            if self.vertical.velocity.abs() < 35.0 {
                self.vertical.velocity = 0.0;
            }
            if self.horizontal.velocity.abs() < 35.0 {
                self.horizontal.velocity = 0.0;
            }
        }

        if now.saturating_duration_since(self.last_emit) < EMIT_INTERVAL {
            return;
        }

        let vertical = if self.config.vertical_scroll {
            take_units(&mut self.vertical.pending)
        } else {
            0
        };
        let horizontal = if self.config.horizontal_scroll {
            take_units(&mut self.horizontal.pending)
        } else {
            0
        };
        if vertical != 0 || horizontal != 0 {
            inject_wheel(vertical, horizontal);
            self.last_emit = now;
        }
        if self.config.pinch_zoom {
            let zoom = take_units(&mut self.zoom_pending);
            if zoom != 0 {
                inject_zoom(zoom);
                self.last_emit = now;
            }
        }
    }

    fn accumulate_scroll(&mut self, dx: f32, dy: f32, dt: f32, now: Instant) {
        if !self.config.vertical_scroll && !self.config.horizontal_scroll {
            return;
        }
        self.gesture_dx += dx;
        self.gesture_dy += dy;

        if self.axis.is_none() {
            if self.gesture_dx.abs().max(self.gesture_dy.abs()) < AXIS_LOCK_THRESHOLD {
                return;
            }
            let axis = match (self.config.vertical_scroll, self.config.horizontal_scroll) {
                (true, false) => Axis::Vertical,
                (false, true) => Axis::Horizontal,
                _ => {
                    if self.gesture_dy.abs() >= self.gesture_dx.abs() {
                        Axis::Vertical
                    } else {
                        Axis::Horizontal
                    }
                }
            };
            self.axis = Some(axis);
            let (dx, dy) = (self.gesture_dx, self.gesture_dy);
            self.apply_axis_motion(axis, dx, dy, dt, now);
            self.gesture_dx = 0.0;
            self.gesture_dy = 0.0;
            return;
        }

        if let Some(axis) = self.axis {
            self.apply_axis_motion(axis, dx, dy, dt, now);
        }
    }

    fn apply_axis_motion(&mut self, axis: Axis, dx: f32, dy: f32, dt: f32, now: Instant) {
        let enabled = match axis {
            Axis::Vertical => self.config.vertical_scroll,
            Axis::Horizontal => self.config.horizontal_scroll,
        };
        if !enabled {
            return;
        }
        let scale = WHEEL_DELTA_UNITS / RAW_UNITS_PER_NOTCH * self.config.scroll_speed;
        let direction = if self.config.natural_scrolling {
            -1.0
        } else {
            1.0
        };

        // The Y axis of the device points the other way round from wheel
        // "forward", which is why vertical movement is negated.
        let (units, axis_state) = match axis {
            Axis::Vertical => (-dy * scale * direction, &mut self.vertical),
            Axis::Horizontal => (dx * scale * direction, &mut self.horizontal),
        };

        axis_state.pending += units;
        let instant_velocity = units / dt.max(0.002);
        axis_state.velocity =
            (axis_state.velocity * 0.55 + instant_velocity * 0.45).clamp(-4_000.0, 4_000.0);
        // The other axis is parked for the rest of this gesture.
        match axis {
            Axis::Vertical => self.horizontal.velocity = 0.0,
            Axis::Horizontal => self.vertical.velocity = 0.0,
        }
        self.last_scroll_motion = Some(now);
    }

    fn update_swipe(&mut self, report: &TouchReport, now: Instant) {
        if !self.config.two_finger_swipe || report.points.len() != 2 {
            self.swipe = None;
            return;
        }

        let (sum_x, sum_y) = report
            .points
            .iter()
            .fold((0.0f32, 0.0f32), |(x, y), point| {
                (x + point.x as f32, y + point.y as f32)
            });
        let x = sum_x * 0.5;
        let y = sum_y * 0.5;
        let swipe = self.swipe.get_or_insert(Swipe {
            start_x: x,
            start_y: y,
            fired: false,
            cancelled: false,
        });
        if swipe.fired || swipe.cancelled {
            return;
        }
        let dx = x - swipe.start_x;
        let dy = y - swipe.start_y;
        if dx.abs() > dy.abs() * 1.6 && dx.abs() > SWIPE_THRESHOLD {
            swipe.fired = true;
            let back = swipe_navigates_back(dx);
            inject_navigation(back);
            crate::logging::info(&format!(
                "Two-finger swipe detected ({:.0} raw units, {}).",
                dx,
                if back { "back" } else { "forward" }
            ));
        } else if dy.abs() > SWIPE_THRESHOLD * 1.25 {
            swipe.cancelled = true;
        }
        let _ = now;
    }

    /// Detect a two-finger pinch before it becomes a scroll or swipe.
    /// Returns true while the current gesture is a zoom.
    fn update_pinch(&mut self, report: &TouchReport) -> bool {
        if !self.config.pinch_zoom || report.points.len() != 2 {
            self.pinch = None;
            self.pinching = false;
            return false;
        }
        let a = &report.points[0];
        let b = &report.points[1];
        let dx = (a.x - b.x) as f32;
        let dy = (a.y - b.y) as f32;
        let distance = (dx * dx + dy * dy).sqrt();

        if self.pinch.is_none() {
            self.pinch = Some(Pinch {
                start_distance: distance,
                last_distance: distance,
                active: false,
            });
        }

        let mut just_activated = false;
        {
            let pinch = self.pinch.as_mut().expect("pinch was just initialised");
            if pinch.active {
                let delta = distance - pinch.last_distance;
                pinch.last_distance = distance;
                let units = delta
                    * (WHEEL_DELTA_UNITS / PINCH_UNITS_PER_NOTCH)
                    * self.config.pinch_sensitivity;
                self.zoom_pending += units;
                return true;
            }
            if (distance - pinch.start_distance).abs() >= PINCH_START_DISTANCE {
                pinch.active = true;
                pinch.last_distance = distance;
                just_activated = true;
            }
        }

        if just_activated {
            // Fingers are spreading or closing: this gesture is not scrolling.
            self.pinching = true;
            self.cancel_scroll_gesture();
        }
        self.pinching
    }

    fn finish_scroll_gesture(&mut self, now: Instant) {
        let recent_motion = self
            .last_scroll_motion
            .map(|last| now.saturating_duration_since(last) < Duration::from_millis(140))
            .unwrap_or(false);
        let keep_inertia = self.config.smooth_scrolling
            && self.config.inertia
            && self.axis.is_some()
            && recent_motion;
        if keep_inertia {
            match self.axis {
                Some(Axis::Vertical) => {
                    self.horizontal.clear_inertia();
                }
                Some(Axis::Horizontal) => {
                    self.vertical.clear_inertia();
                }
                None => {}
            }
        } else {
            self.stop_inertia();
        }
        self.axis = None;
        self.gesture_dx = 0.0;
        self.gesture_dy = 0.0;
    }

    fn cancel_scroll_gesture(&mut self) {
        self.stop_inertia();
        self.axis = None;
        self.gesture_dx = 0.0;
        self.gesture_dy = 0.0;
    }

    fn stop_inertia(&mut self) {
        self.vertical.clear_inertia();
        self.horizontal.clear_inertia();
    }

    fn should_middle_click(&self, report: &TouchReport) -> bool {
        if !self.config.middle_click || report.points.len() != 1 {
            return false;
        }
        let point = &report.points[0];
        point.size >= FIRM_TOUCH_SIZE && point.x.abs() <= MIDDLE_ZONE
    }
}

fn take_units(pending: &mut f32) -> i32 {
    if pending.abs() < 2.0 {
        return 0;
    }
    let value = pending
        .trunc()
        .clamp(-MAX_UNITS_PER_EVENT, MAX_UNITS_PER_EVENT);
    *pending -= value;
    value as i32
}

fn mouse_input(
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    data: u32,
) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_input(vk: VIRTUAL_KEY, key_up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> u32 {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        crate::logging::warn(&format!(
            "SendInput injected {sent} of {} events (error {}).",
            inputs.len(),
            std::io::Error::last_os_error()
        ));
    }
    sent
}

fn inject_wheel(vertical: i32, horizontal: i32) {
    let mut inputs = Vec::with_capacity(2);
    if vertical != 0 {
        inputs.push(mouse_input(MOUSEEVENTF_WHEEL, vertical as u32));
    }
    if horizontal != 0 {
        inputs.push(mouse_input(MOUSEEVENTF_HWHEEL, horizontal as u32));
    }
    if !inputs.is_empty() {
        send(&inputs);
    }
}

fn middle_click_inputs() -> [INPUT; 2] {
    [
        mouse_input(MOUSEEVENTF_MIDDLEDOWN, 0),
        mouse_input(MOUSEEVENTF_MIDDLEUP, 0),
    ]
}

fn inject_middle_click() {
    send(&middle_click_inputs());
}

/// Ctrl held while the wheel turns is the standard Windows zoom shortcut.
fn zoom_inputs(units: i32) -> [INPUT; 3] {
    [
        key_input(VK_CONTROL, false),
        mouse_input(MOUSEEVENTF_WHEEL, units as u32),
        key_input(VK_CONTROL, true),
    ]
}

fn inject_zoom(units: i32) {
    send(&zoom_inputs(units));
}

/// Swipe left goes back; swipe right goes forward.
fn swipe_navigates_back(dx: f32) -> bool {
    dx < 0.0
}

fn inject_navigation(back: bool) {
    // Native mouse navigation buttons work in browsers and Explorer without
    // stealing or emulating keyboard shortcuts from the foreground app.
    let button = if back { XBUTTON_BACK } else { XBUTTON_FORWARD };
    send(&[
        mouse_input(MOUSEEVENTF_XDOWN, button),
        mouse_input(MOUSEEVENTF_XUP, button),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid_daemon::TouchPoint;

    fn point(x: i32, y: i32) -> TouchPoint {
        TouchPoint { x, y, size: 5 }
    }

    fn one_finger(x: i32, y: i32) -> TouchReport {
        TouchReport {
            points: vec![point(x, y)],
            left: false,
            right: false,
        }
    }

    fn two_fingers(x1: i32, y1: i32, x2: i32, y2: i32) -> TouchReport {
        TouchReport {
            points: vec![point(x1, y1), point(x2, y2)],
            left: false,
            right: false,
        }
    }

    fn mouse_flags(input: &INPUT) -> u32 {
        unsafe { input.Anonymous.mi.dwFlags.0 }
    }

    fn mouse_data(input: &INPUT) -> u32 {
        unsafe { input.Anonymous.mi.mouseData }
    }

    fn key_flags(input: &INPUT) -> u32 {
        unsafe { input.Anonymous.ki.dwFlags.0 }
    }

    #[test]
    fn centre_click_is_detected_without_two_finger_conflict() {
        let mut simulator = MouseSimulator::new(Config::default());
        let base = TouchReport {
            points: vec![TouchPoint {
                x: 100,
                y: 100,
                size: 20,
            }],
            left: true,
            right: false,
        };
        assert!(simulator.should_middle_click(&base));
        let mut edge = base.clone();
        edge.points[0].x = 900;
        assert!(!simulator.should_middle_click(&edge));

        let mut two = base.clone();
        two.points.push(TouchPoint {
            x: 800,
            y: 800,
            size: 20,
        });
        assert!(!simulator.should_middle_click(&two));

        simulator.config.middle_click = false;
        assert!(!simulator.should_middle_click(&base));
    }

    #[test]
    fn one_finger_never_scrolls() {
        let mut simulator = MouseSimulator::new(Config::default());
        let now = Instant::now();
        simulator.handle_report(&one_finger(100, 100), now);
        simulator.handle_report(&one_finger(300, 400), now);
        assert_eq!(simulator.vertical.pending, 0.0);
        assert_eq!(simulator.horizontal.pending, 0.0);
        assert!(simulator.axis.is_none());
    }

    #[test]
    fn two_fingers_scroll_vertically_and_horizontally() {
        let now = Instant::now();

        let mut vertical = MouseSimulator::new(Config::default());
        vertical.handle_report(&two_fingers(0, 0, 200, 0), now);
        vertical.handle_report(&two_fingers(0, 200, 200, 200), now);
        assert!(vertical.vertical.pending.abs() > 0.0);
        assert_eq!(vertical.horizontal.pending, 0.0);

        let mut horizontal = MouseSimulator::new(Config::default());
        horizontal.handle_report(&two_fingers(0, 0, 0, 200), now);
        horizontal.handle_report(&two_fingers(200, 0, 200, 200), now);
        assert!(horizontal.horizontal.pending.abs() > 0.0);
        assert_eq!(horizontal.vertical.pending, 0.0);
    }

    #[test]
    fn pinch_emits_ctrl_plus_wheel_events() {
        let mut simulator = MouseSimulator::new(Config::default());
        let now = Instant::now();
        simulator.handle_report(&two_fingers(100, 100, 300, 100), now);
        simulator.handle_report(&two_fingers(0, 100, 400, 100), now);
        assert!(simulator.pinching);
        simulator.handle_report(&two_fingers(0, 100, 460, 100), now);
        assert!(simulator.zoom_pending.abs() > 0.0);

        let inputs = zoom_inputs(120);
        assert_eq!(unsafe { inputs[0].Anonymous.ki.wVk }, VK_CONTROL);
        assert_eq!(inputs[0].r#type, INPUT_KEYBOARD);
        assert_eq!(mouse_flags(&inputs[1]), MOUSEEVENTF_WHEEL.0);
        assert_eq!(mouse_data(&inputs[1]), 120);
        assert_eq!(key_flags(&inputs[2]) & KEYEVENTF_KEYUP.0, KEYEVENTF_KEYUP.0);
    }

    #[test]
    fn middle_click_inputs_are_down_then_up() {
        let inputs = middle_click_inputs();
        assert_eq!(inputs[0].r#type, INPUT_MOUSE);
        assert_eq!(mouse_flags(&inputs[0]), MOUSEEVENTF_MIDDLEDOWN.0);
        assert_eq!(inputs[1].r#type, INPUT_MOUSE);
        assert_eq!(mouse_flags(&inputs[1]), MOUSEEVENTF_MIDDLEUP.0);
    }

    #[test]
    fn partial_wheel_units_are_preserved() {
        let mut pending = 17.8;
        assert_eq!(take_units(&mut pending), 17);
        assert!((pending - 0.8).abs() < 0.001);
        assert_eq!(take_units(&mut pending), 0);
    }

    #[test]
    fn huge_deltas_are_limited_per_event() {
        let mut pending = 10_000.0;
        assert_eq!(take_units(&mut pending), 480);
        assert!(pending > 0.0);
    }

    #[test]
    fn swipe_left_is_back_and_swipe_right_is_forward() {
        assert!(swipe_navigates_back(-10.0));
        assert!(!swipe_navigates_back(10.0));
    }
}
