# clackd

> **An unprivileged, VIA-first asynchronous keyboard configuration daemon for Linux.**

`clackd` is the backend layer that makes seamless keyboard configuration on Linux possible. It runs as a lightweight systemd user service, talks directly to your keyboard hardware over `hidraw`, and exposes a stable **D-Bus session API** (`io.github.clackd`) that any frontend — GUI configurator, CLI tool, scripted hot-reloader, or desktop shell integration — can consume without touching hardware or needing elevated privileges.

The goal is simple: **lay the groundwork so that building a first-class keyboard configuration experience on Linux requires only a frontend.** The daemon handles hardware quirks, async I/O, protocol translation, device lifecycle, crash recovery, and multi-client coherence — so frontend authors never have to.

---

## Why clackd?

The name is a nod to the sound a keyboard makes — and to the `d` that Unix daemons have worn for decades. It is a small, quiet process running in the background, doing exactly one thing well: being the reliable bridge between your keyboard hardware and anything that wants to talk to it. The "clack" is yours; the daemon just makes sure the right signal gets through.

Keyboard configuration on Linux has long been an afterthought. Mechanical keyboard enthusiasts who invest in open-source QMK firmware end up remapping keys through a browser using WebHID — a workaround that requires Chrome, an internet connection, and a silent prayer that the USB handshake cooperates. Users of Logitech or Razer boards fare even worse, stuck with Windows-only software or community-reverse-engineered tools that need root and tend to break on kernel updates. The underlying protocols and hardware interfaces have been there all along; what was missing was a proper, unprivileged, always-on daemon to own that layer and expose it cleanly to the rest of the system.

`clackd` is that daemon. It speaks VIA natively over raw `hidraw` character devices, runs entirely as your user via `udev` session ACLs, and wraps the whole thing in a stable D-Bus session API that any frontend can consume without knowing anything about USB HID frames, EEPROM semantics, or vendor binary blobs. Legacy keyboards with proprietary protocols get a shadow-state polyfill that makes them look like VIA hardware to the rest of the stack — no special cases above the HAL. Multiple clients can talk to the daemon simultaneously without racing each other, because the hardware is always the source of truth. And if a keyboard drops off the bus or a device task panics, the supervisor recovers cleanly without touching the rest of your connected boards.

The longer-term ambition is straightforward: make it so that anyone who wants to build a keyboard configurator for Linux — a GTK app, a KDE plasmoid, a Waybar module, a Tauri GUI, a terminal UI — only needs to speak D-Bus. The hard, tedious, hardware-facing work lives here, done once, done correctly, and available to every frontend that comes after.

---

## Architecture Overview

`clackd` is built around three cleanly separated domains with strict error-type boundaries between them:

```
┌─────────────────────────────────────────────────────────────────┐
│  Frontends  (GUI, CLI, scripts, desktop integrations)           │
│  clackctl · any future GTK/Qt/Tauri/web app · shell scripts     │
└──────────────────────┬──────────────────────────────────────────┘
                       │  D-Bus session bus  (io.github.clackd)
┌──────────────────────▼──────────────────────────────────────────┐
│  IPC Layer  (src/ipc/)                                          │
│  zbus session interface · signal emission · DaemonError → fdo   │
└──────────────────────┬──────────────────────────────────────────┘
                       │  tokio mpsc  (EngineCommand)
┌──────────────────────▼──────────────────────────────────────────┐
│  Engine  (src/engine/)                                          │
│  Device registry · worker supervision · topology structs        │
│  DriverError → DaemonError boundary                             │
└──────────────────────┬──────────────────────────────────────────┘
                       │  KeyboardDriver trait (async)
┌──────────────────────▼──────────────────────────────────────────┐
│  HAL  (src/hal/)                                                │
│  VIA native driver · Legacy shadow-state polyfill               │
│  AsyncFd hidraw I/O · 32-byte HID frames · exponential backoff  │
└─────────────────────────────────────────────────────────────────┘
```

### Source Tree

