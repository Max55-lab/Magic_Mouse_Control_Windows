//! On-demand battery reading for Bluetooth Magic Mice.
//!
//! Windows reserves the mouse's primary HID collection, and Apple's scroll
//! filter hides the vendor collection that carries battery report `0x90`.
//! While the filter is active the battery is therefore unreadable from user
//! mode. The only safe, driver-free way to obtain a fresh value is to briefly
//! remove `applewirelessmouse` from the device's `LowerFilters`, restart that
//! one BTHENUM devnode, read the vendor report, and put the filter back.
//!
//! That is what this module does. It is deliberately **on demand**:
//!   * scrolling and the pointer pause for a few seconds,
//!   * Windows asks for administrator approval once,
//!   * the elevated script always restores the filter in a `finally` block,
//!   * the script re-reads `LowerFilters` and reports whether the restore was
//!     actually verified.
//!
//! The elevated script calls this same executable with `--battery-once`, which
//! runs before the single-instance mutex and never creates a window. The whole
//! cycle is isolated from the GUI thread; the main window stays responsive.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hidapi::HidApi;
use serde::{Deserialize, Serialize};

use crate::config;

const APPLE_VIDS: [u16; 2] = [0x004c, 0x05ac];
const MAGIC_MOUSE_PIDS: [u16; 3] = [0x0269, 0x030d, 0x0323];
const VENDOR_USAGE_PAGE: u16 = 0xff00;
const VENDOR_USAGE: u16 = 0x0014;
const BATTERY_REPORT_ID: u8 = 0x90;
const HELPER_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct FlipOutcome {
    pub percent: Option<u8>,
    /// True when no filter change happened, or when the elevated script read
    /// the registry back and confirmed that `applewirelessmouse` is present.
    pub restored: bool,
    pub error: Option<String>,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct CachedBattery {
    pub percent: u8,
    pub charging: bool,
    pub measured_at: SystemTime,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    percent: u8,
    charging: bool,
    unix_secs: u64,
}

fn cache_path() -> PathBuf {
    config::data_dir().join("battery-cache.json")
}

fn pending_marker_path() -> PathBuf {
    config::data_dir().join("mode-flip-pending")
}

/// True when a previous flip did not confirm that scroll mode was restored.
/// The app shows a prominent restore action in that case.
pub fn flip_pending() -> bool {
    pending_marker_path().exists()
}

fn mark_flip_pending() -> Result<(), String> {
    let path = pending_marker_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
    }
    fs::write(&path, "A battery read was started and has not confirmed restore.\n")
        .map_err(|error| format!("Could not write {}: {error}", path.display()))
}

fn clear_flip_pending() {
    let _ = fs::remove_file(pending_marker_path());
}

pub fn load_cached() -> Option<CachedBattery> {
    let text = fs::read_to_string(cache_path()).ok()?;
    let cache: CacheFile = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    if !(1..=100).contains(&cache.percent) {
        return None;
    }
    Some(CachedBattery {
        percent: cache.percent,
        charging: cache.charging,
        measured_at: UNIX_EPOCH + Duration::from_secs(cache.unix_secs),
    })
}

pub fn save_cached(percent: u8, charging: bool) {
    if !(1..=100).contains(&percent) {
        return;
    }
    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let cache = CacheFile {
        percent,
        charging,
        unix_secs,
    };
    let Ok(json) = serde_json::to_string_pretty(&cache) else {
        return;
    };
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let temp = path.with_extension("json.tmp");
    if fs::write(&temp, json).is_ok() {
        if fs::rename(&temp, &path).is_err() {
            let _ = fs::remove_file(&path);
            let _ = fs::rename(&temp, &path);
        }
    }
}

/// Entry point for `--battery-once <file>`. Runs before logging and before the
/// single-instance mutex, writes a one-line result and exits.
pub fn read_battery_once(path: &Path) -> i32 {
    let result = read_vendor_battery();
    let text = match &result {
        Ok(percent) => {
            // Any successful read is worth keeping, including this short-lived
            // helper, so the next launch can show the last known value.
            save_cached(*percent, false);
            format!("PERCENT={percent}\n")
        }
        Err(error) => format!("ERROR={error}\n"),
    };
    let _ = fs::write(path, text);
    if result.is_ok() {
        0
    } else {
        1
    }
}

