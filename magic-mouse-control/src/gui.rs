//! Main settings window: sidebar navigation, live device card, Features,
//! FAQs and Help pages, plus the system tray integration.
//!
//! Everything in the window is drawn with egui primitives. The tray icon is
//! created on this thread because `tray-icon` is reference counted and not
//! `Send`; worker threads communicate through a single `UiEvent` channel.

use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use egui::{
    Align, Color32, FontFamily, FontId, Frame, Label, Layout, Margin, Rect, RichText, Rounding,
    ScrollArea, Sense, Stroke, Ui, Vec2,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

use crate::autostart;
use crate::battery_flip::{self, FlipOutcome};
use crate::config::{self, Config};
use crate::hid_daemon::{HidEvent, HidHandle, MouseModel, TouchPoint, Transport};
use crate::icons;
use crate::logging;
use crate::notifications;
use crate::widgets;

const FAQ_QUESTIONS: [&str; 6] = [
    "How do I pair my Magic Mouse with Windows?",
    "Does it work with Secure Boot enabled?",
    "Why don't scrolling and gestures work over Bluetooth?",
    "How accurate is the battery percentage?",
    "Which Magic Mouse models are supported?",
    "Why do gestures not work after reconnecting?",
];

const NOTICE_TIMEOUT: Duration = Duration::from_secs(10);
const BATTERY_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const SAVE_DEBOUNCE: Duration = Duration::from_millis(350);
const SAVE_RETRY: Duration = Duration::from_secs(5);

/// Events arriving from the HID worker and from native tray callbacks. A
/// single standard channel keeps ordering deterministic and avoids creating a
/// second runtime just for the UI.
pub enum UiEvent {
    Hid(HidEvent),
    TrayOpen,
    TrayCheck,
    TrayAutostart,
    TrayExit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Features,
    Faqs,
    Help,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Self::Features => "Features",
            Self::Faqs => "FAQs",
            Self::Help => "Help",
        }
    }
}

#[derive(Clone)]
enum ConnectionStatus {
    Starting,
    Searching,
    Connected {
        name: String,
        model: MouseModel,
        transport: Transport,
        touch_capable: bool,
        input_blocked: bool,
    },
    Disconnected,
}

impl ConnectionStatus {
    fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Starting => "Starting",
            Self::Searching => "Searching",
            Self::Connected { .. } => "Connected",
            Self::Disconnected => "Disconnected",
        }
    }

    fn color(&self) -> Color32 {
        match self {
            Self::Starting => widgets::FAINT,
            Self::Searching => widgets::AMBER,
            Self::Connected { .. } => widgets::ACCENT,
            Self::Disconnected => widgets::RED,
        }
    }

    fn raw_input_blocked(&self) -> bool {
        matches!(
            self,
            Self::Connected {
                input_blocked: true,
                ..
            }
        )
    }

    fn is_bluetooth(&self) -> bool {
        matches!(
            self,
            Self::Connected {
                transport: Transport::Bluetooth,
                ..
            }
        )
    }

    /// Title, secondary detail, touch-capable flag and connected flag.
    fn device_info(&self) -> (String, String, bool, bool) {
        match self {
            Self::Connected {
                name,
                model,
                transport,
                touch_capable,
                input_blocked,
            } => {
                let transport_label = if *input_blocked {
                    format!("{} | Windows driver mode", transport.label())
                } else {
                    transport.label().to_string()
                };
                let detail = if name.eq_ignore_ascii_case(model.title()) {
                    transport_label
                } else {
                    format!("{name} | {transport_label}")
                };
                (model.title().to_string(), detail, *touch_capable, true)
            }
            Self::Starting => (
                "Magic Mouse".to_string(),
                "Starting the background HID service...".to_string(),
                false,
                false,
            ),
            Self::Searching => (
                "Magic Mouse".to_string(),
                "Pair the mouse in Windows Bluetooth settings. Searching continues in the background."
                    .to_string(),
                false,
                false,
            ),
            Self::Disconnected => (
                "Magic Mouse".to_string(),
                "Connection lost. Wake the mouse or pair it again; reconnecting is automatic."
                    .to_string(),
                false,
                false,
            ),
        }
    }
}

#[derive(Clone, Copy)]
struct BatteryState {
    percent: u8,
    charging: bool,
}

struct Notice {
    text: String,
    error: bool,
    at: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TrayVisual {
    connected: bool,
    percent: Option<u8>,
    low: bool,
}

pub struct App {
    config: Config,
    saved_config: Config,
    sent_config: Config,
    hid: Option<HidHandle>,
    ui_rx: Receiver<UiEvent>,

    status: ConnectionStatus,
    battery: Option<BatteryState>,
    battery_unavailable: bool,
    /// When the displayed reading was taken. `None` means it came from an
    /// unknown point in the past, so the UI labels it as last known instead of
    /// live.
    battery_measured_at: Option<SystemTime>,
    low_battery_notified: bool,
    pending_battery_check: Option<Instant>,
    /// Two-step confirmation before the privileged battery flip. The app never
    /// asks for elevation without this explicit consent.
    flip_confirm: bool,
    flip_pending: bool,
    flip_rx: Option<Receiver<FlipOutcome>>,
    /// The elevated helper did not verify that Apple's scroll filter is back.
    flip_restore_failed: bool,
    restore_pending: bool,
    restore_rx: Option<Receiver<Result<(), String>>>,
    touches: Vec<TouchPoint>,
    last_touch_at: Instant,

    autostart_enabled: bool,
    tab: Tab,
    faq_open: [bool; FAQ_QUESTIONS.len()],

    icon_texture: egui::TextureHandle,

    tray: Option<TrayIcon>,
    tray_autostart_item: Option<CheckMenuItem>,
    tray_available: bool,
    tray_visual: Option<TrayVisual>,
    tray_tooltip: String,
    tray_autostart_checked: bool,
    tray_error: Option<String>,

    notice: Option<Notice>,
    startup_error: Option<String>,
    save_retry_at: Option<Instant>,
    last_change_at: Instant,
    exit_requested: bool,
    start_minimized: bool,
    startup_shown: bool,
    /// The window was hidden with "close to tray". egui does not report
    /// visibility, so the app tracks it to avoid painting an invisible window.
    window_hidden: bool,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        hid: Option<HidHandle>,
        hid_events: UnboundedReceiver<HidEvent>,
        ui_rx: Receiver<UiEvent>,
        ui_tx: Sender<UiEvent>,
        start_minimized: bool,
        mut startup_error: Option<String>,
    ) -> Self {
        widgets::apply_theme(&cc.egui_ctx);

        let mut config = config;
        config.sanitize();
        let autostart_enabled = autostart::is_enabled();
        // The registry is the source of truth: a config file copied to another
        // PC must not claim that autostart is enabled when it is not.
        config.start_with_windows = autostart_enabled;
        let saved_config = config.clone();
        let sent_config = config.clone();

        // A reading from a previous session is still useful, but it is shown
        // as "last known" rather than live. Apple's scroll filter hides the
        // battery on Bluetooth, so a fresh read needs explicit consent.
        let cached_battery = battery_flip::load_cached();
        let initial_battery = cached_battery.map(|cached| BatteryState {
            percent: cached.percent,
            charging: cached.charging,
        });
        let initial_battery_at = cached_battery.map(|cached| cached.measured_at);
        let flip_restore_failed = battery_flip::flip_pending();

        let app_icon = icons::app_icon_rgba(64);
        let icon_texture = cc.egui_ctx.load_texture(
            "magic-mouse-app-icon",
            egui::ColorImage::from_rgba_unmultiplied([64, 64], &app_icon),
            egui::TextureOptions::LINEAR,
        );

        let (tray, tray_autostart_item, tray_available, tray_error) =
            match create_tray(&cc.egui_ctx, &ui_tx, autostart_enabled) {
                Ok((tray, item)) => (Some(tray), Some(item), true, None),
                Err(error) => {
                    logging::warn(&format!("Could not create the system tray icon: {error}"));
                    (None, None, false, Some(error))
                }
            };

        // The HID worker owns a tokio receiver. This thread bridges it into a
        // standard channel and wakes egui even while the window is hidden.
        let forward_tx = ui_tx.clone();
        let forward_ctx = cc.egui_ctx.clone();
        let mut hid_events = hid_events;
        if let Err(error) = std::thread::Builder::new()
            .name("hid-event-forwarder".to_string())
            .spawn(move || {
                while let Some(event) = hid_events.blocking_recv() {
                    if forward_tx.send(UiEvent::Hid(event)).is_err() {
                        break;
                    }
                    forward_ctx.request_repaint();
                }
            })
        {
            let message = format!("Could not start the UI event bridge: {error}");
            logging::error(&message);
            startup_error = Some(match startup_error {
                Some(existing) => format!("{existing} {message}"),
                None => message,
            });
        }

        Self {
            config,
            saved_config,
            sent_config,
            hid,
            ui_rx,
            status: ConnectionStatus::Starting,
            battery: initial_battery,
            battery_unavailable: false,
            battery_measured_at: initial_battery_at,
            low_battery_notified: false,
            pending_battery_check: None,
            flip_confirm: false,
            flip_pending: false,
            flip_rx: None,
            flip_restore_failed,
            restore_pending: false,
            restore_rx: None,
            touches: Vec::new(),
            last_touch_at: Instant::now(),
            autostart_enabled,
            tab: Tab::Features,
            faq_open: [false; FAQ_QUESTIONS.len()],
            icon_texture,
            tray,
            tray_autostart_item,
            tray_available,
            tray_visual: None,
            tray_tooltip: String::new(),
            tray_autostart_checked: autostart_enabled,
            tray_error,
            notice: None,
            startup_error,
            save_retry_at: None,
            last_change_at: Instant::now(),
            exit_requested: false,
            start_minimized,
            startup_shown: false,
            window_hidden: start_minimized,
        }
    }

