# Magic Mouse Control

Besplatna, open-source alternativa aplikaciji **Magic Utilities** za Apple Magic Mouse na Windowsu 10 i 11. Aplikacija se sastoji od pozadinskog HID servisa i grafičkog sučelja u sustavskoj traci, a namjerno **ne instalira kernel driver**, ne traži test signing i ne zahtijeva isključivanje Secure Boota.

> Ovo je neovisni projekt. Nije povezan s Appleom, Magic Utilitiesom ni Boot Campom.

## Značajke

- Prepoznavanje Magic Mousea 1, Magic Mousea 2 i USB-C modela (Apple VID `004C`/`05AC`, PID `0269`, `030D`, `0323`).
- Osvježavanje statusa, modela, načina povezivanja i razine baterije.
- **Sigurni način rada prema zadanim postavkama**: prati vezu i bateriju, bez čitanja sirovog HID unosa i bez diranja Bluetooth pokazivača.
- **Read now** na kartici Features: eksplicitno, uz jednu UAC potvrdu, nakratko čita bateriju preko Bluetootha i automatski vraća Appleov filter za skrolanje. Zadnje očitanje se pamti između pokretanja i prikazuje oznakom `LAST`.
- Opcionalne geste (eksperimentalno, samo preko USB-a): glatko vertikalno i horizontalno skrolanje s inercijom preko `SendInput`, Natural Scrolling, podešavanje brzine, srednji klik i dvoprstni swipe.
- Dvoprstni swipe lijevo/desno šalje Alt+Left / Alt+Right.
- Windows toast obavijest kada baterija padne ispod 15 %.
- Automatsko ponovno povezivanje nakon spavanja ili Bluetooth prekida.
- Živa ilustracija dodirne površine, visok DPI i skaliranje sučelja.
- Sustavska traka: dinamična ikona, tooltip s baterijom, `Open Settings`, `Check Battery`, `Start with Windows` i `Exit`.
- Pokretanje sa sustavom preko `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (bez administratora).
- Nema telemetrije ni mrežnog prometa.

## Sigurni način rada (zadano)

Aplikacija je namjerno napravljena tako da **ne može zamrznuti računalo**:

- Prema zadanim postavkama ne čita sirove HID izvještaje i ne ubacuje nikakve ulazne događaje. Samo prati je li miš povezan i, kad ga miš sam pošalje, razinu baterije.
- Bluetooth HID kolekcija ostaje isključivo Windowsima. Aplikacija je ne otvara za čitanje dok je u sigurnom načinu.
- Baterija preko Bluetootha čita se samo neblokirajuće; pozadinska dretva nikad ne čeka blokirajući HID zahtjev.
- Appleov filter za skrolanje dira se **samo** nakon što korisnik pritisne **Read now** i potvrdi upozorenje. Tada se filter na nekoliko sekundi ukloni, baterija pročita i filter vrati u `finally` bloku. Aplikacija zatim provjeri registar i stanje servisa; ako povratak nije potvrđen, prikaže gumb **Restore scroll mode**.
- Zatvaranje prozora, izlaz iz trake i gašenje aplikacije ne čekaju HID dretvu, pa se aplikacija ne može "zalediti" pri izlazu.
- Druga kopija aplikacije automatski se gasi (named mutex), pa nema dvostrukog čitanja uređaja.
- Aplikacija ne instalira kernel driver, ne traži test signing i ne dira Secure Boot.

Ako želite isprobati geste, uključite **Experimental touch input** na kartici Features i spojite miš USB kabelom. Tek tada aplikacija pokušava čitati dodirnu površinu. Preko Bluetootha se to nikad ne radi.

## Važno: Bluetooth i USB

Windows **ne dopušta običnim korisničkim aplikacijama čitanje sirovih HID izvještaja Bluetooth miševa**. Takve kolekcije pripadaju sustavskom input stacku: datoteka se može otvoriti, ali `ReadFile` vraća *Access is denied*. Zato:

- **Preko USB kabela** (punjenje) uz uključen **Experimental touch input** rade geste: skrolanje, inercija, srednji klik i swipe.
- **Preko Bluetootha** aplikacija ostaje u sigurnom načinu: pokazivač i native skrolanje rade normalno, a dodirna površina se ne čita.
- Appleov filter driver (`applewirelessmouse`) drži native skrolanje, ali sakriva vendor kolekciju s baterijom. Zato je svježa baterija preko Bluetootha dostupna samo kroz **Read now**: filter se nakratko ukloni, pročita se HID izvještaj `0x90`, filter se vrati i provjeri. Skrolanje i pokazivač u tom trenutku pauziraju otprilike 3–10 sekundi.
- Ako je na računalu instaliran Appleov Boot Camp HID filter driver, native skrolanje preko Bluetootha može raditi bez ove aplikacije. Magic Mouse Control ga dira samo tijekom gore opisanog, eksplicitno potvrđenog očitanja.
- Zaobilazak ograničenja za čitanje dodirne površine zahtijeva kernel driver. Takav driver **nije dio aplikacije** i nikad se ne instalira automatski; projektna dokumentacija i eksperimentalni Rust scaffold (samo za testni VM) nalaze se u [`docs/kernel-driver.md`](docs/kernel-driver.md) i [`driver/`](driver/README.md).

Ova granica dolazi od Windowsa, ne od miša. Ista aplikacija zato nema smisla pokušavati "popraviti" postavkom.

## Preduvjeti

- Windows 10 ili Windows 11 (x64).
- [Rust](https://rustup.rs/) sa stabilnim toolchainom i MSVC linkerom (Visual Studio Build Tools).
- Za normalno korištenje nisu potrebna administratorska prava.

## Izgradnja

```powershell
cd magic-mouse-control
cargo test
cargo build --release
```

Gotova aplikacija je:

```text
target\release\magic-mouse-control.exe
```

Može se kopirati bilo gdje i pokrenuti dvoklikom. Nema instalera ni pozadinskih servisa.

## Korištenje

1. Uparite Magic Mouse u Windows postavkama (Bluetooth & devices > Add device).
2. Pokrenite `magic-mouse-control.exe`.
3. Za geste spojite miš USB kabelom i uključite **Experimental touch input** na kartici Features. Preko Bluetootha očekujte sigurni način rada i normalan pokazivač; za svježu bateriju pritisnite **Read now** i potvrdite upozorenje.
4. Podesite skrolanje, Natural Scrolling, inerciju, srednji klik i swipe na kartici **Features**.
5. Zatvaranjem prozora aplikacija se skriva u sustavsku traku (ako je tray dostupan). Desni klik na ikonu nudi izlaz i provjeru baterije.

Pokretanje minimizirano, bez prikaza prozora:

```powershell
.\magic-mouse-control.exe --minimized
```

## Datoteke i postavke

| Put | Sadržaj |
| --- | --- |
| `%APPDATA%\MagicMouseControl\config.json` | Sve postavke aplikacije |
| `%APPDATA%\MagicMouseControl\magic-mouse.log` | Dijagnostički log (rotira se na 1,5 MB) |
| `%APPDATA%\MagicMouseControl\battery-cache.json` | Zadnje potvrđeno očitanje baterije |
| `%APPDATA%\MagicMouseControl\mode-flip-pending` | Oznaka da povratak filtera nije potvrđen; nestaje nakon uspješnog **Restore scroll mode** |
| `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` | Opcionalni autostart unos |

Autostart se može uključiti u sučelju ili izborniku trake. Registar je izvor istine pri pokretanju, a unos se ažurira ako se izvršna datoteka premjesti.

## Arhitektura

- `src/main.rs` — ulazna točka, `--minimized`, `--battery-once` pomoćni način, eframe i poruke o pogreškama.
- `src/hid_daemon.rs` — pronalaženje HID sučelja, report descriptor parser, sigurno praćenje veze, neblokirajuće čitanje baterije, reconnect i opcionalna multi-touch inicijalizacija.
- `src/battery_flip.rs` — eksplicitno Bluetooth očitanje baterije: elevated PowerShell pomoćnik, privremeno uklanjanje/vraćanje `applewirelessmouse` filtera, provjera povratka, keš i sigurnosna oznaka.
- `src/mouse_simulator.rs` — pretvorba dodira u glatko skrolanje, inerciju, srednji klik i swipe.
- `src/gui.rs` — Features, FAQs i Help kartice, sustavska traka i obavijesti.
- `src/icons.rs`, `src/widgets.rs` — programski crtana ikona i vizualni stil bez vanjskih slika/fontova.
- `src/config.rs`, `src/autostart.rs`, `src/logging.rs`, `src/notifications.rs` — podrška.

HID rad radi na vlastitoj dretvi. UI dobiva samo male događaje preko kanala, pa uspavan miš ili HID timeout nikada ne blokira prozor. Pri izlazu se HID dretva namjerno ne čeka, nego je preuzima proces; tako izlaz ne može visjeti na Bluetooth čitanju.

## Poznata ograničenja

- Bluetooth sirovi HID je blokiran od Windowsa (vidi gore). Zato su geste preko Bluetootha isključene, a sigurni način rada je zadani.
- Ako je prozor minimiziran ili skriven u traci, aplikacija ne crta sučelje i troši gotovo ništa CPU-a.
- Native skrolanje se ne može potisnuti bez potpisanog filter drivera. Preko USB-a se glatko skrolanje dodaje na native tok, pa u nekim aplikacijama može djelovati udvostručeno. Isključite **Smooth scrolling** za usporedbu.
- Status punjenja je procjena (USB transport ili rast razine baterije). Apple ne izlaže pouzdan charging bit.
- Swipe šalje Alt+Left/Right i radi samo u aplikacijama koje podržavaju te prečace.
- Baterija preko Bluetootha nije trajno "live": Appleov filter sakriva vendor kolekciju. **Read now** daje svježe očitanje, ali traži UAC i nakratko pauzira skrolanje. Između očitanja prikazuje se zadnja potvrđena vrijednost s oznakom `LAST`.
- Ako **Restore scroll mode** ne uspije, skrolanje može ostati isključeno do ponovnog pokretanja Windowsa. Registar tada sadrži `LowerFilters=applewirelessmouse`, ali servis `applewirelessmouse` nije `RUNNING`. Ponovno pokretanje vraća filter i skrolanje.
- Druge Magic Mouse/trackpad utilities i Appleov Boot Camp driver mogu držati HID sučelje otvorenim.
- Ne postoji podrška za Magic Trackpad; projekt je namjerno fokusiran na Magic Mouse.

## Licenca

MIT. Vidi [LICENSE](LICENSE).
