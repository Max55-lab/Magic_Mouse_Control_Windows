# mmk.sys - experimental kernel tap for Magic Mouse 2

**Read [`../docs/kernel-driver.md`](../docs/kernel-driver.md) first.** The
shipped observation filter is a Phase 0 probe: it observes the BTHENUM device
stack and logs what it sees. It is not part of the shipped app.

The same crate also contains the Phase 3 scaffold for the separate
**`mmk-vhid`** function driver, which exposes the glass as a composite
**mouse + Windows Precision Touchpad**. It only compiles with the `vhf`
feature and is never part of `mmk.sys`.

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
test certificate and writes a package to `target\release\mmk_package\`.

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
| `src/lib.rs` | `DriverEntry`, global ring lock |
| `src/device.rs` | `EvtDeviceAdd`: `WdfFdoInitSetFilter` + queue + dispatching |
| `src/forward.rs` | dispatch -> `WdfRequestSend` -> completion -> complete |
| `src/probe.rs` | `CTL_CODE` decoding + hex prefix, rate limited |
| `src/ring.rs` | pre-allocated non-paged ring, spin-locked |
| `src/control.rs` | control device + `IOCTL_MMK_PING` / `IOCTL_MMK_READ_EVENTS` / (scaffold) `IOCTL_MMK_VHID_SUBMIT` |
| `src/hidp.rs` | HIDP transaction header helpers |
| `src/magicmouse.rs` | Apple `0x12`/`0x27` / `0x90` decoder (host-testable) |
| `src/vhf_report.rs` | composite mouse + PTP descriptor, packed reports, `0x27` translator (host-testable) |
| `src/vhf.rs` | VHF source skeleton behind the `vhf` feature (not shipped) |
| `mmk.inx` | extension INF (filter service + `LowerFilters` append) |
| `mmk-vhid.inx` | **draft** INF for the Phase 3 `mmk-vhid` root function driver (INF not yet verified) |

## Phase 3 device shape (not part of `mmk.sys`)

One VHF device, three top-level collections:

| Report | Direction | Contents |
| --- | --- | --- |
| `0x01` | input | standard mouse: 3 buttons + relative X/Y from the optical sensor |
| `0x02` | input | PTP touch: 5 contacts + scan time + contact count + (always zero) buttons |
| `0x03` | feature | contact count maximum (5, low nibble) + pad type (high nibble) |
| `0x04` | feature | surface switch (bit 0) + button switch (bit 1) |
| `0x05` | feature | 256-byte PTPHQA certification blob (257 bytes with the report ID) |
| `0x06` | feature | input mode: `0` = mouse, `3` = touchpad |

Rules encoded in [`src/vhf_report.rs`](src/vhf_report.rs):

- `translate_apple_touch_to_ptp` parses a raw `0x27` (Bluetooth) or `0x12`
  (USB) report and fills a packed `PtpInputReport` with **zero allocations**.
- A frame with fewer than two active fingers returns `false` and yields an
  empty report, so one finger on the glass never moves the pointer, scrolls or
  triggers a gesture. `vhf.rs` submits that empty report once to release stale
  contacts.
- Contact size below 8 is treated as palm/hover noise and dropped, exactly like
  `magicmouse.rs`.
- `translate_apple_mouse` always carries the optical-sensor delta and the
  physical buttons on report `0x01`; clicks never travel through the PTP
  collection.

Host tests for the pure modules (no WDK needed):

```powershell
rustc --edition 2021 --test src\vhf_report.rs -o $env:TEMP\mmvhf.exe
& $env:TEMP\mmvhf.exe          # 17 tests: descriptor, translator, mouse, features
rustc --edition 2021 --test src\magicmouse.rs -o $env:TEMP\mmmouse.exe
& $env:TEMP\mmmouse.exe        # 6 tests: 0x12/0x27 parsing, battery
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
