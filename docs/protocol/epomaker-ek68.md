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
| `0x01` | RGB: set global solid color (all keys) | ✅ | red/green/blue | R@1 G@2 B@3; byte9=`10`, byte10=`0b` const; footer `AA 55`@14 |
| `0x01` byte 9? | RGB: brightness | ☐ | | hypothesis: byte 9 (=`0x10`); confirm via brightness sweep |
| `0x01` byte 10? | RGB: effect/mode id | ☐ | | hypothesis: byte 10 (=`0x0b`=static); confirm via mode change |
| `TODO` | RGB: speed | ☐ | | |
| `TODO` | RGB: direction | ☐ | | |
| `TODO` | Per-key RGB | ☐ | | key-index encoding |
| `TODO` | Key remap (set keycode at position) | ☐ | | matrix-index + keycode map |
| `TODO` | Commit / persist to EEPROM | ☐ | | the "save" click |
| `TODO` | Macro: define/slot | ☐ | | 2- vs 3-key macro diff |

### Heartbeat / poll frames (ignore for config)
`04 02 …`, `04 F5 … (byte8=09)`, `04 F0 …`, `04 18 …`, `04 13 … (byte8=01)` — sent
continuously (~2/sec) while the app is open. Not configuration writes.

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
