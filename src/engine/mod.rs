// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Engine — device registry, worker supervision, and the daemon-wide error type.
//!
//! # Boundary
//!
//! The engine module is the conversion seam between the hardware layer
//! ([`crate::hal`]) and the IPC frontend (`src/ipc`). Per CLAUDE.md §3.3, the
//! engine and the D-Bus layer **must never observe** [`crate::hal::DriverError`]
//! directly. Driver errors are wrapped into [`DaemonError`] inside the device
//! worker task — the same task that owns the [`crate::hal::KeyboardDriver`]
//! instance — and the wrapped form is what travels back over the request's
//! `oneshot::Sender`. A second conversion lives at the D-Bus boundary
//! (`impl From<DaemonError> for zbus::fdo::Error` in `src/ipc/frontend.rs`,
//! Mission 3) and is responsible for mapping daemon errors to the matching
//! `org.freedesktop.DBus.Error.*` codes.
//!
//! # Mission 1 Scope
//!
//! This file currently exposes [`DaemonError`] only. The registry struct,
//! the supervisor's worker-spawn logic, the `EngineCommand` dispatcher, and
//! the per-device reconnection backoff arrive in Mission 2 alongside
//! `engine/messages.rs` and `engine/topology.rs`.

use thiserror::Error;

use crate::hal::DriverError;

/// Errors observed at and above the engine boundary.
///
/// **Context:** This is the daemon-wide unified error type. It encapsulates
/// hardware failures (`DriverError`), inter-task messaging errors, and
/// high-level policy rejections, making it safe for the DBus frontend to consume.
///
/// **Variant discipline:** Every variant carries enough identifying context
/// (`device_id`, op name, or the underlying transport error) that the
/// supervisor can route or log the failure without re-querying the worker
/// task that produced it. This matters because by the time `DaemonError`
/// surfaces, the producing task may already have terminated under
/// `DriverError::Disconnected` (SKILLS.md §2.1).
///
/// **Forbidden conversions:** Do **not** add a blanket `From<std::io::Error>`
/// or `From<anyhow::Error>` impl on this type. All raw I/O errors must
/// travel through [`DriverError`] first so the boundary in CLAUDE.md §3.3
/// stays auditable.
#[derive(Debug, Error)]
pub enum DaemonError {
    /// A driver method returned an error.
    ///
    /// **Context:** This is the mandated conversion point per CLAUDE.md §3.3.
    /// The worker task invoking [`crate::hal::KeyboardDriver`] applies `?` to
    /// the driver result; the `#[from]` impl below promotes the
    /// [`DriverError`] into a `DaemonError` before it crosses the
    /// `oneshot::Sender` reply channel.
    #[error("driver error: {0}")]
    Driver(#[from] DriverError),

    /// No device is registered under the supplied identifier.
    ///
    /// **Context:** Raised by the engine's dispatch when an `EngineCommand`
    /// names a `device_id` that the registry has no worker for — typically
    /// because the device has been unplugged between the IPC client's last
    /// observation and the current request. Maps to
    /// `org.freedesktop.DBus.Error.UnknownObject` at the IPC boundary.
    #[error("no device registered with id `{device_id}`")]
    DeviceNotFound {
        /// The stable device identifier (see `engine/topology.rs`, Mission 2)
        /// supplied by the caller.
        device_id: String,
    },

    /// Policy rejected the operation on this device.
    ///
    /// **Context:** Distinct from [`DriverError::PermissionDenied`], which
    /// reports a raw kernel `EACCES` on the hidraw node. `DeviceUnauthorized`
    /// is the engine-level rejection after a policy lookup — e.g. a future
    /// polkit check (CLAUDE.md §6) deciding that this session may enumerate
    /// the device but not mutate its keymap. Maps to
    /// `org.freedesktop.DBus.Error.AccessDenied`.
    #[error("session not authorized for device `{device_id}`")]
    DeviceUnauthorized {
        /// The device the caller is not allowed to touch.
        device_id: String,
    },

    /// The device's worker task panicked.
    ///
    /// **Context:** Raised when the supervisor's `JoinHandle` for a worker
    /// resolves to `Err(JoinError)` with a panic payload (CLAUDE.md §1.4 +
    /// §3.3). The supervisor is responsible for catching this, dropping the
    /// stale registry entry, and attempting a fresh attach via the
    /// exponential-backoff path (SKILLS.md §2.2). This variant exists so
    /// any in-flight `oneshot` reply that races the panic gets a typed
    /// error rather than a dropped channel.
    #[error("worker task for device `{device_id}` panicked")]
    DeviceTaskPanicked {
        /// Identifier of the device whose worker died.
        device_id: String,
    },

    /// Failed to enqueue a command into the engine.
    ///
    /// **Context:** Raised when the IPC frontend's `mpsc::Sender::send` on
    /// the engine command channel fails because the receiver was dropped —
    /// i.e. the engine has shut down. Capacity-induced backpressure surfaces
    /// instead as `try_send` returning `Full`, which the IPC layer must
    /// handle directly (SKILLS.md §1.2); it does not reach this variant.
    ///
    /// **TODO(mission-2):** The wrapped payload should be
    /// `tokio::sync::mpsc::error::SendError<EngineCommand>` once
    /// `engine/messages.rs` lands. Until that module exists, declaring the
    /// concrete generic here would create a forward dependency on a missing
    /// type; we carry the diagnostic detail as a `String` instead.
    #[error("failed to dispatch command to engine: {0}")]
    EngineSend(String),

    /// A reply channel was closed before the engine sent the response.
    ///
    /// **Context:** The matched failure for a worker that drops its
    /// `oneshot::Sender` without sending — typically because the worker
    /// terminated mid-command under [`DriverError::Disconnected`]. The IPC
    /// caller should treat this as equivalent to a transport failure and
    /// surface `org.freedesktop.DBus.Error.Failed`.
    #[error("engine reply channel was closed: {0}")]
    ReplyDropped(#[from] tokio::sync::oneshot::error::RecvError),

    /// An engine-level deadline elapsed.
    ///
    /// **Context:** Wraps `tokio::time::error::Elapsed` for timeouts that
    /// the engine imposes *on top of* per-driver deadlines — for example,
    /// a coarse-grained 5 s budget for a remap operation that may consist
    /// of dozens of driver round-trips. The per-call hidraw timeout
    /// surfaces as [`DriverError::Timeout`] instead and reaches this type
    /// via [`DaemonError::Driver`].
    #[error("engine-level deadline elapsed: {0}")]
    Elapsed(#[from] tokio::time::error::Elapsed),

    /// IPC routing failure that the engine wants the D-Bus layer to translate.
    ///
    /// **Context:** A stash variant for errors the engine detects but that
    /// are semantically D-Bus-shaped (malformed object path, unknown method
    /// suffix, etc.). The IPC frontend's `From<DaemonError>` mapping is the
    /// canonical place where the human-readable detail becomes an
    /// `org.freedesktop.DBus.Error.*` code.
    #[error("dbus routing error: {0}")]
    DbusRouting(String),
}