fn read_vendor_battery() -> Result<u8, String> {
    let mut api = HidApi::new().map_err(|error| format!("HID init failed: {error}"))?;
    let mut last_error = String::from("vendor battery collection not found");
    for _ in 0..40 {
        let _ = api.refresh_devices();
        for info in api.device_list() {
            if !APPLE_VIDS.contains(&info.vendor_id())
                || !MAGIC_MOUSE_PIDS.contains(&info.product_id())
                || info.usage_page() != VENDOR_USAGE_PAGE
                || info.usage() != VENDOR_USAGE
            {
                continue;
            }
            let device = match api.open_path(info.path()) {
                Ok(device) => device,
                Err(error) => {
                    last_error = format!("open failed: {error}");
                    continue;
                }
            };
            for _ in 0..5 {
                let mut buffer = [0u8; 64];
                buffer[0] = BATTERY_REPORT_ID;
                match device.get_input_report(&mut buffer) {
                    Ok(length)
                        if length >= 3
                            && buffer[0] == BATTERY_REPORT_ID
                            && (1..=100).contains(&buffer[2]) =>
                    {
                        return Ok(buffer[2]);
                    }
                    Ok(length) if length >= 3 && buffer[0] == BATTERY_REPORT_ID => {
                        last_error = format!("report has no valid percentage ({})", buffer[2]);
                    }
                    Ok(_) => {
                        last_error = "device returned a different report".to_string();
                    }
                    Err(error) => {
                        last_error = error.to_string();
                    }
                }
                thread::sleep(Duration::from_millis(200));
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err(last_error)
}

/// Start an elevated read. The returned receiver gets exactly one outcome.
/// The caller polls it from the UI thread, so the window never blocks.
pub fn start_read() -> Result<Receiver<FlipOutcome>, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("Could not locate the application: {error}"))?;
    let exe = exe.to_string_lossy().to_string();
    let nonce = nonce();
    let temp = std::env::temp_dir();
    let script = temp.join(format!("magic-mouse-battery-{nonce}.ps1"));
    let launcher = temp.join(format!("magic-mouse-battery-{nonce}-launch.ps1"));
    let result = temp.join(format!("magic-mouse-battery-{nonce}.result"));
    let reader = temp.join(format!("magic-mouse-battery-{nonce}.reader"));

    fs::write(
        &script,
        build_read_script(&result, &reader, Path::new(&exe)),
    )
    .map_err(|error| format!("Could not write the battery helper: {error}"))?;
    fs::write(&launcher, build_launcher(&script, &result))
        .map_err(|error| format!("Could not write the elevation helper: {error}"))?;

    if let Err(error) = mark_flip_pending() {
        let _ = fs::remove_file(&script);
        let _ = fs::remove_file(&launcher);
        return Err(error);
    }

    let (tx, rx) = mpsc::channel();
    let thread_launcher = launcher.clone();
    let thread_result = result.clone();
    let thread_script = script.clone();
    let thread_reader = reader.clone();
    thread::Builder::new()
        .name("battery-flip".to_string())
        .spawn(move || {
            let outcome = run_helper(&thread_launcher, &thread_result);
            if outcome.restored {
                clear_flip_pending();
            }
            crate::logging::info(&format!(
                "Battery flip finished: percent={:?} restored={} cancelled={} error={:?}",
                outcome.percent, outcome.restored, outcome.cancelled, outcome.error
            ));
            let _ = tx.send(outcome);
            let _ = fs::remove_file(&thread_launcher);
            let _ = fs::remove_file(&thread_script);
            let _ = fs::remove_file(&thread_result);
            let _ = fs::remove_file(&thread_reader);
        })
        .map_err(|error| format!("Could not start the battery helper: {error}"))?;
    Ok(rx)
}

/// Put Apple's scroll filter back and verify it. Used when the automatic
/// restore could not be confirmed, or after an interrupted cycle.
pub fn start_restore() -> Result<Receiver<Result<(), String>>, String> {
    let nonce = nonce();
    let temp = std::env::temp_dir();
    let script = temp.join(format!("magic-mouse-restore-{nonce}.ps1"));
    let launcher = temp.join(format!("magic-mouse-restore-{nonce}-launch.ps1"));
    let result = temp.join(format!("magic-mouse-restore-{nonce}.result"));

    fs::write(&script, build_restore_script(&result))
        .map_err(|error| format!("Could not write the restore helper: {error}"))?;
    fs::write(&launcher, build_launcher(&script, &result))
        .map_err(|error| format!("Could not write the elevation helper: {error}"))?;

    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("battery-restore".to_string())
        .spawn(move || {
            let outcome = run_restore_helper(&launcher, &result);
            if outcome.is_ok() {
                clear_flip_pending();
            }
            crate::logging::info(&format!("Scroll restore finished: {outcome:?}"));
            let _ = tx.send(outcome);
            let _ = fs::remove_file(&launcher);
            let _ = fs::remove_file(&script);
            let _ = fs::remove_file(&result);
        })
        .map_err(|error| format!("Could not start the restore helper: {error}"))?;
    Ok(rx)
}

fn nonce() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("{millis}-{}", std::process::id())
}

