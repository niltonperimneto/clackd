# Epomaker EK68 HID protocol

Reverse-engineered specification for the Epomaker EK68 keyboard's proprietary
configuration protocol, as implemented by the clackd `gmk67` driver
([`src/hal/gmk67.rs`](../../src/hal/gmk67.rs), selected with
`driver = "gmk67"` in `devices.toml`). The driver is named for the shared
`hfd.cn` "GMK67" protocol rather than a single board, since the Epomaker EK68
and Zuoya GMK67 are firmware-identical rebadges that both speak it.

The EK68 is firmware-identical to the Zuoya GMK67: identical USB identity and
report format. The protocol below was decoded from the GMK67 OpenRGB driver and
from USB captures of the official Epomaker Windows application, and is confirmed
against the physical keyboard. See [Source attribution](#9-source-attribution).

## Status

| Area | State |
|---|---|
| Transport (feature reports, interface 0) | Confirmed |
| Lighting: static, brightness, effects | Confirmed on hardware |
| Per-key lighting (Direct / Custom) | Protocol confirmed; not yet exposed through clackd |
| Keymap remap, base layer | Confirmed on hardware |
| Keymap remap, Fn layer | Confirmed from capture (command `0x27`, same layout) |
| Key / LED index map | Confirmed on hardware |
| Macros | Not yet decoded |

## Scope

- Connection: wired USB only. Configuration persists in the on-board EEPROM.
- In scope: RGB lighting and effects, key remapping, macros.
- Out of scope: battery level (the device exposes none over this channel).

---

## 1. Device identification

| Field | Value |
|---|---|
| Vendor ID | `0x05AC` (reports as "Apple"; spoofed — real maker `Zuoya`) |
| Product ID | `0x024F` |
| Manufacturer / product strings | `Zuoya` / `EK68` |
| bcdDevice | `0x0104` |
| HID interfaces | 2 (composite) |
| Configuration interface | Interface 0 |
| Configuration report | 64-byte HID Feature report, report ID `0x00` |
| Transport | `SET_REPORT` / `GET_REPORT` (Feature) control transfers |
| OUT endpoint | None; both endpoints (`0x81`, `0x82`) are interrupt IN |

Interface map:

- **Interface 0** — boot keyboard. Its report descriptor also declares the
  64-byte vendor Feature report (inside the keyboard application collection).
  This is the configuration channel.
- **Interface 1** — consumer/system control, NKRO keyboard, mouse, and a vendor
  input collection. Event reporting only; not used for configuration.

Selection on Linux: bind the hidraw node for interface 0, identified by the
presence of a 64-byte Feature report. There is no distinct vendor usage page to
match (the feature report sits inside the keyboard collection), so selection is
by VID/PID and interface number.

---

## 2. Transport

All host-to-device configuration uses `SET_REPORT(Feature)`; the device is read
with `GET_REPORT(Feature)`. Because there is no OUT endpoint, the driver uses the
`HIDIOCSFEATURE` / `HIDIOCGFEATURE` ioctls on the interface-0 hidraw descriptor
(not `read`/`write`). These are synchronous control transfers and run on a
blocking task bounded by a per-call timeout.

A frame is 65 bytes on the wire: byte 0 is the report ID (`0x00`) followed by the
64-byte payload. In Windows USBPcap captures the 64-byte payload appears in the
`usb.data_fragment` field.

Two primitives are used throughout this document:

- **Send(payload)** — one `SET_REPORT(Feature)` of `[0x00, payload(64)]`.
- **Read()** — one `GET_REPORT(Feature)`. The application issues a Read after
  most Sends and the firmware expects this pacing; the response is not otherwise
  consumed.

There is no payload checksum. The `0xAA 0x55` bytes that appear at fixed
positions are a constant page-check code, not a sum.

---

## 3. Transaction model

Configuration is applied as a **transaction**, not a single frame. A transaction
wraps a data payload (a lighting mode frame, a key-definition buffer, or a
per-key color buffer) in framing commands. A single data frame sent on its own
has no effect.

A framing-command frame is:

- byte 0 = `0x04` (the packet header)
- byte 1 = command ID

Command IDs (byte 1):

| ID | Command |
|---|---|
| `0x02` | End communication |
| `0x05` | Get basic info |
| `0x10` / `0x11` | Read / write key-definition area (base layer) |
| `0x27` | Write key-definition area (Fn layer) |
| `0x12` | Read LED effect-definition area |
| `0x13` | Write LED special-effect area |
| `0x14` / `0x15` | Read / write macro-definition area |
| `0x16` / `0x17` | Read / write game-mode area |
| `0x18` / `0x19` | Customization on / off |
| `0xF0` | LED effect start |
| `0xF1` / `0xF2` / `0xF3` | LED sync initial / start / stop |

(Command `0x27` is not in the GMK67 OpenRGB source, which does not implement
remapping; it was decoded from the EK68 Fn-remap captures.)

Common constants:

| Name | Value |
|---|---|
| Report ID | `0x00` |
| Payload length | 64 bytes (65 on the wire including the report ID) |
| Packet header | `0x04` |
| Page-check code | `0xAA 0x55` |
| Brightness / speed range | `0x00`–`0x0F` |

---

## 4. Lighting

### 4.1 Mode IDs

Byte 0 of the mode frame:

| ID | Mode | ID | Mode |
|---|---|---|---|
| `0x01` | Static | `0x0B` | Horizontal gradient / rainbow wave |
| `0x02` | Keystroke light-up | `0x0C` | Around edges |
| `0x03` | Keystroke dim | `0x0D` | Keystroke horizontal lines |
| `0x04` | Sparkle | `0x0E` | Keystroke tilted lines |
| `0x05` | Rain | `0x0F` | Keystroke ripples |
| `0x06` | Random colors | `0x10` | Sequence |
| `0x07` | Breathing | `0x11` | Wave line |
| `0x08` | Spectrum cycle | `0x12` | Tilted lines |
| `0x09` | Ring gradient | `0x13` | Back-and-forth |
| `0x0A` | Vertical gradient | | |

Special modes: `0x20` Direct (per-key, volatile), `0x23` Custom (per-key,
saved), `0x80` Lights-off.

Color (bytes 1–3) applies only to modes that carry a mode-specific color; Random
colors, Spectrum cycle, and Lights-off ignore it. Direction (byte 11) applies
only to the gradient, sequence, and back-and-forth modes.

### 4.2 Mode frame

Used for static color and all effects:

| Byte | Field |
|---|---|
| 0 | Mode ID |
| 1–3 | R, G, B |
| 8 | Random-color flag (`1` = on) |
| 9 | Brightness (`0x00`–`0x0F`) |
| 10 | Speed (`0x00`–`0x0F`) |
| 11 | Direction |
| 14–15 | `0xAA 0x55` (page-check code) |

### 4.3 Static / effect transaction

```
Send  04 18                 Customization on
Read
Send  04 13  [byte 8]=01    Start effect page (1 packet)
Read
Send  <mode frame>          See section 4.2
Read
Send  04 02                 End communication
Read
Send  04 F0                 LED effect start
```

### 4.4 Per-key lighting

Per-key colors are written to a 128-slot framebuffer of 8 packets × 64 bytes,
4 bytes per slot. The slot index is the key's `light_index` (section 6).

Framebuffer packet layout (8 packets, 16 slots each):

```
slot l:  byte (l*4 + 0) = l        slot index
         byte (l*4 + 1..3) = R, G, B
```

**Direct mode (`0x20`)** is volatile and must be refreshed at least every
2 seconds, or the device reverts:

```
Send  04 20  [byte 8]=08    Direct-mode header
Send  <8 framebuffer packets>   (no 0x04 header)
Read
Send  04 02                 End communication
```

**Custom mode (`0x23`)** is the saved equivalent: customization-on, then the
`0x23` header and framebuffer, then end-communication and effect-start.

---

## 5. Keymap / remap

A remap writes a per-layer key-definition area, a 9-page / 576-byte buffer,
inside the transaction. The command selects the layer: `0x11` for the base layer
and `0x27` for the Fn layer. The two areas are otherwise identical.

```
Send  04 18                 Customization on
Read
Send  04 NN  [byte 8]=09    Start key-definition page (NN = 0x11 base / 0x27 Fn)
Read
Send  <page 0> .. <page 8>  576-byte buffer; page 8 carries 0xAA 0x55
Read  (after each page)     at bytes 62–63
Send  04 02                 End communication
Read
Send  04 F0                 LED effect start (re-render)
```

Each key's entry is 4 bytes at absolute offset `key_index * 4`:

```
offset + 0 : 0x02            entry marker
offset + 1 : 0x00
offset + 2 : keycode low     16-bit little-endian VIA/QMK keycode (KC_A = 0x0004)
offset + 3 : keycode high
```

The page is `(key_index * 4) / 64`; the in-page offset is `(key_index * 4) % 64`.

Rules:

- A zero entry keeps the factory default; a marked entry overrides it. Send the
  full keymap on every commit so previously remapped keys are not reset.
- Keycodes are standard VIA/QMK 16-bit values.
- The `02 00 <scancode>` select frame seen in some application captures only
  highlights the key on screen; it is not required for the write.

Confirmed offsets (`offset = key_index * 4`): Esc(1) = 4, `1`(20) = 80,
`5`(24) = 96, `=`(31) = 124, Tab(37) = 148, Q(38) = 152, Caps(55) = 220,
Space(94) = 376.

The Fn layer uses the same offset scheme in its own area (command `0x27`),
confirmed from the Fn-remap captures: Fn+A (key_index 56 → offset 224 → page 3,
in-page offset 32) set to F9 (`0x42`) produced `[32]=02 [34]=42` in the `0x27`
area. A commit sends one transaction per layer that has edits.

---

## 6. Key and LED index map

The official driver ships a `KeyboardLayout.xml` mapping each key to its HID
usage `code`, a `key_index` (the keymap slot for command `0x11`), and a
`light_index` (the per-key LED framebuffer slot). On this board
`key_index == light_index` for every key. The indices are sparse (1–121): they
are the firmware's flat scan/buffer positions, not a contiguous matrix.

Entries below are `key (code / index)`, grouped by physical row:

- **Row 0:** Esc (`0x29` / 1), 1 (`0x1e` / 20), 2 (`0x1f` / 21), 3 (`0x20` / 22),
  4 (`0x21` / 23), 5 (`0x22` / 24), 6 (`0x23` / 25), 7 (`0x24` / 26),
  8 (`0x25` / 27), 9 (`0x26` / 28), 0 (`0x27` / 29), `-` (`0x2d` / 30),
  `=` (`0x2e` / 31), Backspace (`0x2a` / 103)
- **Row 1:** Tab (`0x2b` / 37), Q (`0x14` / 38), W (`0x1a` / 39), E (`0x08` / 40),
  R (`0x15` / 41), T (`0x17` / 42), Y (`0x1c` / 43), U (`0x18` / 44),
  I (`0x0c` / 45), O (`0x12` / 46), P (`0x13` / 47), `[` (`0x2f` / 48),
  `]` (`0x30` / 49), `\` (`0x31` / 67), Del (`0x4c` / 119)
- **Row 2:** Caps (`0x39` / 55), A (`0x04` / 56), S (`0x16` / 57), D (`0x07` / 58),
  F (`0x09` / 59), G (`0x0a` / 60), H (`0x0b` / 61), J (`0x0d` / 62),
  K (`0x0e` / 63), L (`0x0f` / 64), `;` (`0x33` / 65), `'` (`0x34` / 66),
  Enter (`0x28` / 85), PageUp (`0x4b` / 118)
- **Row 3:** LShift (`0xe1` / 73), Z (`0x1d` / 74), X (`0x1b` / 75),
  C (`0x06` / 76), V (`0x19` / 77), B (`0x05` / 78), N (`0x11` / 79),
  M (`0x10` / 80), `,` (`0x36` / 81), `.` (`0x37` / 82), `/` (`0x38` / 83),
  RShift (`0xe5` / 84), Up (`0x52` / 101), PageDown (`0x4e` / 121)
- **Row 4:** LCtrl (`0xe0` / 91), Win (`0xe3` / 92), LAlt (`0xe2` / 93),
  Space (`0x2c` / 94), RAlt (`0xe6` / 95), Fn (`0xaf` / 96), Left (`0x50` / 99),
  Down (`0x51` / 100), Right (`0x4f` / 102)

---

## 7. Matrix and topology

| Field | Value |
|---|---|
| Keymap matrix | 5 rows × 15 columns (from the vendor layout) |
| Addressable keys | 66 |
| Layers | 2 (base + Fn); only base-layer remap is wired |
| Addressing | Flat `key_index` (1–121); `(row, col)` is a clackd convenience layer |

The driver's `KEY_INDEX` table in
[`src/hal/gmk67.rs`](../../src/hal/gmk67.rs) maps each `(row, col)` to a
`key_index`.

---

## 8. Driver implementation notes

`src/hal/gmk67.rs`:

- **Lighting** — `set_lighting` runs the static/effect transaction (section 4.3).
  Brightness and speed are clamped to `0x00`–`0x0F`. Per-key Direct/Custom is not
  yet exposed through the `KeyboardDriver` trait.
- **Remap** — `commit_to_nvram` sends one key-definition transaction per layer
  that has edits (base layer command `0x11`, Fn layer `0x27`; section 5), each
  carrying that layer's full keymap.
- **EEPROM wear** — the device writes the EEPROM on every key-definition commit;
  commits should be coalesced.
- **Keymap read-back** — the device exposes no keymap read, so the driver keeps a
  host-side shadow and a frontend overlays factory defaults.

---

## 9. Source attribution

The EK68 shares firmware with the GMK67, made by Zuoya. The command and lighting
structure was decoded from the GMK67 OpenRGB driver written by Jurre Kol:

- Repository: `gitlab.com/aethernali.live/OpenRGB`, branch `gmk67`
  (commit `310cf818537e958c54572939ca6c4c5e624404cf`, 2024-07-20),
  `Controllers/GMK67KeyboardController/` (GPL-2.0).
- OpenRGB device request / merge request:
  `gitlab.com/CalcProgrammer1/OpenRGB` issue
  [#4512](https://gitlab.com/CalcProgrammer1/OpenRGB/-/work_items/4512).

The key/LED index map is from the official Epomaker driver's
`KeyboardLayout.xml`. All values are confirmed against the physical EK68.

---

## 10. Open items

- Macro format (commands `0x14` / `0x15`).
- Per-key Direct lighting through the clackd engine, including the 2-second
  keepalive.

---

## Appendix A. Capture and extraction

USB captures of the Windows application are taken with USBPcap / Wireshark.
Extract distinct `SET_REPORT` payloads:

```
tshark -r CAPTURE.pcapng -Y "usbhid.setup.bRequest == 0x09" \
  -T fields -e usb.data_fragment
```

The 64-byte payload is in `usb.data_fragment` (the `usb.capdata` and
`usbhid.data` fields are empty for this device). Decoding a frame's non-zero
bytes reads the offsets directly.

Captures used to confirm this specification: solid red/green/blue and a
brightness sweep (mode frame); a 20-mode sweep (mode IDs); and the `pos`, `shuf`,
and `remap` captures that establish `offset = key_index * 4`.

## Appendix B. History

Early attempts sent only the bare mode frame and could not render lighting, even
with byte-exact replay of the application's frames. The cause was the missing
`0x04`-header transaction framing (the frames initially mistaken for status
heartbeats). This is recorded so the mistake is not repeated: a data frame is
inert outside its transaction.