    fn handle_ui_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.ui_rx.try_recv() {
            match event {
                UiEvent::Hid(event) => self.handle_hid_event(event),
                UiEvent::TrayOpen => self.show_window(ctx),
                UiEvent::TrayCheck => self.check_battery(ctx),
                UiEvent::TrayAutostart => {
                    self.config.start_with_windows = !self.autostart_enabled;
                }
                UiEvent::TrayExit => {
                    self.exit_requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn handle_hid_event(&mut self, event: HidEvent) {
        match event {
            HidEvent::Searching => {
                self.status = ConnectionStatus::Searching;
                // Keep the last known percentage visible across short
                // Bluetooth dropouts and the few seconds a battery flip takes.
                self.battery_unavailable = false;
                self.touches.clear();
                if self.pending_battery_check.take().is_some() {
                    notifications::not_connected();
                }
            }
            HidEvent::Disconnected => {
                self.status = ConnectionStatus::Disconnected;
                self.battery_unavailable = false;
                self.touches.clear();
                if self.pending_battery_check.take().is_some() {
                    notifications::not_connected();
                }
            }
            HidEvent::Connected {
                name,
                model,
                transport,
                touch_capable,
                input_blocked,
            } => {
                self.status = ConnectionStatus::Connected {
                    name,
                    model,
                    transport,
                    touch_capable,
                    input_blocked,
                };
                self.battery_unavailable = false;
                self.low_battery_notified = false;
                self.touches.clear();
            }
            HidEvent::Battery { percent, charging } => {
                self.battery = Some(BatteryState { percent, charging });
                self.battery_measured_at = Some(SystemTime::now());
                self.battery_unavailable = false;
                battery_flip::save_cached(percent, charging);

                let threshold = self.config.low_battery_threshold;
                if charging || percent >= threshold {
                    self.low_battery_notified = false;
                }
                if !charging
                    && percent < threshold
                    && self.config.low_battery_notifications
                    && !self.low_battery_notified
                {
                    notifications::low_battery(percent);
                    self.low_battery_notified = true;
                    self.show_info(format!(
                        "Battery low: {percent}%. Charge your Magic Mouse."
                    ));
                }

                if self.pending_battery_check.take().is_some() {
                    notifications::battery_status(percent, charging);
                    self.show_info(format!("Battery level: {percent}%."));
                }
            }
            HidEvent::BatteryUnavailable => {
                let was_pending = self.pending_battery_check.take().is_some();
                if self.flip_pending {
                    // The device is restarting as part of the consented
                    // battery read; a missing report right now is expected.
                    return;
                }
                let fresh = self.battery_is_fresh();
                if let Some(battery) = self.battery {
                    // A cached value is still useful. Treat a failed refresh as
                    // a non-event instead of replacing a correct percentage
                    // with an error state.
                    self.battery_unavailable = false;
                    if was_pending {
                        if fresh {
                            notifications::battery_status(battery.percent, battery.charging);
                            self.show_info(format!(
                                "Battery level: {}%. The mouse did not send a newer reading; showing the last known value.",
                                battery.percent
                            ));
                        } else if self.status.is_bluetooth() {
                            self.flip_confirm = true;
                            self.show_info(
                                "Apple's scroll mode hides the battery. Use Read now for a fresh value; scrolling pauses for a moment.",
                            );
                        }
                    }
                } else {
                    self.battery_unavailable = true;
                    if was_pending {
                        if self.status.is_bluetooth() {
                            // The direct HID request cannot work while the
                            // scroll filter is loaded. Offer the explicit,
                            // consented flip instead of showing a dead end.
                            self.flip_confirm = true;
                            notifications::battery_unavailable();
                            self.show_info(
                                "Apple's scroll mode hides the battery. Use Read now; scrolling pauses for about 3 seconds.",
                            );
                        } else {
                            notifications::battery_unavailable();
                            self.show_error(
                                "The mouse did not answer the battery request. Try again after moving it.",
                            );
                        }
                    }
                }
            }
            HidEvent::Touch { points } => {
                if points.is_empty() {
                    self.touches.clear();
                } else {
                    self.last_touch_at = Instant::now();
                    self.touches = points;
                }
            }
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        self.window_hidden = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        if self.exit_requested || !self.config.close_to_tray || !self.tray_available {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.window_hidden = true;
    }

    fn check_battery(&mut self, ctx: &egui::Context) {
        if !self.status.is_connected() {
            notifications::not_connected();
            self.show_info("Connect your Magic Mouse before checking the battery.");
            return;
        }
        let Some(hid) = self.hid.as_ref() else {
            notifications::generic_error(
                "The background HID service is not running. Check the log file.",
            );
            self.show_error("The background HID service is not running.");
            return;
        };
        match hid.poll_battery() {
            Ok(()) => {
                self.pending_battery_check = Some(Instant::now());
                // Keep the known value on screen while the fresh request is in
                // flight. The HID worker asks the mouse directly over Bluetooth
                // and reports the new value when it arrives.
                if let Some(battery) = self.battery {
                    self.show_info(format!(
                        "Battery level: {}%. Refreshing from the mouse...",
                        battery.percent
                    ));
                } else {
                    self.show_info("Checking the battery...");
                }
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            Err(error) => {
                notifications::generic_error(&error);
                self.show_error(error);
            }
        }
    }

    fn battery_is_fresh(&self) -> bool {
        self.battery_measured_at
            .and_then(|measured| SystemTime::now().duration_since(measured).ok())
            .is_some_and(|age| age <= Duration::from_secs(120))
    }

    /// Start the explicit, consented battery read. This runs on its own thread;
    /// the UI only polls for the result.
    fn start_flip(&mut self, ctx: &egui::Context) {
        if self.flip_pending || self.restore_pending {
            return;
        }
        self.flip_confirm = false;
        match battery_flip::start_read() {
            Ok(receiver) => {
                self.flip_rx = Some(receiver);
                self.flip_pending = true;
                self.show_info(
                    "Waiting for administrator approval. Scrolling resumes automatically when the reading is done.",
                );
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            Err(error) => {
                self.show_error(format!("Could not read the battery: {error}"));
            }
        }
    }

    fn apply_flip(&mut self, outcome: FlipOutcome) {
        if let Some(percent) = outcome.percent {
            self.battery = Some(BatteryState {
                percent,
                charging: false,
            });
            self.battery_measured_at = Some(SystemTime::now());
            self.battery_unavailable = false;
            battery_flip::save_cached(percent, false);

            if outcome.restored {
                self.flip_restore_failed = false;
                notifications::battery_status(percent, false);
                self.show_info(format!(
                    "Battery level: {percent}%. Apple's scroll filter is back on."
                ));
            } else {
                self.flip_restore_failed = true;
                self.show_error(format!(
                    "Battery level: {percent}%, but scroll mode could not be verified. Click Restore scroll mode."
                ));
            }
            return;
        }

        if outcome.cancelled {
            self.show_info("Battery read cancelled. Nothing was changed.");
            return;
        }

        if !outcome.restored {
            self.flip_restore_failed = true;
        }
        self.show_error(outcome.error.unwrap_or_else(|| {
            "The battery could not be read. Wake the mouse and try again.".to_string()
        }));
    }

    fn start_restore(&mut self, ctx: &egui::Context) {
        if self.restore_pending || self.flip_pending {
            return;
        }
        match battery_flip::start_restore() {
            Ok(receiver) => {
                self.restore_rx = Some(receiver);
                self.restore_pending = true;
                self.show_info("Restoring Apple's scroll filter...");
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            Err(error) => {
                self.show_error(format!("Could not restore scroll mode: {error}"));
            }
        }
    }

    fn apply_restore(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.flip_restore_failed = false;
                self.show_info("Scroll mode is back on and verified.");
            }
            Err(error) => {
                self.flip_restore_failed = true;
                self.show_error(format!(
                    "Scroll mode could not be restored: {error}. Restart Windows if scrolling is still off."
                ));
            }
        }
    }

    fn after_ui(&mut self, ctx: &egui::Context) {
        // Apply "Start with Windows" only when the switch actually changed.
        // On failure the value is reverted, so the UI reflects the registry.
        if self.config.start_with_windows != self.autostart_enabled {
            match autostart::set_enabled(self.config.start_with_windows) {
                Ok(()) => {
                    self.autostart_enabled = self.config.start_with_windows;
                    if self.autostart_enabled {
                        self.show_info("Magic Mouse Control will now start with Windows.");
                    } else {
                        self.show_info("Start with Windows has been disabled.");
                    }
                }
                Err(error) => {
                    self.config.start_with_windows = self.autostart_enabled;
                    notifications::generic_error(&error);
                    self.show_error(error);
                }
            }
        }

        if self.config != self.sent_config {
            self.config.sanitize();
            if let Some(hid) = self.hid.as_ref() {
                if let Err(error) = hid.update_config(&self.config) {
                    self.startup_error = Some(format!(
                        "The background HID service stopped unexpectedly: {error}"
                    ));
                }
            }
            self.sent_config = self.config.clone();
            self.last_change_at = Instant::now();
        }

        if let Some(battery) = self.battery {
            if battery.charging || battery.percent >= self.config.low_battery_threshold {
                self.low_battery_notified = false;
            }
        }

        if self.config != self.saved_config {
            let now = Instant::now();
            let due = now.saturating_duration_since(self.last_change_at) >= SAVE_DEBOUNCE;
            let can_retry = self.save_retry_at.is_none_or(|retry| now >= retry);
            if due && can_retry {
                match self.config.save() {
                    Ok(()) => {
                        self.saved_config = self.config.clone();
                        self.save_retry_at = None;
                    }
                    Err(error) => {
                        self.save_retry_at = Some(now + SAVE_RETRY);
                        self.show_error(format!("Could not save settings: {error}"));
                    }
                }
            }
            ctx.request_repaint_after(Duration::from_millis(400));
        }
    }

    fn refresh_tray(&mut self) {
        let connected = self.status.is_connected();
        let percent = self.battery.map(|battery| battery.percent);
        let low = percent
            .map(|percent| percent < self.config.low_battery_threshold)
            .unwrap_or(false);
        let visual = TrayVisual {
            connected,
            percent,
            low,
        };

        if self.tray_visual != Some(visual) {
            if let Some(tray) = self.tray.as_ref() {
                match Icon::from_rgba(icons::tray_icon_rgba(32, connected, percent, low), 32, 32) {
                    Ok(icon) => {
                        if let Err(error) = tray.set_icon(Some(icon)) {
                            logging::warn(&format!("Could not update the tray icon: {error}"));
                        }
                    }
                    Err(error) => {
                        logging::warn(&format!("Could not render the tray icon: {error}"));
                    }
                }
            }
            self.tray_visual = Some(visual);
        }

        let tooltip = match (&self.status, percent) {
            (ConnectionStatus::Connected { .. }, Some(percent)) => {
                format!("Magic Mouse: {percent}%")
            }
            (ConnectionStatus::Connected { .. }, None) => "Magic Mouse: Connected".to_string(),
            (ConnectionStatus::Starting, _) => "Magic Mouse: Starting...".to_string(),
            _ => "Magic Mouse: Not connected".to_string(),
        };
        if tooltip != self.tray_tooltip {
            let mut updated = true;
            if let Some(tray) = self.tray.as_ref() {
                if let Err(error) = tray.set_tooltip(Some(tooltip.as_str())) {
                    logging::warn(&format!("Could not update the tray tooltip: {error}"));
                    updated = false;
                }
            }
            if updated {
                self.tray_tooltip = tooltip;
            }
        }

        if self.autostart_enabled != self.tray_autostart_checked {
            if let Some(item) = self.tray_autostart_item.as_ref() {
                item.set_checked(self.autostart_enabled);
            }
            self.tray_autostart_checked = self.autostart_enabled;
        }
    }

    fn update_async_ui(&mut self, ctx: &egui::Context, visible: bool) {
        if self.flip_pending {
            match self.flip_rx.as_ref().map(Receiver::try_recv) {
                Some(Ok(outcome)) => {
                    self.flip_rx = None;
                    self.flip_pending = false;
                    self.apply_flip(outcome);
                }
                Some(Err(TryRecvError::Empty)) => {
                    ctx.request_repaint_after(Duration::from_millis(300));
                }
                Some(Err(TryRecvError::Disconnected)) => {
                    self.flip_rx = None;
                    self.flip_pending = false;
                    self.flip_restore_failed = true;
                    self.show_error(
                        "The battery helper stopped unexpectedly. Use Restore scroll mode if scrolling does not work.",
                    );
                }
                None => {
                    self.flip_pending = false;
                }
            }
        }

        if self.restore_pending {
            match self.restore_rx.as_ref().map(Receiver::try_recv) {
                Some(Ok(result)) => {
                    self.restore_rx = None;
                    self.restore_pending = false;
                    self.apply_restore(result);
                }
                Some(Err(TryRecvError::Empty)) => {
                    ctx.request_repaint_after(Duration::from_millis(300));
                }
                Some(Err(TryRecvError::Disconnected)) => {
                    self.restore_rx = None;
                    self.restore_pending = false;
                    self.show_error(
                        "The restore helper stopped unexpectedly. Restart Windows if scrolling is still off.",
                    );
                }
                None => {
                    self.restore_pending = false;
                }
            }
        }

        if !self.touches.is_empty() && self.last_touch_at.elapsed() > Duration::from_millis(450) {
            self.touches.clear();
        }
        if visible && !self.touches.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(80));
        }

        if let Some(since) = self.pending_battery_check {
            if since.elapsed() >= BATTERY_CHECK_TIMEOUT {
                self.pending_battery_check = None;
                if let Some(battery) = self.battery {
                    notifications::battery_status(battery.percent, battery.charging);
                    self.show_info(format!(
                        "Battery level: {}%. The mouse did not send a newer reading; showing the last known value.",
                        battery.percent
                    ));
                } else {
                    notifications::battery_unavailable();
                    self.show_error(
                        "Battery check timed out. Move the mouse to wake it and try again.",
                    );
                }
            } else {
                // While hidden, only wake up when the timeout itself expires.
                let delay = if visible {
                    Duration::from_millis(500)
                } else {
                    BATTERY_CHECK_TIMEOUT.saturating_sub(since.elapsed())
                };
                ctx.request_repaint_after(delay);
            }
        }

        if self
            .notice
            .as_ref()
            .is_some_and(|notice| notice.at.elapsed() >= NOTICE_TIMEOUT)
        {
            self.notice = None;
        }
        // A hidden window needs no notice animation. The notice is cleared on
        // the next visible frame.
        if visible && self.notice.is_some() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn show_error(&mut self, text: impl Into<String>) {
        let text = text.into();
        logging::error(&text);
        self.notice = Some(Notice {
            text,
            error: true,
            at: Instant::now(),
        });
    }

    fn show_info(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice {
            text: text.into(),
            error: false,
            at: Instant::now(),
        });
    }

    fn banners(&mut self, ui: &mut Ui) {
        if let Some(error) = self.startup_error.as_ref() {
            widgets::notice_banner(ui, error, true);
            ui.add_space(10.0);
        }
        if self.tray_error.is_some() {
            widgets::notice_banner(
                ui,
                "The system tray is unavailable, so close-to-tray is disabled. Closing the window exits the app.",
                true,
            );
            ui.add_space(10.0);
        }
        if self.flip_restore_failed {
            widgets::notice_banner(
                ui,
                "Apple's scroll filter is not confirmed to be back. Scrolling may be off until you restore it. This needs administrator approval.",
                true,
            );
            ui.add_space(6.0);
            let mut restore_clicked = false;
            ui.horizontal(|ui| {
                if widgets::accent_button(ui, "Restore scroll mode").clicked() {
                    restore_clicked = true;
                }
                if self.restore_pending {
                    ui.label(
                        RichText::new("Waiting for administrator approval...")
                            .size(11.0)
                            .color(widgets::MUTED),
                    );
                }
            });
            ui.add_space(10.0);
            if restore_clicked {
                self.start_restore(ui.ctx());
            }
        }
        if let Some(notice) = self.notice.as_ref() {
            if notice.at.elapsed() < NOTICE_TIMEOUT {
                widgets::notice_banner(ui, &notice.text, notice.error);
                ui.add_space(10.0);
            }
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        let status_label = self.status.label();
        let status_color = self.status.color();
        let battery = self.battery;
        let threshold = self.config.low_battery_threshold;

        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
            ui.painter().image(
                self.icon_texture.id(),
                rect,
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("Magic Mouse").size(16.0).strong().color(widgets::TEXT));
                ui.label(
                    RichText::new("CONTROL")
                        .size(9.0)
                        .strong()
                        .color(widgets::ACCENT),
                );
            });
        });

        ui.add_space(28.0);
        for tab in [Tab::Features, Tab::Faqs, Tab::Help] {
            if widgets::nav_item(ui, tab.label(), self.tab == tab).clicked() {
                self.tab = tab;
            }
            ui.add_space(2.0);
        }

        ui.add_space((ui.available_height() - 168.0).max(12.0));

        Frame::none()
            .fill(widgets::CARD)
            .stroke(Stroke::new(1.0_f32, widgets::BORDER))
            .rounding(Rounding::same(12.0))
            .inner_margin(Margin::same(12.0))
            .show(ui, |ui| {
                widgets::eyebrow(ui, "Background service");
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 4.0, status_color);
                    ui.label(
                        RichText::new(status_label)
                            .size(12.0)
                            .strong()
                            .color(widgets::TEXT_SOFT),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(battery) = battery {
                            ui.label(
                                RichText::new(format!("{}%", battery.percent))
                                    .size(12.0)
                                    .strong()
                                    .color(battery_color(battery.percent, threshold)),
                            );
                        }
                    });
                });
                ui.add_space(8.0);
                if let Some(battery) = battery {
                    battery_bar(
                        ui,
                        battery.percent,
                        if battery.charging {
                            widgets::BLUE
                        } else {
                            battery_color(battery.percent, threshold)
                        },
                    );
                } else {
                    ui.label(
                        RichText::new(if self.status.is_connected() {
                            "Waiting for a battery reading"
                        } else {
                            "No mouse connected"
                        })
                        .size(11.0)
                        .color(widgets::MUTED),
                    );
                }
                ui.add_space(9.0);
                ui.label(
                    RichText::new("Right-click the tray icon for quick actions.")
                        .size(10.5)
                        .color(widgets::FAINT),
                );
            });

        ui.add_space(8.0);
        ui.label(
            RichText::new(format!("v{}  |  MIT License", env!("CARGO_PKG_VERSION")))
                .size(10.0)
                .color(widgets::FAINT),
        );
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        let status_label = self.status.label();
        let status_color = self.status.color();
        let battery = self.battery;
        let threshold = self.config.low_battery_threshold;
        let mut check_clicked = false;

        ui.horizontal(|ui| {
            widgets::eyebrow(ui, "Magic Mouse Control");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::ghost_button(ui, "Check battery").clicked() {
                    check_clicked = true;
                }
                if let Some(battery) = battery {
                    let label = if battery.charging {
                        format!("{}% charging", battery.percent)
                    } else {
                        format!("{}%", battery.percent)
                    };
                    let color = if battery.charging {
                        widgets::BLUE
                    } else {
                        battery_color(battery.percent, threshold)
                    };
                    widgets::pill(ui, &label, color);
                }
                widgets::status_pill(ui, status_label, status_color);
            });
        });

        if check_clicked {
            self.check_battery(ui.ctx());
        }
    }

    fn page(&mut self, ui: &mut Ui) {
        self.banners(ui);
        match self.tab {
            Tab::Features => self.features(ui),
            Tab::Faqs => self.faqs(ui),
            Tab::Help => self.help(ui),
        }
    }

    fn features(&mut self, ui: &mut Ui) {
        widgets::eyebrow(ui, "Features");
        widgets::page_heading(
            ui,
            "Your Magic Mouse, tuned.",
            "Scrolling, gestures and background behaviour, without installing a kernel driver.",
        );
        ui.add_space(16.0);

        self.device_card(ui);
        if self.status.raw_input_blocked() {
            ui.add_space(14.0);
            self.bluetooth_limitation_card(ui);
        }
        ui.add_space(14.0);
        self.scrolling_card(ui);
        ui.add_space(14.0);
        self.gestures_card(ui);
        ui.add_space(14.0);
        self.background_card(ui);
    }

    fn bluetooth_limitation_card(&mut self, ui: &mut Ui) {
        let experimental = self.config.experimental_input;
        Frame::none()
            .fill(Color32::from_rgb(36, 30, 17))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(92, 74, 36)))
            .rounding(Rounding::same(16.0))
            .inner_margin(Margin::same(18.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    widgets::eyebrow(
                        ui,
                        if experimental {
                            "Bluetooth limitation"
                        } else {
                            "Stability protection"
                        },
                    );
                    widgets::pill(
                        ui,
                        if experimental { "Limited mode" } else { "Safe mode" },
                        widgets::AMBER,
                    );
                });
                ui.add_space(3.0);
                ui.label(
                    RichText::new(if experimental {
                        "Windows keeps Bluetooth touch data to itself."
                    } else {
                        "Touch gestures are off by default."
                    })
                    .size(17.0)
                    .strong()
                    .color(widgets::TEXT),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(if experimental {
                        "Experimental input is enabled, but Windows reserves raw HID reports for its own input stack. Bluetooth touch data is therefore still unavailable."
                    } else {
                        "The app only monitors the connection and battery. It never reads raw HID input or injects gestures unless you explicitly enable experimental touch input, which keeps Bluetooth pointer input stable."
                    })
                    .size(12.4)
                    .color(widgets::TEXT_SOFT),
                );
                ui.add_space(9.0);
                widgets::bullet_line(
                    ui,
                    "Your mouse, clicks and Windows' native scrolling keep working normally.",
                );
                widgets::bullet_line(
                    ui,
                    "To try touch gestures, connect the mouse with its charging cable (USB) and enable experimental touch input below.",
                );
                widgets::bullet_line(
                    ui,
                    "Apple's scroll filter hides the battery over Bluetooth. Use Read now on the device card: scrolling pauses for about 3 seconds, Windows asks for administrator approval once, and the filter is restored automatically.",
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if !experimental
                        && widgets::ghost_button(ui, "Enable for USB").clicked()
                    {
                        self.config.experimental_input = true;
                    }
                    if widgets::ghost_button(ui, "Read the FAQ").clicked() {
                        self.tab = Tab::Faqs;
                    }
                    if widgets::ghost_button(ui, "Bluetooth settings").clicked() {
                        self.open_target("ms-settings:bluetooth");
                    }
                });
            });
    }

    fn device_card(&mut self, ui: &mut Ui) {
        let (title, detail, touch_capable, connected) = self.status.device_info();
        let status_label = self.status.label();
        let status_color = self.status.color();
        let battery = self.battery;
        let unavailable = self.battery_unavailable;
        let battery_fresh = self.battery_is_fresh();
        let is_bluetooth = self.status.is_bluetooth();
        let touches = self.touches.clone();
        let threshold = self.config.low_battery_threshold;
        let flip_confirm = self.flip_confirm;
        let flip_pending = self.flip_pending;
        let restore_pending = self.restore_pending;
        let mut check_clicked = false;
        let mut pairing_clicked = false;
        let mut bluetooth_clicked = false;
        let mut read_clicked = false;
        let mut confirm_flip = false;
        let mut cancel_flip = false;

        widgets::card().show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(222.0, 218.0), Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(rect, Rounding::same(16.0), Color32::from_rgb(14, 19, 25));
                painter.rect_stroke(
                    rect,
                    Rounding::same(16.0),
                    Stroke::new(1.0_f32, widgets::BORDER_SOFT),
                );
                painter.text(
                    rect.left_top() + Vec2::new(13.0, 11.0),
                    egui::Align2::LEFT_TOP,
                    "LIVE TOUCH",
                    FontId::new(9.0, FontFamily::Proportional),
                    widgets::FAINT,
                );
                icons::draw_mouse(painter, rect.shrink2(Vec2::new(14.0, 20.0)), &touches);

                ui.add_space(18.0);
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        widgets::status_pill(ui, status_label, status_color);
                        if connected {
                            if touch_capable {
                                widgets::pill(ui, "Multi-touch", widgets::BLUE);
                            } else {
                                widgets::pill(ui, "Basic mode", widgets::AMBER);
                            }
                        }
                    });
                    ui.add_space(7.0);
                    ui.label(RichText::new(title).size(25.0).strong().color(widgets::TEXT));
                    ui.add(
                        Label::new(RichText::new(detail).size(12.2).color(widgets::MUTED)).wrap(),
                    );

                    ui.add_space(13.0);
                    ui.horizontal(|ui| {
                        match battery {
                            Some(battery) => {
                                ui.label(
                                    RichText::new(battery.percent.to_string())
                                        .size(40.0)
                                        .strong()
                                        .color(battery_color(battery.percent, threshold)),
                                );
                                ui.vertical(|ui| {
                                    ui.add_space(10.0);
                                    ui.label(
                                        RichText::new("% battery")
                                            .size(12.0)
                                            .color(widgets::MUTED),
                                    );
                                });
                            }
                            None => {
                                ui.label(
                                    RichText::new(if connected { "Unknown" } else { "-" })
                                        .size(28.0)
                                        .strong()
                                        .color(widgets::FAINT),
                                );
                            }
                        }
                        if battery.is_some_and(|battery| battery.charging) {
                            ui.add_space(7.0);
                            widgets::pill(ui, "Charging (est.)", widgets::BLUE);
                        }
                        if connected && battery.is_some() {
                            ui.add_space(7.0);
                            if battery_fresh {
                                widgets::pill(ui, "LIVE", widgets::ACCENT);
                            } else {
                                widgets::pill(ui, "LAST", widgets::AMBER);
                            }
                        }
                    });

                    if let Some(battery) = battery {
                        battery_bar(
                            ui,
                            battery.percent,
                            if battery.charging {
                                widgets::BLUE
                            } else {
                                battery_color(battery.percent, threshold)
                            },
                        );
                    }

                    if !connected {
                        ui.add_space(4.0);
                        widgets::small_muted(
                            ui,
                            "Pair the mouse in Windows Bluetooth settings to start.",
                        );
                    } else if is_bluetooth && !battery_fresh {
                        ui.add_space(4.0);
                        widgets::small_muted(
                            ui,
                            if battery.is_some() {
                                "Last known reading. Apple's scroll mode hides the battery on Bluetooth."
                            } else {
                                "Apple's scroll mode hides the battery on Bluetooth. Use Read now for a fresh value."
                            },
                        );
                    } else if unavailable {
                        ui.add_space(4.0);
                        widgets::small_muted(
                            ui,
                            "The last battery request failed. Showing the last known reading.",
                        );
                    }

                    ui.add_space(13.0);
                    ui.horizontal(|ui| {
                        if connected {
                            if widgets::accent_button(ui, "Check battery").clicked() {
                                check_clicked = true;
                            }
                            if is_bluetooth && !battery_fresh && !flip_pending && !restore_pending {
                                if widgets::ghost_button(ui, "Read now").clicked() {
                                    read_clicked = true;
                                }
                            }
                            if widgets::ghost_button(ui, "Bluetooth settings").clicked() {
                                bluetooth_clicked = true;
                            }
                            if flip_pending {
                                ui.label(
                                    RichText::new("Reading...")
                                        .size(11.0)
                                        .color(widgets::AMBER),
                                );
                            }
                        } else {
                            if widgets::accent_button(ui, "Pairing help").clicked() {
                                pairing_clicked = true;
                            }
                            if widgets::ghost_button(ui, "Open Bluetooth settings").clicked() {
                                bluetooth_clicked = true;
                            }
                        }
                    });

                    if flip_confirm && !flip_pending {
                        ui.add_space(8.0);
                        Frame::none()
                            .fill(widgets::ACCENT_DARK)
                            .stroke(Stroke::new(1.0_f32, widgets::BORDER))
                            .rounding(Rounding::same(9.0))
                            .inner_margin(Margin::symmetric(11.0, 9.0))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(
                                        "Reading the battery briefly disables Apple's scroll filter. Scrolling and the pointer pause for about 3 seconds, and Windows asks for administrator approval once. The filter is restored automatically.",
                                    )
                                    .size(11.2)
                                    .color(widgets::TEXT_SOFT),
                                );
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    if widgets::accent_button(ui, "Read now").clicked() {
                                        confirm_flip = true;
                                    }
                                    if widgets::ghost_button(ui, "Cancel").clicked() {
                                        cancel_flip = true;
                                    }
                                });
                            });
                    }
                });
            });
        });

        if check_clicked {
            self.check_battery(ui.ctx());
        }
        if read_clicked {
            self.flip_confirm = true;
        }
        if confirm_flip {
            self.start_flip(ui.ctx());
        }
        if cancel_flip {
            self.flip_confirm = false;
        }
        if pairing_clicked {
            self.tab = Tab::Faqs;
            self.faq_open[0] = true;
        }
        if bluetooth_clicked {
            self.open_target("ms-settings:bluetooth");
        }
    }

    fn scrolling_card(&mut self, ui: &mut Ui) {
        widgets::card().show(ui, |ui| {
            widgets::section_title(
                ui,
                "Scrolling",
                "Smooth wheel movement is injected with SendInput, so it works in every app.",
            );

            widgets::setting_toggle(
                ui,
                "Natural scrolling",
                "Move content in the same direction as your fingers.",
                &mut self.config.natural_scrolling,
            );
            widgets::hairline(ui);
            widgets::setting_toggle(
                ui,
                "Smooth scrolling",
                "Replace stepped wheel jumps with continuous motion.",
                &mut self.config.smooth_scrolling,
            );
            widgets::hairline(ui);
            widgets::setting_toggle(
                ui,
                "Inertia",
                "Let the page glide for a moment after you lift your finger.",
                &mut self.config.inertia,
            );
            widgets::hairline(ui);
            widgets::setting_slider(
                ui,
                "Scroll speed",
                "Scales the distance travelled by each finger movement.",
                &mut self.config.scroll_speed,
                0.25..=3.0,
                2,
                "x",
            );

            ui.add_space(8.0);
            Frame::none()
                .fill(widgets::ACCENT_DARK)
                .rounding(Rounding::same(9.0))
                .inner_margin(Margin::symmetric(12.0, 8.0))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(
                            "Windows keeps its native touch scrolling. This app adds smooth movement and gestures on top; a signed filter driver would be required to suppress the native stream.",
                        )
                        .size(11.5)
                        .color(widgets::ACCENT.gamma_multiply(0.92)),
                    );
                });
        });
    }

    fn gestures_card(&mut self, ui: &mut Ui) {
        widgets::card().show(ui, |ui| {
            widgets::section_title(
                ui,
                "Gestures",
                "Touch gestures are opt-in and only attempted over USB.",
            );
            widgets::setting_toggle(
                ui,
                "Experimental touch input",
                "Off by default. Raw HID reads are never attempted over Bluetooth, which keeps pointer input stable.",
                &mut self.config.experimental_input,
            );
            widgets::hairline(ui);
            widgets::setting_toggle(
                ui,
                "Middle click",
                "Click with one firm finger in the centre, or with two fingers anywhere.",
                &mut self.config.middle_click,
            );
            widgets::hairline(ui);
            widgets::setting_toggle(
                ui,
                "Two-finger swipe",
                "Swipe sideways to go Back or Forward in browsers and Explorer.",
                &mut self.config.two_finger_swipe,
            );

            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                widgets::small_muted(ui, "Swipe left:");
                widgets::keycap(ui, "Alt");
                widgets::keycap(ui, "Left");
                ui.add_space(10.0);
                widgets::small_muted(ui, "Swipe right:");
                widgets::keycap(ui, "Alt");
                widgets::keycap(ui, "Right");
            });
            ui.add_space(2.0);
            ui.label(
                RichText::new(
                    "The middle click is emulated from the touch surface. Physical left and right clicks keep working normally.",
                )
                .size(11.0)
                .color(widgets::FAINT),
            );
        });
    }

    fn background_card(&mut self, ui: &mut Ui) {
        widgets::card().show(ui, |ui| {
            widgets::section_title(
                ui,
                "Battery & background",
                "Alerts, startup behaviour and what happens when you close the window.",
            );
            widgets::setting_toggle(
                ui,
                "Low battery notifications",
                "Show a Windows toast when the battery drops below the threshold.",
                &mut self.config.low_battery_notifications,
            );

            let mut threshold = self.config.low_battery_threshold as f32;
            ui.add_space(2.0);
            widgets::setting_slider(
                ui,
                "Low battery threshold",
                "The level at which the warning appears.",
                &mut threshold,
                5.0..=50.0,
                0,
                "%",
            );
            let threshold = threshold.round().clamp(5.0, 50.0) as u8;
            if threshold != self.config.low_battery_threshold {
                self.config.low_battery_threshold = threshold;
            }

            widgets::hairline(ui);
            widgets::setting_toggle(
                ui,
                "Close to tray",
                "Keep gestures and battery monitoring active after closing the window.",
                &mut self.config.close_to_tray,
            );
            widgets::hairline(ui);
            widgets::setting_toggle(
                ui,
                "Start with Windows",
                "Launch quietly into the system tray when you sign in.",
                &mut self.config.start_with_windows,
            );

            if !self.tray_available {
                ui.add_space(4.0);
                widgets::small_muted(
                    ui,
                    "The tray icon could not be created, so closing the window will exit.",
                );
            }
        });
    }

    fn faqs(&mut self, ui: &mut Ui) {
        widgets::eyebrow(ui, "FAQs");
        widgets::page_heading(
            ui,
            "Questions, answered.",
            "Pairing, Secure Boot and the practical details of running this driver-free.",
        );
        ui.add_space(16.0);

        for index in 0..FAQ_QUESTIONS.len() {
            let mut open = self.faq_open[index];
            faq_card(
                ui,
                index,
                FAQ_QUESTIONS[index],
                &mut open,
                |ui| self.faq_answer(ui, index),
            );
            self.faq_open[index] = open;
        }
    }

    fn faq_answer(&mut self, ui: &mut Ui, index: usize) {
        match index {
            0 => {
                ui.label(
                    RichText::new(
                        "Windows can pair the Magic Mouse without any extra driver. Pairing enables pointer movement and clicks immediately. Reading Apple's touch surface is separate: Windows exposes raw touch reports to apps over USB, not over Bluetooth (see the Bluetooth question below).",
                    )
                    .size(12.6)
                    .color(widgets::TEXT_SOFT),
                );
                ui.add_space(8.0);
                help_step(
                    ui,
                    "1",
                    "Open Settings > Bluetooth & devices > Add device > Bluetooth.",
                );
                help_step(
                    ui,
                    "2",
                    "Turn the mouse off and on, then hold the power switch for about three seconds until the green LED blinks.",
                );
                help_step(
                    ui,
                    "3",
                    "Choose Magic Mouse in the list. If it does not appear, remove the old Apple Wireless Mouse entry and try again.",
                );
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    if widgets::ghost_button(ui, "Open Bluetooth settings").clicked() {
                        self.open_target("ms-settings:bluetooth");
                    }
                    widgets::small_muted(ui, "No pairing mode? Charge the mouse first.");
                });
            }
            1 => {
                ui.label(
                    RichText::new(
                        "Yes. This application is a user-mode HID client. It does not install a kernel driver, request test signing or ask you to disable Secure Boot.",
                    )
                    .size(12.6)
                    .color(widgets::TEXT_SOFT),
                );
                ui.add_space(7.0);
                widgets::bullet_line(ui, "Secure Boot and Memory Integrity can stay enabled.");
                widgets::bullet_line(
                    ui,
                    "No administrator prompt is needed for normal use, pairing or battery checks.",
                );
                widgets::bullet_line(
                    ui,
                    "Company device policies can still block raw HID access; this app cannot override them.",
                );
                widgets::bullet_line(
                    ui,
                    "Windows' Bluetooth HID restriction is independent of Secure Boot and cannot be bypassed from user mode.",
                );
            }
            2 => {
                ui.label(
                    RichText::new(
                        "Windows reserves Bluetooth mouse collections for its own input stack. An app can open the device, but read requests are denied, so this app cannot see the touch surface or send the multi-touch feature report while the mouse is connected over Bluetooth.",
                    )
                    .size(12.6)
                    .color(widgets::TEXT_SOFT),
                );
                ui.add_space(7.0);
                widgets::bullet_line(
                    ui,
                    "Connect the Magic Mouse with its charging cable (USB) to unlock smooth scrolling, middle click and two-finger swipes.",
                );
                widgets::bullet_line(
                    ui,
                    "Over Bluetooth the app stays connected, watches for disconnects and attempts battery readings only.",
                );
                widgets::bullet_line(
                    ui,
                    "If Apple's Boot Camp HID driver is installed, native scrolling may already work over Bluetooth; this app leaves that stream untouched.",
                );
                widgets::bullet_line(
                    ui,
                    "This is a Windows platform limitation, not a hidden setting. A driver-free portable app cannot bypass it, and suppressing native scrolling would also require a signed kernel filter.",
                );
            }
            3 => {
                ui.label(
                    RichText::new(
                        "The level is read from Apple's HID battery report. Many firmware revisions report exact percentages, while others round to 5% steps. A reading of 0 is treated as unavailable.",
                    )
                    .size(12.6)
                    .color(widgets::TEXT_SOFT),
                );
                ui.add_space(7.0);
                ui.label(
                    RichText::new(
                        "On Bluetooth, Apple's scroll filter hides the vendor collection that carries the battery report. The app keeps the last confirmed value and shows it as LAST. Read now briefly removes the filter, reads the report, and restores the filter in a finally block; scrolling pauses for a few seconds and Windows asks for administrator approval. The result is verified before the app says scroll mode is back.",
                    )
                    .size(12.6)
                    .color(widgets::TEXT_SOFT),
                );
                ui.add_space(7.0);
                ui.label(
                    RichText::new(
                        "Charging is estimated from the USB transport or a rising battery trend, because Apple does not expose a documented charging bit. Treat the charging badge as a hint, not a measurement.",
                    )
                    .size(12.6)
                    .color(widgets::TEXT_SOFT),
                );
            }
            4 => {
                ui.label(
                    RichText::new(
                        "Magic Mouse (1st generation, product 0x0269), Magic Mouse 2 (0x030D) and the USB-C Magic Mouse (0x0323) are detected over Bluetooth and USB. Full touch features require raw HID access, which Windows normally provides over USB.",
                    )
                    .size(12.6)
                    .color(widgets::TEXT_SOFT),
                );
                ui.add_space(7.0);
                widgets::bullet_line(
                    ui,
                    "The device must appear as an Apple HID collection in Windows.",
                );
                widgets::bullet_line(
                    ui,
                    "Bluetooth connections are detected, but Windows may expose them only in restricted driver mode.",
                );
                widgets::bullet_line(
                    ui,
                    "Clones and keyboards with different product IDs are ignored on purpose.",
                );
                widgets::bullet_line(
                    ui,
                    "If another Magic Mouse utility is running, close it so both apps do not compete for the same HID interface.",
                );
            }
            _ => {
                ui.label(
                    RichText::new(
                        "After sleep or a Bluetooth reconnect, Windows may withdraw raw HID access. If the app shows Limited mode, touch gestures and the battery are unavailable until you reconnect over USB or the system driver releases the collection. The daemon keeps retrying in the background.",
                    )
                    .size(12.6)
                    .color(widgets::TEXT_SOFT),
                );
                ui.add_space(7.0);
                help_step(ui, "1", "Wake the mouse and wait a few seconds.");
                help_step(
                    ui,
                    "2",
                    "Connect the charging cable to switch to the USB HID collection.",
                );
                help_step(
                    ui,
                    "3",
                    "Close other Magic Mouse or trackpad utilities, then reopen this app.",
                );
                ui.add_space(4.0);
                widgets::small_muted(
                    ui,
                    "Pointer movement and physical clicks always keep working, even if touch reports are unavailable.",
                );
            }
        }
    }

    fn help(&mut self, ui: &mut Ui) {
        let scale = (ui.ctx().pixels_per_point() * 100.0).round() as i32;
        let log_path = logging::log_file();
        let settings_path = config::config_path();
        let data_dir = config::data_dir();

        widgets::eyebrow(ui, "Guide");
        widgets::page_heading(
            ui,
            "Help & support.",
            "Get connected, understand the limits and find every file the app writes.",
        );
        ui.add_space(16.0);

        Frame::none()
            .fill(Color32::from_rgb(15, 36, 32))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(39, 82, 72)))
            .rounding(Rounding::same(16.0))
            .inner_margin(Margin::same(20.0))
            .show(ui, |ui| {
                widgets::eyebrow(ui, "Quick start");
                ui.add_space(2.0);
                ui.label(
                    RichText::new("Set up in under a minute.")
                        .size(21.0)
                        .strong()
                        .color(widgets::TEXT),
                );
                ui.add_space(10.0);
                help_step(
                    ui,
                    "1",
                    "Pair the Magic Mouse in Windows Bluetooth settings, or connect its USB cable for full touch gestures.",
                );
                help_step(
                    ui,
                    "2",
                    "Leave Magic Mouse Control running in the tray. It reconnects automatically after sleep.",
                );
                help_step(
                    ui,
                    "3",
                    "Tune scrolling and gestures on the Features page. Changes apply as soon as raw HID access is available.",
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if widgets::accent_button(ui, "Open Bluetooth settings").clicked() {
                        self.open_target("ms-settings:bluetooth");
                    }
                    if widgets::ghost_button(ui, "Check battery").clicked() {
                        self.check_battery(ui.ctx());
                    }
                });
            });

        ui.add_space(14.0);
        widgets::card().show(ui, |ui| {
            widgets::section_title(
                ui,
                "Start with Windows",
                "A per-user registry entry, no scheduled task and no elevation.",
            );
            widgets::setting_toggle(
                ui,
                "Launch at sign-in",
                "Starts minimized in the system tray with the --minimized flag.",
                &mut self.config.start_with_windows,
            );
            ui.add_space(5.0);
            widgets::inset_card().show(ui, |ui| {
                ui.label(
                    RichText::new(r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run")
                        .font(FontId::new(10.5, FontFamily::Monospace))
                        .color(widgets::TEXT_SOFT),
                );
                ui.add_space(2.0);
                widgets::small_muted(
                    ui,
                    "The entry points at the current executable and is updated when you move the app.",
                );
            });
        });

        ui.add_space(14.0);
        widgets::card().show(ui, |ui| {
            widgets::section_title(
                ui,
                "Known limitations",
                "What this app can and cannot do without a driver.",
            );
            widgets::bullet_line(
                ui,
                "Windows reserves Bluetooth mouse collections for its own input stack. Raw touch and multi-touch feature reports normally work only over USB.",
            );
            widgets::bullet_line(
                ui,
                "Native touch scrolling cannot be suppressed. Over USB, smooth scrolling is added on top of it, so some apps may feel doubled.",
            );
            widgets::bullet_line(
                ui,
                "Charging state is estimated; Apple's HID reports do not expose a reliable bit.",
            );
            widgets::bullet_line(
                ui,
                "Two-finger swipes send Alt+Left/Right, so they only navigate in apps that support those shortcuts.",
            );
            widgets::bullet_line(
                ui,
                "A firm one-finger touch in the centre can trigger a middle click by design.",
            );
            widgets::bullet_line(
                ui,
                "Other Magic Mouse utilities and Apple's Boot Camp filter driver may hold the HID interface open. Close them if gestures stop.",
            );
        });

        ui.add_space(14.0);
        widgets::card().show(ui, |ui| {
            widgets::section_title(
                ui,
                "Display and files",
                "High-DPI aware at every scale, with a small footprint on disk.",
            );
            ui.horizontal(|ui| {
                widgets::pill(ui, &format!("{scale}% interface scale"), widgets::BLUE);
                widgets::pill(ui, "No telemetry", widgets::ACCENT);
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "The window follows the Windows scale factor and renders at native pixel density, so text and the mouse preview stay sharp on high-DPI and Retina-class displays.",
                )
                .size(12.2)
                .color(widgets::MUTED),
            );
            ui.add_space(10.0);
            Frame::none()
                .fill(Color32::from_rgb(16, 21, 28))
                .stroke(Stroke::new(1.0_f32, widgets::BORDER_SOFT))
                .rounding(Rounding::same(10.0))
                .inner_margin(Margin::same(12.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Settings")
                                .size(12.5)
                                .strong()
                                .color(widgets::TEXT_SOFT),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::link_button(ui, "Open folder").clicked() {
                                self.open_folder(&data_dir);
                            }
                        });
                    });
                    ui.add(
                        Label::new(
                            RichText::new(settings_path.display().to_string())
                                .font(FontId::new(10.0, FontFamily::Monospace))
                                .color(widgets::MUTED),
                        )
                        .wrap(),
                    );
                });
            ui.add_space(8.0);
            Frame::none()
                .fill(Color32::from_rgb(16, 21, 28))
                .stroke(Stroke::new(1.0_f32, widgets::BORDER_SOFT))
                .rounding(Rounding::same(10.0))
                .inner_margin(Margin::same(12.0))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Log file")
                                .size(12.5)
                                .strong()
                                .color(widgets::TEXT_SOFT),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::link_button(ui, "Open log folder").clicked() {
                                self.open_folder(&data_dir);
                            }
                        });
                    });
                    ui.add(
                        Label::new(
                            RichText::new(
                                log_path
                                    .as_ref()
                                    .map(|path| path.display().to_string())
                                    .unwrap_or_else(|| {
                                        format!("{}\\magic-mouse.log", data_dir.display())
                                    }),
                            )
                            .font(FontId::new(10.0, FontFamily::Monospace))
                            .color(widgets::MUTED),
                        )
                        .wrap(),
                    );
                });
        });

        ui.add_space(14.0);
        widgets::card().show(ui, |ui| {
            widgets::section_title(
                ui,
                "Links and about",
                "Free, open source and not affiliated with Apple.",
            );
            ui.horizontal_wrapped(|ui| {
                if widgets::link_button(ui, "Apple Magic Mouse support").clicked() {
                    self.open_target("https://support.apple.com/magic-mouse");
                }
                if widgets::link_button(ui, "Windows mouse settings").clicked() {
                    self.open_target("ms-settings:mousetouchpad");
                }
                if widgets::link_button(ui, "Bluetooth settings").clicked() {
                    self.open_target("ms-settings:bluetooth");
                }
            });
            ui.add_space(7.0);
            ui.horizontal(|ui| {
                widgets::pill(
                    ui,
                    &format!("Version {}", env!("CARGO_PKG_VERSION")),
                    widgets::ACCENT,
                );
                widgets::pill(ui, "MIT License", widgets::BLUE);
                widgets::pill(ui, "Rust + egui", widgets::AMBER);
            });
            ui.add_space(7.0);
            ui.label(
                RichText::new(
                    "Not affiliated with Apple Inc. Magic Mouse is a trademark of Apple Inc. This utility talks to the standard Windows HID stack and ships no Apple code.",
                )
                .size(10.8)
                .color(widgets::FAINT),
            );
        });
    }

    fn open_target(&mut self, target: &str) {
        let result = if target.starts_with("ms-settings:") {
            std::process::Command::new("explorer.exe").arg(target).spawn()
        } else {
            std::process::Command::new("rundll32.exe")
                .args(["url.dll,FileProtocolHandler", target])
                .spawn()
        };
        if let Err(error) = result {
            self.show_error(format!("Could not open {target}: {error}"));
        }
    }

    fn open_folder(&mut self, path: &Path) {
        if let Err(error) = std::process::Command::new("explorer.exe").arg(path).spawn() {
            self.show_error(format!("Could not open {}: {error}", path.display()));
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Some winit/eframe combinations create the first viewport hidden even
        // though `with_visible(true)` was requested. Make one explicit request
        // on the first frame, but leave autostart launches in the tray.
        if !self.start_minimized && !self.startup_shown {
            self.startup_shown = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }

        self.handle_ui_events(ctx);
        self.handle_close_request(ctx);

        // Do not lay out or paint an invisible window. Tray events and HID
        // events still wake the app, and settings are still saved. This keeps
        // idle CPU near zero when the app lives in the tray.
        let minimized = ctx.input(|input| input.viewport().minimized.unwrap_or(false));
        if self.window_hidden || minimized {
            self.after_ui(ctx);
            self.refresh_tray();
            self.update_async_ui(ctx, false);
            return;
        }

        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(226.0)
            .frame(
                Frame::none()
                    .fill(widgets::SIDEBAR)
                    .inner_margin(Margin::symmetric(16.0, 18.0)),
            )
            .show(ctx, |ui| self.sidebar(ui));

        egui::CentralPanel::default()
            .frame(
                Frame::none()
                    .fill(widgets::BG)
                    .inner_margin(Margin {
                        left: 28.0,
                        right: 28.0,
                        top: 18.0,
                        bottom: 16.0,
                    }),
            )
            .show(ctx, |ui| {
                self.top_bar(ui);
                ui.add_space(10.0);
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.page(ui);
                        ui.add_space(26.0);
                    });
            });

        self.after_ui(ctx);
        self.refresh_tray();
        self.update_async_ui(ctx, true);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Always persist the latest values, even if the debounce timer had not
        // fired yet when the user chose Exit.
        if self.config != self.saved_config {
            if let Err(error) = self.config.save() {
                logging::error(&format!("Could not save settings on exit: {error}"));
            }
        }
        self.hid.take();
        TrayIconEvent::set_event_handler::<fn(TrayIconEvent)>(None);
        MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
        logging::info("Magic Mouse Control shutting down.");
    }
}

