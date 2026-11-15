//! Background HID worker for Apple Magic Mouse.
//!
//! The worker opens the relevant HID top-level collections, switches the mouse
//! into multi-touch mode, decodes touch reports, polls the battery and forwards
//! a small set of events to the UI. It is intentionally a plain blocking
//! thread: HID reads are blocking on Windows and this keeps the UI completely
//! independent from device timeouts. Reconnection after sleep, Bluetooth
//! drops and Windows driver restarts happens in the same loop.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hidapi::{BusType, DeviceInfo, HidApi, HidDevice, HidError, MAX_REPORT_DESCRIPTOR_SIZE};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config::Config;
use crate::logging;
use crate::mouse_simulator::MouseSimulator;

pub const APPLE_BLUETOOTH_VID: u16 = 0x004C;
pub const APPLE_USB_VID: u16 = 0x05AC;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseModel {
    MagicMouse1,
    MagicMouse2,
    MagicMouseUsbC,
}

impl MouseModel {
    pub fn from_product_id(product_id: u16) -> Option<Self> {
        // Product IDs follow the Linux hid-magicmouse driver and Apple's
        // Bluetooth/USB descriptors: 0x030D is the first-generation Magic
        // Mouse, 0x0269 is the Magic Mouse 2 and 0x0323 is the USB-C model.
        match product_id {
            0x030D => Some(Self::MagicMouse1),
            0x0269 => Some(Self::MagicMouse2),
            0x0323 => Some(Self::MagicMouseUsbC),
            _ => None,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::MagicMouse1 => "Magic Mouse",
            Self::MagicMouse2 => "Magic Mouse 2",
            Self::MagicMouseUsbC => "Magic Mouse (USB-C)",
        }
    }


}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Bluetooth,
    Usb,
    Other,
}

impl Transport {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bluetooth => "Bluetooth",
            Self::Usb => "USB cable",
            Self::Other => "HID",
        }
    }

    fn charging_is_likely(self) -> bool {
        matches!(self, Self::Usb)
    }
}

#[derive(Clone, Debug)]
pub struct TouchPoint {
    pub id: u8,
    pub x: i32,
    pub y: i32,
    pub size: u8,
}

#[derive(Clone, Debug, Default)]
pub struct TouchReport {
    pub points: Vec<TouchPoint>,
    pub left: bool,
    pub right: bool,
}

#[derive(Clone, Debug)]
pub enum HidEvent {
    /// No matching device is currently present, or the daemon lost it.
    Searching,
    Connected {
        name: String,
        model: MouseModel,
        transport: Transport,
        touch_capable: bool,
        /// Windows refused raw HID input reads. This normally happens for
        /// Bluetooth mice, which are reserved for the system input stack.
        input_blocked: bool,
    },
    Disconnected,
    Battery {
        percent: u8,
        /// Best-effort only: Apple does not expose a reliable charging bit.
        charging: bool,
    },
    BatteryUnavailable,
    Touch {
        points: Vec<TouchPoint>,
    },
}

#[derive(Clone, Debug)]
pub enum HidCommand {
    UpdateConfig(Config),
    PollBattery,
    Shutdown,
}

pub struct HidHandle {
    sender: UnboundedSender<HidCommand>,
    join: Option<JoinHandle<()>>,
}

impl HidHandle {
    pub fn update_config(&self, config: &Config) -> Result<(), String> {
        self.sender
            .send(HidCommand::UpdateConfig(config.clone()))
            .map_err(|_| "The background HID worker has stopped.".to_string())
    }

    pub fn poll_battery(&self) -> Result<(), String> {
        self.sender
            .send(HidCommand::PollBattery)
            .map_err(|_| "The background HID worker has stopped.".to_string())
    }
}

impl Drop for HidHandle {
    fn drop(&mut self) {
        let _ = self.sender.send(HidCommand::Shutdown);
        // Detach instead of joining. A Bluetooth HID read can take a moment
        // and the UI must never wait for it when the user closes the app.
        // The process exit reclaims the worker thread.
        let _ = self.join.take();
    }
}

pub fn spawn(
    config: Config,
    events: UnboundedSender<HidEvent>,
) -> Result<HidHandle, String> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let join = thread::Builder::new()
        .name("magic-mouse-hid".to_string())
        .spawn(move || {
            let mut daemon = Daemon::new(config, receiver, events);
            daemon.run();
        })
        .map_err(|error| format!("Could not start the HID background thread: {error}"))?;
    Ok(HidHandle {
        sender,
        join: Some(join),
    })
}

fn map_bus(bus: BusType) -> Transport {
    match bus {
        BusType::Bluetooth => Transport::Bluetooth,
        BusType::Usb => Transport::Usb,
        _ => Transport::Other,
    }
}

fn matching_info(info: &DeviceInfo) -> Option<MouseModel> {
    if info.vendor_id() != APPLE_BLUETOOTH_VID && info.vendor_id() != APPLE_USB_VID {
        return None;
    }
    MouseModel::from_product_id(info.product_id())
}

#[derive(Default, Debug)]
struct DescriptorInfo {
    input_reports: BTreeSet<u8>,
    feature_reports: BTreeSet<u8>,
    output_reports: BTreeSet<u8>,
    usage_pages: BTreeSet<u16>,
}

