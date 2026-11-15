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
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY, VK_LEFT, VK_LMENU, VK_RIGHT,
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

pub struct MouseSimulator {
    config: Config,
    left_was_down: bool,
    right_was_down: bool,
    active_touches: usize,
    last_single: Option<(u8, i32, i32)>,
    last_report_at: Option<Instant>,
    gesture_dx: f32,
    gesture_dy: f32,
    axis: Option<Axis>,
    vertical: AxisState,
    horizontal: AxisState,
    swipe: Option<Swipe>,
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
            last_single: None,
            last_report_at: None,
            gesture_dx: 0.0,
            gesture_dy: 0.0,
            axis: None,
            vertical: AxisState::default(),
            horizontal: AxisState::default(),
            swipe: None,
            last_scroll_motion: None,
            last_tick: now,
            last_emit: now - EMIT_INTERVAL,
        }
    }

    pub fn update_config(&mut self, config: Config) {
        if config.natural_scrolling != self.config.natural_scrolling
            || !config.smooth_scrolling
        {
            self.stop_inertia();
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
            if self.active_touches == 1 && count == 0 {
                self.finish_scroll_gesture(now);
            } else {
                self.cancel_scroll_gesture();
            }
            self.swipe = None;
        }

        if clicking {
            self.cancel_scroll_gesture();
            self.swipe = None;
        } else {
            self.update_swipe(report, now);
        }

        if !clicking && count == 1 {
            if self.active_touches == 1 {
                let p = &report.points[0];
                if let Some((previous_id, previous_x, previous_y)) = self.last_single {
                    if previous_id == p.id {
                        let dx = (p.x - previous_x) as f32;
                        let dy = (p.y - previous_y) as f32;
                        self.accumulate_scroll(dx, dy, delta_time, now);
                    }
                }
            }
        } else {
            self.gesture_dx = 0.0;
            self.gesture_dy = 0.0;
        }

        self.active_touches = count;
        self.last_single = if count == 1 {
            let p = &report.points[0];
            Some((p.id, p.x, p.y))
        } else {
            None
        };
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

        let vertical = take_units(&mut self.vertical.pending);
        let horizontal = take_units(&mut self.horizontal.pending);
        if vertical != 0 || horizontal != 0 {
            inject_wheel(vertical, horizontal);
            self.last_emit = now;
        }
    }

    fn accumulate_scroll(&mut self, dx: f32, dy: f32, dt: f32, now: Instant) {
        self.gesture_dx += dx;
        self.gesture_dy += dy;

        if self.axis.is_none() {
            if self.gesture_dx.abs().max(self.gesture_dy.abs()) < AXIS_LOCK_THRESHOLD {
                return;
            }
            let axis = if self.gesture_dy.abs() >= self.gesture_dx.abs() {
                Axis::Vertical
            } else {
                Axis::Horizontal
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
        axis_state.velocity = (axis_state.velocity * 0.55 + instant_velocity * 0.45)
            .clamp(-4_000.0, 4_000.0);
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
            if dx < 0.0 {
                inject_navigation(false);
            } else {
                inject_navigation(true);
            }
            crate::logging::info(&format!(
                "Two-finger swipe detected ({:.0} raw units).",
                dx
            ));
        } else if dy.abs() > SWIPE_THRESHOLD * 1.25 {
            swipe.cancelled = true;
        }
        let _ = now;
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
        if !self.config.middle_click {
            return false;
        }
        match report.points.len() {
            2 => true,
            1 => {
                let point = &report.points[0];
                point.size >= FIRM_TOUCH_SIZE && point.x.abs() <= MIDDLE_ZONE
            }
            _ => false,
        }
    }
}

fn take_units(pending: &mut f32) -> i32 {
    if pending.abs() < 2.0 {
        return 0;
    }
    let value = pending.trunc().clamp(-MAX_UNITS_PER_EVENT, MAX_UNITS_PER_EVENT);
    *pending -= value;
    value as i32
}

fn mouse_input(flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
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

fn key_input(vk: VIRTUAL_KEY, key_up: bool, extended: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_KEYBOARD,
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
    let sent = unsafe {
        SendInput(
            inputs,
            std::mem::size_of::<INPUT>() as i32,
        )
    };
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

fn inject_middle_click() {
    send(&[
        mouse_input(MOUSEEVENTF_MIDDLEDOWN, 0),
        mouse_input(MOUSEEVENTF_MIDDLEUP, 0),
    ]);
}

fn inject_navigation(back: bool) {
    // A dedicated Sonoma-style trackpad swipe on Windows maps to the universal
    // Alt+Left / Alt+Right shortcuts used by browsers and File Explorer.
    let arrow = if back { VK_LEFT } else { VK_RIGHT };
    send(&[
        key_input(VK_LMENU, false, false),
        key_input(arrow, false, true),
        key_input(arrow, true, true),
        key_input(VK_LMENU, true, false),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid_daemon::TouchPoint;

    #[test]
    fn centre_and_two_finger_clicks_are_detected() {
        let mut simulator = MouseSimulator::new(Config::default());
        let base = TouchReport {
            points: vec![TouchPoint {
                id: 1,
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
            id: 2,
            x: 800,
            y: 800,
            size: 20,
        });
        assert!(simulator.should_middle_click(&two));
        simulator.config.middle_click = false;
        assert!(!simulator.should_middle_click(&base));
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
}
