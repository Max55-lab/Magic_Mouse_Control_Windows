//! Persistent application configuration.
//!
//! The config lives in `%APPDATA%\MagicMouseControl\config.json`. Unknown
//! fields are ignored and missing fields fall back to sensible defaults, so
//! upgrades never destroy a user's settings.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const CONFIG_VERSION: u32 = 1;

/// All user-facing options. This is serialized to disk, so every field must
/// have a stable name and a default (see `impl Default`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,

    // Scrolling
    pub natural_scrolling: bool,
    pub smooth_scrolling: bool,
    pub inertia: bool,
    pub scroll_speed: f32,

    // Gestures
    /// Raw touch input is opt-in. It is disabled by default because Windows
    /// reserves the Bluetooth mouse collection for its own input stack, and
    /// probing it from user mode can disturb pointer input on some systems.
    pub experimental_input: bool,
    pub middle_click: bool,
    pub two_finger_swipe: bool,

    // Alerts & background behaviour
    pub low_battery_notifications: bool,
    pub low_battery_threshold: u8,
    pub close_to_tray: bool,
    pub start_with_windows: bool,
    pub battery_poll_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            natural_scrolling: false,
            smooth_scrolling: true,
            inertia: true,
            scroll_speed: 1.0,
            experimental_input: false,
            middle_click: true,
            two_finger_swipe: true,
            low_battery_notifications: true,
            low_battery_threshold: 15,
            close_to_tray: true,
            start_with_windows: false,
            battery_poll_seconds: 60,
        }
    }
}

impl Config {
    /// Clamp values that could otherwise make the daemon misbehave.
    pub fn sanitize(&mut self) {
        self.version = CONFIG_VERSION;
        self.scroll_speed = self.scroll_speed.clamp(0.25, 3.0);
        self.low_battery_threshold = self.low_battery_threshold.clamp(5, 50);
        self.battery_poll_seconds = self.battery_poll_seconds.clamp(20, 600);
    }

    pub fn load() -> Self {
        let path = config_path();
        let mut config = match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Self>(text.trim_start_matches('\u{feff}')) {
                Ok(config) => config,
                Err(error) => {
                    crate::logging::warn(&format!(
                        "Could not parse config at {}: {error}. Using defaults.",
                        path.display()
                    ));
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                crate::logging::warn(&format!(
                    "Could not read config at {}: {error}. Using defaults.",
                    path.display()
                ));
                Self::default()
            }
        };
        config.sanitize();
        config
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = data_dir();
        fs::create_dir_all(&dir)
            .map_err(|error| format!("Could not create {}: {error}", dir.display()))?;
        let path = config_path();
        let json = serde_json::to_string_pretty(self)
            .map_err(|error| format!("Could not serialize settings: {error}"))?;
        // Write to a temporary file first, then replace the old config. This
        // prevents a crash halfway through a write from corrupting settings.
        let temp = dir.join("config.json.tmp");
        fs::write(&temp, json)
            .map_err(|error| format!("Could not write {}: {error}", temp.display()))?;
        if let Err(first_error) = fs::rename(&temp, &path) {
            // On some network/filtered filesystems rename-over-existing fails.
            let _ = fs::remove_file(&path);
            fs::rename(&temp, &path).map_err(|error| {
                format!(
                    "Could not save settings (first attempt: {first_error}; second: {error})"
                )
            })?;
        }
        Ok(())
    }
}

/// `%APPDATA%\MagicMouseControl`, or a local fallback for unusual systems.
pub fn data_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("MagicMouseControl");
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            return parent.join("MagicMouseControlData");
        }
    }
    PathBuf::from("MagicMouseControlData")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let mut config = Config::default();
        config.sanitize();
        assert_eq!(config.low_battery_threshold, 15);
        assert!(config.battery_poll_seconds >= 20);
    }

    #[test]
    fn sanitize_clamps_outliers() {
        let mut config = Config {
            scroll_speed: 99.0,
            low_battery_threshold: 0,
            battery_poll_seconds: 1,
            ..Config::default()
        };
        config.sanitize();
        assert_eq!(config.scroll_speed, 3.0);
        assert_eq!(config.low_battery_threshold, 5);
        assert_eq!(config.battery_poll_seconds, 20);
    }
}
