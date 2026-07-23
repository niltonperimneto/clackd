# Logitech G915 / Lightspeed HID++ 2.0 protocol

Specification for the Logitech HID++ 2.0 protocol subset implemented by the
clackd `logitech` driver
([`src/hal/legacy/logitech.rs`](../../src/hal/legacy/logitech.rs), selected
with `driver = "logitech"` **plus `experimental = true`** in `devices.toml`
— the backend is gated off by default until this record is
hardware-confirmed). The first target is the **G915 Lightspeed** keyboard
reached **through its Lightspeed USB receiver**; a cable-connected board
uses the same protocol at the wired device index.

Unlike the GMK67 record, nothing below has been confirmed against physical
hardware yet. Every section is assembled **from public reverse engineering** —
primarily the libratbag `hidpp20` driver and the Solaar project — and is
marked accordingly. The framing (§2) and feature model (§3) are the stable,
widely-reimplemented core of HID++ 2.0; the profile-sector layout for G-keys
(§5.3) is the most provisional part.

## Status

| Area | State |
|---|---|
| Report framing (short/long, SW id, error replies) | From public sources; pending hardware confirmation |
| Receiver device-index routing + discovery | From public sources; pending hardware confirmation |
| Feature enumeration (IRoot `0x0000`) | From public sources; pending hardware confirmation |
| Device name (`0x0005`) | From public sources; pending hardware confirmation |
| Onboard Profiles (`0x8100`) memory model + CRC | From libratbag; pending hardware confirmation |
| G-key binding layout inside the profile sector | **Provisional** — extrapolated from the libratbag mouse profile format; needs a G HUB USB capture |
| RGB Effects (`0x8071`) cluster-effect frame | From libratbag/Solaar; parameter layout pending hardware confirmation |
| Macros | Not implemented (deferred; `NotSupported`) |

## Scope

- Connection: Lightspeed receiver (primary) or wired USB (fallback index).
- In scope: G-key remapping via onboard profiles, whole-cluster RGB effects.
- Out of scope for this stage: macro storage, per-key lighting patterns,
  battery, host-mode (software-driven) G-key diversion via `0x8010`.

---

## 1. Device identification

| Field | Value |
|---|---|
| Vendor ID | `0x046D` (Logitech) |
| Lightspeed receiver PID | `0xC541` (G915; receiver PIDs vary per bundle — configure the real one in `devices.toml`) |
| Wired PID | varies by revision — configure in `devices.toml` |
| Configuration interface | The receiver/keyboard interface whose report descriptor declares the HID++ vendor usage page (`0xFF00` on receivers, `0xFF43` on modern wired devices) |
| Transport | Numbered interrupt reports over `hidraw` `read(2)`/`write(2)` |

Selection on Linux: the receiver is a composite HID device; only the
interface carrying the vendor usage page accepts HID++ frames. The driver
probes the report descriptor at open and rejects the other nodes, exactly
like the VIA (`0xFF60`) and GMK67 (64-byte feature report) probes.

## 2. Report framing

Three numbered reports exist; clackd sends **long** requests and accepts
short or long replies:

| Report ID | Length | Name |
|---|---|---|
| `0x10` | 7 bytes | short |
| `0x11` | 20 bytes | long |
| `0x12` | 64 bytes | very long (not used by this driver) |

Frame layout (request and reply):

```text
[report_id, device_index, feature_index, (function << 4) | sw_id, params...]
```

- `device_index`: `0xFF` for a wired device; `0x01`–`0x06` for a slot on a
  receiver.
- `sw_id`: a host-chosen 4-bit tag echoed in replies, used to match
  responses on a channel shared with other software and with unsolicited
  notifications. clackd uses `0x0A`.
- A long frame carries 16 parameter bytes.

### 2.1 Error replies

Two error shapes exist on the same channel:

- **HID++ 2.0 error** (from the device):
  `[.., device_index, 0xFF, feature_index, (function << 4) | sw_id, error_code, ..]`.
  Codes: 2 `InvalidArgument`, 3 `OutOfRange`, 6 `InvalidFeatureIndex`,
  7 `InvalidFunctionId`, 8 `Busy`, 9 `Unsupported`.
- **HID++ 1.0 error** (from the receiver):
  `[0x10, device_index, 0x8F, sub_id, address, error_code, 0x00]`.
  Codes of interest: 4 `ConnectFail`, 8 `UnknownDevice`, 9 `ResourceError`
  — all three mean "the slot's device is unreachable" (powered off, out of
  range, not paired) and map to a clean disconnect, not a protocol fault.

### 2.2 Unsolicited traffic