fn run_helper(launcher: &Path, result: &Path) -> FlipOutcome {
    let mut command = Command::new("powershell.exe");
    command
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(launcher);
    hide_window(&mut command);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return FlipOutcome {
                percent: None,
                restored: true,
                error: Some(format!("Could not launch the battery helper: {error}")),
                cancelled: false,
            };
        }
    };

    let deadline = Instant::now() + HELPER_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return FlipOutcome {
                        percent: None,
                        restored: false,
                        error: Some(
                            "The battery helper did not finish in time. Check whether scrolling still works; use Restore scroll mode if it does not."
                                .to_string(),
                        ),
                        cancelled: false,
                    };
                }
                thread::sleep(Duration::from_millis(250));
            }
            Err(error) => {
                return FlipOutcome {
                    percent: None,
                    restored: false,
                    error: Some(format!("The battery helper failed: {error}")),
                    cancelled: false,
                };
            }
        }
    }

    let text = fs::read_to_string(result).unwrap_or_default();
    parse_outcome(&text)
}

fn run_restore_helper(launcher: &Path, result: &Path) -> Result<(), String> {
    let mut command = Command::new("powershell.exe");
    command
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(launcher);
    hide_window(&mut command);

    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not launch the restore helper: {error}"))?;
    let deadline = Instant::now() + HELPER_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err("The restore helper did not finish in time.".to_string());
                }
                thread::sleep(Duration::from_millis(250));
            }
            Err(error) => return Err(format!("The restore helper failed: {error}")),
        }
    }

    let text = fs::read_to_string(result).unwrap_or_default();
    parse_restore_outcome(&text)
}

fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

fn parse_outcome(text: &str) -> FlipOutcome {
    let mut percent = None;
    let mut error = None;
    let mut cancelled = false;
    let mut restored = false;
    let mut changed_filter = false;
    let mut saw_restore_token = false;

    for raw in text.lines() {
        let line = clean_line(raw);
        if line.is_empty() {
            continue;
        }
        if line.starts_with("FILTER-REMOVED") {
            changed_filter = true;
        }
        if line == "RESTORED=True" {
            restored = true;
            saw_restore_token = true;
        }
        if line == "RESTORED=False" || line == "RESTORED=Unknown" {
            saw_restore_token = true;
        }
        if let Some(value) = line.strip_prefix("PERCENT=") {
            percent = value.trim().parse::<u8>().ok().filter(|p| (1..=100).contains(p));
        }
        if let Some(value) = line.strip_prefix("READER=") {
            if let Some(value) = value.strip_prefix("PERCENT=") {
                percent = value.trim().parse::<u8>().ok().filter(|p| (1..=100).contains(p));
            } else if let Some(value) = value.strip_prefix("ERROR=") {
                if error.is_none() {
                    error = Some(value.to_string());
                }
            }
        }
        if let Some(value) = line.strip_prefix("ERROR=") {
            if value.starts_with("uac-cancelled") {
                cancelled = true;
            } else if error.is_none() {
                error = Some(value.to_string());
            }
        }
        if let Some(value) = line.strip_prefix("ERROR-RESTORE=") {
            error = Some(format!("Scroll filter restore failed: {value}"));
        }
    }

    if !changed_filter {
        // Nothing was changed (no target, UAC cancelled, or the filter was
        // already absent): there is nothing to restore.
        restored = true;
    } else if !saw_restore_token {
        restored = false;
        if error.is_none() {
            error = Some(
                "The helper did not confirm that Apple's scroll filter was restored."
                    .to_string(),
            );
        }
    } else if !restored && error.is_none() {
        error = Some(
            "Apple's scroll filter could not be verified after the battery read.".to_string(),
        );
    }

    FlipOutcome {
        percent,
        restored,
        error,
        cancelled,
    }
}