/// Create the tray icon and menu on the egui/winit thread. Both callbacks send
/// into `ui_tx`; the App decides what to do and wakes the window with a
/// `request_repaint`.
fn create_tray(
    ctx: &egui::Context,
    ui_tx: &Sender<UiEvent>,
    autostart_checked: bool,
) -> Result<(TrayIcon, CheckMenuItem), String> {
    let menu = Menu::new();
    let open = MenuItem::with_id("open", "Open Settings", true, None);
    let check = MenuItem::with_id("check", "Check Battery", true, None);
    let autostart_item = CheckMenuItem::with_id(
        "autostart",
        "Start with Windows",
        true,
        autostart_checked,
        None,
    );
    let separator = PredefinedMenuItem::separator();
    let quit = MenuItem::with_id("exit", "Exit", true, None);
    menu.append_items(&[&open, &check, &autostart_item, &separator, &quit])
        .map_err(|error| format!("Could not build the tray menu: {error}"))?;

    let icon = Icon::from_rgba(icons::tray_icon_rgba(32, false, None, false), 32, 32)
        .map_err(|error| format!("Could not render the tray icon: {error}"))?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip("Magic Mouse: Starting...")
        .with_menu_on_left_click(false)
        .build()
        .map_err(|error| format!("Could not create the tray icon: {error}"))?;

    let menu_tx = ui_tx.clone();
    let menu_ctx = ctx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let ui_event = match event.id.0.as_str() {
            "open" => Some(UiEvent::TrayOpen),
            "check" => Some(UiEvent::TrayCheck),
            "autostart" => Some(UiEvent::TrayAutostart),
            "exit" => Some(UiEvent::TrayExit),
            _ => None,
        };
        if let Some(event) = ui_event {
            if menu_tx.send(event).is_ok() {
                menu_ctx.request_repaint();
            }
        }
    }));

    let tray_tx = ui_tx.clone();
    let tray_ctx = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let open = matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            }
        );
        if open && tray_tx.send(UiEvent::TrayOpen).is_ok() {
            tray_ctx.request_repaint();
        }
    }));

    Ok((tray, autostart_item))
}