The receiver multiplexes notifications (device arrival `0x41` / departure
`0x40`, battery, etc.) onto the same node. The transport's read loop skips
any frame that does not match the pending request's
`(device_index, feature_index, function, sw_id)`, bounded by the per-call
1000 ms timeout.

## 3. Feature discovery (IRoot, feature `0x0000`)

IRoot is always at feature index 0.

| Function | Params | Reply |
|---|---|---|
| `0x0` getFeature | `[feat_id_hi, feat_id_lo]` | `[feature_index, type, version]`; index 0 = absent |
| `0x1` ping / getProtocolVersion | `[0, 0, magic]` | `[major, minor, magic-echo]`; a HID++ 1.0-only responder returns the `0x8F` error instead |

### 3.1 Device-index discovery

At attach the driver probes, in order: `0xFF` (wired), then slots `1..=6`.
A slot qualifies when the ping succeeds **and** `getFeature(0x8100)` returns
a non-zero index — that filters out paired mice and empty slots in one pass.
If nothing qualifies the attach fails as *disconnected* (the supervisor's
exponential backoff naturally covers "keyboard currently switched off").

Features resolved at attach: `0x8100` Onboard Profiles (required), `0x8071`
RGB Effects (optional — lighting reports `NotSupported` without it),
`0x0005` Device Name (optional, display only).

## 4. Device name (feature `0x0005`)

| Function | Params | Reply |
|---|---|---|
| `0x0` getDeviceNameCount | — | `[length]` |
| `0x1` getDeviceName | `[offset]` | 16 ASCII bytes |

Read once at attach for `model_name()`; falls back to `"Logitech G915"`.

## 5. Onboard Profiles (feature `0x8100`)

### 5.1 Functions

| Function | Purpose | Params → Reply |
|---|---|---|
| `0x0` getDescription | memory topology | → `[mem_model, profile_fmt, macro_fmt, profile_count, oob_count, button_count, sector_count, sector_size_hi, sector_size_lo, ..]` |
| `0x1` setOnboardMode | `[1]` = onboard, `[2]` = host | |
| `0x2` getOnboardMode | | `[mode]` |
| `0x3` setCurrentProfile | `[0, profile_id]` | |
| `0x4` getCurrentProfile | | `[sector_hi, sector_lo]` |
| `0x5` memoryRead | `[sector_hi, sector_lo, offset_hi, offset_lo]` | 16 bytes |
| `0x6` memoryAddrWrite | `[sector_hi, sector_lo, offset_hi, offset_lo, count_hi, count_lo]` | opens a write session |
| `0x7` memoryWrite | 16 data bytes | repeated per chunk |
| `0x8` memoryWriteEnd | | commits the session |

### 5.2 Memory model

- Sector `0x0000` is the profile directory; writable profiles live in
  sectors `1..=profile_count`. Factory (OOB) profiles mirror at `0x0100+`.
- Reads and writes move in 16-byte chunks; a write session is
  `memoryAddrWrite` → n × `memoryWrite` → `memoryWriteEnd`.
- The final 2 bytes of every valid sector are a **CRC-CCITT** checksum
  (polynomial `0x1021`, init `0xFFFF`, no final XOR — the "CCITT-FALSE"
  variant; `"123456789"` → `0x29B1`), big-endian, computed over
  `sector_size - 2` bytes.

The driver reads the **active profile sector** at attach as its compile
baseline, so a commit only ever patches G-key slots into an otherwise
device-authored profile. If the attach read fails, commits are refused
(`VendorCompile`) rather than pushing a fabricated profile image.

### 5.3 G-key bindings inside the profile sector (provisional)

Extrapolated from the libratbag mouse-profile button table (4-byte binding
entries starting at offset `0x20`), adapted to the G915's three M-key banks:

```text
binding_offset(bank, gkey) = 0x20 + bank * 0x40 + gkey * 4
```

- `bank` 0–2 ↔ clackd layers 0–2 (M1/M2/M3); `gkey` 0–4 ↔ matrix cols
  (G1–G5, matrix row 0).
- Binding entry, keyboard HID usage form: `[0x80, 0x02, modifier_mask, hid_usage]`
  (`modifier_mask` per HID boot-keyboard byte: bit 0 LCtrl … bit 7 RGui).
  `[00 00 00 00]` = unassigned (factory behavior).

**This layout is the part most likely to move once a G HUB capture of a
G915 G-key remap exists.** The compiler and parser sit behind pure
functions (`compile_profile_image` / `parse_gkey_bindings`) so a corrected
offset table is a constants-only change.

### 5.4 Keycode translation

The D-Bus surface speaks 16-bit QMK keycodes. Mapping to the binding entry:

