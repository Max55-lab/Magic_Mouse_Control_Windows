# mmk.sys - experimental kernel tap for Magic Mouse 2

**Read [`../docs/kernel-driver.md`](../docs/kernel-driver.md) first.** This is a
Phase 0 probe: it observes the BTHENUM device stack and logs what it sees. It
does not parse touch reports yet, and it is not part of the shipped app.

## What it does

- Attaches as a **KMDF lower filter** on the BTHENUM devnode, ordered **below**
  `applewirelessmouse` (append to `LowerFilters`).
- Forwards every request unchanged, then completes it with the original status
  and `Information`.
- Copies a bounded prefix of every relevant buffer into a non-paged ring buffer
  and (rate-limited) into the kernel debug log.

It never blocks, fails, delays or rewrites I/O. If it misbehaves, the only
expected symptom is "our feature is silent", not a broken mouse.

## Build (needs a WDK environment, not this repo's toolchain)

```bat
c:\ewdk\LaunchBuildEnv.cmd
cd driver
cargo make
```

`cargo make` (not `cargo build`) runs `inf2cat`/`infverif`, signs with a generated
test certificate and writes a package to `target\release\package\`.

Prerequisites: eWDK, VS2022 Build Tools, LLVM 17 (`libclang`), stable Rust,
`cargo-make`.

## Install (test VM only)

```powershell
# admin PowerShell in a VM with testsigning on and Secure Boot off
.\install-test.ps1 -Install
.\install-test.ps1 -VerifyStack
# ...observe with DbgView (Capture Kernel) or WinDbg...
.\install-test.ps1 -Uninstall
```

## Expected result

```text
Device_Stack = \Driver\HidBth | \Driver\applewirelessmouse | \Driver\MMKFilter | \Driver\BthEnum
```

If `MMKFilter` is missing, the INF did not install or the device was not
restarted. If it appears *above* `applewirelessmouse`, do not proceed: the
ordering assumption is wrong and the observed traffic is already Apple-filtered.

## Files

| File | Purpose |
| --- | --- |
| `src/lib.rs` | `DriverEntry`, device interface GUID, global ring lock |
| `src/device.rs` | `EvtDeviceAdd`: `WdfFdoInitSetFilter` + queue + dispatching |
| `src/forward.rs` | dispatch -> `WdfRequestSend` -> completion -> complete |
| `src/probe.rs` | `CTL_CODE` decoding + hex prefix, rate limited |
| `src/ring.rs` | pre-allocated non-paged ring, spin-locked |
| `src/control.rs` | control device + `IOCTL_MMK_PING` / `IOCTL_MMK_READ_EVENTS` |
| `src/hidp.rs` | HIDP transaction header helpers |
| `src/magicmouse.rs` | report `0x12` / `0x90` decoder (host-testable) |
| `src/vhf_report.rs` | Phase 3 PTP descriptor + 30-byte encoder (host-testable) |
| `src/vhf.rs` | Phase 3 VHF skeleton behind the `vhf` feature (not shipped) |
| `mmk.inx` | extension INF (filter service + `LowerFilters` append) |

## Phase 3 skeleton (not part of `mmk.sys`)

`src/vhf_report.rs` is pure encoding and is tested on the host:

```powershell
rustc --edition 2021 --test src\vhf_report.rs -o $env:TEMP\mmtest.exe
& $env:TEMP\mmtest.exe
```

`src/vhf.rs` is the VHF source skeleton. It is **not** linked into the shipped
filter; build it only to compile-check the module:

```bat
cargo build --release --features vhf
```

The real virtual touchpad is a separate root-enumerated function driver
(`mmk-vhid.sys`) that the app feeds with filtered contacts. Design, report
layout, INF sketch and acceptance criteria are in
[`../docs/kernel-driver.md`](../docs/kernel-driver.md), section 14.

## Status / caveats

- Not compiled in this repository: there is no WDK on the development host.
  Import paths in `wdk-sys` occasionally move between releases; if the build
  fails on a single `use`, check `cargo doc -p wdk-sys` in the eWDK shell.
- The `IOCTL`/`BRB` surface between `HidBth` and `BthEnum` is undocumented.
  Phase 0 exists to discover it before committing to a parser.
- Per-report logging is capped by `LOG_BUDGET` in `src/lib.rs` (48 lines). For a
  longer probe session raise it, then watch with DbgView (Capture Kernel) and
  filter on `[mmk]`.
- Never install on a machine you are not prepared to restore from a snapshot.