impl DescriptorInfo {
    fn has_touch_report(&self) -> bool {
        [0x12, 0x27, 0x29]
            .iter()
            .any(|id| self.input_reports.contains(id))
    }

    fn has_battery_feature(&self) -> bool {
        self.feature_reports.contains(&0x47)
    }

    fn has_battery_input(&self) -> bool {
        self.input_reports.contains(&0x90)
    }

    fn has_vendor_page(&self) -> bool {
        self.usage_pages.contains(&0xFF00)
    }
}

/// Minimal HID report-descriptor scanner. We only need report IDs, report
/// types and usage pages; a full parser would add complexity and new failure
/// modes without helping the driver.
fn parse_descriptor(data: &[u8]) -> DescriptorInfo {
    let mut info = DescriptorInfo::default();
    let mut usage_page: u16 = 0;
    let mut report_id: u8 = 0;
    let mut index = 0usize;

    while index < data.len() {
        let prefix = data[index];
        index += 1;

        if prefix == 0xFE {
            // Long item: FE, data size, long item tag, data.
            if index + 2 > data.len() {
                break;
            }
            let size = data[index] as usize;
            index += 2;
            index = index.saturating_add(size);
            continue;
        }

        let size = match prefix & 0x03 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        if index + size > data.len() {
            break;
        }
        let mut value = 0u32;
        for (shift, byte) in data[index..index + size].iter().enumerate() {
            value |= (*byte as u32) << (8 * shift);
        }
        index += size;

        let item_type = (prefix >> 2) & 0x03;
        let tag = prefix >> 4;
        match item_type {
            1 => match tag {
                0x0 => {
                    usage_page = value as u16;
                    info.usage_pages.insert(usage_page);
                }
                0x8 => report_id = value as u8,
                _ => {}
            },
            0 => match tag {
                0x8 => {
                    info.input_reports.insert(report_id);
                }
                0x9 => {
                    info.output_reports.insert(report_id);
                }
                0xB => {
                    info.feature_reports.insert(report_id);
                }
                _ => {}
            },
            _ => {}
        }
        let _ = usage_page;
    }

    info
}

#[derive(Clone, Debug)]
struct FoundInterface {
    path: CString,
    model: MouseModel,
    name: String,
    transport: Transport,
    has_touch: bool,
    has_battery_feature: bool,
    has_battery_input: bool,
    has_vendor_page: bool,
    interface_number: i32,
    usage_page: u16,
    usage: u16,
}

impl FoundInterface {
    fn primary_score(&self) -> u32 {
        let mut score = 0;
        if self.has_touch {
            score += 1000;
        }
        if self.has_battery_input {
            score += 100;
        }
        if self.has_battery_feature {
            score += 50;
        }
        if self.has_vendor_page {
            score += 2;
        }
        score
    }

    fn battery_score(&self) -> u32 {
        let mut score = 0;
        if self.has_battery_input {
            score += 100;
        }
        if self.has_battery_feature {
            score += 50;
        }
        if self.has_vendor_page {
            score += 2;
        }
        score
    }

    fn is_battery_candidate(&self) -> bool {
        self.has_battery_feature || self.has_battery_input || self.has_vendor_page
    }
}

struct Discovery {
    interfaces: Vec<FoundInterface>,
    /// Set when no descriptor explicitly advertised touch, so the daemon must
    /// be optimistic and still try the multi-touch enable request.
    descriptor_touch_known: bool,
}

fn discover(api: &mut HidApi) -> Discovery {
    let _ = api.refresh_devices();
    let devices: Vec<DeviceInfo> = api
        .device_list()
        .filter(|info| matching_info(info).is_some())
        .cloned()
        .collect();

    let mut interfaces = Vec::new();
    let mut descriptor_touch_known = false;

    for info in devices {
        let Some(model) = matching_info(&info) else {
            continue;
        };
        let Ok(device) = api.open_path(info.path()) else {
            continue;
        };
        let mut descriptor = vec![0u8; MAX_REPORT_DESCRIPTOR_SIZE];
        let descriptor_info = match device.get_report_descriptor(&mut descriptor) {
            Ok(length) if length > 0 => parse_descriptor(&descriptor[..length]),
            _ => DescriptorInfo::default(),
        };
        let has_touch = descriptor_info.has_touch_report();
        if has_touch {
            descriptor_touch_known = true;
        }
        let name = info
            .product_string()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| model.title())
            .to_string();
        interfaces.push(FoundInterface {
            path: info.path().to_owned(),
            model,
            name,
            transport: map_bus(info.bus_type()),
            has_touch,
            has_battery_feature: descriptor_info.has_battery_feature(),
            has_battery_input: descriptor_info.has_battery_input(),
            has_vendor_page: descriptor_info.has_vendor_page(),
            interface_number: info.interface_number(),
            usage_page: info.usage_page(),
            usage: info.usage(),
        });
    }

    Discovery {
        interfaces,
        descriptor_touch_known,
    }
}

struct Connection {
    primary: HidDevice,
    battery: Option<HidDevice>,
    battery_has_input: bool,
    path: CString,
    name: String,
    model: MouseModel,
    transport: Transport,
    touch_capable: bool,
    input_blocked: bool,
}

