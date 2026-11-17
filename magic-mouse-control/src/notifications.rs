//! Windows toast notifications.
//!
//! Unpackaged desktop apps need a registered AppUserModelID to show a toast.
//! Registering one requires a Start Menu shortcut, which a portable app cannot
//! assume exists, so we use the well-known PowerShell AUMID. The notification
//! itself is fully branded by its title and text; it contains no PowerShell
//! content. Toasts are always sent from a worker thread so the UI never blocks
//! on the WinRT activation call.

use tauri_winrt_notification::{Duration as ToastDuration, Sound, Toast};

const APP_TITLE: &str = "Magic Mouse Control";

fn show(title: &str, text: &str) {
    let title = title.to_owned();
    let text = text.to_owned();
    std::thread::spawn(move || {
        let result = Toast::new(Toast::POWERSHELL_APP_ID)
            .title(APP_TITLE)
            .text1(&title)
            .text2(&text)
            .sound(Some(Sound::Default))
            .duration(ToastDuration::Short)
            .show();
        if let Err(error) = result {
            crate::logging::warn(&format!("Could not show a Windows toast: {error}"));
        }
    });
}

pub fn low_battery(percent: u8) {
    show(
        "Battery low",
        &format!("{percent}% remaining. Charge your Magic Mouse."),
    );
}

pub fn battery_status(percent: u8, charging: bool) {
    let suffix = if charging && percent < 100 {
        " Charging."
    } else if percent >= 100 {
        " Fully charged."
    } else {
        ""
    };
    show(
        "Battery level",
        &format!("Your Magic Mouse is at {percent}%.{suffix}"),
    );
}

pub fn not_connected() {
    show(
        "Mouse not connected",
        "Pair the Magic Mouse in Windows Bluetooth settings, then try again.",
    );
}

pub fn battery_unavailable() {
    show(
        "Battery unavailable",
        "The mouse is connected, but it did not answer the battery request. Move it and try again.",
    );
}

pub fn generic_error(message: &str) {
    show("Something went wrong", message);
}
