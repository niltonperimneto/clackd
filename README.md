# clackd

An unprivileged, VIA-first asynchronous keyboard daemon for Linux, written in Rust.

`clackd` connects your QMK/VIA compatible keyboards (as well as legacy/proprietary boards via shadow-state emulation) to the Linux desktop using a modern, zero-blocking asynchronous engine. It exposes a session-bound DBus API for frontends (GUIs, CLIs, or scripted hot-reloaders) to observe and dynamically remap your keyboard layout in real-time.

## Core Philosophy

- **VIA-First & Zero-State:** `clackd` treats the hardware as the ultimate source of truth. By default, the daemon caches absolutely no keymap state. Every read forces a direct 32-byte `hidraw` transaction, ensuring multi-client coherence without complex synchronization. 
- **Unprivileged Execution:** The daemon runs entirely in user-space without root privileges, leveraging standard `udev` rules (`TAG+="uaccess"`) to dynamically acquire session-bound ACLs to `/dev/hidrawX` nodes.
- **Strictly Asynchronous:** Built on `tokio`, the core engine is fully non-blocking. It uses `tokio::io::unix::AsyncFd` (avoiding `tokio::fs` entirely) to poll USB interrupt-in endpoints, ensuring scaling across multiple active devices without stalling the runtime blocking pool.
- **Legacy Polyfilling:** Support for proprietary protocols (Logitech, Razer) is achieved via "Shadow State" drivers. The core engine is kept completely agnostic and believes it is talking to standard VIA hardware, while the driver polyfill transparently emulates the VIA protocol, debounces writes, and compiles vendor-specific binary blobs.
- **Crash Isolation:** The hardware-abstraction layer operates under strict error boundaries. If a device drops off the bus or its task panics, the supervisor cleanly drops the handle and attempts exponential-backoff reconnection without taking down the daemon or other connected keyboards.

## Architecture

The project is structured into three distinct domains:

1. **HAL (`src/hal`):** The Hardware Abstraction Layer. Defines the strict `KeyboardDriver` contract and the `DriverError` boundary. 
2. **Engine (`src/engine`):** The device supervisor and messaging bus. Owns the device tasks, safely translates raw hardware errors into `DaemonError`s, and routes `EngineCommand` IPC.
3. **IPC (`src/ipc`):** The `zbus`-powered D-Bus session gateway (`io.github.clackd`).

## Project Status

**Work in Progress.** The daemon is currently being developed in structured architectural missions.
- **Mission 1 (Completed):** Foundation, Error Boundaries, and HAL Contract.
- **Mission 2 (Upcoming):** Engine supervisor, device topology structures, and worker lifecycles.
- **Mission 3 (Upcoming):** DBus session API integration, and native `hidraw` driver implementations.

---
*License: GPL-3.0-or-later*