/// Windows reserves Bluetooth mouse collections for the system input stack.
/// hidapi can open the handle but `ReadFile` then fails with access denied.
pub(crate) fn is_access_denied(error: &HidError) -> bool {
    match error {
        HidError::IoError { error } => error.raw_os_error() == Some(5),
        HidError::HidApiError { message } => {
            message.contains("Access is denied") || message.contains("0x00000005")
        }
        _ => false,
    }
}

/// Probe the collection once before enabling multi-touch. A successful read
/// (including a timeout with no data) means user-mode input reports are
/// available. Access denied is the expected result over Bluetooth.
fn probe_input_access(device: &HidDevice) -> bool {
    let mut buffer = [0u8; 64];
    match device.read_timeout(&mut buffer, 80) {
        Ok(_) => false,
        Err(error) => is_access_denied(&error),
    }
}

#[derive(Default)]
struct BatteryTrend {
    last_percent: Option<u8>,
    charging: bool,
    logged_failure: bool,
}

impl BatteryTrend {
    fn reset(&mut self) {
        self.last_percent = None;
        self.charging = false;
        self.logged_failure = false;
    }

    fn update(&mut self, percent: u8, transport: Transport) -> bool {
        let charging = if percent >= 100 {
            false
        } else if transport.charging_is_likely() {
            true
        } else if let Some(previous) = self.last_percent {
            if percent > previous {
                true
            } else if percent < previous {
                false
            } else {
                self.charging
            }
        } else {
            false
        };
        self.last_percent = Some(percent);
        self.charging = charging;
        charging
    }
}

struct Daemon {
    config: Config,
    commands: UnboundedReceiver<HidCommand>,
    events: UnboundedSender<HidEvent>,
    simulator: MouseSimulator,
    api: Option<HidApi>,
    shutdown: bool,
    pending_battery_poll: bool,
    battery_trend: BatteryTrend,
    battery_logged_percent: Option<u8>,
    /// Last percentage the mouse reported. Kept across reconnects so a short
    /// Bluetooth dropout does not turn the UI into "Unknown".
    last_battery: Option<u8>,
    last_touch_emit: Option<Instant>,
    last_touch_count: usize,
    last_touch_was_empty: bool,
}

impl Daemon {
    fn new(
        config: Config,
        commands: UnboundedReceiver<HidCommand>,
        events: UnboundedSender<HidEvent>,
    ) -> Self {
        Self {
            simulator: MouseSimulator::new(config.clone()),
            config,
            commands,
            events,
            api: None,
            shutdown: false,
            pending_battery_poll: false,
            battery_trend: BatteryTrend::default(),
            battery_logged_percent: None,
            last_battery: None,
            last_touch_emit: None,
            last_touch_count: 0,
            last_touch_was_empty: true,
        }
    }

    fn send(&mut self, event: HidEvent) -> bool {
        let result = self.events.send(event);
        if result.is_err() {
            self.shutdown = true;
            false
        } else {
            true
        }
    }

    fn run(&mut self) {
        let mut searching_sent = false;
        while !self.shutdown {
            self.drain_idle_commands();
            if self.shutdown {
                break;
            }

            if self.api.is_none() {
                match HidApi::new() {
                    Ok(api) => self.api = Some(api),
                    Err(error) => {
                        logging::error(&format!("Could not initialise HIDAPI: {error}"));
                        self.sleep_with_commands(Duration::from_secs(2));
                        continue;
                    }
                }
            }

            let Some(api) = self.api.as_mut() else {
                continue;
            };
            let discovery = discover(api);
            if discovery.interfaces.is_empty() {
                if !searching_sent {
                    self.send(HidEvent::Searching);
                    logging::info("No matching Apple Magic Mouse HID interface found; waiting.");
                    searching_sent = true;
                }
                self.sleep_with_commands(Duration::from_millis(900));
                continue;
            }

            match self.open_connection(&discovery) {
                Ok(connection) => {
                    searching_sent = false;
                    self.battery_trend.reset();
                    self.battery_logged_percent = None;
                    self.last_touch_emit = None;
                    self.last_touch_count = 0;
                    self.last_touch_was_empty = true;
                    self.simulator = MouseSimulator::new(self.config.clone());
                    self.send(HidEvent::Connected {
                        name: connection.name.clone(),
                        model: connection.model,
                        transport: connection.transport,
                        touch_capable: connection.touch_capable,
                        input_blocked: connection.input_blocked,
                    });
                    // Keep showing the last known level while the first fresh
                    // request is in flight. A reconnect no longer makes the
                    // battery jump to "Unknown".
                    if let Some(percent) = self.last_battery {
                        self.report_battery(percent, connection.transport);
                    }
                    logging::info(&format!(
                        "Connected to {} [{:?}, {}, touch={}, input_blocked={}]",
                        connection.name,
                        connection.model,
                        connection.transport.label(),
                        connection.touch_capable,
                        connection.input_blocked
                    ));
                    self.session(connection);
                    if !self.shutdown {
                        self.send(HidEvent::Disconnected);
                        logging::info("Mouse connection ended; entering reconnect loop.");
                    }
                    self.api = None;
                }
                Err(error) => {
                    logging::warn(&format!(
                        "Could not open the Magic Mouse HID interface: {error}"
                    ));
                    self.api = None;
                    if !searching_sent {
                        self.send(HidEvent::Searching);
                        searching_sent = true;
                    }
                    self.sleep_with_commands(Duration::from_millis(1200));
                }
            }
        }
    }

