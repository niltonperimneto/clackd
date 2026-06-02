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
report ID). The Windows USBPcap capture's SET_REPORT data field is the 64 payload bytes.

```
Offset  Size  Field        Notes
------  ----  -----------  ---------------------------------
0       1     command      command/header byte — see §3   (TODO: confirm offset)
...     ...   payload      TODO
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