fn battery_color(percent: u8, threshold: u8) -> Color32 {
    if percent <= threshold {
        widgets::RED
    } else if percent <= 30 {
        widgets::AMBER
    } else {
        widgets::ACCENT
    }
}

fn battery_bar(ui: &mut Ui, percent: u8, color: Color32) {
    let width = ui.available_width().clamp(80.0, 330.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 8.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, Rounding::same(4.0), Color32::from_rgb(32, 40, 49));
    if percent > 0 {
        let width = rect.width() * (percent.min(100) as f32 / 100.0);
        painter.rect_filled(
            Rect::from_min_size(rect.min, Vec2::new(width, rect.height())),
            Rounding::same(4.0),
            color,
        );
    }
}

/// An accordion card used by the FAQ page.
fn faq_card(
    ui: &mut Ui,
    index: usize,
    question: &str,
    open: &mut bool,
    answer: impl FnOnce(&mut Ui),
) {
    ui.push_id(("faq", index), |ui| {
        Frame::none()
            .fill(widgets::CARD)
            .stroke(Stroke::new(1.0_f32, widgets::BORDER))
            .rounding(Rounding::same(12.0))
            .inner_margin(Margin::symmetric(16.0, 4.0))
            .show(ui, |ui| {
                let (rect, response) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), 50.0),
                    Sense::click(),
                );
                let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
                if response.clicked() {
                    *open = !*open;
                }
                let painter = ui.painter();
                if response.hovered() {
                    painter.rect_filled(
                        rect,
                        Rounding::same(8.0),
                        Color32::from_white_alpha(3),
                    );
                }
                painter.text(
                    rect.left_center(),
                    egui::Align2::LEFT_CENTER,
                    question,
                    FontId::new(14.2, FontFamily::Proportional),
                    widgets::TEXT,
                );
                painter.text(
                    rect.right_center(),
                    egui::Align2::RIGHT_CENTER,
                    if *open { "-" } else { "+" },
                    FontId::new(18.0, FontFamily::Proportional),
                    if *open { widgets::ACCENT } else { widgets::MUTED },
                );

                if *open {
                    ui.add_space(2.0);
                    let line_rect = ui.available_rect_before_wrap();
                    ui.painter().line_segment(
                        [
                            egui::pos2(line_rect.left(), line_rect.top()),
                            egui::pos2(line_rect.right(), line_rect.top()),
                        ],
                        Stroke::new(1.0_f32, widgets::BORDER_SOFT),
                    );
                    ui.add_space(12.0);
                    answer(ui);
                    ui.add_space(12.0);
                }
            });
    });
    ui.add_space(8.0);
}

/// Numbered step with a small accent badge, used in the FAQ and Help pages.
fn help_step(ui: &mut Ui, number: &str, text: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 9.0, widgets::ACCENT_DARK);
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            number,
            FontId::new(10.0, FontFamily::Proportional),
            widgets::ACCENT,
        );
        ui.add_space(2.0);
        ui.vertical(|ui| {
            ui.add(
                Label::new(
                    RichText::new(text)
                        .size(12.5)
                        .color(widgets::TEXT_SOFT),
                )
                .wrap(),
            );
        });
    });
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_colours_follow_the_threshold() {
        assert_eq!(battery_color(10, 15), widgets::RED);
        assert_eq!(battery_color(25, 15), widgets::AMBER);
        assert_eq!(battery_color(80, 15), widgets::ACCENT);
    }

    #[test]
    fn faq_array_matches_questions() {
        assert_eq!(FAQ_QUESTIONS.len(), 6);
    }
}
