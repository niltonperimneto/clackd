# Epomaker EK68 (non-VIA) — proprietary HID protocol

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
| USB Vendor ID (VID) | `TODO` |
| USB Product ID (PID) | `TODO` |
| Manufacturer string | `TODO` |
| Number of HID interfaces | `TODO` |
| **Vendor/config interface** number | `TODO` |
| Vendor interface **Usage Page** | `TODO` (hypothesis: `0xFF00`) |
| Vendor interface Usage | `TODO` |
| Report ID (if any) | `TODO` |
| Report length (bytes) | `TODO` (hypothesis: 64) |
| Transport (SET_REPORT control / interrupt OUT) | `TODO` |
| OUT endpoint address | `TODO` |
| IN endpoint address (replies, if any) | `TODO` |

Linux `/dev/hidrawN` selection note (which collection to bind): `TODO`

---

## 2. Frame format  (Phase A4)

```
Offset  Size  Field        Notes
------  ----  -----------  ---------------------------------
TODO    TODO  report_id    TODO
TODO    TODO  command      command/header byte — see §3
TODO    TODO  ...          payload
TODO    TODO  checksum     algorithm: TODO
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
| `TODO` | RGB: set mode | ☐ | | |
| `TODO` | RGB: brightness | ☐ | | byte sweep 0/50/100 |
| `TODO` | RGB: speed | ☐ | | |
| `TODO` | RGB: direction | ☐ | | |
| `TODO` | RGB: global color (R/G/B layout) | ☐ | | pure R/G/B/white |
| `TODO` | Per-key RGB | ☐ | | key-index encoding |
| `TODO` | Key remap (set keycode at position) | ☐ | | matrix-index + keycode map |
| `TODO` | Commit / persist to EEPROM | ☐ | | the "save" click |
| `TODO` | Macro: define/slot | ☐ | | 2- vs 3-key macro diff |

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

| # | Action performed (single variable) | File | Decoded delta |
|---|---|---|---|
| 1 | `TODO` | | |
