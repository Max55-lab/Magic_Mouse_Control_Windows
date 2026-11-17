<#
.SYNOPSIS
    Install, inspect or remove the experimental Magic Mouse 2 observation filter.

.DESCRIPTION
    TEST VM ONLY. The driver is not signed by Microsoft; it only loads with
    `testsigning on` and Secure Boot disabled.

    Safety properties of this script:
      * never touches applewirelessmouse's own files or service,
      * on uninstall it restores LowerFilters to exactly what was there before
        the first install (saved in a side file), and verifies the result,
      * if the device stack does not look the way we expect, it says so instead
        of pretending success.

.EXAMPLE
    .\install-test.ps1 -Install

.EXAMPLE
    .\install-test.ps1 -VerifyStack

.EXAMPLE
    .\install-test.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [switch]$Install,
    [switch]$Uninstall,
    [switch]$VerifyStack,
    [string]$InfPath
)

$ErrorActionPreference = 'Stop'

function Assert-Admin {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run this script from an elevated PowerShell.'
    }
}

function Get-MouseInstance {
    $instance = Get-PnpDevice -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like 'BTHENUM*0269*' } |
        Select-Object -First 1
    if (-not $instance) {
        throw 'Bluetooth Magic Mouse 2 (BTHENUM ... 0269) not found. Connect the mouse first.'
    }
    $instance.InstanceId
}

function Get-Stack([string]$InstanceId) {
    (Get-PnpDeviceProperty -InstanceId $InstanceId -KeyName 'DEVPKEY_Device_Stack' -ErrorAction SilentlyContinue).Data
}

function Get-LowerFilters([string]$InstanceId) {
    (Get-PnpDeviceProperty -InstanceId $InstanceId -KeyName 'DEVPKEY_Device_LowerFilters' -ErrorAction SilentlyContinue).Data
}

function Show-State([string]$InstanceId) {
    $stack = Get-Stack $InstanceId
    $filters = Get-LowerFilters $InstanceId
    Write-Host "Instance : $InstanceId"
    Write-Host "Lower    : $($filters -join ', ')"
    Write-Host "Stack    : $stack"

    if ($stack -match 'MMKFilter') {
        $position = @($stack -split '\|').Trim().IndexOf('\Driver\MMKFilter')
        $apple = @($stack -split '\|').Trim().IndexOf('\Driver\applewirelessmouse')
        Write-Host "Position : MMKFilter index=$position, applewirelessmouse index=$apple"
        if ($position -lt 0 -or $apple -lt 0) {
            Write-Warning 'Unexpected stack; MMKFilter and applewirelessmouse should both be present.'
        } elseif ($position -lt $apple) {
            Write-Warning 'MMKFilter is ABOVE applewirelessmouse. Ordering is wrong: abort and fix the INF before trusting any log.'
        } else {
            Write-Host 'Ordering : OK (MMKFilter below applewirelessmouse)' -ForegroundColor Green
        }
    } else {
        Write-Host 'MMKFilter is not in the stack.' -ForegroundColor Yellow
    }
}

function Resolve-Inf {
    if ($InfPath) { return (Resolve-Path $InfPath).Path }
    foreach ($candidate in @('release', 'debug')) {
        $path = Join-Path $PSScriptRoot "target\$candidate\mmk_package\mmk.inf"
        if (Test-Path $path) { return $path }
    }
    throw 'mmk.inf not found. Build first with `cargo make default --release` in the WDK environment.'
}

function Import-TestCertificate([string]$InfPath) {
    $cert = Join-Path (Split-Path $InfPath -Parent) 'WDRLocalTestCert.cer'
    if (-not (Test-Path $cert)) {
        Write-Warning "Test certificate not found next to $InfPath; skipping import."
        return
    }
    # Test-signed drivers only load when the signing certificate is trusted
    # machine-wide, both as a root and as a publisher.
    foreach ($store in @('Root', 'TrustedPublisher')) {
        & certutil -addstore -f $store $cert | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "certutil -addstore $store failed with exit code $LASTEXITCODE"
        }
    }
    Write-Host "Imported $cert into LocalMachine\Root and LocalMachine\TrustedPublisher."
}

function Test-TestSigning {
    $secureBoot = try { Confirm-SecureBootUEFI -ErrorAction Stop } catch { $false }
    if ($secureBoot) {
        Write-Warning 'Secure Boot is ON. Test-signed drivers will not load.'
    }
    $bcd = & bcdedit /enum '{current}' | Out-String
    if ($bcd -notmatch 'testsigning\s+Yes') {
        Write-Warning 'testsigning is not enabled (bcdedit /set testsigning on; reboot).'
    }
}

Assert-Admin

if (-not ($Install -or $Uninstall -or $VerifyStack)) {
    Write-Host 'Nothing selected. Use -Install, -VerifyStack or -Uninstall.'
    exit 1
}

$instanceId = Get-MouseInstance
$stateFile = Join-Path $PSScriptRoot 'lowerfilters-before.txt'

if ($Install) {
    Test-TestSigning
    $inf = Resolve-Inf
    Write-Host "Installing $inf ..."
    Import-TestCertificate $inf

    if (-not (Test-Path $stateFile)) {
        (Get-LowerFilters $instanceId) -join ',' | Set-Content -Path $stateFile -Encoding ascii
        Write-Host "Saved original LowerFilters to $stateFile"
    }

    & pnputil /add-driver $inf /install
    if ($LASTEXITCODE -ne 0) { throw "pnputil failed with exit code $LASTEXITCODE" }

    & pnputil /restart-device $instanceId
    Start-Sleep -Seconds 2
    Show-State $instanceId
    exit 0
}

if ($VerifyStack) {
    Show-State $instanceId
    exit 0
}

if ($Uninstall) {
    Write-Host 'Removing driver package ...'
    $oem = & pnputil /enum-drivers | Out-String
    $matches = [regex]::Matches($oem, 'Published Name:\s+(oem\d+\.inf)[\s\S]*?Original Name:\s+(\S*mmk\.inf)', 'IgnoreCase')
    foreach ($match in $matches) {
        $published = $match.Groups[1].Value
        Write-Host "Deleting $published ..."
        & pnputil /delete-driver $published /uninstall /force
    }

    # pnputil usually restores LowerFilters, but a half-installed filter can
    # leave a stale entry behind. Fix it explicitly, then verify.
    if (Test-Path $stateFile) {
        $original = (Get-Content $stateFile -Raw).Trim()
        $current = (Get-LowerFilters $instanceId) -join ','
        if ($current -ne $original) {
            Write-Host "Restoring LowerFilters to '$original' ..."
            $key = "HKLM:\SYSTEM\CurrentControlSet\Enum\$instanceId"
            if ($original) {
                Set-ItemProperty -Path $key -Name LowerFilters -Value ($original -split ',') -Type MultiString
            } else {
                Remove-ItemProperty -Path $key -Name LowerFilters -ErrorAction SilentlyContinue
            }
        }
    }

    & pnputil /restart-device $instanceId | Out-Null
    Start-Sleep -Seconds 2
    Show-State $instanceId

    if ((Get-Stack $instanceId) -match 'MMKFilter') {
        Write-Warning 'MMKFilter is still in the stack. Reboot the VM and check again.'
    } else {
        Write-Host 'Clean: MMKFilter removed from the stack.' -ForegroundColor Green
    }
    exit 0
}
