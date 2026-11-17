# Magic Mouse 2 — kernel-mode (KMDF) arhitektura u Rustu

> Dokument opisuje **napredni smjer**: kernel filter u Rustu koji radi ispod Appleovog
> filtera na Bluetooth HID stogu. User-mode aplikacija (`magic-mouse-control.exe`)
> ostaje primarni, sigurni put; kernel driver je opcionalni dodatak i **nikad** nije
> preduvjet za rad aplikacije.
>
> Status: projektna dokumentacija + scaffold koji se **ne kompila** na ovom stroju
> (traži WDK/eWDK, test signing i VM). Vidi [Faza 0](#6-faza-0--probni-filter-koji-ništa-ne-mijenja).

---

## 0. TL;DR

| Pitanje | Odgovor |
| --- | --- |
| Treba li kernel driver za skrolanje, desni klik, bateriju? | **Ne.** To već radi user-mode app (Mode A + `SendInput`, ili Mode B + Apple filter). |
| Treba li kernel driver za *prave* Windows geste (pinch, 3-prsta) preko BT-a? | **Da**, u praksi: treba (a) čitanje sirovih touch reporta ispod Appleovog filtera i (b) VHF virtualni precision touchpad. VHF je odvojen driver, vidi [§14](#14-faza-3--vhf-virtualni-precision-touchpad). |
| Gdje točno sjediti u stogu? | **Lower filter na BTHENUM devnodeu, narjeđen *iza* `applewirelessmouse`** (bliže `BthEnum`). |
| Je li to dokumentirano? | Ne. Zato je prvi korak **pasivni probni filter** koji samo loga, ništa ne mijenja. |
| Raw HCI/BLE sniffing? | **Odbačeno** kao primarni put (nema API-ja, link je šifriran ispod hosta, nedokumentirano). Vidi [§3](#3-zašto-raw-hcible-nije-put). |
| Rizik BSOD-a? | Samo ako driver dira buffer/request lifecycle. Uz pravila iz [§10](#10-pravila-da-ne-bude-bsod-a) rizik je sveden na "driver se ne učita" umjesto "sustav padne". |
| Potpisivanje? | Test Mode + self-signed za razvoj; produkcija = Partner Center **attestation** (ili WHQL). Vidi [§12](#12-potpisivanje-i-distribucija). |

---

## 1. Stvarni stog na ovom računalu (dokaz, ne pretpostavka)

Očitano s uređaja:

```text
BTHENUM\{00001124-0000-1000-8000-00805F9B34FB}_VID&0001004C_PID&0269
  \7&3B332B66&0&ACBC32D89D8C_C00000000

  Service        = HidBth
  LowerFilters   = applewirelessmouse
  Device_Stack   = \Driver\HidBth | \Driver\applewirelessmouse | \Driver\BthEnum
```

`DEVPKEY_Device_Stack` se ispisuje **od vrha prema dnu**. To mijenja sve:

```text
        hidclass.sys  (HID devnode: HID\...&0269\..., Col01/Col02)
              │
        HidBth.sys            ← HID transport (function driver BTHENUM devnodea)
              │
   applewirelessmouse.sys     ← Appleov filter, LOWER filter BTHENUM devnodea
              │
        BthEnum / bthport     ← L2CAP, HIDP, veza prema kontroleru
              │
        Bluetooth radio (link-layer enkripcija je već odrađena ispod ovoga)
```

Posljedice koje treba razumjeti prije prvog retka koda:

1. **Appleov filter nije HID filter.** Ne sjedi između `hidclass` i `HidBth`, nego
   **ispod** `HidBth`, na transportnom (L2CAP/HIDP) sloju. Zato u Mode B:
   - HID descriptor koji vidi `hidclass` izgleda kao običan 3-button miš,
   - vendor kolekcija (`Col02`, report `0x90` za bateriju, report `0x12` za touch)
     jednostavno **ne postoji** u descriptoru koji dođe do `hidclass`,
   - a skrolanje radi jer ga Appleov filter sam sintetizira iz touch reporta.
2. **Naša user-mode aplikacija u Mode B razgovara s `hidclass`**, dakle vidi već
   obrađen, "prežvakan" tok. Zato `feature 0x47` vraća `err=87` i `input 0x90`
   vraća `err=1` — tih reporta u tom descriptoru nema.
3. **Točka presretanja "ispod Applea" je jedina koja vidi sirovi promet**, ali to
   nisu HID reporti nego **HIDP transakcije unutar L2CAP payloada**. To je bitno
   drugačiji posao od "pročitaj report buffer".

> Zato je plan napisan kao *verifikacija pa implementacija*, a ne "napiši parser i
> nadaj se". Prvi driver koji napišemo ne parsira ništa — samo bilježi što vidi.

---

## 2. Četiri moguće točke presretanja

| # | Gdje | Što vidiš | Dokumentirano | Rizik | Preporuka |
| --- | --- | --- | --- | --- | --- |
| A | **Upper filter na HID collection devnodeu** (`HID\...&0269&Col02`) | `IRP_MJ_READ` s HID reportima koje `hidclass` daje klijentu; descriptor preko `IOCTL_HID_GET_REPORT_DESCRIPTOR` | Da (Firefly, HidHide, HIDGuardian) | Nizak | Za Mode A i za "vidim 0x12 bez da mijenjam stog" |
| B | **Lower filter na HID devnodeu** (između `hidclass` i `HidBth`) | `IRP_MJ_INTERNAL_DEVICE_CONTROL` s `IOCTL_HID_*` | Djelomično | Srednji | **Ne računaj na input reporte** — na interrupt-driven transportima idu preko `HidInputReport()` callbacka, ne kroz I/O stog |
| C | **Lower filter na BTHENUM devnodeu, iza `applewirelessmouse`** | L2CAP/HIDP promet prije Appleove obrade | **Ne** („BRB/L2CAP sučelje") | Srednji–visok, ali izoliran | **Odabrano** za Fazu 0/1: najviše informacija, Apple netaknut |
| D | Raw HCI / BLE link layer | HCI paketi, advertising, L2CAP framing | Ne | Vrlo visok | Odbačeno, vidi §3 |

Napomena za B: filter **ispod** `hidclass` u praksi vidi zahtjeve za descriptorom i
`GET/SET_REPORT`, ali velik dio input prometa ne prolazi kroz njega. Zato "tee svih
reporta na lower filteru HID devnodea" nije pouzdan dizajn — to je i razlog zašto
Faza 0 mjeri, a ne pretpostavlja.

### Zašto baš *iza* Applea?

`LowerFilters` je lista **od vrha prema dnu**. Append (`0x00010008`, `REG_MULTI_SZ`
append) stavlja novi filter na **kraj**, tj. najbliže PDO-u:

```text
LowerFilters = applewirelessmouse,MMKFilter
Device_Stack = \Driver\HidBth | \Driver\applewirelessmouse | \Driver\MMKFilter | \Driver\BthEnum
```

To je deterministički i provjerljivo jednom naredbom:

```powershell
(Get-PnpDeviceProperty -InstanceId 'BTHENUM\{00001124-0000-1000-8000-00805F9B34FB}_VID&0001004C_PID&0269\...' `
  -KeyName DEVPKEY_Device_Stack).Data
```

Deklarativni `AddFilter` (`[DDInstall.Filters]` + `FilterPosition=Lower`) **nije**
prikladan ovdje: Appleov base INF ne definira filter levels, a Microsoft eksplicitno
kaže da se u tom slučaju filter ubacuje u "effectively arbitrary order". Za
"moram biti točno ispod Applea" jedini deterministički mehanizam je legacy append.

---

## 3. Zašto raw HCI/BLE nije put

Konkretno, bez marketinga:

- **Nema javnog API-ja.** Windows exposea profile-level sučelja (`BthPort`/`BthEnum`:
  SDP, L2CAP, RFCOMM). HCI ide između vendor miniporta (Intel/MediaTek/Qualcomm
  `Bth*` miniport) i kontrolera; nema dokumentiranog načina da se ubaciš u taj tok.
- **Link je šifriran ispod hosta.** LE link-layer enkripcija se odvija u kontroleru.
  Ono što bi "sniffao" na HCIju je ciphertext; za dekriptiranje treba link key i
  procedura koju Windows ne izlaže.
- **Sniffing je hardware problem, ne software.** nRF Sniffer / Ubertooth / Ellisys
  su **zasebni uređaji**. To je odličan *dijagnostički* alat (potvrda da mouse šalje
  `0x12` reporte), ali nije driver i ne može se distribuirati korisniku.
- **ETW nije payload.** `Microsoft-Windows-Bluetooth-*` provideri daju evente
  (connect/disconnect, role, RSSI), ne HIDP sadržaj.
- **User-mode raw HID i Raw Input su zatvoreni.** `ReadFile` na Bluetooth
  kolekciji vraća `Access is denied`, `HidD_GetInputReport` na report `0x27`
  vraća `ERROR_INVALID_FUNCTION`, a Raw Input registracija za `0x01/0x02`
  prima samo `RAWMOUSE` — nikad `RAWHID` s touch reportom. Zato je kernel
  filter jedini put do sirovih touch podataka preko Bluetootha.
- **Custom dongle** (npr. CSR8510 s custom firmwareom, ili ESP32 u HID host modu) je
  jedini "pravi raw HCI" put — ali tada **zaobilaziš Windows Bluetooth stack** i
  gubiš native pairing/scroll, plus tražiš od korisnika drugi adapter. Za ovaj
  proizvod: ne.

**Zaključak:** najniže što ima smisla ići je **L2CAP/HIDP razina** (točka C).
Sve ispod toga je istraživački alat, ne feature.

---

## 4. Ciljana arhitektura

```text
  Explorer / Chrome / sve aplikacije
        ▲  WM_POINTER / WM_MOUSEWHEEL / SendInput
        │
  ┌─────┴──────────────────────────────────────────────┐
  │ user mode: magic-mouse-control.exe                 │
  │  • čita ring buffer (poll IOCTL, 4–8 ms)           │
  │  • gesture engine (tap, 2-finger scroll, pinch)     │
  │  • fallback: današnji hidapi put (Mode A/B)         │
  └─────┬──────────────────────────────────────────────┘
        │ DeviceIoControl(IOCTL_MMK_READ_EVENTS)
  ┌─────┴──────────────────────────────────────────────┐
  │ kernel: mmk.sys  (KMDF, lower filter BTHENUM)      │
  │  • passthrough: NIKAD ne mijenja, NIKAD ne faila   │
  │  • kopira HIDP DATA(Input) payload u ring buffer   │
  │  • ne dira VHF: virtualni touchpad je odvojen      │
  └─────┬───────────────────────────┬──────────────────┘
        │ (naše kopiranje)          │ originalni IRP ide dalje
        ▼                           ▼
   ring buffer                applewirelessmouse.sys   ← netaknut, skrolanje radi
                                    │
                                  HidBth.sys
                                    │
                              hidclass.sys → mouhid → MouClass
```

Dvije stvari su ključne i vrijedi ih ponoviti:

1. **Native put se ne dira.** Appleov filter i dalje radi svoj posao; app i dalje ima
   skrolanje, pa čak i ako naš driver pukne, miš normalno radi.
2. **Naš put je samo kopija.** Nema "consuming", nema "exclusive", nema
   `IOCTL_HID_READ_REPORT` otimanja. To je i razlog zašto ovo ne može "zamrznuti"
   miš, samo eventualno našu feature granu.

VHF virtualni touchpad **nije** child ovog filtera. To je odvojen,
root-enumerirani function driver (`mmk-vhid.sys`) kojeg app puni filtriranim
kontaktima; detalji su u [§14](#14-faza-3--vhf-virtualni-precision-touchpad).
Zato Faza 3 radi i ako `mmk.sys` nije instaliran.

---

## 5. Faze

| Faza | Sadržaj | Rizik | Vrijednost |
| --- | --- | --- | --- |
| **0** | Pasivni probe filter: loga sve što prođe, ništa ne mijenja | Vrlo nizak | Odgovara na "što se stvarno vidi na točki C" |
| **1** | Tee HIDP DATA(Input) u ring buffer + user-mode čitanje | Nizak | Baterija 24/7, native scroll ostaje |
| **2** | Gesture engine u user mode (`SendInput`) | Nizak | 2-prsta scroll, tap-to-right-click, swipe |
| **3** | VHF virtual precision touchpad | Srednji | Prave Windows geste (pinch, 3-prsta) |
| **4** | Produkcija: attestation signing, installer, telemetrija | — | Distribucija |

Faza 3 je jedina "teška": VHF zahtijeva HID descriptor virtualnog touchpada,
`VhfCreate`/`VhfReadReportSubmit`, i pažljivo mapiranje kontakata. Ali **ne
ovisi o Fazi 1**: app već ima hidapi put i parser za `0x12`, pa se VHF može
graditi paralelno. Detalji su u [§14](#14-faza-3--vhf-virtualni-precision-touchpad).

---

## 6. Faza 0 — probni filter koji ništa ne mijenja

Cilj: **jedan** log fajl / DbgView output koji odgovara na pitanja:

1. Koji `IOCTL` kodovi prolaze prema `BthEnum`? (očekivano: L2CAP/BRB sučelje, ne `IOCTL_HID_*`)
2. Dolazi li input promet kao **IRP koji se dovršava** (pa ga vidimo u completion
   routineu) ili kao nešto drugo?
3. Je li payload **plaintext HIDP**? Prvi bajtovi trebaju biti u skupu
   `0xA1/0xA2/0xA3` (DATA Input/Output/Feature), `0x40..0x5F` (GET_REPORT),
   `0x50..` (SET_REPORT), `0x10..` (HID_CONTROL).
4. Koliki su bufferi i dolazi li izvještaj cijeli ili fragmentiran?

Kriteriji prihvaćanja Faze 0:

- [ ] `Device_Stack` potvrđuje `... | applewirelessmouse | MMKFilter | BthEnum`.
- [ ] Vidimo najmanje jedan descriptor-transfer i najmanje jedan input DATA payload.
- [ ] Identificiran je točan `IOCTL` (kod, buffered/direct, in/out veličine) za input.
- [ ] Tijekom 30 min tipičnog korištenja: 0 bugchecka, 0 izgubljenih reporta na
      native putu (miš se ponaša identično kao bez drivera).
- [ ] Driver se čisto učitava i zaustavlja (`pnputil /restart-device`).

Ako (3) ne vrijedi — npr. vidimo samo neki proprietary framing — tada točku C
odbacujemo i prelazimo na **točku A** (HID upper filter na `Col02` u Mode A), gdje
već znamo da `0x12` reporti postoje jer ih naša app čita preko `hidapi`.

Scaffold za ovo je u [`driver/`](../driver/README.md).

---

## 7. Okruženje

### 7.1 VM (obavezno)

Nikad ne razvijaj kernel driver na hostu na kojem radiš.

```powershell
# Host (Hyper-V, admin)
New-VM -Name MMK-DEV -Generation 2 -MemoryStartupBytes 8GB `
       -NewVHDPath  D:\VMs\MMK-DEV\disk.vhdx -NewVHDSizeBytes 80GB `
       -SwitchName  'Default Switch'
Set-VMFirmware -VMName MMK-DEV -EnableSecureBoot Off
```

U VM-u: Windows 11 23H2/24H2, snapshot prije svake instalacije drivera, i
**uključen kernel debugging** (inače nema šanse da dijagnosticiraš bugcheck):

```powershell
bcdedit /debug on
bcdedit /dbgsettings net hostip:192.168.1.10 port:50000 key:1.2.3.4
# VM mora imati serijski/named-pipe ili network debugging; network je najlakši
```

Na hostu: WinDbg (Store app "WinDbg"), `File → Attach to kernel → Net`, isti
port/key. Bez ovoga Faza 0 nema smisla.

### 7.2 Alati

| Alat | Verzija | Napomena |
| --- | --- | --- |
| Visual Studio 2022 Build Tools | 17.x | samo MSVC toolchain + Windows SDK |
| **eWDK** | najnoviji (npr. 24H2/25H2) | ISO → `c:\ewdk`, pokreće se `LaunchBuildEnv.cmd`; **mora biti ≥ target OS** |
| LLVM/Clang | **17.0.6** | `bindgen` traži `libclang`; LLVM 18 ima bug za ARM64 |
| Rust | stable ≥ 1.82 | `rustup default stable-x86_64-pc-windows-msvc` |
| cargo-make | 0.37.x | `cargo install --locked cargo-make --no-default-features --features tls-native` |

```powershell
winget install -i LLVM.LLVM --version 17.0.6 --force
cargo install --locked cargo-make --no-default-features --features tls-native
```

### 7.3 Toolchain i veza s WDK-om

`windows-drivers-rs` crates s crates.io ciljaju **KMDF 1.33** (Win11). To je točno
ono što nam treba. Za drugu WDF verziju treba klonirati repo i mijenjati
`crates/wdk-sys/build.rs` — ne raditi to bez potrebe.

Build se **mora** izvršavati iz eWDK konzole, jer `wdk-build` čita `WDKContentRoot`,
`WindowsSdkDir` i `KMDF_VERSION` iz okoline:

```bat
c:\ewdk\LaunchBuildEnv.cmd
cd C:\src\magic-mouse-control\driver
cargo make
```

`cargo make` (bez `cargo build`) je obavezan jer osim kompilacije pokreće
`inf2cat`, `infverif`, potpisivanje i pakiranje. Rezultat:

```text
target\release\mmk_package\
  mmk.sys
  mmk.inf
  mmk.cat
  WDRLocalTestCert.cer
```

`cargo build` sam po sebi proizvede `.sys`, ali **ne** `.cat` ni `.inf` — takav paket
se ne može instalirati.

---

## 8. Struktura projekta i build

```text
magic-mouse-control/
├─ src/                      # postojeća user-mode app (nepromijenjeno)
├─ docs/kernel-driver.md     # ovaj dokument
└─ driver/                   # zaseban Cargo workspace (ne dira app build)
   ├─ Cargo.toml
   ├─ build.rs
   ├─ Makefile.toml
   ├─ .cargo/config.toml
   ├─ mmk.inx                # extension INF (filter service + LowerFilters append)
   ├─ install-test.ps1       # test-mode install / verify / uninstall
   └─ src/
      ├─ lib.rs              # DriverEntry, globalni GUID-evi, ring lock
      ├─ device.rs           # EvtDeviceAdd, WdfFdoInitSetFilter, queue
      ├─ forward.rs          # dispatch + completion (passthrough)
      ├─ probe.rs            # dekodiranje CTL_CODE + hexdump (samo log)
      ├─ control.rs          # control device + IOCTL_MMK_PING / READ_EVENTS
      ├─ hidp.rs             # HIDP transaction header (Faza 0/1)
      ├─ magicmouse.rs       # parser reporta 0x12 / 0x90 (testira se na hostu)
      └─ ring.rs             # non-paged ring, spin-locked, bez alokacije u putu
```

`driver/` ima svoj `[workspace]`, pa `cargo build --release` u korijenu projekta
**nikad** ne pokušava kompilirati kernel kod (to bi tražilo WDK i srušilo build).

---

## 9. WDF: što se zove, kojim redom i zašto

### 9.1 `DriverEntry`

```rust
#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS
```

- `WdfDriverCreate` — stvori `WDFDRIVER`, postavi `EvtDriverDeviceAdd`.
- `WdfControlDeviceInitAllocate` + `WdfDeviceCreate` — **kontrolni** device objekt
  koji nije vezan na PnP. Služi isključivo za `DeviceIoControl` iz user modea.
  SDDL: `D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)` (SYSTEM/Admin puni pristup,
  interaktivni korisnici read/write jer app ne radi elevated).
- `WdfDeviceInitAssignName` + `WdfDeviceCreateSymbolicLink` — kontrolni device ne
  može registrirati PnP device interface, pa user mode otvara fiksni
  `\\.\MMKControl` (simbolički link `\DosDevices\MMKControl`).
- Alokacija ring buffera (`ExAllocatePool2(POOL_FLAG_NON_PAGED, ...)`) i
  `KeInitializeSpinLock`. Radi se **jednom**, prije nego ijedan IRP može doći.

### 9.2 `EvtDeviceAdd` (za BTHENUM devnode)

Redoslijed je obavezan:

```rust
WdfFdoInitSetFilter(device_init);              // 1. ja sam FILTER, ne function driver
WdfDeviceCreate(&mut device_init, ...);        // 2. tek sada
WdfIoQueueCreate(device, &mut cfg, ...);       // 3. queue
WdfDeviceConfigureRequestDispatching(          // 4. OBAVEZNO za filter
    device, queue, WdfRequestTypeDeviceControlInternal
);
```

Bez koraka 4 WDF **automatski forwarda** IRP-e i naš callback se nikad ne pozove.
To je česta zamka: driver se "učita", ali log je prazan.

Za `IRP_MJ_READ` (relevantno samo za točku A) dodatno:
`WdfDeviceConfigureRequestDispatching(device, queue, WdfRequestTypeRead)`.

### 9.3 Dispatch → forward → completion

Ovo je srce drivera i mjesto gdje se ljudi najčešće ubiju:

```rust
// dispatch (bilo koja IRQL, bez blokiranja, bez alokacije)
let device = WdfIoQueueGetDevice(queue);
let target = WdfDeviceGetIoTarget(device);          // sljedeći niži driver

WdfRequestFormatRequestUsingCurrentType(request);   // 1. sačuvaj originalni format
WdfRequestSetCompletionRoutine(request, Some(on_complete), ctx);
if WdfRequestSend(request, target, null_mut()) == 0 {   // null == WDF_NO_SEND_OPTIONS
    // 2. NIJE poslano → mi smo vlasnik → moramo dovršiti
    WdfRequestComplete(request, WdfRequestGetStatus(request));
}
// 3. ako je poslano: NE diraj request, dolazi u completion
```

```rust
unsafe extern "C" fn on_complete(
    request: WDFREQUEST,
    params: *mut WDF_REQUEST_COMPLETION_PARAMS,
    _ctx: WDFCONTEXT,
) {
    let p = &*params;
    // p.IoStatus.Status / p.IoStatus.Information
    // p.Parameters.DeviceIoControl.{IoControlCode, OutputBuffer, OutputBufferLength}
    // → kopiraj u ring, NIKAD ne mijenjaj buffer
    WdfRequestCompleteWithInformation(request, p.IoStatus.Status, p.IoStatus.Information);
}
```

Pravila koja se ne krše:

| Pravilo | Posljedica kršenja |
| --- | --- |
| Completion routine **mora** dovršiti request točno jednom | `0x9F`/`0xC2` bugcheck ili visak |
| Ako `WdfRequestSend` vrati 0, request je **naš** | zauvijek visi → aplikacija blokira, ne BSOD |
| Ne mijenjati `Information`/status | `hidclass`/`bthport` dobije krivi `bytesReturned` |
| Ne blokirati u dispatchu (`KeWaitFor*`, file I/O, `Zw*`) | **zamrzne cijeli sustav** |
| Ne alocirati u completionu | pool u DPC-u → spor, ili BSOD uz pogrešan flag |
| Ne logirati svaki report s `DbgPrint` u produkciji | 1000 Hz × DbgPrint = pad performansi |

### 9.4 HID sloj — što je što

Ako (kad) radimo točku A, ovo su kodovi koje treba prepoznati. **Brojeve uvijek
potvrdi u `hidport.h`** i u probnom logu; ovdje su navedeni kao orijentir:

| Konstanta | Vrijednost (orijentir) | Značenje |
| --- | --- | --- |
| `IOCTL_HID_GET_DEVICE_DESCRIPTOR` | `0x0B0000` | HID descriptor (ne report descriptor) |
| `IOCTL_HID_GET_REPORT_DESCRIPTOR` | `0x0B0001` | **report descriptor** — ovdje se vidi je li `0x12`/`0x90` prisutan |
| `IOCTL_HID_READ_REPORT` | `0x0B0002` | čitanje input reporta (transport-dependent!) |
| `IOCTL_HID_WRITE_REPORT` | `0x0B0003` | output report |
| `IOCTL_HID_GET_FEATURE` | `0x0B0192` | feature report (npr. `0x47`, multitouch enable) |
| `IOCTL_HID_SET_FEATURE` | `0x0B0191` | feature report write |
| `IOCTL_HID_GET_INPUT_REPORT` | `0x0B0193` | pollani input report |
| `IOCTL_HID_GET_COLLECTION_INFORMATION` | `0x0B0195` | `HID_COLLECTION_INFORMATION` (VID/PID/usage) |

Veličina input reporta dolazi iz `HIDP_CAPS.InputReportByteLength` (descriptor), a ne
iz onoga što vrati `ReadFile` — buffer mora biti ≥ te veličine, inače `hidclass`
vraća `STATUS_BUFFER_TOO_SMALL`.

### 9.5 HIDP (točka C)

Na L2CAP sloju nema "report ID na indeksu 0". Postoji HIDP transakcijski header:

```text
byte 0: 0xA0 | param   → DATA
        0x40 | param   → GET_REPORT
        0x50 | param   → SET_REPORT
        0x10 | param   → HID_CONTROL (suspend/exit suspend)
        0x00 | param   → HANDSHAKE
param (DATA): 0 = Other, 1 = Input, 2 = Output, 3 = Feature
```

Dakle `0xA1` = DATA Input. Iza headera, za Input, slijedi **report payload** čiji je
prvi bajt report ID (uključen samo ako descriptor koristi report IDs — kod Magic
Mousea koristi). To znači: HIDP parser je ~80 linija, ali **reassembly preko L2CAP
fragmenata** je ono što boli (payload može doći u više IRP-ova, a `bthport` ne daje
jamstvo da je granica HIDP okvira = granica IRP-a).

To je upravo ono što Faza 0 mora izmjeriti: dolazi li jedan report po IRP-u ili
fragmentirano.

---

## 10. Pravila da ne bude BSOD-a

1. **Driver nikad nije na kritičnom putu.** Ako nešto ne znamo, forwardamo dalje.
2. **Nema `panic!`, nema `unwrap`, nema panike u kernelu.** U `lib.rs` je
   `wdk-panic` koji radi `KeBugCheckEx` — to je *ispravno* ponašanje za
   nepopravljivu grešku, ali znači da svaki `unwrap()` na `None` ruši stroj.
   Zato: svi `Option`/`Result` se rješavaju grananjem, ne `?`/`unwrap`.
3. **Nema blokirajućih poziva** u `EvtIo*` i completion rutinama: bez
   `KeWaitForSingleObject`, bez `ZwReadFile`, bez `IoAllocateMdl` + mapiranja.
4. **Nema alokacije po reportu.** Ring se alocira jednom, u `DriverEntry`.
5. **Spinlock samo oko kopiranja u ring**, nikad oko poziva u WDF.
6. **Rate-limit loganja.** Brojač po `IOCTL` kodu; prvih N (npr. 20) hexdumpova,
   dalje samo brojač.
7. **Nikad ne dovršavaj request koji nije tvoj** i nikad dvaput.
8. **Ne diraj `IRP_MJ_PNP`/`IRP_MJ_POWER`.** WDF ih forwarda; svaka "pametna"
   obrada ovdje je klasik za `0xD1`/`0x9F`.
9. **Ne mijenjaj I/O type** (`WdfDeviceInitSetIoType`) za tuđe IRP-ove.
10. **Watchdog u user modeu.** Aplikacija svakih 5 s pita `IOCTL_MMK_PING`; ako 3
    pokušaja padnu, prikaže banner "kernel modul ne odgovara" i **ne** šalje dalje
    input. Driver koji visi ne smije objesiti ni app.
11. **Testiraj s Driver Verifierom** od prvog dana:
    `verifier /standard /driver mmk.sys`.
12. **Snapshot prije svake instalacije.** Bez iznimke.

### Recovery (kad ipak padne)

```text
1. VM se reboota; ako bugcheck je u mmk.sys → sustav se digne (driver nije boot-critical)
2. Ako se ne digne: boot menu → "Disable driver signature enforcement"
3. Safe Mode → pnputil /delete-driver oemXX.inf /uninstall /force
4. ili: bcdedit /deletevalue testsigning  (pa reboot)
5. WinRE → C:\Windows\System32\drivers\mmk.sys preimenovati/obrisati
```

Filter koji nije `Boot`/`System` start ne može spriječiti boot. Zato u INF-u
`StartType = 3` (DEMAND_START), nikad 0/1.

---

## 11. Test Mode — procedura od nule

### 11.1 Priprema VM-a

```powershell
# admin, VM
bcdedit /set testsigning on
bcdedit /set nointegritychecks off
Shutdown /r /t 0
# nakon reboota u donjem desnom kutu stoji "Test Mode"
```

> Secure Boot mora biti **OFF** u VM firmwareu, inače `testsigning` ne radi.

### 11.2 Build + potpis

`cargo make` sam generira test cert i potpiše `.sys` + `.cat`:

```text
target\release\mmk_package\WDRLocalTestCert.cer
```

Certifikat importaj u **Trusted Root** i **Trusted Publishers** (LocalMachine).
Bez toga `pnputil` odbije paket s `0xE000024B` („ne može verificirati potpis").

```powershell
Import-Certificate -FilePath .\target\release\mmk_package\WDRLocalTestCert.cer `
  -CertStoreLocation Cert:\LocalMachine\Root
Import-Certificate -FilePath .\target\release\mmk_package\WDRLocalTestCert.cer `
  -CertStoreLocation Cert:\LocalMachine\TrustedPublisher
```

### 11.3 Instalacija

```powershell
# 1. u paket, bez instalacije na device
pnputil /add-driver .\target\release\mmk_package\mmk.inf /install

# 2. provjeri da je u stogu (OBAVEZNO)
Get-PnpDevice -Class HIDClass | Where-Object InstanceId -like 'BTHENUM*0269*' |
  ForEach-Object {
    "{0}`n  {1}" -f $_.InstanceId,
      (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName DEVPKEY_Device_Stack).Data
  }
```

Očekivano: `\Driver\HidBth | \Driver\applewirelessmouse | \Driver\MMKFilter | \Driver\BthEnum`.

### 11.4 Log i debug

#### 11.4.1 DbgView

1. Pokreni `Dbgview64.exe` **kao administrator**.
2. `Capture` → uključi **Capture Kernel** (`Ctrl+K`). Bez toga se `DbgPrint`
   iz drivera ne prikazuje.
3. `Edit` → **Filter/Highlight** (`Ctrl+L`), u polje *Include* upiši `[mmk]`.
4. `Capture` → **Log Buffer Size** postavi na 4 MB; default se prebrzo prebriše.
5. Reproduciraj: pomakni miš, dva prsta, desni klik, pričekaj 10 s.

Za log u datoteku:

```powershell
Dbgview64.exe /accepteula /k /l C:\mmk-phase0.log
```

> `/k` uključuje kernel capture, `/l` piše u datoteku. DbgView mora ostati
> pokrenut do kraja testa; zatvaranje flushe buffer.

`println!` iz `wdk` ide kroz `DbgPrintEx`, pa ga vide i DbgView i WinDbg
(`!dbgprint`). Ako se ne pojavljuje ništa osim `filter attached`, provjeri da
driver nije izbačen iz stoga (`pnputil /enum-devices /connected /class HIDClass`)
i da `[mmk]` nije slučajno u *Exclude* filteru.

#### 11.4.2 Što se mora vidjeti

Točan oblik linija (budžeti su namjerni, vidi napomenu ispod):

| Linija | Značenje |
| --- | --- |
| `[mmk] filter attached` | `EvtDeviceAdd` je prošao; filter je u stogu. |
| `[mmk] -> ioctl=0x000B0000 dev=0x000B fn=0x000 method=buffered access=0 in=0 out=256` | Prema dolje ide `IOCTL`; `dev`/`fn`/`method` su dekodirani iz koda. |
| `[mmk] <- ioctl=... st=0x00000000 info=256 n=256 data=a1...` | Completion. `data` je **prvih 24 bajta**; `a1` = HIDP DATA Input. |
| `[mmk] -> READ out=64` i `[mmk] <- READ ... data=a1...` | Samo ako je `WATCH_READS=true` (HID devnode). Na BTHENUM ih **ne** očekuj. |
| `[mmk] power stop action=1` | `EvtIoStop` tijekom D3/suspenda; svaki request je potvrđen. |
| `[mmk] power resume action=0` | `EvtIoResume` nakon D0. |

Gornji `ioctl` primjer je HID-class kod (`dev=0x000B`); na BTHENUM će `dev`/`fn`
biti drugačiji, ali oblik linije je isti. Bitno je da `method`, `in` i `out`
odgovaraju onome što vidiš u `!irp`/`!irpfind` za isti request.

`LOG_BUDGET` je 48 linija po učitavanju drivera, a `POWER_LOG_BUDGET` 24. Kad se
potroše, driver namjerno šuti; **tišina nije hang**. Ring buffer prima 256 zapisa
i ne ovisi o logu.

Ako vidiš descriptor transfer, a nijedan `data=a1...`, to znači da input ne
prolazi kroz ovaj devnode (npr. Appleov filter ga je već potrošio) ili da je
payload fragmentiran. To je upravo nalaz Faze 0, ne bug u driveru.

#### 11.4.3 Čega ne smije biti

Nijedna `[mmk]` linija koja sadrži `failed`, i nijedan od ovih bugcheckova:

| Kod | Ime | Najčešći uzrok u ovom driveru |
| --- | --- | --- |
| `0x0A` | `IRQL_NOT_LESS_OR_EQUAL` | pristup pagiranom bufferu na DISPATCH_LEVEL |
| `0xD1` | `DRIVER_IRQL_NOT_LESS_OR_EQUAL` | isto, s atribucijom na `mmk.sys` |
| `0x9F` | `DRIVER_POWER_STATE_FAILURE` | `EvtIoStop` nije potvrdio svaki outstanding request |
| `0xC4` | `DRIVER_VERIFIER_DETECTED_VIOLATION` | dvostruko completeanje, pool corruption, pogrešan IRQL |
| `0x7E` | `SYSTEM_THREAD_EXCEPTION_NOT_HANDLED` | panic u driveru (index/overflow) |
| `0x50` | `PAGE_FAULT_IN_NONPAGED_AREA` | pokazivač na oslobođenu ili pagiranu memoriju |

`0x9F` nakon sleepa je gotovo uvijek `EvtIoStop` koji nije odgovorio na **svaki**
request, uključujući forwardane. `forward.rs` zato radi
`WdfRequestStopAcknowledge(request, FALSE)` za svaki request koji drži; ako se to
promijeni, test sna je obavezan prije mergea.

#### 11.4.4 WinDbg

Kernel debug postavi po [§7.1](#71-vm-obavezno), pa u WinDbg:

```text
!drvobj MMKFilter 7            # device objecti filtera
!devstack <DEVICE_OBJECT>      # HidBth -> applewirelessmouse -> MMKFilter -> BthEnum
!wdfkd.wdfldr                  # učitani KMDF driveri i verzije
!wdfkd.wdflogdump MMKFilter    # WDF log: power prijelazi, queue stanja, greške
!irpfind                       # outstanding IRP-ovi; kad miruje, nijedan ne smije biti na MMKFilter
!verifier 3 MMKFilter          # statistika Verifiera za driver
```

Ime u `!drvobj` i `!wdfkd.wdflogdump` je **ime servisa** (`MMKFilter`), ne ime
fajla (`mmk.sys`). `!devstack` address dobiješ iz `!drvobj MMKFilter 7`.
Redoslijed je bitan:
`\Driver\HidBth | \Driver\applewirelessmouse | \Driver\MMKFilter | \Driver\BthEnum`.
Ako je `MMKFilter` **iznad** Applea, INF je pogrešan; ne vjeruj nijednom logu.

`!wdfkd.wdflogdump MMKFilter` mora pokazati power prijelaze i queue stanja bez
`Error`/`Fatal` zapisa. `!irpfind` na mirnom sustavu ne smije naći IRP koji
targetira `MMKFilter`; ako nađe, request je leakao (naš driver ga je zaboravio
completeati).

#### 11.4.5 Driver Verifier

```powershell
verifier /standard /driver mmk.sys
verifier /querysettings
# reboot
# ... 30 min rada + 3 x sleep/wake ...
verifier /reset
```

`/standard` je flag maska `0x209BB`: special pool, pool tracking, force IRQL
checking, I/O verification, deadlock detection, security/misc checks i DDI
compliance. **Low resources simulation nije u standardu** (to je flag `0x4`);
ako ga želiš, koristi `/flags 0x209BF`, ali očekuj agresivne alokacijske
failove i ne tumači ih kao regresiju dok ne ponoviš bez njega.

Za produkcijski soak koristi i `verifier /bootmode resetonbootfail`, da stroj
sam resetira Verifier ako prvi boot nakon uključivanja padne.

#### 11.4.6 Test sna i power prijelaza

```powershell
powercfg /a                    # vidi je li S3 ili S0ix dostupan
# Sleep iz Start izbornika, ili:
rundll32.exe powrprof.dll,SetSuspendState 0,1,0
# probudi mišem, pričekaj 5 s, provjeri da miš radi
# ponovi 3x
```

Zatim restart devnodea (D0 -> remove -> D0) i disable/enable (D3 -> D0):

```powershell
$id = (Get-PnpDevice -Class HIDClass | Where-Object InstanceId -like 'BTHENUM*0269*').InstanceId
pnputil /restart-device $id
Disable-PnpDevice -InstanceId $id -Confirm:$false
Enable-PnpDevice -InstanceId $id -Confirm:$false
```

Nakon svakog koraka: miš mora odmah raditi, a u logu mora biti
`power stop action=1` pa `power resume action=0`. Ako se pojavi `0x9F`, ne
nastavljaj s Fazom 1.

#### 11.4.7 Acceptance checklist za Fazu 0

- [ ] `[mmk] filter attached` točno jednom po startu devnodea; `!devstack` pokazuje
      točno jedan `MMKFilter`.
- [ ] Najmanje jedan `<- ioctl` s `data=a1...` nakon pokreta prsta.
- [ ] `!wdfkd.wdflogdump MMKFilter` bez `Error`/`Fatal`; power prijelazi vidljivi.
- [ ] `!irpfind` na mirnom sustavu: nijedan IRP na `MMKFilter`.
- [ ] 3 x sleep/wake + 1 x restart + 1 x disable/enable: 0 bugchecka, miš radi
      nakon svakog koraka.
- [ ] 30 min s `verifier /standard`: 0 bugchecka.
- [ ] Nakon deinstalacije `LowerFilters` je točno `applewirelessmouse`.
- [ ] Native skrolanje i dalje radi bez promjene osjećaja (A/B usporedba).

### 11.5 Deinstalacija

```powershell
pnputil /enum-drivers | Select-String -Context 3 mmk   # nađi oemNN.inf
pnputil /delete-driver oemNN.inf /uninstall /force
# provjeri da je LowerFilters opet samo applewirelessmouse
```

Ako se `LowerFilters` ne očisti (poznati problem kod ručnog `sc create`), počisti
ručno i restartaj devnode:

```powershell
$k = 'HKLM:\SYSTEM\CurrentControlSet\Enum\BTHENUM\{00001124-0000-1000-8000-00805f9b34fb}_VID&0001004c_PID&0269\7&3b332b66&0&acbc32d89d8c_c00000000'
Set-ItemProperty $k -Name LowerFilters -Value @('applewirelessmouse') -Type MultiString
pnputil /restart-device '<instance id>'
```

---

## 12. Potpisivanje i distribucija

| Namjena | Što treba |
| --- | --- |
| Vlastiti razvoj | Test Mode + self-signed (generira `cargo make`) |
| Interna distribucija | Test Mode na svakom stroju — **neprihvatljivo za korisnike** |
| Javna distribucija (Win10 1607+) | **EV code-signing cert** + Partner Center račun + **attestation signing** |
| WHQL / "Windows Hardware Compatible" | HLK testovi + submission (za filter driver obično nije potrebno) |

Attestation signing je realan put: predaš `.cab`/`.msix` s `.sys`+`.inf`+`.cat`,
Microsoft potpiše, ti distribuiraš potpisani paket. **Ne** treba HLK za obični
filter driver, ali treba proći `InfVerif` i "universal INF" pravila.

Za distribuciju **bez** drivera: aplikacija ostaje onakva kakva je danas (user mode,
bez test modea, bez BSOD rizika). To je i preporučeni default za sve korisnike koji
ne žele eksperimentalni mod.

---

## 13. Integracija s postojećom aplikacijom

Driver je **opcionalan** i app ga mora tretirati kao takvog:

```rust
pub enum TouchSource {
    UserModeHid,   // današnji put (Mode A, Col02, report 0x12)
    KernelTap,     // mmk.sys ring buffer
    None,
}
```

- Detekcija: `CreateFileW` na `\\.\MMKControl`; ako ne uspije → `UserModeHid`.
- Ako `KernelTap` prekine s radom (3 × timeout) → tiho prebaci na `UserModeHid`
  i upiši u log; **ne** prikazuj grešku koja blokira app.
- Sve postavke koje diraju kernel put idu pod "Advanced / Experimental", uz
  eksplicitni checkbox "I understand this installs a kernel driver".
- `experimental_input` je korisnička postavka za user-mode čitanje (default
  `true`); kernel driver se nikad ne instalira ni uključuje bez tog checkboxa.

Time zadržavamo obećanje iz README-a: aplikacija nikad ne može zamrznuti računalo
ako kernel modul nije instaliran.

---

## 14. Faza 3 — VHF virtualni precision touchpad

Cilj: Windows dobije **pravi precision touchpad**, pa sam radi inerciju, pinch,
rotaciju, 3-prstni swipe i četveroprstne geste. Naš user-mode gesture engine
ostaje fallback kad VHF driver nije instaliran.

Stanje koda: `driver/src/vhf_report.rs` (kompozitni descriptor, packed reporti,
`0x27` translator) je host-testiran (17 testova), a `driver/src/magicmouse.rs`
(0x12/0x27 parser) ima 6 testova. `driver/src/vhf.rs` je skeleton iza featurea
`vhf` i **nije kompajliran na ovom stroju** (nema WDK-a). Ostalo je za
integraciju: IOCTL ulaz u `mmk-vhid`, INF/paket i watchdog za stuck kontakte.

### 14.1 Što VHF jest (i što nije)

VHF (Virtual HID Framework) nije bus driver i ne stvara device sam. To je
kernel biblioteka (`vhf.sys`) kojoj **naš function driver** preda report
descriptor i onda ubacuje input reporte. Zato:

- VHF source mora biti **KMDF function driver** s vlastitim FDO-om; UMDF ne
  može biti VHF source.
- `vhf.sys` mora biti **lower filter** tog drivera:
  `HKR,,"LowerFilters",0x00010000,"vhf"`.
- VHF se ne može "ugurati" u BTHENUM stog kao upper filter Appleovog devnodea.

### 14.2 Zašto odvojen driver

| Layout | Kako radi | Rizik | Presuda |
| --- | --- | --- | --- |
| **A** | `mmk.sys` (BTHENUM filter) ujedno je VHF source | VHF ulazi u power putanju fizičkog miša; bug u touchpadu može srušiti native input; filter devnode nema svoj HID stack | ne |
| **B** | `mmk-vhid.sys` root-enumerated, app ga puni preko IOCTL-a | touchpad bug ne dira miš; radi i bez `mmk.sys`; app je već tu | **preporučeno** |
| **C** | `mmk.sys` šalje frameove kernel-to-kernel u `mmk-vhid` | radi i kad je app zatvorena; dodaje I/O target i još jedan lifecycle u filter | kasnije, ako treba |

Ključna posljedica: **Faza 3 ne ovisi o Fazi 0/1.** App već ima hidapi put
(Mode A) i parser za `0x12`; dovoljno je te reporte filtrirati i proslijediti.
Ako app ne radi, nema virtualnih gesta, ali fizički miš i native scroll rade
normalno.

```text
Apple 0x12 (hidapi, Col02) ──► app: filter + mapiranje ──► IOCTL_MMK_VHID_SUBMIT
                                                                    │
                                                                    ▼
                                                        mmk-vhid.sys (ROOT\MMKVHID)
                                                                    │ VhfReadReportSubmit
                                                                    ▼
                                                        vhf.sys → hidclass → PTP stack
```

`mmk.sys`/ring je opcionalan paralelni put; za VHF ga ne treba.

### 14.3 Lifecycle

| Korak | Gdje | IRQL | Napomena |
| --- | --- | --- | --- |
| `WdfDeviceCreate` | `EvtDeviceAdd` | PASSIVE | function driver, **ne** `WdfFdoInitSetFilter` |
| `VhfCreate` | nakon device create | PASSIVE | `VHF_CONFIG.Size` je obavezan; descriptor mora ostati živ do deletea |
| `VhfStart` | odmah nakon | PASSIVE | tek nakon ovoga submit ima smisla |
| submit | `EvtIoDeviceControl` (app) | ≤ DISPATCH | `HID_XFER_PACKET` na stacku je OK jer VHF kopira report |
| feature callback | VHF | ≤ DISPATCH | `VhfAsyncOperationComplete` točno jednom, i na grešci |
| stop | `EvtCleanupCallback` | PASSIVE | `VhfDelete(handle, TRUE)`; prvo zaustavi producente |
| cleanup callback | VHF tijekom deletea | ≤ DISPATCH | samo počisti handle; **ne** zvati VHF API |

Handle je `AtomicPtr`: `submit` radi `load(Acquire)`, `stop` radi
`swap(null, AcqRel)`. Nakon swapa nijedan novi submit ne prolazi, a
`VhfDelete(handle, TRUE)` čeka in-flight operacije.

> **Race koji atomic sam ne rješava:** submit koji je pročitao handle **prije**
> swapa može ući u `VhfReadReportSubmit` nakon što je `VhfDelete` počeo. Za
> produkciju koristi `EX_RUNDOWN_REF`: submit radi
> `ExAcquireRundownProtection` (≤ DISPATCH), `stop` radi
> `ExWaitForRundownProtectionRelease` (PASSIVE) **prije** `VhfDelete`. Atomics
> je dovoljan samo ako je queue već prazan i nema in-flight handlera.

Kostur konfiguracije iz `driver/src/vhf.rs`:

```rust
let mut config: VHF_CONFIG = unsafe { core::mem::zeroed() };
config.Size = core::mem::size_of::<VHF_CONFIG>() as u32;
config.VhfClientContext = context.cast();
config.OperationContextSize = 0;
config.DeviceObject = wdf_device_to_wdm(device);
config.VendorID = 0x1209;          // placeholder, vidi 14.8
config.ProductID = 0x0001;
config.ReportDescriptor = REPORT_DESCRIPTOR.as_ptr().cast_mut();
config.ReportDescriptorLength = REPORT_DESCRIPTOR.len() as u16;
config.EvtVhfAsyncOperationGetFeature = Some(on_get_feature);
config.EvtVhfAsyncOperationSetFeature = Some(on_set_feature);
config.EvtVhfCleanup = Some(on_cleanup);
```

`OperationContextSize = 0` znači da VHF neće dati per-operacijski kontekst
(`VhfOperationContext` je NULL); naši callbackovi ga ne trebaju. Ako ga trebaš,
postavi veličinu i koristi `VhfOperationContext` umjesto globalnog stanja.

Build (u `driver/`, iz eWDK shell-a):

```powershell
cargo build --release --features vhf
```

To je **compile-check** VHF modula u postojećem crateu; pravi `mmk-vhid.sys` je
zaseban crate/paket (vidi 14.10). Ne distribuiraj `mmk.sys` s uključenim `vhf`
featureom: default build bez featurea je samo observation filter i ne linka
`vhf.sys`.

### 14.4 Descriptor i report

Descriptor je **539 bajtova** i opisuje **kompozitni uređaj** s tri top-level
kolekcije:

- standardni **Mouse** (report `0x01`): relativni X/Y optičkog senzora i fizičke
  tipke. Pokazivač i klikovi uvijek idu ovim putem;
- **Touch Pad** (report `0x02`): pet kontakata, scan time, contact count i
  rezervirane tipke (uvijek 0);
- **Configuration** (report `0x06`): input mode.

Svaki prst nosi 40 bita: confidence + tip switch (2), contact ID (4), padding
(2), X (16), Y (16). Pet prstiju + scan time + contact count + tipke = 29
bajtova payloada.

| Report ID | Tip | Payload | `reportBufferLen` | Svrha |
| --- | --- | --- | --- | --- |
| `0x01` | Input | 5 B | 6 B | mouse: 3 tipke + padding, dx i16, dy i16 |
| `0x02` | Input | 29 B | 30 B | 5 kontakata + scan time + count + buttons |
| `0x03` | Feature | 1 B | 2 B | contact count max (low nibble 5) + pad type (high nibble 0) |
| `0x04` | Feature | 1 B | 2 B | surface switch (bit 0) + button switch (bit 1) |
| `0x05` | Feature | 256 B | 257 B | PTPHQA certifikacijski blob |
| `0x06` | Feature | 1 B | 2 B | input mode: 0 = mouse, 3 = touchpad |

Layout PTP input reporta:

```text
byte 0        report ID 0x02
bytes 1..25  5 x 5-byte kontakt
bytes 26..27  scan time, 100 us jedinice, little endian
byte 28       broj aktivnih kontakata
byte 29       tipke: bit 0 lijevo, bit 1 desno, bit 2 srednje (uvijek 0)
```

Layout mouse reporta:

```text
byte 0        report ID 0x01
byte 1        tipke: bit 0 lijevo, bit 1 desno, bit 2 srednje, 5 bita paddinga
bytes 2..4    relativni dx, i16 little endian
bytes 4..6    relativni dy, i16 little endian
```

Layout kontakta (LSB first, kako HID pakira polja):

```text
bit 0      confidence (1 = valjan prst; palme filtriramo prije prijevoda)
bit 1      tip switch (1 = prst na povrsini)
bits 2..5  contact ID, 0..15
bits 6..7  padding
bytes 1..2 X, 0..4095, little endian
bytes 3..4 Y, 0..4095, little endian
```

Pravila prijevoda (`translate_apple_touch_to_ptp`, `translate_apple_mouse`):

- Ulaz je sirovi Apple report `0x27` (Bluetooth) ili `0x12` (USB): bajt 0 je ID,
  bajt 1 tipke, bajtovi 2..6 dx/dy, a touch zapisi počinju na bajtu 14 (8
  bajtova po zapisu, do 15 zapisa).
- **Šalje se samo `contact_count >= 2`.** Kad padne na 0 ili 1, translator
  vraća `false` i puni prazan report; `submit_apple_report` ga pošalje jednom da
  Windows podigne zakačene kontakte. Jedan prst zato nikad ne pomiče pokazivač,
  ne skrola i ne pokreće gestu.
- Filtriraju se zapisi sa `size < 8` (palme/hover) i sa `state & 0xf0 == 0`.
- `translate_apple_mouse` radi za svaki report, neovisno o broju prstiju:
  optički senzor ostaje jedini izvor pokazivača.
- Mapiranje: `x_abs = clamp(x + 2048, 0, 4095)`. Y je pri dekodiranju negiran
  (`y = -y_raw`), pa je stvarni raspon `-2047..=2048`; clamp je isti.
- Fizički maksimumi: X = 224 (2.24 in = 57 mm), Y = 445 (4.45 in = 113 mm).
  To odgovara izmjerenoj površini Magic Mousea 2.
- Contact ID se uzima iz low 4 bita; duplicate ID se zamijeni prvim slobodnim.
  Bez toga Windows preskoči frame ili skače kursorom.
- Ako se vertikalni smjer na hardveru pokaže obrnutim, invertiraj **samo u
  translatoru** (`4095 - y_abs`), ne u parseru; parser je već testiran.

`scan_time` je delta u 100 µs jedinicama. Za 8 ms report interval to je 80;
`0` je dopušten, ali Windows bolje procjenjuje brzinu s pravom vrijednošću.
Vrijednost dolazi iz app-a (monotoni sat), kernel je ne izmišlja.

### 14.5 Feature reporti i input mode

- **0x06 input mode**: default je 0 (mouse). Windows pošalje 3 nakon što pročita
  descriptor i configuration collection. Dok je 0, `submit_ptp` ne šalje touch
  reporte. Report `0x01` (mouse) i dalje radi jer je to put pokazivača.
- Prijelaz 3 → 0 mora prvo poslati **jedan prazan PTP frame** (count 0), inače
  zadnji kontakti ostanu "dolje". To je implementirano u `store_feature`.
- **0x04 surface/button switch**: Windows Settings njime uključuje i isključuje
  površinu i tipke. Gašenje surface bita također prvo prazni kontakte. Button
  switch ne gasi fizičke klikove jer oni idu mouse kolekcijom.
- **0x03 contact count max / pad type**: low nibble = 5, high nibble = 0
  (depressible/click-pad). Windows to čita prije uključivanja gesta; pogrešan
  pad type mijenja ponašanje klika.
- **0x05 PTPHQA**: 256 bajtova payloada + report ID = 257. Report je **obavezan**
  čim uređaj koristi HID Touchpad usage; bez njega Windows ne tretira uređaj
  kao precision touchpad. Potpisani blob je opcionalan na Windows 10 (dovoljan
  je Microsoftov default sample blob), a Windows 8.1 traži certificirani.
  Naš blob je upravo taj default; uređaj radi na Win10/11, ali je "uncertified".
  Partner Center/HLK certifikacija je zaseban zadatak, ne kod.

Konvencija feature buffera (najčešća greška):

- `HID_XFER_PACKET.reportBuffer[0]` je **report ID**, payload počinje na `[1]`.
- `reportBufferLen` **uključuje** ID byte.
- `reportId` polje također postavi; ako ga ne postaviš, HID klasa očekuje 0.
- Na GET provjeri `reportBufferLen >= 1 + payload`; na SET čitaj payload s
  `[1]`. Vrati `STATUS_BUFFER_TOO_SMALL` ako host buffer nije dovoljan.

### 14.6 Callbackovi i ponašanje na grešku

| Callback | Smiješ | Ne smiješ |
| --- | --- | --- |
| `EvtVhfAsyncOperationGetFeature` | napuniti buffer, postaviti `reportBufferLen`, zvati `VhfAsyncOperationComplete` | ostaviti operaciju bez `VhfAsyncOperationComplete` (callback je VOID) |
| `EvtVhfAsyncOperationSetFeature` | parsirati `[1]`, spremiti mode, complete | blokirati ili čekati na događaj |
| `EvtVhfAsyncOperationGetInputReport` | ostaviti `NULL` (nemamo input feature reporta) | implementirati bez descriptor reporta |
| `EvtVhfAsyncOperationWriteReport` | ostaviti `NULL` (nemamo output reporta) | implementirati "za svaki slučaj" |
| `EvtVhfReadyForNextReadReport` | ostaviti `NULL` = VHF kopira report prije returna | s implementiranim callbackom pustiti buffer prije nego VHF zatraži sljedeći |
| `EvtVhfCleanup` | počistiti handle | zvati `VhfDelete`/`VhfReadReportSubmit` (rekurzija u delete) |

`VhfReadReportSubmit` vraća `NTSTATUS`: nenegativan status je uspjeh (report je
prihvaćen/buffered), negativan znači drop. Ne retryaj u petlji; baci frame i
pusti sljedeći. Ako uključiš `EvtVhfReadyForNextReadReport`, buffer mora živjeti
do callbacka i tada **ne** smiješ submitati prije njega.

### 14.7 INF i instalacija

Skica `mmk-vhid.inx` (function driver, root-enumerated):

```inf
[Version]
Signature   = "$WINDOWS NT$"
Class       = HIDClass
ClassGuid   = {745a17a0-74d3-11d0-b6fe-00a0c90f57da}
Provider    = %Provider%
CatalogFile = mmk-vhid.cat
PnpLockdown = 1
DriverVer   = 11/15/2026,0.1.0.0

[DestinationDirs]
DefaultDestDir = 13

[SourceDisksNames]
1 = %DiskName%,,,""

[SourceDisksFiles]
mmk-vhid.sys = 1

[Manufacturer]
%Mfg% = MMKVhid, NT$ARCH$

[MMKVhid.NT$ARCH$]
%DeviceDesc% = MMKVhid_Inst, ROOT\MMKVHID

[MMKVhid_Inst.NT]
CopyFiles = @mmk-vhid.sys

[MMKVhid_Inst.NT.HW]
AddReg = MMKVhid_HW

[MMKVhid_HW]
HKR,,"LowerFilters",0x00010000,"vhf"

[MMKVhid_Inst.NT.Services]
AddService = MMKVhid, 0x00000002, MMKVhid_Service

[MMKVhid_Service]
ServiceType    = 1
StartType      = 3
ErrorControl   = 1
ServiceBinary  = %13%\mmk-vhid.sys

[MMKVhid_Inst.NT.Wdf]
KmdfService = MMKVhid, MMKVhid_Wdf

[MMKVhid_Wdf]
KmdfLibraryVersion = $KMDFVERSION$

[Strings]
Provider   = "Magic Mouse Control"
Mfg        = "Magic Mouse Control"
DeviceDesc = "Magic Mouse Control virtual precision touchpad"
DiskName   = "Magic Mouse Control VHF driver"
```

`0x00000002` u `AddService` je `SPSVCINST_ASSOCSERVICE` (ovaj driver je funkcijski,
ne filter). `LowerFilters` je `REG_SZ` (`0x00010000`), ne append varijanta.

**ABI prema app-u (predloženo, još nije u kodu).** Driver u `EvtDeviceAdd` zove
`WdfDeviceCreateDeviceInterface` s:

```rust
// {7f4c2a91-3b6e-4d2a-9c1f-5e8d0b7a6c24}
pub const GUID_DEVINTERFACE_MMK_VHID: GUID = GUID {
    Data1: 0x7f4c_2a91,
    Data2: 0x3b6e,
    Data3: 0x4d2a,
    Data4: [0x9c, 0x1f, 0x5e, 0x8d, 0x0b, 0x7a, 0x6c, 0x24],
};
```

App ga otvara `CreateFileW` i šalje fiksni frame od 30 bajtova (report ID + 29 B payloada):

```c
#define IOCTL_MMK_VHID_SUBMIT \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_WRITE_DATA)

#pragma pack(push, 1)
typedef struct _MMK_VHID_CONTACT {
    UINT8 id;      // low 4 bits
    INT16 x;       // -2048..2047
    INT16 y;       // -2048..2047
} MMK_VHID_CONTACT;          // 5 bytes with pack(1)

typedef struct _MMK_VHID_FRAME {
    UINT8  report_id;             // 0x02 = PTP touch input (informational)
    MMK_VHID_CONTACT contacts[5]; // 5 bytes each = 25, offsets 1..25
    UINT16 scan_time_100us;       // offset 26, 100 us, little endian
    UINT8  count;                 // offset 28, 0..5
    UINT8  buttons;               // offset 29, bit 0 left, 1 right, 2 middle
} MMK_VHID_FRAME;                  // 30 bytes: 1 + 25 + 2 + 1 + 1, pack(1)
#pragma pack(pop)
```

Frame je doslovno **30 B** (`report_id` + 29 B payloada), što odgovara
`TOUCH_REPORT_LEN = 1 + MAX_CONTACTS*CONTACT_LEN + 2 + 1 + 1 = 30`. Driver
traži barem 30 B, čita `count` s offseta **28** i zove `encode_touch_report`
(HID bit packing ostaje u kernelu). Ako je `count < 2`, šalje se prazan PTP
frame, pa app ne može zaobići pravilo jednog prsta. App i dalje filtrira
`size >= 8` i down stanje.

Kernel ostaje glup i bez gesture logike. Ako app nije otvoren, nema frameova;
watchdog (14.8) šalje prazan frame nakon 500 ms.

> **ACL:** ako app ne radi kao administrator, provjeri SDDL device interfacea.
> Default PnP ACL može dopustiti samo SYSTEM/Administrators; tada
> `CreateFileW` na `GUID_DEVINTERFACE_MMK_VHID` vraća `ERROR_ACCESS_DENIED`.
> Postavi eksplicitni SDDL (`WdfDeviceInitAssignSDDLString`) koji dopušta
> interaktivnom korisniku `IOCTL_MMK_VHID_SUBMIT`, ili app pokreni povišeno.

Instalacija root uređaja:

```powershell
pnputil /add-driver .\mmk-vhid.inf /install
devgen /add /bus ROOT /hardwareid "ROOT\MMKVHID" /instance "0000"
# ili, ako imaš devcon:
# devcon install mmk-vhid.inf ROOT\MMKVHID
pnputil /restart-device "ROOT\MMKVHID\0000"
```

Provjera:

```powershell
Get-PnpDevice -Class HIDClass | Where-Object InstanceId -match 'MMKVHID'
Get-PnpDeviceProperty -InstanceId 'ROOT\MMKVHID\0000' -KeyName DEVPKEY_Device_Stack
Get-PnpDeviceProperty -InstanceId 'ROOT\MMKVHID\0000' -KeyName DEVPKEY_Device_HardwareIds
```

U stogu se `vhf` mora pojaviti **ispod** `MMKVhid`. Ako ga nema, Windows neće
učitati virtualni HID uređaj (Device Manager: Code 10/28).

### 14.8 Sigurnosna pravila specifična za VHF

- **Nikad mouse kolekcija u virtualnom padu.** Fizički miš već pomiče kursor;
  druga relativna mouse kolekcija udvostručila bi brzinu i borbu za kursor.
  Ako se pokaže da Windows ne pošalje input mode 3 bez mouse kolekcije, dodaj
  je, ali iz nje **ne reportaj motion**.
- **Ne koristi Appleov VID/PID** `0x05AC/0x0269`. hidapi aplikacije (i naša)
  enumeriraju po VID/PID i mogle bi otvoriti virtualni pad umjesto pravog miša.
  Placeholder `0x1209/0x0001` je iz pid.codes open-source rangea; za distribuciju
  uzmi svoj par.
- **Ne rotiraj i ne alociraj po frameu.** Encoder piše u stack buffer od 30 B;
  nema `Vec`, nema `Box`, nema čekanja.
- **Ne submitaj nakon deletea.** Atomic handle je jedina zaštita; `stop` prvo
  swapira handle, pa zove `VhfDelete(TRUE)`.
- **Ne ostavljaj kontakt dolje.** Ako app umre ili 500 ms nema frameova, driver
  mora poslati prazan frame (ili ugasiti input mode). Inače Windows misli da
  prst još drži i touchpad "zamrzne".
- **Ne logaj po frameu bez budžeta.** 100 Hz x `DbgPrint` je sam po sebi lag.
  `SUBMIT_LOG_BUDGET` je 8.

### 14.9 Acceptance kriteriji za Fazu 3

- [ ] `ROOT\MMKVHID\0000` se digne bez Code 10/43; stack sadrži `vhf` ispod
      `MMKVhid`.
- [ ] Settings → Bluetooth & devices → Touchpad prikaže virtualni uređaj.
- [ ] Descriptor se parsira: 4 report ID-a; input `0x01` = 29 B payloada;
      feature `0x02` = 1 B, `0x03` = 256 B, `0x04` = 1 B.
- [ ] Jedan prst glatko pomiče kursor bez dvostrukog kursora; dva prsta daju
      scroll s inercijom; pinch zoom radi u Photos/Maps; 3-prstni swipe radi.
- [ ] Input mode počinje na 0; nakon što host pošalje 3, frameovi izlaze;
      nakon 0 frameovi prestaju i kontakti su počišćeni.
- [ ] Sleep/wake i replug fizičkog miša: virtualni pad se oporavi, nema
      stuck kontakta.
- [ ] App exit/crash s prstom na površini: nema stuck kontakta (prazan frame
      ili timeout).
- [ ] 1 h soak s pet prstiju: nema WDF errora, nema bugchecka;
      `!wdfkd.wdflogdump MMKVhid` čist.
- [ ] `verifier /standard /driver mmk-vhid.sys`: 30 min gesta, nema `0xC4`.
- [ ] Deinstalacija: `pnputil /remove-device ROOT\MMKVHID\0000`,
      `pnputil /delete-driver`, `vhf` nestane iz `LowerFilters`, fizički miš
      nepromijenjen.

### 14.10 Što još nije u kodu

- `IOCTL_MMK_VHID_SUBMIT` (`ctl_code(0x22, 0x802, METHOD_BUFFERED, FILE_WRITE_DATA)`
  = `0x00226008`), arm u `EvtIoDeviceControl` i `vhf::submit_frame` su sada dio
  drivera kao scaffold; cijeli zaseban `mmk-vhid.sys` function driver paket,
  watchdog i finalni INF i dalje nisu.
- Nema `mmk-vhid.inx` ni build targeta; gornji INF je skica, ne kopiraj ga
  bez provjere `InfVerif`.
- Nema watchdog timera za stuck kontakte; to je obavezan dio prije distribucije.
- `AtomicPtr` treba zamijeniti `EX_RUNDOWN_REF` prije nego submit i delete mogu
  stvarno trčati paralelno (vidi 14.3).
- VHF dio nije kompajliran; prvi build na eWDK je pravi test (vidi §11.2).

---

## 15. Plan rada i procjena

| Tjedan | Posao | Izlaz |
| --- | --- | --- |
| 1 | VM, eWDK, toolchain, "hello world" KMDF filter se učitava | dokaz da build/instalacija rade |
| 2 | Faza 0 probe + log analiza | odgovor "što vidimo na točki C" |
| 3 | Odluka: C ili fallback A; HIDP parser + ring | parser s unit testovima |
| 4 | User-mode čitanje ringa + gesture engine | scroll/tap rade iz kernel puta |
| 5–6 | VHF precision touchpad (opcionalno) | native pinch/3-prsta |
| 7 | Driver Verifier, 72 h soak, recovery testovi | stabilnost |
| 8 | Attestation signing + installer | distributabilan paket |

**Acceptance kriteriji za Fazu 1:**

- 72 h neprekidnog rada, Driver Verifier uključen, 0 bugchecka.
- Native skrolanje i dalje radi (usporedba A/B).
- Live baterija u app-u bez "flipa" filtera.
- Touch reporti ≤ 20 ms od događaja do user modea.
- Isključivanje drivera ne ruši miš ni app.

---

## 16. Reference

- HID arhitektura: <https://learn.microsoft.com/en-us/windows-hardware/drivers/hid/hid-architecture>
- HID kolekcije i `IOCTL_HID_*`: <https://learn.microsoft.com/en-us/windows-hardware/drivers/hid/hid-collections>
- Filter driver ordering / `AddFilter`: <https://learn.microsoft.com/en-us/windows-hardware/drivers/develop/device-filter-driver-ordering>
- Instalacija filter drivera: <https://learn.microsoft.com/en-us/windows-hardware/drivers/install/installing-a-filter-driver>
- Extension INF: <https://learn.microsoft.com/en-us/windows-hardware/drivers/install/using-an-extension-inf-file>
- KMDF HID filter sample (Firefly): <https://github.com/microsoft/Windows-driver-samples/tree/main/hid/firefly>
- VHF: <https://learn.microsoft.com/en-us/windows-hardware/drivers/hid/virtual-hid-framework--vhf->
- Precision touchpad kolekcija: <https://learn.microsoft.com/en-us/windows-hardware/design/component-guidelines/touchpad-windows-precision-touchpad-collection>
- Precision touchpad configuration collection: <https://learn.microsoft.com/en-us/windows-hardware/design/component-guidelines/touchpad-configuration-collection>
- Sample report descriptors: <https://learn.microsoft.com/en-us/windows-hardware/design/component-guidelines/touchpad-sample-report-descriptors>
- `windows-drivers-rs`: <https://github.com/microsoft/windows-drivers-rs>
- Rust driver samples: <https://github.com/microsoft/Windows-rust-driver-samples>
- Linux referenca za decode: `drivers/hid/hid-magicmouse.c`
- Test signing: <https://learn.microsoft.com/en-us/windows-hardware/drivers/install/test-signing>
- Attestation signing: <https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation>
