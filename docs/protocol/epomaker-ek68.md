# Epomaker EK68 (non-VIA) — proprietary HID protocol

> **Driver:** implemented in `src/hal/epomaker.rs` (registered via
> `driver = "epomaker"` in `devices.toml`). Transport + connect handshake are
> confirmed on hardware.
>
> ## ⚡ BREAKTHROUGH (2026-06-04): the EK68 *is* a GMK67 — lighting protocol fully decoded
>
> The EK68 shares its exact USB identity (`05AC:024F`, interface 0, usage
> `0001:0006`) and firmware with the **Zuoya GMK67**, which was already
> reverse-engineered for OpenRGB by Aubry Flora ("aethernali.live"). See
> **§3.6** for the complete decoded protocol ported from that source.
>
> **Why our lighting never rendered:** the render is a *multi-frame transaction*,
> not a single frame. We were sending only the middle `01 R G B … AA 55` frame.
> The frames we had dismissed as "heartbeats" (`04 18`, `04 13 …`, `04 02`,
> `04 f0`) are in fact the **mandatory transaction framing** (`PACKET_HEADER`
> = `0x04`): turn-on-customization → start-effect-page → [mode frame] →
> end-communication → start-effect-command. Omitting that wrapper is exactly why
> even byte-exact replay failed (§3.5). The per-key framebuffer seen on `GET` is
> the `Direct`/`Custom` mode buffer (`index,R,G,B` × 128), written by a *separate*
> `0x20`/`0x23` command. Lighting is no longer blocked.
>
> **Status: lighting protocol CONFIRMED ON HARDWARE (2026-06-04).** The full
> UpdateMode transaction renders static colours, brightness, and effects from
> our own process via `tools/ek68_smoke.py`; mirrored in `src/hal/epomaker.rs`.
> Keymap is shadow-state + select/write commit pending hardware confirmation of
> the write offset; macros are shadow-only (the GMK67 source exposes the
> `WRITE_MACRO_DEFINITION_AREA_COMMAND = 0x15` / `READ … = 0x14` area — a lead).
> This is a living document. Fields still marked `TODO` are unconfirmed.

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

### 3.0 Enter-PC-lighting-mode handshake (confirmed on hardware)

> **SUPERSEDED by §3.6.** The "INIT_A/INIT_B" frames below were a partial,
> mis-decoded view of the real transaction framing (`PACKET_HEADER = 0x04`:
> `04 18` = customization-on, `04 13` = start-effect-page, `04 02` =
> end-communication, `04 f0` = start-effect). The real "enter PC mode" is
> `SetCustomization(ON)` *inside* each render transaction. Kept here for history.

The board **stores** host lighting frames (they read back via `GET_REPORT`) but
keeps **rendering its onboard effect** until the app's one-time connect sequence
runs. Captured host→device (`SET_REPORT`) in the first second after the Windows
app connects, the distinct non-color frames (in order) are:

| # | Frame (non-zero bytes) | Note |
|---|---|---|
| 1 | `00=04 01=18` | heartbeat |
| 2 | `00=04 01=13 08=01` | heartbeat |
| 3 | `09=01 0a=01 0e=AA 0f=55` | INIT_A — an onboard LED-off lighting frame |
| 4 | `00=04 01=02` | heartbeat |
| 5 | `00=04 01=f0` | heartbeat |
| 6 | `00=04 01=17 02=01 08=01` | heartbeat |
| 7 | `05=01 08=02 3e=AA 3f=55` | **INIT_B — enter PC-control (the activation)** |

`INIT_B` (footer at byte 62, unlike the lighting footer at byte 14) is the
load-bearing frame. The driver replays this whole sequence once per attach
before the first lighting write (`pc_mode_handshake()` in `src/hal/epomaker.rs`).

### 3.1 Lighting frame & effect-mode table (confirmed)

