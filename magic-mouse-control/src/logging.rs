//! Tiny dependency-free logger. It writes a rotating text file in the app
//! data directory and mirrors to stderr in debug builds.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

pub fn init() {
    let dir = crate::config::data_dir();
    if let Err(error) = fs::create_dir_all(&dir) {
        eprintln!("Magic Mouse Control: could not create log directory: {error}");
        return;
    }

    let path = dir.join("magic-mouse.log");
    if let Ok(metadata) = fs::metadata(&path) {
        if metadata.len() > 1_500_000 {
            let old = dir.join("magic-mouse.old.log");
            let _ = fs::remove_file(&old);
            let _ = fs::rename(&path, old);
        }
    }

    let _ = LOG_FILE.set(path);
    std::panic::set_hook(Box::new(|info| {
        error(&format!("PANIC: {info}"));
    }));

    info(&format!(
        "Magic Mouse Control {} starting on {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    ));
}

pub fn log_file() -> Option<PathBuf> {
    LOG_FILE.get().cloned()
}

pub fn info(message: &str) {
    write("INFO", message);
}

pub fn warn(message: &str) {
    write("WARN", message);
}

pub fn error(message: &str) {
    write("ERROR", message);
}

fn write(level: &str, message: &str) {
    let line = format!(
        "[{}] [{}] [{}] {}\n",
        timestamp(),
        level,
        std::thread::current().name().unwrap_or("thread"),
        message
    );
    #[cfg(debug_assertions)]
    eprint!("{line}");

    if let Some(path) = LOG_FILE.get() {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// UTC timestamp without pulling in a date/time crate.
fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let day_seconds = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era as i32 + era as i32 * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}
