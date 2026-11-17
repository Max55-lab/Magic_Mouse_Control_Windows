#![windows_subsystem = "windows"]
//! Magic Mouse Control - a free, open-source settings app and HID daemon for
//! Apple Magic Mouse 1/2 and the USB-C model on Windows.
//!
//! The background worker owns all HID reads. The egui window and the system
//! tray only consume small events, so a slow or sleeping mouse can never block
//! the interface.

mod autostart;
mod battery_flip;
mod config;
mod gui;
mod hid_daemon;
mod icons;
#[allow(dead_code)]
mod kernel_tap;
mod logging;
mod mouse_simulator;
mod notifications;
#[allow(dead_code)]
mod touch_source;
mod widgets;

use std::sync::mpsc;

use eframe::egui;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

use crate::config::Config;

fn main() {
    // The elevated battery helper runs this executable in a short-lived,
    // windowless mode. Handle it before logging and before the single-instance
    // mutex so it can never open a second GUI or fight the running copy.
    if let Some(path) = battery_once_path() {
        std::process::exit(battery_flip::read_battery_once(&path));
    }

    // Hidden diagnostic used to verify the elevated flip end-to-end without
    // opening the settings window. It still asks for the same consent prompt.
    if let Some(path) = argument_path("--battery-flip-once") {
        logging::init();
        let text = match battery_flip::start_read() {
            Ok(receiver) => match receiver.recv_timeout(std::time::Duration::from_secs(150)) {
                Ok(outcome) => {
                    if let Some(percent) = outcome.percent {
                        battery_flip::save_cached(percent, false);
                    }
                    format!("{outcome:#?}\n")
                }
                Err(error) => format!("HELPER-TIMEOUT {error}\n"),
            },
            Err(error) => format!("HELPER-ERROR {error}\n"),
        };
        let _ = std::fs::write(path, text);
        std::process::exit(0);
    }

    logging::init();

    // A second copy would open the same HID collections and create a second
    // tray icon. The named mutex makes the process single-instance and is
    // released by the kernel even after a crash.
    let _instance_guard = match unsafe {
        CreateMutexW(None, false, w!("MagicMouseControl.SingleInstance"))
    } {
        Ok(handle) => {
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                logging::info(
                    "Another Magic Mouse Control instance is already running; exiting this copy.",
                );
                return;
            }
            Some(handle)
        }
        Err(error) => {
            logging::warn(&format!(
                "Could not create the single-instance guard: {error}. Continuing anyway."
            ));
            None
        }
    };

    let minimized = std::env::args().any(|argument| argument == "--minimized");
    logging::info(&format!(
        "Startup mode: {}",
        if minimized { "tray (--minimized)" } else { "window" }
    ));
    let config = Config::load();

    // HID events travel over a tokio channel because the daemon's command
    // channel is asynchronous; the UI thread bridges them into UiEvent.
    let (hid_tx, hid_rx) = tokio::sync::mpsc::unbounded_channel();
    let hid = match hid_daemon::spawn(config.clone(), hid_tx) {
        Ok(handle) => Some(handle),
        Err(error) => {
            logging::error(&format!("Could not start the HID daemon: {error}"));
            None
        }
    };
    let startup_error = hid.is_none().then(|| {
        "The background HID service could not start. See the log file in the app data folder."
            .to_string()
    });

    let (ui_tx, ui_rx) = mpsc::channel();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Magic Mouse Control")
            .with_inner_size([1000.0, 720.0])
            .with_min_inner_size([860.0, 620.0])
            .with_icon(egui::IconData {
                rgba: icons::app_icon_rgba(64),
                width: 64,
                height: 64,
            })
            .with_visible(!minimized),
        centered: true,
        ..Default::default()
    };

    let result = eframe::run_native(
        "Magic Mouse Control",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(gui::App::new(
                cc,
                config,
                hid,
                hid_rx,
                ui_rx,
                ui_tx,
                minimized,
                startup_error,
            )))
        }),
    );

    if let Err(error) = result {
        logging::error(&format!("The settings window could not start: {error}"));
        show_error_box(&format!(
            "Magic Mouse Control could not start.\n\n{error}\n\nSee the log file in %APPDATA%\\MagicMouseControl for details."
        ));
    }
}

/// A Win32 message box is the only reliable error channel in a GUI-subsystem
/// executable, where stderr is usually not visible.
fn show_error_box(message: &str) {
    let text = wide(message);
    let title = wide("Magic Mouse Control");
    unsafe {
        MessageBoxW(
            HWND(std::ptr::null_mut()),
            PCWSTR::from_raw(text.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `--battery-once <result-file>` is used by the elevated battery helper.
fn battery_once_path() -> Option<std::path::PathBuf> {
    argument_path("--battery-once")
}

fn argument_path(flag: &str) -> Option<std::path::PathBuf> {
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == std::ffi::OsStr::new(flag) {
            return Some(
                arguments
                    .next()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        std::env::temp_dir().join("magic-mouse-battery-once.txt")
                    }),
            );
        }
    }
    None
}