Lighting frame (64-byte payload):
```
byte 0    effect/mode ID (table below)
byte 1-3  R, G, B (mode-specific color; only if MODE_FLAG_HAS_MODE_SPECIFIC_COLOR)
byte 8    random-color flag (1 = MODE_COLORS_RANDOM)
byte 9    brightness 0x00..0x0F   (CORRECTED — app "15" = 0x0F; not 0x01..0x10)
byte 10   speed      0x00..0x0F   (was mislabeled "0x0B const")
byte 11   direction               (per-mode: LR / UD)
byte 14-15 AA 55  = EFFECT_PAGE_CHECK_CODE_L / _H
```
> **NOTE:** This frame is only the *middle* of the render transaction and does
> nothing on its own — it must be wrapped by the `0x04`-header framing. The
> authoritative, confirmed spec (constants, mode IDs, full send sequence, LED
> map) is **§3.6**, decoded from the GMK67 ≡ EK68 OpenRGB source.

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

### 3.5 Hardware deep-dive (Windows, direct HID via hidapi — 2026-06-04)

Drove the EK68 directly (Python `hid`, interface 0 = the 64-byte Feature collection),
Epomaker app closed unless noted. This refines §3.4 and **invalidates the simple
handshake hypothesis**:

- Writes reach the device. `GET_FEATURE` works, and the handshake visibly perturbs the
  LEDs, so `SET_FEATURE` is delivered, not silently dropped by Windows.
- Static `01 R G B ...` does not render from us. With no handshake the keyboard ignores
  host config entirely (even an LED-off frame leaves the onboard colour on). The handshake
  changes state (flicker/blank) but our `01` colour still does not render after it.
- Byte-exact replay fails. Replaying the app's EXACT recorded 502 SET frames from
  `ek68-connect.pcapng` only flickered (stayed on the prior colour). Replaying the FULL
  recorded choreography (2705 ops = 502 SET + 2203 GET, in capture order) drove the LEDs
  off, never the captured colour. Identical bytes + identical read pattern, writes
  landing, yet no reproduction.
- While lit, `GET_FEATURE` returns a paged per-key buffer (`<idx> R G B` groups, 16 per
  64-byte page; successive GETs walk index ranges 0x10-0x1f, 0x20-0x2f, 0x40-0x4f ...).
  The app issues ~4x more GET than SET (2203 vs 502), continuously reading these pages.

**Conclusion:** the live-render path is NOT the static `01` command (it can't be
reproduced even by byte-exact replay). It is therefore either stateful (a live value the
keyboard returns via GET that must be folded into subsequent writes, so a recorded replay
can never satisfy it), or the app renders via per-key framebuffer writes never captured
(all prior captures set static colours, not custom per-key).

**To resolve (next session):** (a) admin tshark (USBPcap) capture of the app setting
custom per-key colours, to decode the per-key framebuffer write; and/or (b) capture and
inspect the GET response payloads live for a changing nonce. Until then live RGB via
clackd is blocked. Transport, the connect handshake, and the keymap path are unaffected.

## 3.5 Windows direct-HID reproduction attempts (2026-06-04) — render NOT reproducible

Tested directly against the keyboard from Windows via Python `hidapi` on interface 0
(`MI_00`, the keyboard collection that carries the 64-byte Feature report). The connect
handshake was captured and is now implemented, **but lighting still does not render from
our process**, and the cause is deeper than a missing handshake:

- ✅ **Writes reach the device.** `GET`/`SET_FEATURE` both work; sending the handshake
  visibly changes the lighting (flicker / off), so Windows is **not** silently dropping
  our writes.
- ❌ **No frame sequence we send renders a colour.** All of the following were tried with
  the app closed, and **none** reproduced the colour (results: ignored / flicker / off):
  1. static `01 R G B` once; 2. static streamed continuously; 3. handshake + continuous
  colour + heartbeats ("app-mimic"); 4. handshake + SET+GET polling; 5. **verbatim replay
  of the app's exact 502 recorded SET frames**; 6. **full 2705-op SET+GET choreography
  replay** (every SET and GET in original order).