fn parse_restore_outcome(text: &str) -> Result<(), String> {
    let mut restored = false;
    let mut error = None;
    for raw in text.lines() {
        let line = clean_line(raw);
        if line == "RESTORED=True" {
            restored = true;
        }
        if let Some(value) = line.strip_prefix("ERROR=") {
            if error.is_none() {
                error = Some(value.to_string());
            }
        }
    }
    if restored {
        Ok(())
    } else {
        Err(error.unwrap_or_else(|| {
            "The restore helper did not confirm that scroll mode is back.".to_string()
        }))
    }
}

fn clean_line(raw: &str) -> &str {
    raw.trim_start_matches(|character: char| character == '\u{feff}' || character.is_whitespace())
        .trim_end()
}

fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn build_launcher(script: &Path, result: &Path) -> String {
    let script = ps_quote(&script.to_string_lossy());
    let result = ps_quote(&result.to_string_lossy());
    format!(
        r#"$ErrorActionPreference = 'Stop'
try {{
    $process = Start-Process -FilePath 'powershell.exe' -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',{script})
    exit $process.ExitCode
}} catch {{
    [System.IO.File]::WriteAllText({result}, ('ERROR=uac-cancelled ' + $_.Exception.Message + "`n"), (New-Object System.Text.UTF8Encoding($false)))
    exit 1
}}
"#
    )
}

fn build_read_script(result: &Path, reader: &Path, exe: &Path) -> String {
    let result = ps_quote(&result.to_string_lossy());
    let reader = ps_quote(&reader.to_string_lossy());
    let exe = ps_quote(&exe.to_string_lossy());
    format!(
        r#"$ErrorActionPreference = 'Stop'
$result = {result}
$readerResult = {reader}
$exe = {exe}
function Write-Status([string]$text) {{
    Add-Content -Path $result -Value $text -Encoding UTF8
}}
function Restart-Node([string]$id, [string]$tag) {{
    $out = & pnputil.exe /restart-device $id 2>&1 | Out-String
    Write-Status ($tag + '=' + $LASTEXITCODE)
    if ($LASTEXITCODE -eq 0) {{ return }}
    Write-Status ($tag + '-FALLBACK=' + $LASTEXITCODE + ' ' + $out.Trim())
    try {{
        Disable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop | Out-Null
    }} catch {{
        Write-Status ($tag + '-DISABLE-ERROR=' + $_.Exception.Message)
        return
    }}
    Start-Sleep -Milliseconds 900
    try {{
        Enable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop | Out-Null
        Write-Status ($tag + '-FALLBACK=ok')
    }} catch {{
        Write-Status ($tag + '-ENABLE-ERROR=' + $_.Exception.Message)
        # Last attempt: never leave the Bluetooth parent disabled.
        try {{
            Enable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction SilentlyContinue | Out-Null
        }} catch {{ }}
    }}
}}
Set-Content -Path $result -Value 'STARTED' -Encoding UTF8
$key = $null
$previous = $null
$instanceId = $null
try {{
    $vendorPattern = 'VID&0001004C_PID&0269'
    $vendorKeys = @(Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Enum\BTHENUM' -ErrorAction SilentlyContinue | Where-Object {{ $_.PSChildName -match $vendorPattern }})
    foreach ($vendorKey in $vendorKeys) {{
        $instances = @(Get-ChildItem $vendorKey.PSPath -ErrorAction SilentlyContinue)
        foreach ($instance in $instances) {{
            $props = Get-ItemProperty -Path $instance.PSPath -ErrorAction SilentlyContinue
            if ($null -eq $props) {{ continue }}
            if ($props.Service -ne 'HidBth') {{ continue }}
            $lower = @($props.LowerFilters)
            if ($lower -contains 'applewirelessmouse') {{
                $key = $instance.PSPath
                $previous = $lower
                $instanceId = 'BTHENUM\' + $vendorKey.PSChildName + '\' + $instance.PSChildName
                break
            }}
        }}
        if ($key) {{ break }}
    }}
    if (-not $key) {{
        Write-Status 'ERROR=no-filtered-instance'
        return
    }}
    Write-Status ('TARGET=' + $instanceId)
    Remove-ItemProperty -Path $key -Name LowerFilters -ErrorAction Stop
    Write-Status 'FILTER-REMOVED'
    Restart-Node $instanceId 'RESTART-A'
    Start-Sleep -Milliseconds 1200
    if (Test-Path -LiteralPath $readerResult) {{
        Remove-Item -LiteralPath $readerResult -Force -ErrorAction SilentlyContinue
    }}
    # This executable uses the Windows GUI subsystem, so `& $exe` would return
    # before it has written the result. Start-Process -Wait is required here;
    # restoring the filter while the reader still holds the collection can make
    # Windows request a reboot.
    $readerProcess = Start-Process -FilePath $exe -ArgumentList @('--battery-once', $readerResult) -Wait -PassThru -WindowStyle Hidden
    Write-Status ('READER-EXIT=' + $readerProcess.ExitCode)
    if (Test-Path -LiteralPath $readerResult) {{
        $line = (Get-Content -LiteralPath $readerResult -Raw).Trim()
        Write-Status ('READER=' + $line)
    }} else {{
        Write-Status 'READER=ERROR=no-output'
    }}
}} catch {{
    Write-Status ('ERROR=' + $_.Exception.Message)
}} finally {{
    if ($key -and $previous) {{
        try {{
            Set-ItemProperty -Path $key -Name LowerFilters -Value $previous -Type MultiString -ErrorAction Stop
            Write-Status 'FILTER-RESTORED'
        }} catch {{
            Write-Status ('ERROR-RESTORE=' + $_.Exception.Message)
        }}
        if ($instanceId) {{
            try {{
                Restart-Node $instanceId 'RESTART-B'
            }} catch {{
                Write-Status ('ERROR-RESTART-B=' + $_.Exception.Message)
            }}
        }}
        Start-Sleep -Milliseconds 2500
        try {{
            $now = @((Get-ItemProperty -Path $key -ErrorAction Stop).LowerFilters)
            $service = Get-Service -Name applewirelessmouse -ErrorAction SilentlyContinue
            $serviceUp = ($null -ne $service) -and ($service.Status -eq 'Running')
            $hid = @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | Where-Object {{ $_.InstanceId -match '0269' -and $_.InstanceId -like 'HID*' }})
            $unified = @($hid | Where-Object {{ $_.InstanceId -notmatch '&COL0' }})
            $split = @($hid | Where-Object {{ $_.InstanceId -match '&COL0' }})
            if (($now -contains 'applewirelessmouse') -and ($serviceUp -or ($unified.Count -gt 0 -and $split.Count -eq 0))) {{
                Write-Status 'RESTORED=True'
            }} else {{
                Write-Status ('RESTORED=False service-running=' + $serviceUp + ' unified-paths=' + $unified.Count + ' split-paths=' + $split.Count)
            }}
        }} catch {{
            Write-Status 'RESTORED=Unknown'
        }}
    }}
}}
"#
    )
}