    fn open_connection(&mut self, discovery: &Discovery) -> Result<Connection, String> {
        let primary = discovery
            .interfaces
            .iter()
            .max_by_key(|interface| interface.primary_score())
            .ok_or_else(|| "No usable interface".to_string())?;

        // Prefer a separate battery collection when one exists. This is the
        // USB-C vendor collection (usage page 0xFF00, input report 0x90).
        let battery_interface = discovery
            .interfaces
            .iter()
            .filter(|interface| interface.path.as_c_str() != primary.path.as_c_str())
            .filter(|interface| interface.is_battery_candidate())
            .filter(|interface| interface.battery_score() > primary.battery_score() / 2)
            .max_by_key(|interface| interface.battery_score());

        let api = self
            .api
            .as_ref()
            .ok_or_else(|| "HID API is not initialised".to_string())?;
        let battery_has_input = battery_interface
            .map(|interface| interface.has_battery_input)
            .unwrap_or(false);
        let primary_device = api
            .open_path(&primary.path)
            .map_err(|error| format!("primary interface: {error}"))?;
        let battery_device = battery_interface.and_then(|interface| {
            match api.open_path(&interface.path) {
                Ok(device) => {
                    logging::info(&format!(
                        "Opened battery HID interface (usage {:04X}/{:04X}, interface {}).",
                        interface.usage_page, interface.usage, interface.interface_number
                    ));
                    Some(device)
                }
                Err(error) => {
                    logging::warn(&format!("Could not open battery HID interface: {error}"));
                    None
                }
            }
        });

        // Safe monitoring is the default. We deliberately never read the
        // primary mouse collection unless the user opted into experimental
        // input and the mouse is on USB. Bluetooth collections belong to the
        // Windows input stack; probing them has no benefit and can disturb
        // pointer input on some systems.
        let input_blocked = if !self.config.experimental_input {
            logging::info(
                "Safe monitoring mode: touch gestures are disabled; watching connection and battery only.",
            );
            true
        } else {
            // EXPERIMENT: Bluetooth is probed too. Windows normally refuses
            // raw reads on a Bluetooth mouse collection; if it does, the
            // probe reports "blocked" and we fall back to safe monitoring.
            if primary.transport == Transport::Bluetooth {
                logging::info(
                    "Probing raw HID input over Bluetooth (Windows normally refuses this).",
                );
            }
            let blocked = probe_input_access(&primary_device);
            if blocked {
                logging::warn(&format!(
                    "Windows refused raw HID input reads over {}. Continuing in safe monitoring mode.",
                    primary.transport.label()
                ));
            } else if primary.transport == Transport::Bluetooth {
                logging::info("Raw HID input is readable over Bluetooth; enabling multi-touch.");
            }
            blocked
        };

        let touch_capable = if input_blocked {
            false
        } else if discovery.descriptor_touch_known {
            primary.has_touch
        } else {
            // Older Windows builds may not return the full descriptor. Try to
            // enter multi-touch mode anyway rather than declaring gestures
            // unsupported based on missing information.
            true
        };

        let connection = Connection {
            primary: primary_device,
            battery: battery_device,
            battery_has_input,
            path: primary.path.clone(),
            name: primary.name.clone(),
            model: primary.model,
            transport: primary.transport,
            touch_capable,
            input_blocked,
        };

        if !connection.input_blocked {
            self.enable_multitouch(&connection);
        }
        Ok(connection)
    }

    fn enable_multitouch(&self, connection: &Connection) {
        if !connection.touch_capable {
            return;
        }
        let preferred: &[u8] = match connection.model {
            MouseModel::MagicMouse1 => &[0xD7, 0x01],
            MouseModel::MagicMouse2 | MouseModel::MagicMouseUsbC => &[0xF1, 0x02, 0x01],
        };
        let fallback: &[u8] = match connection.model {
            MouseModel::MagicMouse1 => &[0xF1, 0x02, 0x01],
            MouseModel::MagicMouse2 | MouseModel::MagicMouseUsbC => &[0xD7, 0x01],
        };
        match connection.primary.send_feature_report(preferred) {
            Ok(()) => {
                logging::info("Multi-touch mode requested with the preferred feature report.");
                return;
            }
            Err(first_error) => {
                match connection.primary.send_feature_report(fallback) {
                    Ok(()) => logging::info(
                        "Multi-touch mode requested with the fallback feature report.",
                    ),
                    Err(second_error) => logging::warn(&format!(
                        "Could not request multi-touch mode ({first_error}; fallback: {second_error}). Native mouse input still works."
                    )),
                }
            }
        }
    }