- ➡️ **Byte-exact replay fails.** Since replaying the app's own recorded frames does not
  reproduce its result, the live-render path is **stateful** (a value the keyboard hands
  the host live that must be folded into writes — un-replayable from a capture) **or**
  uses a per-key framebuffer write we never captured, **or** depends on a Windows
  HID-access nuance (handle mode/timing) the `hidapi` path doesn't reproduce.
- 🔎 **Per-key framebuffer exists.** While a colour is active, `GET_FEATURE` returns a
  **paged per-key buffer**: 4-byte entries `〈index〉〈R〉〈G〉〈B〉`, 16 per 64-byte page,
  consecutive GETs returning index ranges (`0x10–0x1f`, `0x20–0x2f`, `0x40–0x4f`, …).
  Showing red ⇒ entries like `10 5f 00 00 … 14 fd 00 00` (R only). **This is the likely
  real render path**; the connect captures contain **no** per-key writes (they only set
  *static* `01` colours), so the per-key **write** protocol is still uncaptured.
- ℹ️ The earlier "read-back of our writes" / "byte 3 = ff anomaly" is explained: `GET`
  returns this **per-key buffer**, not the last static frame we wrote — so it was never a
  faithful read-back of our static command.
- ℹ️ `usb.capdata` is empty in the captures, so GET **responses** weren't recorded by
  USBPcap; can't check for a nonce from file (the live GET looked like per-key state).

**Status:** ~~live RGB via clackd is **paused**~~ — **RESOLVED in §3.6.** The
"stateful / uncaptured per-key write" hypothesis was half right: rendering is a
**stateful multi-frame transaction**, and we were sending only the bare mode
frame. The "heartbeats" we ignored (`04 …`) were the transaction framing. The
per-key buffer seen on `GET` is the `Direct`/`Custom` framebuffer. Decoded in
full below from the GMK67 ≡ EK68 OpenRGB source — no further captures needed.

---

## 3.6 RESOLVED — EK68 ≡ Zuoya GMK67: authoritative lighting protocol

> ✅ **Verified on hardware 2026-06-04:** `tools/ek68_smoke.py` rendered
> red/green/blue, brightness, spectrum, and off exactly as labelled — the
> single-frame approach never did. Root cause was the missing transaction
> framing documented below.


### Identity (confirmed)
The EK68 runs the **Zuoya GMK67** firmware. Identical USB identity: VID `0x05AC`,
PID `0x024F`, **interface 0**, usage_page `0x0001`, usage `0x06`; 64-byte feature
reports, report ID `0x00`, no checksum, `AA 55` page-check footer. Vendor string
"ZUOYA".