fn build_restore_script(result: &Path) -> String {
    let result = ps_quote(&result.to_string_lossy());
    format!(
        r#"$ErrorActionPreference = 'Stop'
$result = {result}
function Write-Status([string]$text) {{
    Add-Content -Path $result -Value $text -Encoding UTF8
}}
function Restart-Node([string]$id, [string]$tag) {{
    $out = & pnputil.exe /restart-device $id 2>&1 | Out-String
    Write-Status ($tag + '=' + $LASTEXITCODE)
    if ($LASTEXITCODE -eq 0) {{ return }}
    Write-Status ($tag + '-FALLBACK=' + $LASTEXITCODE + ' ' + $out.Trim())
    try {{
        Disable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop | Out-Null
    }} catch {{
        Write-Status ($tag + '-DISABLE-ERROR=' + $_.Exception.Message)
        return
    }}
    Start-Sleep -Milliseconds 900
    try {{
        Enable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction Stop | Out-Null
        Write-Status ($tag + '-FALLBACK=ok')
    }} catch {{
        Write-Status ($tag + '-ENABLE-ERROR=' + $_.Exception.Message)
        try {{
            Enable-PnpDevice -InstanceId $id -Confirm:$false -ErrorAction SilentlyContinue | Out-Null
        }} catch {{ }}
    }}
}}
Set-Content -Path $result -Value 'STARTED' -Encoding UTF8
$key = $null
$instanceId = $null
try {{
    $vendorPattern = 'VID&0001004C_PID&0269'
    $vendorKeys = @(Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Enum\BTHENUM' -ErrorAction SilentlyContinue | Where-Object {{ $_.PSChildName -match $vendorPattern }})
    foreach ($vendorKey in $vendorKeys) {{
        $instances = @(Get-ChildItem $vendorKey.PSPath -ErrorAction SilentlyContinue)
        foreach ($instance in $instances) {{
            $props = Get-ItemProperty -Path $instance.PSPath -ErrorAction SilentlyContinue
            if ($null -eq $props) {{ continue }}
            if ($props.Service -ne 'HidBth') {{ continue }}
            $key = $instance.PSPath
            $instanceId = 'BTHENUM\' + $vendorKey.PSChildName + '\' + $instance.PSChildName
            break
        }}
        if ($key) {{ break }}
    }}
    if (-not $key) {{
        Write-Status 'ERROR=no-connected-mouse'
        return
    }}
    $lower = @((Get-ItemProperty -Path $key -ErrorAction Stop).LowerFilters)
    if ($lower -notcontains 'applewirelessmouse') {{
        $lower += 'applewirelessmouse'
        Set-ItemProperty -Path $key -Name LowerFilters -Value $lower -Type MultiString -ErrorAction Stop
        Write-Status 'FILTER-ADDED'
    }}
    Restart-Node $instanceId 'RESTART'
    Start-Sleep -Milliseconds 2500
    $now = @((Get-ItemProperty -Path $key -ErrorAction Stop).LowerFilters)
    $service = Get-Service -Name applewirelessmouse -ErrorAction SilentlyContinue
    $serviceUp = ($null -ne $service) -and ($service.Status -eq 'Running')
    $hid = @(Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue | Where-Object {{ $_.InstanceId -match '0269' -and $_.InstanceId -like 'HID*' }})
    $unified = @($hid | Where-Object {{ $_.InstanceId -notmatch '&COL0' }})
    $split = @($hid | Where-Object {{ $_.InstanceId -match '&COL0' }})
    if (($now -contains 'applewirelessmouse') -and ($serviceUp -or ($unified.Count -gt 0 -and $split.Count -eq 0))) {{
        Write-Status 'RESTORED=True'
    }} else {{
        Write-Status ('RESTORED=False service-running=' + $serviceUp + ' unified-paths=' + $unified.Count + ' split-paths=' + $split.Count)
    }}
}} catch {{
    Write-Status ('ERROR=' + $_.Exception.Message)
}}
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_successful_flip() {
        let outcome = parse_outcome(
            "STARTED\nTARGET=BTHENUM\\x\nFILTER-REMOVED\nRESTART-A=0\nREADER=PERCENT=87\nREADER-EXIT=0\nFILTER-RESTORED\nRESTART-B=0\nRESTORED=True\n",
        );
        assert_eq!(outcome.percent, Some(87));
        assert!(outcome.restored);
        assert!(!outcome.cancelled);
        assert!(outcome.error.is_none());
    }

    #[test]
    fn parses_cancelled_elevation() {
        let outcome = parse_outcome("ERROR=uac-cancelled The operation was canceled by the user\n");
        assert!(outcome.cancelled);
        assert!(outcome.restored);
        assert_eq!(outcome.percent, None);
    }

    #[test]
    fn unverified_restore_is_not_reported_as_success() {
        let outcome = parse_outcome(
            "STARTED\nFILTER-REMOVED\nREADER=PERCENT=42\nREADER-EXIT=0\n",
        );
        assert_eq!(outcome.percent, Some(42));
        assert!(!outcome.restored);
        assert!(outcome.error.is_some());
    }

    #[test]
    fn explicit_restore_failure_keeps_the_reason() {
        let outcome = parse_outcome(
            "STARTED\nFILTER-REMOVED\nRESTART-A=0\nREADER=PERCENT=64\nFILTER-RESTORED\nRESTART-B=3010\nRESTORED=False service-running=False unified-paths=0 split-paths=2\n",
        );
        assert_eq!(outcome.percent, Some(64));
        assert!(!outcome.restored);
        assert!(outcome.error.is_some());
    }

    #[test]
    fn no_filter_change_needs_no_restore() {
        let outcome = parse_outcome("STARTED\nERROR=no-filtered-instance\n");
        assert!(outcome.restored);
        assert_eq!(outcome.percent, None);
    }

    #[test]
    fn clean_line_removes_bom_and_space() {
        assert_eq!(clean_line("\u{feff}  RESTORED=True  "), "RESTORED=True");
    }
}