    fn session(&mut self, mut connection: Connection) {
        if connection.input_blocked {
            self.limited_session(connection);
            return;
        }
        let mut last_battery_poll = Instant::now();
        // Always fetch one reading when the device is opened. The UI otherwise
        // waits a full polling interval before it can show a battery level.
        let mut force_battery_poll = true;
        self.pending_battery_poll = false;
        let mut primary_failures = 0u8;
        let mut buf = [0u8; 512];

        loop {
            if self.shutdown {
                return;
            }
            force_battery_poll |= self.drain_session_commands();

            // The user can disable experimental input at any time. Return to
            // safe monitoring immediately instead of continuing to inject.
            if !self.config.experimental_input {
                logging::info(
                    "Experimental input disabled; switching back to safe monitoring mode.",
                );
                connection.input_blocked = true;
                connection.touch_capable = false;
                self.send(HidEvent::Connected {
                    name: connection.name.clone(),
                    model: connection.model,
                    transport: connection.transport,
                    touch_capable: false,
                    input_blocked: true,
                });
                self.limited_session(connection);
                return;
            }

            let now = Instant::now();
            let interval = self.battery_interval();
            if force_battery_poll || now.saturating_duration_since(last_battery_poll) >= interval {
                let percent = request_battery_now(&connection);
                match percent {
                    Some(percent) => {
                        self.report_battery(percent, connection.transport);
                        self.battery_trend.logged_failure = false;
                    }
                    None => {
                        if !self.battery_trend.logged_failure {
                            logging::warn(
                                "The mouse did not answer the battery request. This is common for some firmware revisions.",
                            );
                            self.battery_trend.logged_failure = true;
                        }
                        if self.last_battery.is_none() {
                            self.send(HidEvent::BatteryUnavailable);
                        }
                    }
                }
                last_battery_poll = Instant::now();
                force_battery_poll = false;
            }

            let mut received_data = false;
            match connection.primary.read_timeout(&mut buf, 18) {
                Ok(n) if n > 0 => {
                    primary_failures = 0;
                    received_data = true;
                    self.process_input(&connection, &buf[..n]);
                }
                Ok(_) => {
                    primary_failures = 0;
                }
                Err(error) => {
                    if is_access_denied(&error) {
                        logging::warn(
                            "Windows withdrew raw HID input access while connected. Switching to limited mode.",
                        );
                        connection.input_blocked = true;
                        connection.touch_capable = false;
                        self.send(HidEvent::Connected {
                            name: connection.name.clone(),
                            model: connection.model,
                            transport: connection.transport,
                            touch_capable: false,
                            input_blocked: true,
                        });
                        self.limited_session(connection);
                        return;
                    }
                    primary_failures = primary_failures.saturating_add(1);
                    if primary_failures == 1 {
                        logging::warn(&format!("Primary HID read failed: {error}"));
                    } else {
                        logging::warn("Mouse did not recover after a HID read error.");
                        return;
                    }
                }
            }

            if let Some(battery) = connection.battery.as_ref() {
                match battery.read_timeout(&mut buf, 0) {
                    Ok(n) if n > 0 => {
                        received_data = true;
                        if let Some(percent) = parse_battery_input(&buf[..n]) {
                            self.report_battery(percent, connection.transport);
                        } else {
                            self.process_input(&connection, &buf[..n]);
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        logging::warn(&format!(
                            "Battery HID interface read failed: {error}. Continuing without it."
                        ));
                        connection.battery = None;
                    }
                }
            }

            let now = Instant::now();
            self.simulator.tick(now);
            if !received_data && self.shutdown {
                return;
            }
            force_battery_poll |= self.drain_session_commands_after_read();
        }
    }

    /// Windows normally reserves Bluetooth mouse collections for its own input
    /// stack. Keep the handle open for battery requests, watch the device path
    /// for disconnects and retry raw access periodically so the app can upgrade
    /// to full gestures if the device becomes readable again (for example after
    /// a driver is unloaded or the mouse is reconnected over USB).
    fn limited_session(&mut self, mut connection: Connection) {
        let mut last_battery_poll = Instant::now();
        let mut force_battery_poll = true;
        let mut last_access_probe = Instant::now();
        let mut last_presence_check = Instant::now();
        let mut battery_buf = [0u8; 64];
        let mut probe_buf = [0u8; 64];
        let mut battery_error_logged = false;

        loop {
            if self.shutdown {
                return;
            }
            // A config change can turn experimental input off while we are
            // running. Re-check it on every command drain.
            force_battery_poll |= self.drain_session_commands();
            if self.shutdown {
                return;
            }

            // The Apple vendor collection exposes battery as an input report
            // (0x90). Besides reading reports the mouse sends on its own,
            // catch any report that arrived since the last loop.
            if connection.battery_has_input {
                if let Some(battery) = connection.battery.as_ref() {
                    match battery.read_timeout(&mut battery_buf, 0) {
                        Ok(length) if length > 0 => {
                            battery_error_logged = false;
                            if let Some(percent) = parse_battery_input(&battery_buf[..length]) {
                                self.report_battery(percent, connection.transport);
                                self.battery_trend.logged_failure = false;
                            }
                        }
                        Ok(_) => {}
                        Err(error) => {
                            if !battery_error_logged {
                                logging::warn(&format!(
                                    "Battery HID read failed: {error}. Continuing without it."
                                ));
                                battery_error_logged = true;
                            }
                            connection.battery = None;
                            connection.battery_has_input = false;
                        }
                    }
                }
            }

            let now = Instant::now();
            let interval = self.battery_interval();
            if force_battery_poll || now.saturating_duration_since(last_battery_poll) >= interval {
                // The vendor collection accepts a direct 0x90 input-report
                // request over Bluetooth (unlike the primary mouse collection,
                // which Windows reserves). This runs on the HID worker, never
                // on the UI thread. If the mouse is asleep the request may take
                // a few seconds; the app keeps showing the last value and its
                // window stays responsive.
                let percent = request_battery_now(&connection);
                match percent {
                    Some(percent) => {
                        self.report_battery(percent, connection.transport);
                        self.battery_trend.logged_failure = false;
                    }
                    None => {
                        if !self.battery_trend.logged_failure {
                            logging::warn(
                                "The mouse did not report a battery level. It may be asleep; move it and try again.",
                            );
                            self.battery_trend.logged_failure = true;
                        }
                        // If a value is already on screen, keep it instead of
                        // replacing it with an error state.
                        if self.last_battery.is_none() {
                            self.send(HidEvent::BatteryUnavailable);
                        }
                    }
                }
                last_battery_poll = Instant::now();
                force_battery_poll = false;
            }

            // Only an explicit opt-in may probe the primary collection
            // (USB and, as an experiment, Bluetooth).
            if self.config.experimental_input
                && last_access_probe.elapsed() >= Duration::from_secs(5)
            {
                last_access_probe = Instant::now();
                let restored = match connection.primary.read_timeout(&mut probe_buf, 0) {
                    Ok(_) => true,
                    Err(error) if is_access_denied(&error) => false,
                    Err(error) => {
                        logging::info(&format!(
                            "HID access probe failed ({error}); waiting for the mouse to reconnect."
                        ));
                        return;
                    }
                };
                if restored {
                    logging::info("Windows now allows raw HID input; enabling gestures.");
                    connection.input_blocked = false;
                    connection.touch_capable = true;
                    self.enable_multitouch(&connection);
                    self.send(HidEvent::Connected {
                        name: connection.name.clone(),
                        model: connection.model,
                        transport: connection.transport,
                        touch_capable: true,
                        input_blocked: false,
                    });
                    self.session(connection);
                    return;
                }
            }

            if last_presence_check.elapsed() >= Duration::from_secs(8) {
                last_presence_check = Instant::now();
                if !self.device_path_present(&connection.path) {
                    logging::info("The Magic Mouse HID path disappeared; waiting for reconnect.");
                    return;
                }
            }

            thread::sleep(Duration::from_millis(120));
        }
    }

    fn device_path_present(&mut self, path: &CString) -> bool {
        let Some(api) = self.api.as_mut() else {
            return true;
        };
        if api.refresh_devices().is_err() {
            return true;
        }
        api.device_list()
            .any(|info| info.path() == path.as_c_str())
    }

    fn process_input(&mut self, connection: &Connection, bytes: &[u8]) {
        if let Some(percent) = parse_battery_input(bytes) {
            self.report_battery(percent, connection.transport);
            return;
        }

        let reports = parse_touch_reports(bytes);
        if reports.is_empty() {
            return;
        }
        let now = Instant::now();
        for report in reports {
            self.simulator.handle_report(&report, now);
            self.emit_touch_state(&report, now);
        }
    }

    fn emit_touch_state(&mut self, report: &TouchReport, now: Instant) {
        let empty = report.points.is_empty();
        let count = report.points.len();
        let count_changed = count != self.last_touch_count || empty != self.last_touch_was_empty;
        let due = self
            .last_touch_emit
            .map(|last| now.saturating_duration_since(last) >= Duration::from_millis(33))
            .unwrap_or(true);
        if !count_changed && !due {
            return;
        }
        self.last_touch_emit = Some(now);
        self.last_touch_count = count;
        self.last_touch_was_empty = empty;
        self.send(HidEvent::Touch {
            points: report.points.clone(),
        });
    }

    /// Retry quickly until the first reading arrives, then use the configured
    /// interval. Without this the UI could wait a full minute after launch if
    /// the very first request raced with the mouse waking up.
    fn battery_interval(&self) -> Duration {
        if self.last_battery.is_none() && self.battery_trend.logged_failure {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(self.config.battery_poll_seconds.max(20))
        }
    }

    fn report_battery(&mut self, percent: u8, transport: Transport) {
        self.last_battery = Some(percent);
        let charging = self.battery_trend.update(percent, transport);
        if self.battery_logged_percent != Some(percent) {
            logging::info(&format!("Battery level reported: {percent}%."));
            self.battery_logged_percent = Some(percent);
        }
        self.send(HidEvent::Battery { percent, charging });
    }

    /// Drain commands while searching. Returns true if a shutdown was requested.
    fn drain_idle_commands(&mut self) -> bool {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                HidCommand::UpdateConfig(config) => {
                    self.config = config;
                    self.config.sanitize();
                    self.simulator.update_config(self.config.clone());
                }
                HidCommand::PollBattery => self.pending_battery_poll = true,
                HidCommand::Shutdown => {
                    self.shutdown = true;
                    return true;
                }
            }
        }
        false
    }

    /// Drain commands during a live session. Returns whether the battery was
    /// explicitly requested.
    fn drain_session_commands(&mut self) -> bool {
        let mut poll_battery = false;
        while let Ok(command) = self.commands.try_recv() {
            match command {
                HidCommand::UpdateConfig(config) => {
                    self.config = config;
                    self.config.sanitize();
                    self.simulator.update_config(self.config.clone());
                }
                HidCommand::PollBattery => poll_battery = true,
                HidCommand::Shutdown => {
                    self.shutdown = true;
                    return poll_battery;
                }
            }
        }
        poll_battery
    }

    fn drain_session_commands_after_read(&mut self) -> bool {
        self.drain_session_commands()
    }

    fn sleep_with_commands(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while !self.shutdown {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            self.drain_idle_commands();
            if self.shutdown {
                break;
            }
            thread::sleep(remaining.min(Duration::from_millis(120)));
        }
    }
}

