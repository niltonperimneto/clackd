// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Hardware Abstraction Layer — VIA-first keyboard driver contract.
//!
//! # The VIA-First Query Paradigm
//!
//! `clackd` treats the device as the sole source of truth for keymap state.
//! Every keycode read is a fresh 32-byte raw-HID round-trip on Usage Page
//! `0xFF60` to the firmware's dynamic-keymap endpoint; the host caches
//! nothing. This is the load-bearing invariant of the daemon (CLAUDE.md
//! §1.1) and is what lets multiple unrelated frontends — a GUI, a CLI, and a
//! scripted hot-reload — observe a coherent view of the keyboard without a
//! cross-process synchronization protocol.
//!
//! There is exactly **one** sanctioned deviation: the opt-in write-coalescing
//! buffer in the VIA driver (`src/hal/via.rs`, see CLAUDE.md §4.1). It is OFF
//! by default, must be explicitly enabled through configuration
//! (`via.coalesce_writes = true`), and must log an INFO-level activation
//! notice. When the buffer is engaged, [`KeyboardDriver::get_keycode`] must
//! consult the buffer before falling through to hardware and
//! [`KeyboardDriver::commit_to_nvram`] must flush it. Every other backend
//! treats `commit_to_nvram` as a no-op or a vendor-blob push; see the
//! per-method docs below.
//!
//! # The EEPROM Wear-Leveling Constraint
//!
//! Vanilla QMK/VIA persists `set_keycode` writes directly to the
//! microcontroller's EEPROM via `eeprom_update_byte`; there is no firmware
//! staging buffer. The reference ATmega32U4 part is rated for roughly
//! **100,000 write cycles per cell**. A naïve frontend that emits a
//! `set_keycode` for every step of a slider drag will measurably shorten the
//! device's service life.
//!
//! Two mitigations are admissible and rank in this order:
//!
//! 1. **Frontend debouncing (preferred default).** Well-behaved D-Bus clients
//!    coalesce rapid changes locally and only invoke `set_keycode` once user
//!    input has settled. This preserves the VIA-first invariant strictly.
//! 2. **Daemon-side write coalescing (opt-in deviation).** The VIA driver
//!    may, under configuration, hold a `(layer, row, col) -> keycode` buffer
//!    for a bounded interval (default 250 ms) before pushing the consolidated
//!    write set to hardware.
//!
//! Drivers that emulate VIA over proprietary protocols (Logitech, Razer)
//! shadow the entire keymap in `$XDG_DATA_HOME/clackd/<device_id>.json` and
//! push a compiled vendor blob on commit (CLAUDE.md §4.2); their wear
//! profile is governed by the debounced compile-and-push, not by individual
//! `set_keycode` calls.
//!
//! # Module Layout
//!
//! This file defines the shared contract only — the [`KeyboardDriver`] trait
//! and the [`DriverError`] type. Concrete backends live in sibling modules:
//!
//! - `via.rs` — QMK/VIA native raw-HID implementation.
//! - `legacy/` — shadow-state polyfill drivers for non-VIA hardware.

use std::path::PathBuf;

use async_trait::async_trait;
use thiserror::Error;

/// Errors raised by [`KeyboardDriver`] implementations.
///
/// **Context:** This enum represents the lowest-level failure modes of the
/// hardware transport. It is exclusively yielded by the driver trait and must
/// be converted into a `DaemonError` by the worker task before routing.
///
/// **Boundary:** `DriverError` is the *only* error type that escapes a
/// `KeyboardDriver` method. The device worker task that drives the driver is
/// responsible for converting `DriverError` into
/// [`crate::engine::DaemonError`] via the `From` impl declared on the engine
/// side before the result crosses an `mpsc`/`oneshot` channel. The engine
/// and IPC layers must never observe `DriverError` directly (CLAUDE.md §3.3).
///
/// **Variant discipline:** Each variant below carries enough context for the
/// supervisor to make a routing decision without re-querying the driver.
/// Specifically: `Disconnected` and `PermissionDenied` are the two variants
/// that retire the worker task; the others propagate to the caller as a
/// reply on the request's `oneshot::Sender`.
#[derive(Debug, Error)]
pub enum DriverError {
    /// Underlying syscall failed.
    ///
    /// **Context:** This is the catch-all for `read(2)` / `write(2)` /
    /// `ioctl(2)` failures against `/dev/hidrawX` that don't fit a more
    /// specific variant below. The worker should inspect
    /// [`std::io::Error::kind`] before deciding whether to retry.
    ///
    /// **Hardware Quirk:** `EIO` from a hidraw node usually means the device
    /// NAK'd or stalled the interrupt-in transfer, not that the host is
    /// broken; treat it as transient and let the per-call `tokio::time::timeout`
    /// drive the eventual surfacing of [`DriverError::Timeout`].
    #[error("hidraw I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The device did not complete an operation within its deadline.
    ///
    /// **Context:** Wraps the `tokio::time::timeout` guard that bounds every
    /// hidraw round-trip (CLAUDE.md §4.1, default 1000 ms for VIA). The
    /// driver translates the inner `Elapsed` into this variant so the
    /// engine can attribute the timeout to a specific device and operation
    /// in its tracing spans.
    #[error("device `{device}` timed out during `{op}`")]
    Timeout {
        /// Stable identifier of the device that stalled.
        device: String,
        /// Operation name (e.g. `"get_keycode"`, `"set_keycode"`).
        op: &'static str,
    },