```
src/
├── main.rs                 # Tokio runtime init & tokio-udev device supervisor
├── bin/
│   └── clackctl.rs         # CLI D-Bus client (shipped alongside the daemon)
├── ipc/
│   ├── mod.rs              # zbus connection setup
│   └── frontend.rs         # #[zbus::interface] — the public API surface
├── engine/
│   ├── mod.rs              # Device registry, worker spawning, DaemonError
│   ├── topology.rs         # Key matrix, Layer, and Profile structs
│   └── messages.rs         # EngineCommand enum (IPC ↔ Engine routing)
└── hal/
    ├── mod.rs              # KeyboardDriver trait & DriverError
    ├── via.rs              # QMK/VIA native AsyncFd hidraw driver
    └── legacy/
        ├── mod.rs          # Shadow state machine & debounce logic
        ├── gmk67.rs        # GMK67-family (Epomaker EK68 / Zuoya) driver
        └── logitech.rs     # Logitech HID++ blob compiler (G915/Lightspeed)
```

---

## Core Design Principles

### 1. Hardware as the Source of Truth (VIA-First)

By default the daemon caches **zero** keymap state for QMK/VIA devices. Every `get_keycode` call performs a real-time 32-byte `hidraw` round-trip to the firmware. This guarantees that multiple clients — a GUI configurator open on one workspace and a hot-reload script running in a terminal on another — always see the same state without any synchronization logic in the daemon.

The only permitted deviation is the opt-in write-coalescing buffer (`CLACKD_VIA_COALESCE_MS`), which exists solely to reduce EEPROM wear under a rapidly-changing UI (e.g. a slider drag). It is explicitly off by default and documented as a trade-off.

### 2. Unprivileged by Design

`clackd` never needs root. Access to `/dev/hidraw*` nodes is granted dynamically by `udev` via `TAG+="uaccess"` when a matching keyboard is plugged in. The daemon runs entirely in your session.

`PermissionDenied` from the kernel is handled gracefully — the device is logged and skipped, not panicked on.

### 3. Strictly Non-Blocking I/O

The async engine is built on Tokio. `hidraw` character devices are accessed via `tokio::io::unix::AsyncFd` wrapping a raw `O_NONBLOCK` file descriptor — **never** via `tokio::fs`, which would stall blocking pool threads waiting for USB interrupt-in transfers. Every HID round-trip is wrapped in a configurable timeout (default 1000 ms).

`udev` events arrive as an async stream from `tokio-udev`, backed by `AsyncFd` over the udev netlink socket. No blocking libudev loops exist in the runtime path.

### 4. Legacy Polyfilling via Shadow State

Proprietary keyboards (Logitech, Razer) don't speak VIA. Their drivers implement a **Shadow State Machine**: keymap state is maintained in a local serde/JSON cache under `$XDG_DATA_HOME/clackd/`. `get_keycode` reads instantly from the cache; `set_keycode` marks state dirty and triggers a debounced compile-and-push of the vendor-specific binary blob. The core engine and IPC layer are completely unaware they are not talking to VIA hardware.

Cache entries carry a `CacheStatus` (`Confirmed` / `Pending` / `Failed`) so frontends can distinguish authoritative hardware state from provisional in-flight state.

### 5. Crash Isolation and Supervised Recovery

Each device gets its own async worker task. A panic, timeout, or USB error in one device's task cleanly drops that device handle and emits a `DeviceRemoved` D-Bus signal — it does not affect other devices or the daemon process. The supervisor detects the failed task and schedules reconnection with exponential backoff before marking the device permanently offline.

`.unwrap()` and `panic!()` are forbidden in worker tasks and the engine except at statically proven invariants marked with `// INVARIANT:` comments.

---

## D-Bus API

The daemon exposes its interface on the session bus at:

- **Bus name:** `io.github.clackd`
- **Object path:** `/io/github/clackd`
- **Interface:** `io.github.clackd.Device`

### Methods

| Method | Signature | Description |
|---|---|---|
| `GetKeycode` | `(s, y, y, y) → q` | Read the keycode at `(device_id, layer, row, col)`. Queries hardware directly (or the coalescing buffer if enabled). |
| `SetKeycode` | `(s, y, y, y, q) → ()` | Write a keycode to `(device_id, layer, row, col)`. |
| `GetMacro` | `(s, q, y) → ay` | Read macro buffer bytes starting at `offset` for `length` bytes. |
| `SetMacro` | `(s, q, ay) → ()` | Write raw bytes into the macro buffer at `offset`. |
| `GetLighting` | `(s, y) → ay` | Query a lighting/RGB value by command byte. |
| `SetLighting` | `(s, y, ay) → ()` | Set a lighting/RGB value by command byte and payload. |
| `CommitToNvram` | `(s) → ()` | Flush pending writes for a device. No-op for unbuffered VIA; meaningful for coalescing VIA and legacy drivers. |

