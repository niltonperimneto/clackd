// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Topology — dimensional metadata for a connected keyboard.
//!
//! # Scope
//!
//! These types describe the *shape* of a keyboard's keymap address space —
//! the matrix dimensions and the count of distinct layers — not the
//! keycodes themselves. The VIA-first invariant (CLAUDE.md §1.1) forbids
//! caching keycode values on the host; every read is a fresh hardware
//! round-trip via [`crate::hal::KeyboardDriver::get_keycode`].
//!
//! Dimensions, however, are stable for the device's session: the firmware
//! does not change its matrix size at runtime, so the engine queries them
//! once at attach time and caches the result for the worker's lifetime.
//! This is the cache-once-on-attach pattern referenced in the trait docs
//! for [`crate::hal::KeyboardDriver::get_matrix_dimensions`].
//!
//! # Out of Scope
//!
//! `Profile` (a user-defined logical grouping of layers and bindings) is
//! intentionally absent until a concrete consumer demands it. Adding it
//! speculatively would conflict with the project's "no premature
//! abstraction" guideline.

/// Physical key-matrix dimensions reported by the device firmware.
///
/// **Context:** Returned (encoded as the `(u8, u8)` tuple) by
/// [`crate::hal::KeyboardDriver::get_matrix_dimensions`]; wrapped here in a
/// named struct so the engine and IPC layers handle it without losing
/// field semantics. The dimensions define the valid bounds for every
/// `(row, col)` pair the engine will accept on
/// [`crate::engine::messages::EngineCommand::GetKeycode`] and
/// [`crate::engine::messages::EngineCommand::SetKeycode`].
///
/// **Invariant:** `rows > 0 && cols > 0`. A zero-dimensioned matrix is a
/// protocol violation — drivers must surface
/// [`crate::hal::DriverError::ProtocolViolation`] rather than constructing
/// an empty `KeyMatrix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyMatrix {
    /// Number of matrix rows reported by the firmware.
    pub rows: u8,
    /// Number of matrix columns reported by the firmware.
    pub cols: u8,
}

impl KeyMatrix {
    /// Returns `true` when `(row, col)` lies inside this matrix.
    ///
    /// **Context:** Used by the engine supervisor to bounds-check incoming
    /// keymap operations before dispatching them to the worker task. This
    /// keeps a malformed request from consuming a hidraw round-trip.
    /// Cancel-safe and allocation-free.
    #[inline]
    #[must_use]
    pub fn contains(self, row: u8, col: u8) -> bool {
        row < self.rows && col < self.cols
    }
}

/// Stable, session-long metadata for a connected keyboard.
///
/// **Context:** Constructed once when the engine spawns a worker task — the
/// supervisor calls [`crate::hal::KeyboardDriver::get_matrix_dimensions`]
/// and [`crate::hal::KeyboardDriver::get_layer_count`] back-to-back, then
/// stores the result alongside the worker's `mpsc::Sender<WorkerOp>` in
/// the registry. The engine consults it for bounds checks; the IPC layer
/// (Mission 3) exposes it on D-Bus as a property.
///
/// **Refresh policy:** Never. If the firmware reconfigures its topology
/// mid-session — which VIA does not support — the only correct response
/// is to treat it as a fresh attach: tear down the worker, evict the
/// registry entry, re-enumerate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceTopology {
    /// Matrix dimensions.
    pub matrix: KeyMatrix,
    /// Number of distinct keymap layers the firmware exposes.
    pub layer_count: u8,
}

impl DeviceTopology {
    /// Returns `true` when `(layer, row, col)` is addressable on this device.
    ///
    /// **Context:** Single bounds check covering both the matrix rectangle
    /// and the layer count. Used at the supervisor's keymap-command
    /// dispatch site; an out-of-range triple is rejected with
    /// [`crate::engine::DaemonError::DbusRouting`] without ever reaching
    /// the worker task. Cancel-safe and allocation-free.
    #[inline]
    #[must_use]
    pub fn contains(self, layer: u8, row: u8, col: u8) -> bool {
        layer < self.layer_count && self.matrix.contains(row, col)
    }
}