    /// udev has not granted, or has revoked, access to the hidraw node.
    ///
    /// **Context:** The daemon runs unprivileged and relies on
    /// `TAG+="uaccess"` to receive a session-bound ACL on `/dev/hidrawX`
    /// (CLAUDE.md §6). This variant fires when the initial `open(2)` returns
    /// `EACCES`/`EPERM`, or when `read`/`write` returns the same after a
    /// session change. The supervisor must drop the device handle and refuse
    /// the device rather than retry.
    #[error("permission denied for hidraw node {path:?}")]
    PermissionDenied {
        /// Resolved path that the open syscall rejected.
        path: PathBuf,
    },

    /// The device returned a frame that does not match the VIA protocol.
    ///
    /// **Context:** Surfaces malformed response frames — unexpected report
    /// IDs, wrong length, or a command-echo byte that does not match the
    /// request. Bumping this to ERROR-level in tracing is appropriate; it
    /// usually indicates firmware version skew or a corrupt write.
    #[error("hidraw protocol violation: {reason}")]
    ProtocolViolation {
        /// Static description of what failed validation. Static `&'static str`
        /// keeps this enum allocation-free on the hot path (SKILLS.md §3.1).
        reason: &'static str,
    },

    /// The hidraw node has gone away mid-session.
    ///
    /// **Context:** Translated from `ErrorKind::NotFound` (`ENODEV`) and
    /// `ErrorKind::BrokenPipe` (`EPIPE`) per SKILLS.md §2.1. The worker must
    /// drop its [`tokio::io::unix::AsyncFd`], emit a `DeviceDisconnected`
    /// event upstream, and terminate cleanly — under no circumstances should
    /// this variant propagate as a panic.
    #[error("device disconnected")]
    Disconnected,

    /// A legacy vendor blob failed to compile from the shadow cache.
    ///
    /// **Context:** Specific to the legacy polyfill drivers
    /// (`src/hal/legacy/*`). Raised when the shadow `serde`-loaded layout
    /// cannot be serialized into the proprietary on-the-wire format — for
    /// example, a keycode that has no Logitech equivalent.
    #[error("vendor `{vendor}` blob compile failed: {detail}")]
    VendorCompile {
        /// Vendor tag, e.g. `"logitech"` or `"razer"`.
        vendor: &'static str,
        /// Human-readable detail. Allocates; only constructed on the cold
        /// error path, never in a per-keystroke loop.
        detail: String,
    },
}

/// The granular, VIA-style keyboard driver contract.
///
/// **Context:** Every concrete backend — the native VIA driver and every
/// legacy polyfill — must implement this trait. The trait is the seam at
/// which the engine stops caring whether it is talking to authentic QMK
/// firmware or to a shadow-state emulator for a proprietary vendor stack.
///
/// **Threading model:** `Send + Sync` is required because each driver
/// instance is *moved into* a dedicated Tokio worker task at device-attach
/// time and never shared. The bound exists to satisfy `tokio::spawn`, not
/// to invite shared access; per SKILLS.md §1.1, a driver is owned by one
/// task and addressed exclusively through `EngineCommand` messages.
///
/// **Cancel-safety:** Every method below is cancel-safe at the *message*
/// boundary — dropping the awaiting future from outside is safe and will
/// not corrupt the driver's internal state. It is **not** safe to drop the
/// future from inside the worker task between the request `write` and the
/// reply `read` of a single round-trip; the worker must always drive the
/// pair to completion (or to a timeout) before accepting the next
/// `EngineCommand`.
///
/// **Migration note:** This trait currently boxes each call's return future
/// via [`async_trait`] (heap allocation per call). Once the project MSRV
/// reaches Rust 1.75+, migrate to native `async fn` in traits (RPITIT) to
/// eliminate the allocation. This is on the hot path for bulk remap
/// operations that may issue hundreds of round-trips per second.
#[async_trait]
pub trait KeyboardDriver: Send + Sync {
    /// Reports the physical key-matrix dimensions as `(rows, cols)`.
    ///
    /// **Context:** Called once at device attach; the engine caches the
    /// result for the lifetime of the worker task and uses it to bounds-check
    /// every subsequent `(row, col)` against this rectangle. Cancel-safe.
    ///
    /// **Backend semantics:** VIA queries `id_get_protocol_version` /
    /// `id_get_keyboard_value`; legacy backends return constants baked into
    /// the device-specific module.
    ///
    /// # Errors
    /// - [`DriverError::Io`] / [`DriverError::Timeout`] if the device fails
    ///   to respond to the topology query.
    /// - [`DriverError::ProtocolViolation`] if the response frame is malformed.
    async fn get_matrix_dimensions(&mut self) -> Result<(u8, u8), DriverError>;