### Signals

| Signal | Arguments | Emitted when |
|---|---|---|
| `DeviceAdded` | `device_id: s` | A keyboard is plugged in and its worker is ready. |
| `DeviceRemoved` | `device_id: s` | A keyboard is unplugged or its worker task has been dropped. |
| `LayoutUpdated` | `device_id: s` | A keymap write has been finalized and committed. |

### Error Mapping

`DaemonError` variants are translated to standard D-Bus fdo errors before crossing the IPC boundary:

| Condition | D-Bus error |
|---|---|
| Device not found | `org.freedesktop.DBus.Error.UnknownObject` |
| Permission denied | `org.freedesktop.DBus.Error.AccessDenied` |
| Protocol / I/O failure | `org.freedesktop.DBus.Error.Failed` |

---

## Frontend Integration

`clackd` is intentionally just the backend. The D-Bus API is the contract that any frontend can build on top of:

- **GUI configurators** (GTK, Qt, Tauri, web-based via a local bridge) can call `GetKeycode` / `SetKeycode` per key and listen for `LayoutUpdated` signals to refresh their view.
- **CLI tools** can use `clackctl` (shipped alongside the daemon) for scripting and quick edits.
- **Desktop shell integrations** (GNOME extensions, KDE plasmoids, Waybar modules) can subscribe to `DeviceAdded` / `DeviceRemoved` signals for live status.
- **Hot-reload scripts** can watch for profile-switching events and programmatically remap layers on the fly.

Because the daemon owns the hardware channel and guarantees non-blocking multi-client behavior, a frontend needs no special knowledge of `hidraw`, USB HID, VIA framing, or vendor quirks. It just speaks D-Bus.

---

## `clackctl` — the bundled CLI client

A `ratbagctl`-style command-line client is included as `src/bin/clackctl.rs` and installed alongside the daemon binary.

```sh
# List connected devices (index: device_id)
clackctl list

# Show matrix dimensions and layer count
clackctl info hidraw0
clackctl info 0           # by index from `list`

# Read a keycode at (layer, row, col) — outputs 4-digit hex
clackctl get hidraw0 0 2 5
# => 0x004c

# Write a keycode — accepts decimal or 0x-prefixed hex; silent on success
clackctl set hidraw0 0 2 5 0x004c
clackctl set 0 0 2 5 76

# Flush pending writes to NVRAM (meaningful with write coalescing enabled)
clackctl commit hidraw0

# Stream device + layout events until Ctrl-C
clackctl monitor
```

---

## Project Status

`clackd` is under active development. The architectural foundation is in place; full hardware driver coverage and legacy vendor polyfills are being added incrementally.

| Mission | Status | Scope |
|---|---|---|
| 1 — Foundation | ✅ Complete | Error boundaries, HAL trait contract, module skeleton |
| 2 — Engine | ✅ Complete | Device supervisor, topology structs, worker lifecycles, `EngineCommand` routing |
| 3 — IPC & VIA Driver | ✅ Complete | D-Bus session gateway, `clackctl` CLI, native `hidraw` VIA driver |
| 4 — Legacy Polyfill | 🚧 In progress | Shadow-state machine ✅, GMK67 driver ✅, Logitech (G915/Lightspeed) blob compiler ✅ — Razer pending |
| 5 — Symbolic keycodes | 🔲 Planned | `KC_A`-style name table for frontends |
| 6 — Profile management | 🔲 Planned | Named profiles, import/export, per-application hot-swap |

---

## Requirements

- Linux ≥ 5.4 (`hidraw` + `uaccess` tag in udev)
- systemd ≥ 240 (for user service `Type=notify`)
- Rust ≥ 1.95 (MSRV declared in `Cargo.toml`)
- A session D-Bus (`dbus-broker` or `dbus-daemon`)

For build, install, udev rule setup, and configuration options, see **[INSTALL.md](INSTALL.md)**.

---

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).
