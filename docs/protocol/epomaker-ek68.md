# Epomaker EK68 (non-VIA) — proprietary HID protocol

> **Driver:** implemented in `src/hal/epomaker.rs` (registered via
> `driver = "epomaker"` in `devices.toml`). Lighting is fully wired; keymap is
> shadow-state + select/write commit pending hardware confirmation of the
> write offset; macros are shadow-only (format not yet decoded).
>
> **Status: IN PROGRESS — reverse engineering.**
> This is a living document. Fields marked `TODO` are unconfirmed and must be
> filled from Wireshark/USBPcap captures (wired USB) of the official Epomaker
> Windows driver before the clackd `epomaker` HAL driver implements them.
> **Do not implement anything in `src/hal/epomaker.rs` against a `TODO` here.**

## Scope

- Connection: **wired USB only** (official driver edits over cable; config
  persists on the on-board EEPROM).
- In scope: **RGB lighting/effects, key remapping, macros**.
- Out of scope: battery level (driver exposes none).

## Prior art

[`strodgers/epomaker-controller`](https://github.com/strodgers/epomaker-controller)
reverse-engineered sibling boards (RT100/EP64/TH80). Working hypotheses to
**confirm** for the EK68, not assume:

- ~64-byte vendor HID reports.
- A leading command/header byte.
- A per-packet checksum (often a simple byte sum — confirm algorithm).
- Per-board VID/PID (RT100 = `0x3151:0x4010`, vendor "ROYUAN").

---

## 1. Device identification  (Phase A1)

Fill from USB Device Tree Viewer / Device Manager on Windows, cross-check with
`lsusb -v` / `udevadm info` on Linux.

| Field | Value |
|---|---|
| USB Vendor ID (VID) | **`0x05AC`** (reports as "Apple" — spoofed; real maker `hfd.cn`) |
| USB Product ID (PID) | **`0x024F`** |
| Manufacturer / Product strings | `"hfd.cn"` / `"EK68"` |
| bcdDevice | `0x0104` |
| Number of HID interfaces | **2** (composite, `usbccgp`) |
| **Vendor/config interface** number | **Interface 0** |
| Config report type | **FEATURE report** (`B1 02`, Report Count 64 / Size 8) |
| Config report length | **64 bytes** |
| Report ID | **`0x00`** (none declared for the feature report) |
| Transport | **SET_REPORT(Feature) / GET_REPORT(Feature) control transfers** |
| OUT endpoint | **none** — both endpoints (`0x81`, `0x82`) are interrupt **IN** |

**Interface map**
- **Interface 0** — boot keyboard (`bInterfaceProtocol=01`), IN ep `0x81`. Report
  descriptor also declares the **64-byte vendor Feature report** (under the
  Generic-Desktop/Keyboard application collection). **This is the config channel.**
- **Interface 1** — consumer control (RID 3), system control (RID 2), NKRO keyboard
  (RID 1, 120 keys), mouse (RID 6), and a vendor-defined input collection (RID 5,
  usage page `0xFFFF`). IN ep `0x82`. Used for *reporting* events, not config.

**Linux `/dev/hidrawN` selection:** bind the hidraw node for **Interface 0** (the one
exposing the 64-byte Feature report). There is no unique vendor *usage page* to probe
(the feature report sits inside the keyboard collection), so selection is by
**VID/PID `05ac:024f` + interface 0** (or by "has a 64-byte feature report"). The
Interface-0 hidraw node is identifiable via its udev `HID_NAME`/interface-number
attribute.

> **Transport implication for `src/hal/epomaker.rs`:** because the channel is a
> *Feature* report with **no OUT endpoint**, the driver must use the
> **`HIDIOCSFEATURE(len)` / `HIDIOCGFEATURE(len)`** ioctls (via `nix`) on the
> Interface-0 hidraw fd — NOT the `read()`/`write()` + `AsyncFd` model that `via.rs`
> uses for its interrupt pipe. These ioctls are synchronous control transfers; wrap
> them in `tokio::task::spawn_blocking` (or a dedicated blocking thread) to keep the
> Tokio runtime unblocked. Reuse `via.rs`'s ioctl-wrapping style
> (`nix::ioctl_*` macros) as the pattern, not its transfer mechanism.

---

## 2. Frame format  (Phase A4)

Confirmed: each transfer is a **64-byte Feature report**, report ID `0x00`.
On Linux the `HIDIOCSFEATURE` buffer is `[0x00, <64 payload bytes...>]` (byte 0 is the
report ID). The Windows USBPcap capture exposes the 64 payload bytes in the
**`usb.data_fragment`** field (not `usb.capdata`/`usbhid.data`, which are empty here).

Observations so far:
- **No checksum.** Across red/green/blue only the RGB bytes change; all other bytes
  (including the `AA 55` footer) stay constant, so nothing is summing the payload.
- **`AA 55` footer magic** appears at offset 14–15 of the `0x01` command.
- The `0x04 xx` frames are high-frequency **status-poll heartbeats** from the app
  (e.g. `04 02 …`, `04 F5 … 09 …`), not configuration writes — ignore them.

```
Offset  Size  Field        Notes
------  ----  -----------  ---------------------------------
0       1     command      command id — see §3
1..     ...   payload      command-specific
14..15  2     footer       AA 55 (seen on the 0x01 command; confirm per-command)
```

**Checksum algorithm:** `TODO` (e.g. `sum(bytes[a..b]) & 0xFF` at offset `TODO`).
Confirmed by diffing two frames differing in one payload byte.

**Multi-packet rules:** `TODO` (how payloads > one report are split; does each
continuation carry its own header/checksum?).

---

## 3. Command table  (Phase A3 → A4)

One row per discovered command. Capture **one variable per capture** and diff.

| Command (hex) | Meaning | Confirmed? | Capture file | Notes |
|---|---|---|---|---|
| `0x01` | RGB: set global **static** color (all keys) | ✅ | red/green/blue, bright | R@1 G@2 B@3; **byte9=brightness**; byte10=`0b` const; footer `AA 55`@14 |
| `0x01` byte 9 | RGB: **brightness** | ✅ | bright | confirmed: byte 9 ranges `0x01`(min)–`0x10`(max), 16 levels |
| byte 0 | RGB: **effect/mode id** | ✅ | fx | **byte 0 IS the mode selector** — see §3.1 (20-mode table) |
| byte 8 | RGB: rainbow/multicolor flag | ◐ | fx | `=01` on Colourful/Spectrum; else `00` |
| `TODO` | RGB: speed | ☐ | | not yet isolated (byte 11? per-mode) |
| `TODO` | RGB: direction | ☐ | | not yet isolated |
| `TODO` | RGB: Random Color toggle | ☐ | | likely a single flag byte; capture on/off diff |

### 3.1 Lighting frame & effect-mode table (confirmed)

Lighting frame (64-byte payload):
```
byte 0    effect/mode ID (table below)
byte 1-3  R, G, B (single-color modes)
byte 8    rainbow/multicolor flag (01 on Colourful/Spectrum)
byte 9    brightness 0x01..0x10
byte 10   0x0B when lit, 0x01 when off
byte 14-15 AA 55 footer
```

| Mode | ID | Mode | ID | Mode | ID | Mode | ID |
|---|---|---|---|---|---|---|---|
| LED off | `0x00` | Falling | `0x05` | Scrolling | `0x0A` | Ripples | `0x0F` |
| Static | `0x01` | Colourful | `0x06` | Rolling | `0x0B` | Flowing | `0x10` |
| SingleOn | `0x02` | Breath | `0x07` | Rotating | `0x0C` | Pulsating | `0x11` |
| Single Off | `0x03` | Spectrum | `0x08` | Explor | `0x0D` | Tilt | `0x12` |
| Glittering | `0x04`* | Outward | `0x09` | Launch | `0x0E` | Shuttle | `0x13` |

*Glittering inferred (filter `data_fragment[0]!=04` dropped it; perfect sequential fit).
| `TODO` | Per-key RGB | ☐ | | key-index encoding |
| `0x00` frame | Key remap / keymap write | ✅ | remap, remap2, pos | see §3.2; 4-byte slots, VIA keycodes |
| persist | **automatic** — written live to EEPROM on every change | ✅ | | **no save command**; app writes immediately (EEPROM-wear risk → clackd must coalesce) |
| `TODO` | Macro: define/slot | ☐ | | 2- vs 3-key macro diff |

### Heartbeat / poll frames (ignore for config)
`04 02 …`, `04 F5 … (byte8=09)`, `04 F0 …`, `04 18 …`, `04 13 … (byte8=01)` — sent
continuously (~2/sec) while the app is open. Not configuration writes.

### 3.2 Keymap / remap frame (confirmed structure)

Frame type: **byte 0 = `0x00`**. The 64-byte payload is a sparse keymap-edit page.
Each remapped key is a **4-byte slot** at **offset = `slot_index * 4`**:

```
slot offset+0 : 0x02         "set this key" marker
slot offset+1 : 0x00
slot offset+2 : keycode low   } 16-bit little-endian VIA/QMK keycode
slot offset+3 : keycode high  }   (e.g. KC_A = 0x0004 -> 04 00)
```
Unwritten slots are zero (left unchanged). A separate all-zero frame ending in
`AA 55` (bytes 62–63) terminates the transaction.

Confirmed keycodes are **standard VIA/QMK**: A=`0x0004`, B=`0x0005`, C=`0x0006`.

**Observed slot indices** (key → slot): Esc→1, `1`→4, Tab→5, Q→6, Space→14.
Slot order is matrix-scan order, **not** visual order (Esc=1, `1`=4 are not adjacent).

**Remap UI choreography (confirmed):** a remap is two steps / two frames:
1. **Select** (click key on layout): `02 00 <source key default scancode>` at **offset 4**
   (a fixed select/scratch register — *not* the key's slot). Informational; ignore for writes.
2. **Write** (pick target keycode): `02 00 <new keycode>` at **offset = slot × 4**.
   This is the real keymap write. Only fires when the target is actually chosen.

A bulk sweep must do BOTH steps per key (click key → pick target) or only selects are sent.

**offset = slot × 4 — PROVEN.** A shuffled-order capture (clicked `5`,`Esc`,`=`,`1`)
placed each key at its *own* fixed offset (32,4,60,16 = slots 8,1,15,4) regardless of
click order — so the position is the physical slot, not edit order. The write buffer is
the **cumulative keymap**: each edited key occupies its 4-byte slot; the app re-sends the
sparse set of this-session edits each time.

**Confirmed slot map (key → slot):**
- `Esc`=1
- number row `1 2 3 4 5 6 7 8 9 0 - =` = slots `4 5 6 7 8 9 10 11 12 13 14 15`
  (anchored by `1`=4, `5`=8, `=`=15 from clean post-reset captures).
- Slots 0, 2, 3 belong to other (non-number-row) keys — scan order is board-specific.

**Row 2 sweep (reset) — offsets COLLIDE with row 1:**
Tab..P → offset/4 = 5..15; then `[ ] \` **wrap back to offset 0** (a new page).
But `E`→offset 8 and (row1) `5`→offset 8 are the **same offset** for different keys.

**⇒ Revised model (important):** the write **offset is NOT a global absolute slot** — it is
a position within the **currently-selected row/page**. The keyboard identifies *which* key
from the **select frame** (`02 00 <source default scancode>`), and the write frame just
delivers the new keycode. The buffer **wraps/pages** after filling. Net effect:

- A remap is **select(source scancode) → write(new keycode)**.
- **Source keys are addressed by their standard default scancode** (Esc=0x29, 1=0x1e,
  Tab=0x2b, Q=0x14, …) — which is the factory US-HID layout and needs **no capture**.
- A hand-built physical slot map is therefore likely **unnecessary** for the driver; the
  exact write-offset/paging only needs to be replicated well enough to satisfy the
  firmware, which is best nailed down by **testing against the real device** during
  driver development.

**Open (resolve during implementation, on hardware):**
- Whether the firmware requires the write at the app's exact offset, or accepts a fixed
  offset given a preceding select. Test: send select(scancode)+write(kc) and observe.
- Exact page/wrap signaling for the buffer.
- Marker `0x02` / byte 1 `0x00`: possibly a layer indicator (only layer 0 tested).

### 3.3 Passive-sniffing ceiling reached (remap addressing)

A fully-unfiltered single remap (`E → F9`, post-reset) produced **only** the write
`02 00 42 @ offset 32` plus generic heartbeats (`04 18`, `04 11…09`, `04 02`, `04 f0`).
The byte-identical write is what `5 → F9` produces too. **There is no key/row/scancode
disambiguator anywhere in the SET_REPORT stream**, so two different physical keys at the
same column offset cannot be told apart from captures alone. The keyboard must rely on
selection state set through another channel (GET_REPORT polling and/or internal state from
the on-screen click).

**Consequence:** a passive remap "slot sweep" cannot yield unique per-key addresses, so it
is abandoned. The remap addressing is to be finalized by **active testing against the real
device** during driver development — e.g. send `select(02 00 <scancode> @20)` then
`write(02 00 <kc> @offset)` and observe which physical key changes; iterate to confirm
whether the select frame, the offset, or both are load-bearing. Lighting is fully decoded
and unaffected.

**Reset:** the app has a restore-to-default that clears all remaps (used between sweeps).

### 3.x Field detail templates (fill per command as confirmed)

#### RGB effect / mode
- mode byte offset: `TODO`, value→effect table: `TODO`
- brightness offset & range: `TODO`
- speed offset & range: `TODO`
- color offsets (R,G,B): `TODO`

#### Per-key RGB
- key index encoding (matrix vs LED index): `TODO`
- key→index map: `TODO`

#### Key remap
- layer support? `TODO`
- position encoding (row/col vs flat index): `TODO`
- keycode encoding (HID usage vs Epomaker code): `TODO`
- keycode → wire value table: `TODO`

#### Macro
- slot count / addressing: `TODO`
- event encoding (down/up, delay): `TODO`
- max length: `TODO`

---

## 3.4 Hardware bring-up findings (wired EK68, Arch Linux, via clackd)

End-to-end run against the physical keyboard (USB cable; config interface = the
hidraw node with the 64-byte Feature report):

- ✅ Daemon attaches the **epomaker** driver to interface 0; interface 1 correctly
  rejected (no Feature report). `clackctl list` shows the device.
- ✅ `clackctl set-lighting` → D-Bus → engine → `HIDIOCSFEATURE` **all succeed**.
- ✅ The keyboard **accepts and stores** our 64-byte lighting frames:
  `HIDIOCGFEATURE` reads back the exact bytes we wrote (e.g. white → `01 ff ff ff …`).
- ❌ **No visible rendering.** Static color, brightness, and effect frames are stored
  but the LEDs don't change (a faint brightness blink was only seen while spamming the
  guessed `04 xx` poll frames).
- **Not a checksum:** the brightness capture changed byte 9 alone with no co-varying
  byte, so there is no payload checksum gating the render.
- **Readback anomaly:** `HIDIOCGFEATURE` byte 3 (B) reads back `0xff` regardless of the
  blue value written — investigate alongside the handshake.

**Working hypothesis / next step:** the Windows app issues a one-time **"enter
PC/software-lighting mode" handshake at connect** that we never captured (all prior
captures were config *changes*, never the first ~1–2 s after the app connects). Needed
capture (Windows, wired): start the tshark dump, *then* open the Epomaker app (or replug
with it open), and record the first SET_REPORTs — diff against the known heartbeats to
find the activation frame. Also capture two **custom** colors with different byte sums
(e.g. `255,128,0` vs `0,128,255`) to double-check no checksum. Once the activation frame
is known, the driver sends it on attach (and likely a periodic `04 xx` keepalive to hold
software mode).

---

## 4. Matrix / topology  (for clackd `get_matrix_dimensions` / `get_layer_count`)

| Field | Value |
|---|---|
| Rows | `TODO` |
| Cols | `TODO` |
| Layer count | `TODO` |
| Physical→matrix key map | `TODO` |

---

## 5. Capture log

Record each capture here so the spec is reproducible.

Extraction recipe (Windows PowerShell + tshark, dedup distinct frames):
```
& "C:\Program Files\Wireshark\tshark.exe" -r CAPTURE.pcapng `
  -Y "usbhid.setup.bRequest == 0x09" -T fields -e usb.data_fragment |
  Group-Object -NoElement | Sort-Object Count |
  ForEach-Object { "{0,5}  {1}" -f $_.Count, $_.Name }
```
Low-count distinct lines = real commands; high-count lines = poll heartbeats.

| # | Action performed (single variable) | Distinct frame (payload) | Decoded delta |
|---|---|---|---|
| 1 | All keys solid **red** | `01 ff 00 00 …10 0b…aa 55` | R=byte1 |
| 2 | All keys solid **green** | `01 00 ff 00 …10 0b…aa 55` | G=byte2 |
| 3 | All keys solid **blue** | `01 00 00 ff …10 0b…aa 55` | B=byte3 |
| 4 | Blue, brightness sweep | `01 00 00 ff …{0f,10,01} 0b…` | byte9=brightness (0x01–0x10) |
| 5 | Switch to animated effect | `05 ff 00 …00 01 10 0b…aa 55` | cmd 0x05 = effect (partial) |
| 6 | Step through all 20 modes (Random Color off, fixed color) | `02..13` + `00`, byte0 incrementing | byte0 = effect/mode id; full table §3.1 |
| 7 | CapsLock → A/B/C | `…02 00 0{4,5,6} 00` @off 30 | keycode = VIA (A=04,B=05,C=06) |
| 8 | Esc/1/Tab/Q/Space → A | `02 00 04 00` @off 4/16/20/24/56 | slot offset = key_index×4; §3.2 |