    /// Reports the number of distinct keymap layers the firmware exposes.
    ///
    /// **Context:** Cached at attach alongside [`Self::get_matrix_dimensions`].
    /// Cancel-safe.
    ///
    /// # Errors
    /// Same conditions as [`Self::get_matrix_dimensions`].
    async fn get_layer_count(&mut self) -> Result<u8, DriverError>;

    /// Returns the current keycode at `(layer, row, col)`.
    ///
    /// **Context:** Cancel-safe.
    ///
    /// **Backend semantics:**
    /// - VIA (unbuffered, default): one synchronous 32-byte hidraw
    ///   round-trip. No host-side cache is consulted.
    /// - VIA (coalescing on): first checks the write buffer for a pending
    ///   `(layer, row, col)` entry; falls through to a hardware query only
    ///   on miss.
    /// - Legacy: reads from the shadow-state struct loaded out of
    ///   `$XDG_DATA_HOME/clackd/<device_id>.json`. The USB bus is not touched.
    ///
    /// **Hardware Quirk:** The VIA path must use [`tokio::io::unix::AsyncFd`]
    /// over a non-blocking fd, not `tokio::fs::File`; the latter parks
    /// blocking-pool threads on the USB interrupt-in transfer and can deadlock
    /// the runtime under multi-device load (CLAUDE.md §4.1).
    ///
    /// # Arguments
    /// * `layer` — zero-indexed layer; must satisfy `layer < get_layer_count()`.
    /// * `row`, `col` — zero-indexed matrix position; must lie inside the
    ///   rectangle reported by [`Self::get_matrix_dimensions`].
    ///
    /// # Errors
    /// - [`DriverError::Io`] / [`DriverError::Timeout`] / [`DriverError::Disconnected`]
    ///   for transport failures.
    /// - [`DriverError::ProtocolViolation`] if the firmware responds with an
    ///   unexpected report.
    async fn get_keycode(&mut self, layer: u8, row: u8, col: u8) -> Result<u16, DriverError>;

    /// Writes `keycode` to `(layer, row, col)`.
    ///
    /// **Context:** Cancel-safe at the message boundary (see trait-level
    /// doc). The driver must drive the request frame to completion before
    /// accepting the next command, otherwise the firmware command-id state
    /// machine desynchronizes.
    ///
    /// **Backend semantics:**
    /// - VIA (unbuffered, default): writes are *immediately* persistent
    ///   on issue; the firmware calls `eeprom_update_byte` synchronously.
    ///   See the EEPROM wear note at the module level — high-frequency
    ///   callers must debounce upstream.
    /// - VIA (coalescing on): updates the `(layer, row, col)` slot in the
    ///   in-memory buffer and arms a flush timer (default 250 ms). The
    ///   actual hidraw write is deferred to [`Self::commit_to_nvram`] or
    ///   to the timer firing.
    /// - Legacy: updates the shadow struct, flags it dirty, and arms the
    ///   500 ms debounce timer described in CLAUDE.md §4.2. The compiled
    ///   blob is pushed only when the timer expires undisturbed.
    ///
    /// **Hardware Quirk (EEPROM):** Reproduced from the module doc — the
    /// reference ATmega32U4 part is rated for ~100,000 write cycles per
    /// cell. Treat this method as wear-inducing on the VIA path.
    ///
    /// # Errors
    /// - [`DriverError::Io`] / [`DriverError::Timeout`] / [`DriverError::Disconnected`]
    ///   for transport failures on the unbuffered path or during the
    ///   deferred flush.
    /// - [`DriverError::VendorCompile`] on legacy backends if the dirty
    ///   shadow state cannot be serialized.
    async fn set_keycode(
        &mut self,
        layer: u8,
        row: u8,
        col: u8,
        keycode: u16,
    ) -> Result<(), DriverError>;

    /// Forces durability of any pending writes.
    ///
    /// **Context:** Cancel-safe. Callers should treat this as best-effort
    /// fsync — success means the driver has handed the consolidated write
    /// to the device, not that the device has acknowledged a flash-page
    /// program.
    ///
    /// **Backend semantics (load-bearing — do not collapse the cases):**
    /// - VIA, unbuffered (default): **no-op**. Returns `Ok(())` immediately.
    ///   Writes were already persistent on issue.
    /// - VIA, coalescing on: flushes every buffered `(layer, row, col)`
    ///   entry to hardware via the same `set_keycode` round-trips that an
    ///   unbuffered driver would have issued, then clears the buffer.
    /// - Legacy: compiles the shadow struct into the vendor-specific binary
    ///   blob and pushes it monolithically to the device, bypassing the
    ///   500 ms debounce timer.
    ///
    /// # Errors
    /// - [`DriverError::Io`] / [`DriverError::Timeout`] / [`DriverError::Disconnected`]
    ///   from the underlying hidraw operations.
    /// - [`DriverError::VendorCompile`] on legacy backends if blob
    ///   compilation fails. On failure, legacy drivers must roll the shadow
    ///   cache back to its last-known-good state per CLAUDE.md §4.2.
    async fn commit_to_nvram(&mut self) -> Result<(), DriverError>;
}
