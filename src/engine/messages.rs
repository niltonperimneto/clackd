#![allow(dead_code)]
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! `EngineCommand` — the single typed channel between IPC and the engine.
//!
//! # Routing Model
//!
//! Every interaction the daemon performs flows through one of these variants
//! traveling on a single `tokio::sync::mpsc::Sender<EngineCommand>` (capacity
//! bounded — see [`crate::engine::ENGINE_CHANNEL_CAPACITY`]). The engine
//! supervisor demultiplexes on receipt: keymap operations are forwarded to
//! the per-device worker that owns the relevant
//! [`crate::hal::KeyboardDriver`]; lifecycle events mutate the
//! [`crate::engine::DeviceRegistry`].
//!
//! # Reply Discipline
//!
//! Reply channels are `oneshot::Sender<Result<_, DaemonError>>`. The error
//! type is always [`crate::engine::DaemonError`]; raw
//! [`crate::hal::DriverError`] never travels on these channels (CLAUDE.md
//! §3.3). The worker task that owns the driver is responsible for the
//! `DriverError -> DaemonError` conversion via `?` and the `#[from]` impl
//! on [`crate::engine::DaemonError::Driver`].
//!
//! # Allocation Note
//!
//! `device_id: String` allocates on each command. This is acceptable here
//! because command dispatch is not on the per-keystroke hot path — only on
//! IPC requests, which are inherently coarse-grained. The HID hot path
//! (worker `read`/`write` of the hidraw `[u8; 32]` frame) remains
//! zero-allocation per SKILLS.md §3.1.

use std::path::PathBuf;

use tokio::sync::oneshot;

use crate::engine::DaemonError;

/// The complete set of operations the engine accepts.
///
/// **Context:** Variants are split into two families: *keymap operations*
/// (`GetKeycode`, `SetKeycode`) that target a specific device worker and
/// carry a `oneshot` reply channel, and *lifecycle events*
/// (`DeviceConnected`, `DeviceDisconnected`, `DeviceError`) that the
/// supervisor handles in-place by mutating the registry. Lifecycle events
/// carry no reply because they are notifications, not requests.
///
/// **Senders:** IPC handlers (Mission 3) send keymap operations; the
/// udev supervisor in `main.rs` sends `DeviceConnected`/`Disconnected`;
/// device worker tasks send `DeviceError` when they terminate abnormally
/// so the supervisor can evict the stale registry entry without waiting
/// for a `JoinHandle` poll.
///
/// **Send-error policy:** A failed `send` on the engine channel must be
/// surfaced as [`DaemonError::EngineSend`] by the caller — the variant
/// captures the dropped command so the caller can retry, log it, or
/// surface a `oneshot::Sender::send` failure to its own client.
#[derive(Debug)]
pub(crate) enum EngineCommand {
    // --- Keymap Operations ---
    /// Read a single keycode from a specific `(layer, row, col)` slot.
    ///
    /// **Context:** Forwarded to the per-device worker for execution. On VIA
    /// devices this is a fresh hidraw round-trip; on legacy devices it
    /// resolves from the shadow cache (CLAUDE.md §4.2).
    ///
    /// **Cancel-safety:** If the IPC caller drops the reply receiver before
    /// the worker has responded, the `oneshot::Sender::send` inside the
    /// worker silently fails — that is the documented mechanism by which
    /// the worker learns the request was cancelled.
    GetKeycode {
        /// Stable engine-side identifier for the target device.
        device_id: String,
        /// Zero-indexed layer.
        layer: u8,
        /// Zero-indexed matrix row.
        row: u8,
        /// Zero-indexed matrix column.
        col: u8,
        /// Reply channel; receives the worker's `DaemonError`-typed result.
        reply: oneshot::Sender<Result<u16, DaemonError>>,
    },

    /// Write a keycode into a specific `(layer, row, col)` slot.
    ///
    /// **Hardware Quirk (EEPROM):** On the VIA unbuffered path this
    /// triggers an immediate flash-cell program; the reference ATmega32U4
    /// part is rated for ~100,000 write cycles per cell. Frontend
    /// debouncing is the preferred mitigation. See
    /// [`crate::hal`] module docs.
    SetKeycode {
        /// Stable engine-side identifier for the target device.
        device_id: String,
        /// Zero-indexed layer.
        layer: u8,
        /// Zero-indexed matrix row.
        row: u8,
        /// Zero-indexed matrix column.
        col: u8,
        /// New keycode for the slot.
        keycode: u16,
        /// Reply channel; receives the worker's `DaemonError`-typed result.
        reply: oneshot::Sender<Result<(), DaemonError>>,
    },