**Source:** OpenRGB device-support request #4512, implemented in Aubry Flora's
fork `gitlab.com/aethernali.live/OpenRGB`, branch **`gmk67`** (HEAD
`310cf818537e958c54572939ca6c4c5e624404cf`, 2024-07-20),
`Controllers/GMK67KeyboardController/` (GPL-2.0). Mirrored as OpenRGB issue
[#4512](https://gitlab.com/CalcProgrammer1/OpenRGB/-/work_items/4512).

### Constants (`GMK67KeyboardController.h`)
```
REPORT_ID                   0x00
PACKET_DATA_LENGTH          64      (HID buffer = REPORT_ID + 64 payload = 65 bytes)
COLOR_BUF_SIZE              512     (= 8 packets × 64 = 128 LED slots × 4 bytes)
EFFECT_PAGE_LENGTH          16
LED_SPECIAL_EFFECT_PACKETS  0x01
PACKET_HEADER               0x04    ← the "heartbeat" byte WAS the framing header
EFFECT_PAGE_CHECK_CODE_L/H  0xAA / 0x55
brightness / speed range    0x00 .. 0x0F   (app "15" = 0x0F)
```

Command IDs (byte 1, when byte 0 = `PACKET_HEADER` 0x04):

| Hex | Name | Hex | Name |
|---|---|---|---|
| `0x02` | COMMUNICATION_END | `0x13` | WRITE_LED_SPECIAL_EFFECT_AREA |
| `0x05` | GET_BASIC_INFO | `0x14`/`0x15` | READ/WRITE_MACRO_DEFINITION_AREA |
| `0x10`/`0x11` | READ/WRITE_KEY_DEFINITION_AREA | `0x16`/`0x17` | READ/WRITE_GAME_MODE_AREA |
| `0x12` | READ_LED_EFFECT_DEFINITION_AREA | `0x18`/`0x19` | TURN_ON/OFF_CUSTOMIZATION |
| `0xF0` | LED_EFFECT_START | `0xF1/F2/F3` | LED_SYNC_INITIAL/START/STOP |
| `0xAB` | RANDOM_PACKET_START | | |

### Mode IDs (byte 0 of the mode frame) — supersedes the old §3.1 table
`0x01` Static · `0x02` Keystroke light-up · `0x03` Keystroke dim · `0x04` Sparkle ·
`0x05` Rain · `0x06` Random colors · `0x07` Breathing · `0x08` Spectrum cycle ·
`0x09` Ring gradient · `0x0A` Vertical gradient · `0x0B` Horizontal gradient /
Rainbow wave · `0x0C` Around edges · `0x0D` Keystroke horizontal lines · `0x0E`
Keystroke tilted lines · `0x0F` Keystroke ripples · `0x10` Sequence · `0x11` Wave
line · `0x12` Tilted lines · `0x13` Back-and-forth.
Special modes: `0x20` **DIRECT** (per-key, volatile), `0x23` **CUSTOM** (per-key,
saved), `0x80` **LIGHTS-OFF**.

`HAS_MODE_SPECIFIC_COLOR` modes use bytes 1–3 for RGB; `Random colors`, `Spectrum
cycle`, and `Off` carry no color. `HAS_DIRECTION` only on the gradient/sequence/
back-and-forth modes (byte 11).

### The render transactions
Helpers: **`Send(buf)`** = `SET_FEATURE` of `[0x00, buf(64)]` (65 bytes).
**`Read()`** = `GET_FEATURE` of 65 bytes — *mandatory*; the app interleaves a GET
after most SETs (this is the ~4× GET vs SET we measured).

**A. Effects / static color — `UpdateMode`** (the path "all keys solid red" uses):
```
1.  04 18                                 SetCustomization(ON)    ; Send ; Read
2.  04 13  [8]=01                          StartEffectPage         ; Send ; Read
3.  <mode> [1..3]=R,G,B  [8]=randomFlag     the MODE FRAME          ; Send ; Read
            [9]=brightness [10]=speed
            [11]=direction [14]=AA [15]=55
4.  04 02                                  EndCommunication        ; Send ; Read
5.  04 F0                                  StartEffectCommand      ; Send
```
> Our entire failure: we sent only step 3. It renders nothing without 1–2 before
> and 4–5 after.

**B. Per-key real-time — `SendDirect` (mode `0x20`, volatile):**
```
1.  04 20 [8]=08                           direct-mode header      ; Send
2.  SendLEDsBuffer  (8 packets, below)     per-key colors          ; Send ×8
3.  Read()
4.  04 02                                  EndCommunication        ; Send
```
> **⚠ Keepalive:** Direct mode is dropped by the board after ~2 s unless re-sent.
> The GMK67 driver runs a 2000 ms keepalive thread. The clackd engine actor must
> schedule a periodic Direct refresh while a Direct frame is active.

**C. Per-key saved — `SendCustom` (mode `0x23`):** `SetCustomization(ON)` →
`04 23 [8]=09` → `SendLEDsBuffer` → `Read` → `EndCommunication` →
`StartEffectCommand` → then a trailing `LIGHTS_OFF` (mode `0x80`, `[1]=FF [8]=01
[9]=0F [10]=0F`, `AA/55`) effect-page write + `EndCommunication` +
`StartEffectCommand` (verbatim from the source).

**`SendLEDsBuffer` — per-key data, 8 × 64-byte packets, NO `0x04` header:**
```
color_buf[512] = {0}
for slot l in 0 .. pos.len():
    if pos[l] != NA:
        color_buf[l*4 + 0] = l                 (slot index)
        color_buf[l*4 + 1..3] = R,G,B of colors[pos[l]]
send color_buf as 8 chunks of 64 bytes
```
This is exactly the framebuffer read back via `GET` (`10 R G B  11 R G B …`),
which finally explains the earlier "per-key buffer" / "byte 3 = ff" observations.

### LED matrix map (16 wide × 5 high; 66 LEDs, indices 0–65; `NA` = no LED)
```
row0: { 0, 5, 7,12,16,20,24,29,33,37,41,46,51,55,NA,NA}
row1: { 1,NA, 8,13,17,21,25,30,34,38,42,47,52,56,58,62}
row2: { 2,NA, 9,14,18,22,26,31,35,39,43,48,53,NA,59,63}
row3: { 3,NA,10,15,19,23,27,32,36,40,44,49,54,NA,60,64}
row4: { 4, 6,11,NA,NA,NA,28,NA,NA,NA,45,50,NA,57,61,65}
```
LED index → key (from `led_names`): 0 Esc, 1 Tab, 2 Caps, 3 LShift, 4 LCtrl,
5 `1`, 6 LWin, 7 `2`, 8 Q, 9 A, 10 Z, 11 LAlt, 12 `3`, 13 W, 14 S, 15 X, 16 `4`,
17 E, 18 D, 19 C, 20 `5`, 21 R, 22 F, 23 V, 24 `6`, 25 T, 26 G, 27 B, 28 Space,
29 `7`, 30 Y, 31 H, 32 N, 33 `8`, 34 U, 35 J, 36 M, 37 `9`, 38 I, 39 K, 40 `,`,
41 `0`, 42 O, 43 L, 44 `.`, 45 RAlt, 46 `-`, 47 P, 48 `;`, 49 `/`, 50 RFn,
51 `=`/Numpad+, 52 `[`, 53 `'`, 54 RShift, 55 Backspace, 56 `]`, 57 Left,
58 `\`, 59 Enter, 60 Up, 61 Down, 62 Del, 63 PgUp, 64 PgDn, 65 Right.
`positions_custom` (128 entries) maps each framebuffer slot `l` → LED index for
Direct/Custom mode.
> EK68 is a 68-key board vs the GMK67 66-LED map here — **verify the EK68's exact
> LED count and any extra slots on hardware**, but start from this map.

### Driver implications for `src/hal/epomaker.rs`
- Replace the single-frame write with the **full `UpdateMode` transaction**
  (effects/static) and **`SendDirect`** (per-key), each `Send` followed by `Read()`.
- Add a **~2 s Direct-mode keepalive** in the engine actor (only while Direct active).
- The guessed `pc_mode_handshake()` (INIT_A/INIT_B) is **superseded** — the real
  "enter PC mode" is `SetCustomization(ON)` *inside* the transaction; drop the guesses.
- Clamp brightness/speed to `0x00..0x0F`.
- Leads for the rest: key-definition area (`0x10`/`0x11`) for remap read/write and
  macro area (`0x14`/`0x15`) for macros.

---

## 4. Matrix / topology  (for clackd `get_matrix_dimensions` / `get_layer_count`)

| Field | Value |
|---|---|
| LED matrix | **16 wide × 5 high** (80 cells, 66 populated LEDs, indices 0–65) — see §3.6 map |
| Rows / Cols (keymap) | `TODO` — keymap matrix may differ from the LED matrix; confirm on hardware |
| Layer count | base + Fn (`EK68_LAYERS = 2`, per approved plan) — `TODO` confirm wire encoding |
| Physical→LED map | confirmed (GMK67 `matrix_map` / `led_names`, §3.6); EK68 68-key delta `TODO` |

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