pub(crate) fn read_feature_battery(device: &HidDevice) -> Option<u8> {
    // The 0x47 feature report contains one byte of payload, so the exact
    // report length is two bytes (ID + value). Windows rejects a report
    // request whose buffer does not match the declared report length on some
    // Bluetooth stacks, and hidapi forwards the buffer length unchanged.
    let mut buf = [0u8; 2];
    buf[0] = 0x47;
    match device.get_feature_report(&mut buf) {
        Ok(length) if length >= 2 && buf[0] == 0x47 => {
            let percent = buf[1];
            if (1..=100).contains(&percent) {
                Some(percent)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Ask the vendor collection for a fresh battery report. The 0x90 input
/// report works over both Bluetooth and USB, which is what makes the level
/// update on its own instead of waiting for the mouse to volunteer a report.
/// This runs on the HID worker because Windows may hold the call for a few
/// seconds while the mouse is asleep; it must never run on the UI thread.
fn request_battery_now(connection: &Connection) -> Option<u8> {
    if let Some(battery) = connection.battery.as_ref() {
        if let Some(percent) = request_input_battery(battery) {
            return Some(percent);
        }
    }
    read_battery_usb(connection)
}

pub(crate) fn request_input_battery(device: &HidDevice) -> Option<u8> {
    let mut buf = [0u8; 64];
    buf[0] = 0x90;
    match device.get_input_report(&mut buf) {
        Ok(length) if length >= 3 && buf[0] == 0x90 => {
            let percent = buf[2];
            if (1..=100).contains(&percent) {
                Some(percent)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Feature-report battery is only used on USB as a fallback. The Bluetooth
/// firmware does not reliably answer it, and a blocking request can stall the
/// HID worker.
fn read_battery_usb(connection: &Connection) -> Option<u8> {
    if connection.transport != Transport::Usb {
        return None;
    }
    if let Some(battery) = connection.battery.as_ref() {
        if let Some(percent) = read_feature_battery(battery) {
            return Some(percent);
        }
    }
    read_feature_battery(&connection.primary)
}

pub(crate) fn parse_battery_input(bytes: &[u8]) -> Option<u8> {
    if bytes.len() >= 3 && bytes[0] == 0x90 {
        let percent = bytes[2];
        if (1..=100).contains(&percent) {
            return Some(percent);
        }
    }
    None
}

fn sign_extend_12(value: u32) -> i32 {
    let value = (value & 0x0FFF) as i32;
    if value & 0x0800 != 0 {
        value - 0x1000
    } else {
        value
    }
}

fn decode_touch_point(data: &[u8]) -> TouchPoint {
    debug_assert!(data.len() >= 8);
    // These offsets are from Apple's HID descriptor and match Linux
    // hid-magicmouse.c. X and Y are 12-bit signed values split across bytes.
    let x_raw = ((data[1] as u32 & 0x0F) << 8) | data[0] as u32;
    let y_raw = ((data[2] as u32) << 4) | (data[1] as u32 >> 4);
    let id = ((data[6] as u32) << 2 | (data[5] as u32) >> 6) & 0x000F;
    let state = data[7] & 0xF0;
    let _ = state;
    TouchPoint {
        id: id as u8,
        x: sign_extend_12(x_raw),
        y: -sign_extend_12(y_raw),
        size: data[5] & 0x3F,
    }
}

/// Decode zero, one or two reports from one HID read. `0xF7` packets wrap two
/// ordinary reports (a quirk inherited from the trackpad firmware).
pub fn parse_touch_reports(bytes: &[u8]) -> Vec<TouchReport> {
    let mut reports = Vec::new();
    parse_one_report(bytes, false, &mut reports);
    reports
}

fn parse_one_report(bytes: &[u8], nested: bool, out: &mut Vec<TouchReport>) {
    if bytes.is_empty() {
        return;
    }
    if bytes[0] == 0xF7 {
        if nested || bytes.len() < 2 {
            return;
        }
        let first_length = bytes[1] as usize;
        if first_length == 0 || bytes.len() < first_length + 2 {
            return;
        }
        parse_one_report(&bytes[2..2 + first_length], true, out);
        if bytes.len() > 2 + first_length {
            parse_one_report(&bytes[2 + first_length..], true, out);
        }
        return;
    }

    let (entries_offset, click_index) = match bytes[0] {
        0x29 => (6usize, 3usize),
        0x12 | 0x27 => (14usize, 1usize),
        _ => return,
    };
    if bytes.len() <= click_index {
        return;
    }
    let clicks = bytes[click_index] & 0x03;
    let mut report = TouchReport {
        points: Vec::with_capacity(8),
        left: clicks & 0x01 != 0,
        right: clicks & 0x02 != 0,
    };

    if bytes[0] == 0x29 {
        if bytes.len() < entries_offset {
            return;
        }
        let entries = &bytes[entries_offset..];
        if entries.len() % 8 != 0 {
            return;
        }
        for entry in entries.chunks_exact(8) {
            let point = decode_touch_point(entry);
            if entry[7] & 0xF0 != 0 {
                report.points.push(point);
            }
        }
    } else {
        // A report can be exactly 8 bytes long and contain only buttons; a
        // full report starts with 14 bytes of header.
        if bytes.len() != 8 {
            if bytes.len() < entries_offset {
                return;
            }
            let entries = &bytes[entries_offset..];
            if entries.len() % 8 != 0 {
                return;
            }
            for entry in entries.chunks_exact(8) {
                let point = decode_touch_point(entry);
                if entry[7] & 0xF0 != 0 {
                    report.points.push(point);
                }
            }
        }
    }

    if report.points.len() > 15 {
        return;
    }
    out.push(report);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MM2_DESCRIPTOR: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x02, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09,
        0x19, 0x01, 0x29, 0x02, 0x15, 0x00, 0x25, 0x01, 0x95, 0x02, 0x75, 0x01, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x06, 0x81, 0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81,
        0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, 0xC0, 0x85, 0x27, 0x05, 0x06, 0x09,
        0x01, 0x15, 0x01, 0x25, 0x41, 0x75, 0x08, 0x95, 0x2E, 0x81, 0x06, 0x85, 0x47, 0x09,
        0x20, 0x15, 0x00, 0x25, 0x64, 0x75, 0x08, 0x95, 0x01, 0xB1, 0xA2, 0xC0,
    ];

    #[test]
    fn maps_apple_product_ids_to_models() {
        assert_eq!(
            MouseModel::from_product_id(0x030D),
            Some(MouseModel::MagicMouse1)
        );
        assert_eq!(
            MouseModel::from_product_id(0x0269),
            Some(MouseModel::MagicMouse2)
        );
        assert_eq!(
            MouseModel::from_product_id(0x0323),
            Some(MouseModel::MagicMouseUsbC)
        );
        assert_eq!(MouseModel::from_product_id(0x0001), None);
    }

    #[test]
    fn descriptor_scanner_finds_touch_and_battery_reports() {
        let info = parse_descriptor(MM2_DESCRIPTOR);
        assert!(info.input_reports.contains(&0x02));
        assert!(info.input_reports.contains(&0x27));
        assert!(info.feature_reports.contains(&0x47));
        assert!(info.has_touch_report());
        assert!(info.has_battery_feature());
        assert!(!info.has_battery_input());
    }

    #[test]
    fn descriptor_scanner_handles_malformed_input() {
        let info = parse_descriptor(&[0x85, 0xFE, 0xFF]);
        assert!(info.input_reports.is_empty());
        let info = parse_descriptor(&[0xFE, 0xFF]);
        assert!(info.input_reports.is_empty());
    }

    #[test]
    fn decodes_a_mm2_touch_report() {
        let mut report = vec![0u8; 14 + 8];
        report[0] = 0x27;
        report[1] = 0x01; // left button
        // x = -100 (0xF9C), y = 200 (0x0C8), size 21, id 3, state 0x30.
        let entry = &mut report[14..22];
        entry[0] = 0x9C;
        entry[1] = 0x8F;
        entry[2] = 0x0C;
        entry[5] = 21 | (3 << 6);
        entry[6] = 0x00;
        entry[7] = 0x30;
        let reports = parse_touch_reports(&report);
        assert_eq!(reports.len(), 1);
        assert!(reports[0].left);
        assert_eq!(reports[0].points.len(), 1);
        assert_eq!(reports[0].points[0].x, -100);
        assert_eq!(reports[0].points[0].y, -200);
        assert_eq!(reports[0].points[0].id, 3);
        assert_eq!(reports[0].points[0].size, 21);
    }

    #[test]
    fn decodes_a_button_only_report() {
        let mut report = vec![0u8; 8];
        report[0] = 0x12;
        report[1] = 0x02;
        let reports = parse_touch_reports(&report);
        assert_eq!(reports.len(), 1);
        assert!(reports[0].right);
        assert!(reports[0].points.is_empty());
    }

    #[test]
    fn decodes_a_double_report() {
        let mut inner = vec![0u8; 8];
        inner[0] = 0x12;
        inner[1] = 0x01;
        let mut packet = vec![0xF7, inner.len() as u8];
        packet.extend_from_slice(&inner);
        packet.extend_from_slice(&inner);
        let reports = parse_touch_reports(&packet);
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|report| report.left));
    }

    #[test]
    fn parser_ignores_irrelevant_reports() {
        assert!(parse_touch_reports(&[0x02, 1, 2, 3, 4, 5]).is_empty());
        assert!(parse_touch_reports(&[]).is_empty());
    }

    #[test]
    fn battery_input_uses_report_0x90_byte_two() {
        assert_eq!(parse_battery_input(&[0x90, 0x00, 92]), Some(92));
        assert_eq!(parse_battery_input(&[0x90, 0x00, 0]), None);
        assert_eq!(parse_battery_input(&[0x02, 0x00, 92]), None);
    }

    #[test]
    fn signed_coordinates_are_extended() {
        assert_eq!(sign_extend_12(0x7FF), 2047);
        assert_eq!(sign_extend_12(0x800), -2048);
    }
}