    // --- Device Lifecycle ---
    /// A new hidraw node has been published by udev.
    ///
    /// **Context:** Sent by the udev supervisor in `main.rs`. The engine
    /// supervisor looks up a driver for `(vendor_id, product_id)` and, on
    /// match, spawns a dedicated worker task that owns the
    /// `Box<dyn KeyboardDriver>`. Devices with no matching driver are
    /// logged and ignored — they are not an error.
    DeviceConnected {
        /// Stable engine-side identifier for the device. Derived by the
        /// supervisor (typically `{vid:04x}:{pid:04x}:{serial}`) and
        /// guaranteed unique within the registry.
        device_id: String,
        /// USB vendor ID, used for driver selection.
        vendor_id: u16,
        /// USB product ID, used for driver selection.
        product_id: u16,
        /// Absolute path to the `/dev/hidrawX` node.
        hidraw_path: PathBuf,
    },

    /// The hidraw node has been removed by udev.
    ///
    /// **Context:** The supervisor drops the device's registry entry, which
    /// closes the per-device `mpsc::Receiver<WorkerOp>` and causes the
    /// worker task to terminate cleanly.
    DeviceDisconnected {
        /// Identifier of the device that vanished.
        device_id: String,
    },

    /// A worker task reports a terminal failure.
    ///
    /// **Context:** Sent by a worker just before it exits its loop — for
    /// example, after observing [`crate::hal::DriverError::Disconnected`]
    /// or [`crate::hal::DriverError::PermissionDenied`]. The supervisor
    /// evicts the registry entry immediately rather than waiting on the
    /// `JoinHandle`. This avoids the race where a fresh IPC request lands
    /// on a dead worker between its loop exit and the `JoinHandle`
    /// settling.
    DeviceError {
        /// Identifier of the device whose worker failed.
        device_id: String,
        /// The failure observed by the worker.
        error: Box<DaemonError>,
    },

    /// Internal reconnection signaling — not part of the public API.
    /// Uses `pub(crate)` `DeviceHandle` which is consistent since
    /// `EngineCommand` itself is only constructed within the crate.
    DeviceReattached {
        device_id: String,
        handle: crate::engine::DeviceHandle,
        token: u64,
    },

    // --- Introspection (CLI / clackctl) ---
    /// Enumerate the IDs of all currently-registered devices.
    ///
    /// **Context:** Answered directly from the [`crate::engine::DeviceRegistry`]
    /// keys without touching any worker — it is registry metadata, not a
    /// hardware query. Drives `clackctl list`.
    ListDevices {
        /// Reply channel; receives the sorted set of registered device IDs.
        reply: oneshot::Sender<Vec<String>>,
    },

    /// Fetch the cached `(rows, cols, layer_count)` topology for a device.
    ///
    /// **Context:** Topology is cached on the registry entry at attach time
    /// (the cache-once-on-attach pattern), so this is answered without a
    /// hardware round-trip. Drives `clackctl info`.
    GetDeviceInfo {
        /// Stable engine-side identifier for the target device.
        device_id: String,
        /// Reply channel; `(rows, cols, layer_count)` on success.
        reply: oneshot::Sender<Result<(u8, u8, u8), DaemonError>>,
    },

    /// Fetch the USB identity `(vendor_id, product_id, product_name)` of a device.
    ///
    /// **Context:** VID/PID are cached on the registry entry at attach time
    /// (from the `DeviceConnected` event), so this is answered without a
    /// hardware round-trip. It lets frontends match a device against an
    /// external layout definition (e.g. a VIA/KLE definition keyed on
    /// `vid:pid`). `product_name` is currently a best-effort string and may be
    /// empty until a udev-sourced model name is plumbed through.
    GetDeviceIdentity {
        /// Stable engine-side identifier for the target device.
        device_id: String,
        /// Reply channel; `(vendor_id, product_id, product_name)` on success.
        reply: oneshot::Sender<Result<(u16, u16, String), DaemonError>>,
    },

    /// Force a commit / NVRAM flush for a device.
    ///
    /// **Context:** Routed to the device worker, which calls
    /// [`crate::hal::KeyboardDriver::commit_to_nvram`]. No-op for unbuffered
    /// VIA; flushes the buffer for coalescing VIA; compiles+pushes the blob
    /// for legacy backends. Drives `clackctl commit`.
    CommitToNvram {
        /// Stable engine-side identifier for the target device.
        device_id: String,
        /// Reply channel; receives the worker's `DaemonError`-typed result.
        reply: oneshot::Sender<Result<(), DaemonError>>,
    },

    // --- Extended Configuration ---
    GetMacro {
        device_id: String,
        offset: u16,
        length: u8,
        reply: oneshot::Sender<Result<Vec<u8>, DaemonError>>,
    },
    SetMacro {
        device_id: String,
        offset: u16,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<(), DaemonError>>,
    },
    GetLighting {
        device_id: String,
        channel: u8,
        value_id: u8,
        reply: oneshot::Sender<Result<Vec<u8>, DaemonError>>,
    },
    SetLighting {
        device_id: String,
        channel: u8,
        value_id: u8,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<(), DaemonError>>,
    },

    // --- Engine Lifecycle ---
    /// Force the engine supervisor to gracefully close all workers and exit.
    Shutdown,
}