| QMK code | Binding |
|---|---|
| `0x0000` (`KC_NO`) | `[00 00 00 00]` (unassigned) |
| `0x0004..=0x00A4` (basic) | `[80 02 00 usage]` — QMK basic codes *are* HID usages |
| `0x00E0..=0x00E7` (modifiers) | `[80 02 mask 00]`, `mask = 1 << (code - 0xE0)` |
| `0x0100..=0x1FFF` (mod + basic) | `[80 02 mask usage]` from the QMK 5-bit mod field |
| anything else | not expressible → `DriverError::VendorCompile { vendor: "logitech" }` |

## 6. RGB Effects (feature `0x8071`)

| Function | Purpose |
|---|---|
| `0x0` getInfo | cluster/effect enumeration (not yet consumed) |
| `0x1` setClusterEffect | apply one effect to one cluster |

`setClusterEffect` params as encoded by this driver (layout pending
hardware confirmation):

```text
[cluster, effect_index, r, g, b, period_hi, period_lo, brightness, 0, persist]
```

- `cluster`: `0x00` (primary — whole keyboard). Per-cluster addressing is a
  later stage.
- `effect_index`: index into the cluster's effect table; `0` is off and `1`
  fixed-colour on the boards inspected by libratbag/Solaar. Exposed raw as
  the packed-payload `mode` byte and the VIA effect id.
- `period`: milliseconds, derived from the VIA 0–255 speed
  (`2000 - speed * 7`).
- `persist`: `0x00` RAM-only (live sliders), `0x01` flash. The driver sends
  RAM-only on every `set_lighting` and re-sends with the flash flag from
  `commit_to_nvram` — the same wear-levelling split VIA's
  `id_custom_save` has.

VIA custom-value compatibility (channel 3 RGB-matrix, value ids
brightness/effect/speed/color, channel 0 packed) matches the GMK67 driver;
the shared helpers live in `src/hal/legacy/mod.rs`.

## 7. Persistence & shadow state

Standard legacy-polyfill model (CLAUDE.md §4.2): the shadow keymap persists
at `$XDG_DATA_HOME/clackd/046d_g915.json` (a stable stem — the receiver and
a cable connection reach the *same* keyboard, so the file is not keyed on
the transport's PID), `set_keycode` marks it dirty,
the engine's 500 ms debounce triggers `commit_to_nvram`, and a failed push
rolls the shadow back to last-known-good.

## 8. Leads for future iterations

Mirroring the GMK67 record's "future lead" notes — each item names the
wire-level entry point so the next iteration doesn't restart from zero.
The module doc (`src/hal/legacy/logitech.rs`, "Future Work") ranks them.

- **Hardware confirmation** (the gate-lifter): capture G HUB performing a
  G-key remap and a lighting change. The two provisional guesses to check
  first are the G-key slot layout (§5.3: base `0x20`, bank stride `0x40`)
  and the `setClusterEffect` parameter order (§6). If the sector layout
  moves, only `GKEY_BANK_BASE`/`GKEY_BANK_STRIDE`-family constants change.
- **Macros**: `getDescription` reports a macro format id (§5.1) and
  profiles reference macro sectors; the binding table has a macro type
  besides `0x80` key bindings. Wire these to the VIA macro-buffer methods.
- **Effect-table discovery**: `0x8071` `getInfo` (fn `0x0`) with
  `[cluster, 0xFF, ...]`-style queries enumerates clusters and their
  per-cluster effect lists, replacing today's raw effect-index passthrough
  and enabling per-cluster addressing (keys vs. edge lighting).
- **Receiver arrival notifications**: sub-id `0x41` (arrival) / `0x40`
  (departure) frames on the receiver channel; today they classify as
  `Ignore`. Feeding arrivals to the supervisor would replace backoff
  polling with event-driven re-attach.
- **Battery**: features `0x1000` (battery unified level) / `0x1004`
  (unified battery) — blocked on a `KeyboardDriver` trait extension, not
  on protocol work.
- **Profile management**: sector 0 profile directory entries +
  `setCurrentProfile` (§5.1) — the backend hook for mission 6.
- **Very-long report `0x12`** (64 bytes): read-side handled, TX not
  emitted; some newer firmwares prefer it for bulk transfers.

## 9. Source attribution

- **libratbag** (`src/hidpp20.c`, `src/driver-hidpp20.c`) — HID++ 2.0
  framing, error codes, Onboard Profiles functions, sector CRC, profile
  button table.
- **Solaar** (`lib/logitech_receiver/`) — receiver device-index routing,
  notification sub-ids, feature ids, ping semantics.
- Both are independent reimplementations of unpublished vendor protocol;
  neither covers the G915 G-key sector layout precisely, hence §5.3's
  provisional marking.
