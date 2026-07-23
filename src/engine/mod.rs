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
//! # Scope
//!
//! This module declares the daemon-wide [`DaemonError`] type, the
//! [`DeviceRegistry`], and the supervisor entry point [`run_supervisor`].
//! Submodules: [`messages`] hosts the wire-level
//! [`messages::EngineCommand`] enum; [`topology`] holds the dimensional
//! metadata cached at device-attach time.
//!
//! Driver selection ([`select_driver`]) is currently a stub returning
//! `None` for every `(vendor_id, product_id)`. Mission 3 plugs in the
//! concrete VIA and legacy backends; until then the supervisor will log
//! every `DeviceConnected` event at INFO and decline to attach a worker.
//!
//! # Reconnection Policy
//!
//! Per CLAUDE.md §1.4, the supervisor "must attempt reconnection with
//! exponential backoff before declaring a device permanently offline".
//! For Mission 2, fatal worker errors evict immediately — the reconnection
//! loop is a TODO(mission-3) item that lands alongside the first concrete
//! driver, when "transient bus error" gains an operational definition.
//! Physical re-plug events are already handled correctly: udev emits a
//! fresh `DeviceConnected`, and the supervisor attaches a new worker.

use std::collections::HashMap;
use std::path::Path;

use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::hal::{DriverError, KeyboardDriver};
use messages::EngineCommand;
use topology::DeviceTopology;

pub mod messages;
pub mod topology;

/// Out-of-band lifecycle event for the IPC layer to translate into D-Bus signals.
///
/// **Context:** Distinct from [`EngineCommand`] — `EngineCommand` is requests
/// **into** the engine; `LifecycleEvent` is fan-out **from** the engine.
/// Carried on a `tokio::sync::broadcast` channel because the IPC layer (and,
/// in the future, observability shims) may want to subscribe in parallel.
/// A lagged or absent receiver does not block the supervisor — the send is
/// best-effort.
///
/// **Variant emission points:**
/// - [`Self::DeviceAdded`]: after a successful `attach_device`, both on a
///   fresh `DeviceConnected` and on a successful reconnection.
/// - [`Self::DeviceRemoved`]: after the supervisor evicts a registry entry
///   on `DeviceDisconnected` or terminal `DeviceError`.
/// - [`Self::LayoutUpdated`]: after a successful `set_keycode` round-trip
///   inside the per-device worker. For the unbuffered VIA path this fires
///   on every write; for the coalescing path it fires only on flush; for
///   legacy backends it fires only after the debounced blob push completes.
#[derive(Clone, Debug)]
pub enum LifecycleEvent {
    /// A device worker has been attached and is ready for keymap commands.
    DeviceAdded(String),
    /// A device worker has been evicted from the registry.
    DeviceRemoved(String),
    /// A device's keymap has changed in a frontend-observable way.
    LayoutUpdated(String),
}

/// Default capacity of the lifecycle broadcast channel.
///
/// **Context:** Sized to absorb a burst of bulk-remap operations (e.g.
/// loading a saved profile) without lagging slow subscribers. A lagged
/// receiver sees a `RecvError::Lagged(skipped)` and resubscribes; this
/// is fine for signal emission because the IPC layer can also republish
/// the device's full state on lag.
pub const LIFECYCLE_CHANNEL_CAPACITY: usize = 64;

/// Default capacity of the engine command channel.
///
/// **Context:** Bounded per SKILLS.md §1.2; unbounded channels are forbidden.
/// The IPC frontend (Mission 3) constructs the channel with this capacity
/// and passes the receiver into [`run_supervisor`].
pub const ENGINE_CHANNEL_CAPACITY: usize = 32;

/// Capacity of a single per-device worker command channel.
///
/// **Context:** Smaller than [`ENGINE_CHANNEL_CAPACITY`] because the worker
/// is single-threaded and processes commands in strict order; deeper
/// queuing here would mostly add latency, not throughput. A saturated
/// worker channel surfaces as [`DaemonError::DbusRouting`] to the caller
/// of the originating [`EngineCommand`].
const WORKER_CHANNEL_CAPACITY: usize = 16;

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
#[allow(dead_code)]
#[derive(Debug, Error)]
pub(crate) enum DaemonError {
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
    /// **Recovered payload:** The wrapped `SendError<EngineCommand>` carries
    /// the dropped command back to the caller. For variants with a `reply`
    /// `oneshot::Sender`, the caller can extract it and surface the failure
    /// to its own client rather than silently swallowing the request.
    #[error("failed to dispatch command to engine: receiver dropped")]
    EngineSend(#[from] tokio::sync::mpsc::error::SendError<EngineCommand>),

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

// ─────────────────────────────────────────────────────────────────────────────
// Registry & supervisor
// ─────────────────────────────────────────────────────────────────────────────

/// Per-device dispatch payload owned by the worker task.
///
/// **Context:** Translated from [`EngineCommand`] by the supervisor; the
/// `device_id` is dropped because the worker is already pinned to a single
/// device. The reply `oneshot::Sender` travels through unchanged so the
/// IPC caller observes the worker's result directly.
#[derive(Debug)]
enum WorkerOp {
    GetKeycode {
        layer: u8,
        row: u8,
        col: u8,
        reply: oneshot::Sender<Result<u16, DaemonError>>,
    },
    SetKeycode {
        layer: u8,
        row: u8,
        col: u8,
        keycode: u16,
        reply: oneshot::Sender<Result<(), DaemonError>>,
    },
    Commit {
        reply: oneshot::Sender<Result<(), DaemonError>>,
    },
    GetMacro {
        offset: u16,
        length: u8,
        reply: oneshot::Sender<Result<Vec<u8>, DaemonError>>,
    },
    SetMacro {
        offset: u16,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<(), DaemonError>>,
    },
    GetLighting {
        channel: u8,
        value_id: u8,
        reply: oneshot::Sender<Result<Vec<u8>, DaemonError>>,
    },
    SetLighting {
        channel: u8,
        value_id: u8,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<(), DaemonError>>,
    },
}

/// Supervisor-side handle to a per-device worker task.
///
/// **Context:** Holds the bounded command channel and the cached topology.
/// The supervisor does **not** retain the worker's `JoinHandle` — abnormal
/// termination is signalled out-of-band via
/// [`EngineCommand::DeviceError`], and channel-closed detection on
/// `try_send` covers the race where a worker dies between command dispatch
/// and the next supervisor tick.
#[derive(Debug, Clone)]
pub(crate) struct DeviceHandle {
    /// Dimensions reported by the device at attach time. Cached for the
    /// session per the cache-once-on-attach pattern documented on
    /// [`crate::hal::KeyboardDriver::get_matrix_dimensions`].
    topology: DeviceTopology,
    /// Human-readable model name from the driver, cached at attach time
    /// alongside the topology (see [`crate::hal::KeyboardDriver::model_name`]).
    model: String,
    /// Bounded sender into the worker's command loop.
    tx: mpsc::Sender<WorkerOp>,
    vendor_id: u16,
    product_id: u16,
    hidraw_path: std::path::PathBuf,
}

/// The set of currently-attached devices keyed by stable `device_id`.
///
/// **Context:** Owned solely by [`run_supervisor`]. Never wrapped in
/// `Arc<Mutex>` or shared across tasks — per SKILLS.md §1.1, state belongs
/// to one task and is addressed exclusively via [`EngineCommand`] messages.
#[derive(Default)]
pub struct DeviceRegistry {
    devices: HashMap<String, DeviceHandle>,
    reconnecting: HashMap<String, (oneshot::Sender<()>, u64)>,
    next_reconnect_token: u64,
}

impl DeviceRegistry {
    /// Constructs an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of currently-attached devices.
    #[must_use]
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Returns `true` when no devices are registered.
    #[allow(dead_code)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

/// Initial backoff delay for device reconnection.
///
/// **Context:** 100 ms allows immediate recovery for minor bus glitches
/// without hammering the CPU or kernel USB subsystem.
const RECONNECT_INITIAL_BACKOFF_MS: u64 = 100;

/// Maximum backoff delay between reconnection attempts.
///
/// **Context:** Capped at 5 seconds. If a device has not recovered after
/// backoff has reached 5 seconds, it is either physically disconnected but
/// udev missed the event, or the firmware has crashed and requires a cold boot.
const RECONNECT_MAX_BACKOFF_MS: u64 = 5000;

/// Maximum number of reconnection attempts before giving up.
///
/// **Context:** 8 attempts with exponential backoff capping at 5s yields a
/// total retry window of roughly 25 seconds.
const RECONNECT_MAX_ATTEMPTS: u32 = 8;

pub type DriverFactory = fn(hidraw_path: &Path) -> Result<Box<dyn KeyboardDriver>, DaemonError>;

#[derive(Default)]
pub struct DriverTable {
    pub by_vid_pid: HashMap<(u16, u16), DriverFactory>,
    pub via_fallback: Option<DriverFactory>, // TODO(mission-3): probed for devices claiming VIA Usage Page 0xFF60
}

/// The engine's main loop.
///
/// **Context:** Owns the [`DeviceRegistry`] and drives it from incoming
/// [`EngineCommand`]s. Single-task: `tokio::spawn`ed exactly once from
/// `main.rs`. Cancel-safe at every await point. Terminates cleanly when
/// every `mpsc::Sender<EngineCommand>` clone is dropped — at which point
/// every per-device worker also receives a closed-channel signal and
/// exits on its own.
///
/// **Backpressure:** Both the engine channel
/// ([`ENGINE_CHANNEL_CAPACITY`]) and the per-device worker channels
/// ([`WORKER_CHANNEL_CAPACITY`]) are bounded. A saturated worker channel
/// causes [`mpsc::Sender::try_send`] to return `Full`; the supervisor
/// surfaces this back to the originating caller as
/// [`DaemonError::DbusRouting`] without blocking the engine loop.
///
/// **Driver selection:** Currently a stub. See [`select_driver`].
pub async fn run_supervisor(
    engine_tx: mpsc::Sender<EngineCommand>,
    mut engine_rx: mpsc::Receiver<EngineCommand>,
    driver_table: std::sync::Arc<DriverTable>,
    lifecycle_tx: broadcast::Sender<LifecycleEvent>,
) {
    let mut registry = DeviceRegistry::new();
    info!(capacity = ENGINE_CHANNEL_CAPACITY, "engine supervisor started");
    while let Some(cmd) = engine_rx.recv().await {
        if matches!(cmd, EngineCommand::Shutdown) {
            info!("supervisor received Shutdown command — dropping all device workers");
            registry.devices.clear(); // Drops all tx clones, terminating workers
            break;
        }
        handle_command(cmd, &mut registry, &engine_tx, &driver_table, &lifecycle_tx).await;
    }
    info!(
        residual_workers = registry.len(),
        "engine channel closed — supervisor shutting down"
    );
}

/// Single-iteration dispatch of one [`EngineCommand`].
///
/// **Context:** Split out of [`run_supervisor`] so each variant's handler
/// is reviewable independently and so a future test harness can exercise
/// dispatch logic without owning a runtime task. Cancel-safe.
async fn handle_command(
    cmd: EngineCommand,
    registry: &mut DeviceRegistry,
    engine_tx: &mpsc::Sender<EngineCommand>,
    driver_table: &std::sync::Arc<DriverTable>,
    lifecycle_tx: &broadcast::Sender<LifecycleEvent>,
) {
    match cmd {
        EngineCommand::GetKeycode { device_id, layer, row, col, reply } => {
            dispatch_get(registry, device_id, layer, row, col, reply);
        }
        EngineCommand::SetKeycode { device_id, layer, row, col, keycode, reply } => {
            dispatch_set(registry, device_id, layer, row, col, keycode, reply);
        }
        EngineCommand::GetMacro { device_id, offset, length, reply } => {
            dispatch_macro_get(registry, device_id, offset, length, reply);
        }
        EngineCommand::SetMacro { device_id, offset, data, reply } => {
            dispatch_macro_set(registry, device_id, offset, data, reply);
        }
        EngineCommand::GetLighting { device_id, channel, value_id, reply } => {
            dispatch_lighting_get(registry, device_id, channel, value_id, reply);
        }
        EngineCommand::SetLighting { device_id, channel, value_id, data, reply } => {
            dispatch_lighting_set(registry, device_id, channel, value_id, data, reply);
        }
        EngineCommand::DeviceConnected { device_id, vendor_id, product_id, hidraw_path } => {
            registry.reconnecting.remove(&device_id);
            if registry.devices.contains_key(&device_id) {
                warn!(device_id, "duplicate DeviceConnected event — ignoring");
                return;
            }
            let factory_opt = driver_table.by_vid_pid.get(&(vendor_id, product_id)).copied()
                .or(driver_table.via_fallback);
            let Some(factory) = factory_opt else {
                info!(
                    device_id,
                    "no driver for vid={vendor_id:04x} pid={product_id:04x} — skipping attach",
                );
                return;
            };
            match factory(&hidraw_path) {
                Ok(driver) => {
                    match attach_device(
                        device_id.clone(),
                        driver,
                        vendor_id,
                        product_id,
                        hidraw_path.clone(),
                        engine_tx.clone(),
                        lifecycle_tx.clone(),
                    )
                    .await
                    {
                        Ok(handle) => {
                            info!(
                                device_id,
                                rows = handle.topology.matrix.rows,
                                cols = handle.topology.matrix.cols,
                                layers = handle.topology.layer_count,
                                "device attached",
                            );
                            registry.devices.insert(device_id.clone(), handle);
                            let _ = lifecycle_tx.send(LifecycleEvent::DeviceAdded(device_id));
                        }
                        Err(e) => {
                            error!(device_id, error = %e, "device attach failed");
                            if is_transient_attach_error(&e) {
                                start_reconnect_backoff(registry, ReconnectCtx {
                                    device_id,
                                    vendor_id,
                                    product_id,
                                    hidraw_path,
                                    driver_table: driver_table.clone(),
                                    engine_tx: engine_tx.clone(),
                                    lifecycle_tx: lifecycle_tx.clone(),
                                    token: 0,
                                });
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(device_id, error = %e, "driver initialization failed");
                    if is_transient_attach_error(&e) {
                        start_reconnect_backoff(registry, ReconnectCtx {
                            device_id,
                            vendor_id,
                            product_id,
                            hidraw_path,
                            driver_table: driver_table.clone(),
                            engine_tx: engine_tx.clone(),
                            lifecycle_tx: lifecycle_tx.clone(),
                            token: 0,
                        });
                    }
                }
            }
        }
        EngineCommand::DeviceDisconnected { device_id } => {
            if let Some((cancel, _token)) = registry.reconnecting.remove(&device_id) {
                let _ = cancel.send(());
            }
            if registry.devices.remove(&device_id).is_some() {
                info!(device_id, "device detached");
                let _ = lifecycle_tx.send(LifecycleEvent::DeviceRemoved(device_id));
            } else {
                debug!(device_id, "DeviceDisconnected for unregistered device — ignoring");
            }
        }
        EngineCommand::DeviceError { device_id, error: e } => {
            if let Some(handle) = registry.devices.remove(&device_id) {
                error!(device_id, error = %e, "worker reported error — evicting");
                let _ = lifecycle_tx.send(LifecycleEvent::DeviceRemoved(device_id.clone()));
                if let DaemonError::Driver(DriverError::Disconnected) = *e {
                    info!(device_id, "device disconnected, starting reconnection backoff");
                    start_reconnect_backoff(registry, ReconnectCtx {
                        device_id,
                        vendor_id: handle.vendor_id,
                        product_id: handle.product_id,
                        hidraw_path: handle.hidraw_path,
                        driver_table: driver_table.clone(),
                        engine_tx: engine_tx.clone(),
                        lifecycle_tx: lifecycle_tx.clone(),
                        token: 0,
                    });
                }
            }
        }
        EngineCommand::DeviceReattached { device_id, handle, token } => {
            match registry.reconnecting.get(&device_id) {
                Some((_, current_token)) if *current_token == token => {
                    registry.reconnecting.remove(&device_id);
                    info!(
                        device_id,
                        rows = handle.topology.matrix.rows,
                        cols = handle.topology.matrix.cols,
                        layers = handle.topology.layer_count,
                        "device reattached after backoff",
                    );
                    registry.devices.insert(device_id.clone(), handle);
                    let _ = lifecycle_tx.send(LifecycleEvent::DeviceAdded(device_id));
                }
                _ => {
                    debug!(device_id, expected_token = ?registry.reconnecting.get(&device_id).map(|t| t.1), actual_token = token, "DeviceReattached ignored (token mismatch or reconnect cancelled)");
                }
            }
        }
        EngineCommand::ListDevices { reply } => {
            let mut ids: Vec<String> = registry.devices.keys().cloned().collect();
            ids.sort();
            let _ = reply.send(ids);
        }
        EngineCommand::GetDeviceInfo { device_id, reply } => {
            let result = match registry.devices.get(&device_id) {
                Some(handle) => Ok((
                    handle.model.clone(),
                    handle.topology.matrix.rows,
                    handle.topology.matrix.cols,
                    handle.topology.layer_count,
                )),
                None => Err(DaemonError::DeviceNotFound { device_id }),
            };
            let _ = reply.send(result);
        }
        EngineCommand::GetDeviceIdentity { device_id, reply } => {
            let result = match registry.devices.get(&device_id) {
                // product name is not yet captured at discovery; report empty
                // and let the frontend fall back to its own labelling.
                Some(handle) => Ok((handle.vendor_id, handle.product_id, String::new())),
                None => Err(DaemonError::DeviceNotFound { device_id }),
            };
            let _ = reply.send(result);
        }
        EngineCommand::CommitToNvram { device_id, reply } => {
            dispatch_commit(registry, device_id, reply);
        }
        EngineCommand::Shutdown => {
            // Handled by run_supervisor, but we must implement the pattern exhaustively.
        }
    }
}

fn dispatch_get(
    registry: &mut DeviceRegistry,
    device_id: String,
    layer: u8,
    row: u8,
    col: u8,
    reply: oneshot::Sender<Result<u16, DaemonError>>,
) {
    let Some(handle) = registry.devices.get(&device_id) else {
        let _ = reply.send(Err(DaemonError::DeviceNotFound { device_id }));
        return;
    };
    if !handle.topology.contains(layer, row, col) {
        let _ = reply.send(Err(out_of_bounds(&device_id, layer, row, col)));
        return;
    }
    let op = WorkerOp::GetKeycode { layer, row, col, reply };
    if let Err(send_err) = handle.tx.try_send(op) {
        surface_worker_send_failure(send_err, &device_id, registry);
    }
}

fn dispatch_set(
    registry: &mut DeviceRegistry,
    device_id: String,
    layer: u8,
    row: u8,
    col: u8,
    keycode: u16,
    reply: oneshot::Sender<Result<(), DaemonError>>,
) {
    let Some(handle) = registry.devices.get(&device_id) else {
        let _ = reply.send(Err(DaemonError::DeviceNotFound { device_id }));
        return;
    };
    if !handle.topology.contains(layer, row, col) {
        let _ = reply.send(Err(out_of_bounds(&device_id, layer, row, col)));
        return;
    }
    let op = WorkerOp::SetKeycode { layer, row, col, keycode, reply };
    if let Err(send_err) = handle.tx.try_send(op) {
        surface_worker_send_failure(send_err, &device_id, registry);
    }
}

fn dispatch_commit(
    registry: &mut DeviceRegistry,
    device_id: String,
    reply: oneshot::Sender<Result<(), DaemonError>>,
) {
    let Some(handle) = registry.devices.get(&device_id) else {
        let _ = reply.send(Err(DaemonError::DeviceNotFound { device_id }));
        return;
    };
    let op = WorkerOp::Commit { reply };
    if let Err(send_err) = handle.tx.try_send(op) {
        surface_worker_send_failure(send_err, &device_id, registry);
    }
}

fn dispatch_macro_get(
    registry: &mut DeviceRegistry,
    device_id: String,
    offset: u16,
    length: u8,
    reply: oneshot::Sender<Result<Vec<u8>, DaemonError>>,
) {
    let Some(handle) = registry.devices.get(&device_id) else {
        let _ = reply.send(Err(DaemonError::DeviceNotFound { device_id }));
        return;
    };
    let op = WorkerOp::GetMacro { offset, length, reply };
    if let Err(send_err) = handle.tx.try_send(op) {
        surface_worker_send_failure(send_err, &device_id, registry);
    }
}

fn dispatch_macro_set(
    registry: &mut DeviceRegistry,
    device_id: String,
    offset: u16,
    data: Vec<u8>,
    reply: oneshot::Sender<Result<(), DaemonError>>,
) {
    let Some(handle) = registry.devices.get(&device_id) else {
        let _ = reply.send(Err(DaemonError::DeviceNotFound { device_id }));
        return;
    };
    let op = WorkerOp::SetMacro { offset, data, reply };
    if let Err(send_err) = handle.tx.try_send(op) {
        surface_worker_send_failure(send_err, &device_id, registry);
    }
}

fn dispatch_lighting_get(
    registry: &mut DeviceRegistry,
    device_id: String,
    channel: u8,
    value_id: u8,
    reply: oneshot::Sender<Result<Vec<u8>, DaemonError>>,
) {
    let Some(handle) = registry.devices.get(&device_id) else {
        let _ = reply.send(Err(DaemonError::DeviceNotFound { device_id }));
        return;
    };
    let op = WorkerOp::GetLighting { channel, value_id, reply };
    if let Err(send_err) = handle.tx.try_send(op) {
        surface_worker_send_failure(send_err, &device_id, registry);
    }
}

fn dispatch_lighting_set(
    registry: &mut DeviceRegistry,
    device_id: String,
    channel: u8,
    value_id: u8,
    data: Vec<u8>,
    reply: oneshot::Sender<Result<(), DaemonError>>,
) {
    let Some(handle) = registry.devices.get(&device_id) else {
        let _ = reply.send(Err(DaemonError::DeviceNotFound { device_id }));
        return;
    };
    let op = WorkerOp::SetLighting { channel, value_id, data, reply };
    if let Err(send_err) = handle.tx.try_send(op) {
        surface_worker_send_failure(send_err, &device_id, registry);
    }
}

#[inline]
fn out_of_bounds(device_id: &str, layer: u8, row: u8, col: u8) -> DaemonError {
    DaemonError::DbusRouting(format!(
        "(layer={layer}, row={row}, col={col}) out of bounds for device `{device_id}`"
    ))
}

/// Translates a per-worker `try_send` failure back to the originating caller.
///
/// **Context:** `Full` is treated as transient backpressure; `Closed`
/// indicates the worker has died and triggers eviction so subsequent
/// requests fail fast with [`DaemonError::DeviceNotFound`] instead of
/// repeatedly hitting the same dead handle.
fn surface_worker_send_failure(
    err: mpsc::error::TrySendError<WorkerOp>,
    device_id: &str,
    registry: &mut DeviceRegistry,
) {
    match err {
        mpsc::error::TrySendError::Full(op) => {
            warn!(device_id, "worker channel full — rejecting command");
            reply_with_error(
                op,
                DaemonError::DbusRouting(format!("device `{device_id}` is busy")),
            );
        }
        mpsc::error::TrySendError::Closed(op) => {
            warn!(device_id, "worker channel closed — evicting registry entry");
            registry.devices.remove(device_id);
            reply_with_error(
                op,
                DaemonError::DeviceNotFound { device_id: device_id.to_owned() },
            );
        }
    }
}

/// Sends `err` back on whichever `oneshot` reply lives inside `op`.
#[inline]
fn reply_with_error(op: WorkerOp, err: DaemonError) {
    match op {
        WorkerOp::GetKeycode { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        WorkerOp::SetKeycode { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        WorkerOp::Commit { reply } => {
            let _ = reply.send(Err(err));
        }
        WorkerOp::GetMacro { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        WorkerOp::SetMacro { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        WorkerOp::GetLighting { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        WorkerOp::SetLighting { reply, .. } => {
            let _ = reply.send(Err(err));
        }
    }
}

/// Queries topology and spawns a per-device worker.
///
/// **Context:** Called once per [`EngineCommand::DeviceConnected`]. Performs
/// the two cache-once-on-attach driver calls
/// ([`KeyboardDriver::get_matrix_dimensions`] and
/// [`KeyboardDriver::get_layer_count`]) up front so the supervisor's
/// bounds checks have authoritative dimensions before any keymap command
/// is accepted.
///
/// # Errors
/// Propagates any [`DriverError`] from the initial topology probes,
/// re-wrapped as [`DaemonError::Driver`] by the `?` operator.
async fn attach_device(
    device_id: String,
    mut driver: Box<dyn KeyboardDriver>,
    vendor_id: u16,
    product_id: u16,
    hidraw_path: std::path::PathBuf,
    engine_tx: mpsc::Sender<EngineCommand>,
    lifecycle_tx: broadcast::Sender<LifecycleEvent>,
) -> Result<DeviceHandle, DaemonError> {
    let (rows, cols) = driver.get_matrix_dimensions().await?;
    let layer_count = driver.get_layer_count().await?;
    let topology = DeviceTopology {
        matrix: topology::KeyMatrix { rows, cols },
        layer_count,
    };
    // Capture the model name before the driver is moved into the worker.
    let model = driver.model_name().to_owned();

    let (tx, rx) = mpsc::channel(WORKER_CHANNEL_CAPACITY);
    tokio::spawn(run_device_worker(device_id, driver, rx, engine_tx, lifecycle_tx));
    Ok(DeviceHandle { topology, model, tx, vendor_id, product_id, hidraw_path })
}

/// Per-device worker loop.
///
/// **Context:** Owns the `Box<dyn KeyboardDriver>` for its device's
/// lifetime. Single-task; commands are processed strictly in receive
/// order so the firmware command-id state machine stays synchronized
/// (see [`crate::hal::KeyboardDriver`] cancel-safety note). On terminal
/// driver error the worker emits an [`EngineCommand::DeviceError`] back
/// to the supervisor and exits its loop.
/// Debounce window for the legacy compile-and-push. After the last `set_keycode`
/// on a buffered (legacy/coalescing) driver, the worker waits this long with no
/// further edits before flushing the shadow to hardware (README §4).
const LEGACY_COMMIT_DEBOUNCE: tokio::time::Duration = tokio::time::Duration::from_millis(500);

async fn run_device_worker(
    device_id: String,
    mut driver: Box<dyn KeyboardDriver>,
    mut rx: mpsc::Receiver<WorkerOp>,
    engine_tx: mpsc::Sender<EngineCommand>,
    lifecycle_tx: broadcast::Sender<LifecycleEvent>,
) {
    debug!(device_id, "worker started");

    // Debounce deadline for buffered (legacy/coalescing) drivers: when set, the
    // worker auto-commits the shadow once edits settle (README §4). VIA's
    // unbuffered path leaves this `None` and persists each write immediately.
    let mut commit_deadline: Option<tokio::time::Instant> = None;

    loop {
        let fatal = tokio::select! {
            biased;

            maybe_op = rx.recv() => {
                let Some(op) = maybe_op else {
                    // All senders dropped: flush any pending debounced edit
                    // before exiting so a just-set keycode is not lost.
                    if commit_deadline.take().is_some()
                        && let Err(e) = driver.commit_to_nvram().await
                    {
                        warn!(device_id, error = %e, "final flush on worker shutdown failed");
                    }
                    break;
                };
                match op {
                    WorkerOp::GetKeycode { layer, row, col, reply } => {
                        let result = driver
                            .get_keycode(layer, row, col)
                            .await
                            .map_err(DaemonError::from);
                        let is_fatal = is_fatal_driver_error(&result);
                        let _ = reply.send(result);
                        is_fatal
                    }
                    WorkerOp::SetKeycode { layer, row, col, keycode, reply } => {
                        let result = driver
                            .set_keycode(layer, row, col, keycode)
                            .await
                            .map_err(DaemonError::from);
                        let is_fatal = is_fatal_driver_error(&result);
                        let succeeded = result.is_ok();
                        let _ = reply.send(result);
                        if succeeded {
                            if driver.emits_layout_event_per_set() {
                                // Unbuffered VIA persists every write immediately;
                                // the layout has changed for any frontend.
                                let _ = lifecycle_tx
                                    .send(LifecycleEvent::LayoutUpdated(device_id.clone()));
                            } else {
                                // Buffered/legacy: (re)arm the debounced
                                // compile-and-push; it fires once edits settle.
                                commit_deadline =
                                    Some(tokio::time::Instant::now() + LEGACY_COMMIT_DEBOUNCE);
                            }
                        }
                        is_fatal
                    }
                    WorkerOp::Commit { reply } => {
                        // An explicit commit supersedes any pending debounce.
                        commit_deadline = None;
                        let result = driver.commit_to_nvram().await.map_err(DaemonError::from);
                        let is_fatal = is_fatal_driver_error(&result);
                        let succeeded = result.is_ok();
                        let _ = reply.send(result);
                        if succeeded {
                            // A successful commit is a layout-affecting checkpoint
                            // for every backend.
                            let _ = lifecycle_tx
                                .send(LifecycleEvent::LayoutUpdated(device_id.clone()));
                        }
                        is_fatal
                    }
                    WorkerOp::GetMacro { offset, length, reply } => {
                        let result = driver
                            .get_macro_buffer(offset, length)
                            .await
                            .map_err(DaemonError::from);
                        let is_fatal = is_fatal_driver_error(&result);
                        let _ = reply.send(result);
                        is_fatal
                    }
                    WorkerOp::SetMacro { offset, data, reply } => {
                        let result = driver
                            .set_macro_buffer(offset, &data)
                            .await
                            .map_err(DaemonError::from);
                        let is_fatal = is_fatal_driver_error(&result);
                        let _ = reply.send(result);
                        is_fatal
                    }
                    WorkerOp::GetLighting { channel, value_id, reply } => {
                        let result = driver
                            .get_lighting(channel, value_id)
                            .await
                            .map_err(DaemonError::from);
                        let is_fatal = is_fatal_driver_error(&result);
                        let _ = reply.send(result);
                        is_fatal
                    }
                    WorkerOp::SetLighting { channel, value_id, data, reply } => {
                        let result = driver
                            .set_lighting(channel, value_id, &data)
                            .await
                            .map_err(DaemonError::from);
                        let is_fatal = is_fatal_driver_error(&result);
                        let _ = reply.send(result);
                        is_fatal
                    }
                }
            }

            // Debounced compile-and-push for buffered drivers. The precondition
            // gates the branch, so `unwrap` runs only when a deadline is set.
            _ = tokio::time::sleep_until(commit_deadline.unwrap_or_else(tokio::time::Instant::now)), if commit_deadline.is_some() => {
                commit_deadline = None;
                let result = driver.commit_to_nvram().await.map_err(DaemonError::from);
                let is_fatal = is_fatal_driver_error(&result);
                match result {
                    Ok(()) => {
                        let _ = lifecycle_tx
                            .send(LifecycleEvent::LayoutUpdated(device_id.clone()));
                    }
                    Err(e) => {
                        warn!(device_id, error = %e, "debounced legacy commit failed");
                    }
                }
                is_fatal
            }
        };

        if fatal {
            // Best-effort notify; if the engine has already shut down the
            // send fails silently and we exit anyway.
            let _ = engine_tx
                .send(EngineCommand::DeviceError {
                    device_id: device_id.clone(),
                    error: Box::new(DaemonError::Driver(DriverError::Disconnected)),
                })
                .await;
            break;
        }
    }
    debug!(device_id, "worker exiting");
}



/// Classifies a driver result as one that should retire the worker.
///
/// **Context:** Only [`DriverError::Disconnected`] and
/// [`DriverError::PermissionDenied`] retire the worker per SKILLS.md §2.1
/// — every other variant is a per-call failure that the caller can retry.
#[inline]
fn is_fatal_driver_error<T>(result: &Result<T, DaemonError>) -> bool {
    matches!(
        result,
        Err(DaemonError::Driver(
            DriverError::Disconnected | DriverError::PermissionDenied { .. }
        ))
    )
}

/// Classifies an attach-time failure as transient and worth a backoff retry.
///
/// **Context:** Two conditions qualify. `PermissionDenied` covers the
/// session-start race: a user-unit daemon's initial hidraw scan can run
/// before systemd-logind has applied the `uaccess` ACLs for the new seat
/// session, so the very first `open(2)` returns `EACCES` even though access
/// arrives moments later. `Disconnected` covers an unplug racing the attach.
/// Everything else (wrong interface, protocol violations, timeouts during
/// probe) is deterministic for a given node and is not retried.
#[inline]
fn is_transient_attach_error(e: &DaemonError) -> bool {
    matches!(
        e,
        DaemonError::Driver(DriverError::Disconnected | DriverError::PermissionDenied { .. })
    )
}

/// Registers a cancellation handle in the registry and spawns the
/// exponential-backoff [`reconnect_task`] for the device described by `ctx`.
///
/// **Context:** The token in `ctx` is overwritten with a fresh registry
/// token; callers pass `token: 0` as a placeholder. A later
/// `DeviceDisconnected` event cancels the task via the stored oneshot.
fn start_reconnect_backoff(registry: &mut DeviceRegistry, mut ctx: ReconnectCtx) {
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let token = registry.next_reconnect_token;
    registry.next_reconnect_token = registry.next_reconnect_token.wrapping_add(1);
    ctx.token = token;
    registry.reconnecting.insert(ctx.device_id.clone(), (cancel_tx, token));
    tokio::spawn(reconnect_task(ctx, cancel_rx));
}

/// Bundles the shared context needed by the reconnection loop.
///
/// **Context:** Exists solely to avoid exceeding clippy's 7-argument limit
/// on [`reconnect_task`]. The fields mirror the device-identity triplet and
/// the engine-side channels that the supervisor already holds.
struct ReconnectCtx {
    device_id: String,
    vendor_id: u16,
    product_id: u16,
    hidraw_path: std::path::PathBuf,
    driver_table: std::sync::Arc<DriverTable>,
    engine_tx: mpsc::Sender<EngineCommand>,
    lifecycle_tx: broadcast::Sender<LifecycleEvent>,
    token: u64,
}

async fn reconnect_task(
    ctx: ReconnectCtx,
    mut cancel_rx: oneshot::Receiver<()>,
) {
    let mut backoff = RECONNECT_INITIAL_BACKOFF_MS;
    let mut attempts = 0;
    loop {
        if attempts >= RECONNECT_MAX_ATTEMPTS {
            info!(ctx.device_id, "reconnection max attempts reached — abandoning device");
            break;
        }

        tokio::select! {
            _ = &mut cancel_rx => {
                info!(ctx.device_id, "reconnection cancelled by DeviceDisconnected event");
                return;
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(backoff)) => {}
        }
        attempts += 1;
        info!(ctx.device_id, attempt = attempts, next_backoff = backoff * 2, "attempting reconnection");

        let factory_opt = ctx.driver_table.by_vid_pid.get(&(ctx.vendor_id, ctx.product_id)).copied()
            .or(ctx.driver_table.via_fallback);

        let Some(factory) = factory_opt else {
            info!(ctx.device_id, "no driver factory — aborting reconnection");
            break;
        };

        match factory(&ctx.hidraw_path) {
            Ok(driver) => {
                match attach_device(
                    ctx.device_id.clone(),
                    driver,
                    ctx.vendor_id,
                    ctx.product_id,
                    ctx.hidraw_path.clone(),
                    ctx.engine_tx.clone(),
                    ctx.lifecycle_tx.clone(),
                )
                .await
                {
                    Ok(handle) => {
                        // Check if cancelled before sending
                        if !matches!(cancel_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
                            info!(ctx.device_id, "reconnection succeeded but task was cancelled — discarding handle");
                            return;
                        }
                        let _ = ctx.engine_tx.send(EngineCommand::DeviceReattached {
                            device_id: ctx.device_id,
                            handle,
                            token: ctx.token,
                        }).await;
                        return;
                    }
                    Err(e) => {
                        if !is_transient_attach_error(&e) {
                            info!(ctx.device_id, error = %e, "non-retriable error during attach — aborting reconnection");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                if !is_transient_attach_error(&e) {
                    info!(ctx.device_id, error = %e, "non-retriable error during driver init — aborting reconnection");
                    break;
                }
            }
        }
        
        backoff = std::cmp::min(backoff * 2, RECONNECT_MAX_BACKOFF_MS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;
    use async_trait::async_trait;

    struct MockDriver {
        pub matrix: (u8, u8),
        pub layers: u8,
        pub get_returns: Mutex<Vec<Result<u16, DriverError>>>,
    }

    #[async_trait]
    impl KeyboardDriver for MockDriver {
        async fn get_matrix_dimensions(&mut self) -> Result<(u8, u8), DriverError> { Ok(self.matrix) }
        async fn get_layer_count(&mut self) -> Result<u8, DriverError> { Ok(self.layers) }
        async fn get_keycode(&mut self, _l: u8, _r: u8, _c: u8) -> Result<u16, DriverError> {
            let mut returns = self.get_returns.lock().unwrap();
            if returns.is_empty() { Ok(0) } else { returns.remove(0) }
        }
        async fn set_keycode(&mut self, _l: u8, _r: u8, _c: u8, _k: u16) -> Result<(), DriverError> { Ok(()) }
        async fn commit_to_nvram(&mut self) -> Result<(), DriverError> { Ok(()) }
        async fn get_macro_buffer(&mut self, _offset: u16, length: u8) -> Result<Vec<u8>, DriverError> { Ok(vec![0; length as usize]) }
        async fn set_macro_buffer(&mut self, _offset: u16, _data: &[u8]) -> Result<(), DriverError> { Ok(()) }
        async fn get_lighting(&mut self, _channel: u8, _value_id: u8) -> Result<Vec<u8>, DriverError> { Ok(vec![0; 4]) }
        async fn set_lighting(&mut self, _channel: u8, _value_id: u8, _data: &[u8]) -> Result<(), DriverError> { Ok(()) }
    }

    /// Buffered (legacy-style) driver: suppresses per-set events and counts
    /// `commit_to_nvram` calls, for exercising the worker's debounce.
    struct CountingDriver {
        commits: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl KeyboardDriver for CountingDriver {
        async fn get_matrix_dimensions(&mut self) -> Result<(u8, u8), DriverError> { Ok((5, 15)) }
        async fn get_layer_count(&mut self) -> Result<u8, DriverError> { Ok(2) }
        async fn get_keycode(&mut self, _l: u8, _r: u8, _c: u8) -> Result<u16, DriverError> { Ok(0) }
        async fn set_keycode(&mut self, _l: u8, _r: u8, _c: u8, _k: u16) -> Result<(), DriverError> { Ok(()) }
        async fn commit_to_nvram(&mut self) -> Result<(), DriverError> {
            self.commits.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn get_macro_buffer(&mut self, _o: u16, length: u8) -> Result<Vec<u8>, DriverError> { Ok(vec![0; length as usize]) }
        async fn set_macro_buffer(&mut self, _o: u16, _d: &[u8]) -> Result<(), DriverError> { Ok(()) }
        async fn get_lighting(&mut self, _channel: u8, _value_id: u8) -> Result<Vec<u8>, DriverError> { Ok(vec![0; 4]) }
        async fn set_lighting(&mut self, _channel: u8, _value_id: u8, _data: &[u8]) -> Result<(), DriverError> { Ok(()) }
        // Buffered/legacy: no per-set event; the worker debounces and commits.
        fn emits_layout_event_per_set(&self) -> bool { false }
    }

    /// Spawns a worker over a `CountingDriver`. Returns its op sender, the commit
    /// counter, and a lifecycle receiver (created before spawn so it sees events).
    fn spawn_counting_worker() -> (
        mpsc::Sender<WorkerOp>,
        Arc<AtomicUsize>,
        broadcast::Receiver<LifecycleEvent>,
    ) {
        let commits = Arc::new(AtomicUsize::new(0));
        let driver = Box::new(CountingDriver { commits: commits.clone() });
        let (op_tx, op_rx) = mpsc::channel(8);
        let (eng_tx, eng_rx) = mpsc::channel(8);
        // Keep the engine receiver alive for the worker's lifetime.
        std::mem::forget(eng_rx);
        let (life_tx, life_rx) = broadcast::channel(16);
        tokio::spawn(run_device_worker("dev".into(), driver, op_rx, eng_tx, life_tx));
        (op_tx, commits, life_rx)
    }

    async fn send_set(op_tx: &mpsc::Sender<WorkerOp>, keycode: u16) {
        let (reply, rr) = oneshot::channel();
        op_tx
            .send(WorkerOp::SetKeycode { layer: 0, row: 0, col: 0, keycode, reply })
            .await
            .unwrap();
        rr.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn legacy_set_debounces_then_commits_once() {
        let (op_tx, commits, mut life_rx) = spawn_counting_worker();

        send_set(&op_tx, 0x0004).await;
        assert_eq!(commits.load(Ordering::SeqCst), 0, "no commit before the debounce window");

        // Advance past the window; awaiting the event synchronizes on the flush.
        tokio::time::advance(LEGACY_COMMIT_DEBOUNCE + std::time::Duration::from_millis(1)).await;
        assert!(matches!(life_rx.recv().await.unwrap(), LifecycleEvent::LayoutUpdated(_)));
        assert_eq!(commits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn legacy_repeated_sets_coalesce_to_one_commit() {
        let (op_tx, commits, mut life_rx) = spawn_counting_worker();

        // Three edits, each well within the window, re-arming the timer.
        for kc in [0x0004u16, 0x0005, 0x0006] {
            send_set(&op_tx, kc).await;
            tokio::time::advance(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(commits.load(Ordering::SeqCst), 0, "still within the re-armed window");

        // Settle past the window since the last edit.
        tokio::time::advance(LEGACY_COMMIT_DEBOUNCE).await;
        let _ = life_rx.recv().await.unwrap();
        assert_eq!(commits.load(Ordering::SeqCst), 1, "coalesced to a single commit");
    }

    #[tokio::test(start_paused = true)]
    async fn legacy_explicit_commit_cancels_pending_debounce() {
        let (op_tx, commits, mut life_rx) = spawn_counting_worker();

        send_set(&op_tx, 0x0004).await; // arms the debounce

        // Explicit commit flushes immediately and clears the timer.
        let (reply, rr) = oneshot::channel();
        op_tx.send(WorkerOp::Commit { reply }).await.unwrap();
        rr.await.unwrap().unwrap();
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert!(matches!(life_rx.recv().await.unwrap(), LifecycleEvent::LayoutUpdated(_)));

        // The cancelled debounce must not fire a second commit.
        tokio::time::advance(LEGACY_COMMIT_DEBOUNCE + std::time::Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(commits.load(Ordering::SeqCst), 1, "debounce cancelled by explicit commit");
        assert!(life_rx.try_recv().is_err(), "no second event");
    }

    /// Test-only harness: returns the four parameters every `handle_command`
    /// call now expects beyond the registry.
    fn harness() -> (
        mpsc::Sender<EngineCommand>,
        std::sync::Arc<DriverTable>,
        broadcast::Sender<LifecycleEvent>,
    ) {
        let (tx, _) = mpsc::channel(1);
        let dt = std::sync::Arc::new(DriverTable::default());
        let (lt, _) = broadcast::channel(LIFECYCLE_CHANNEL_CAPACITY);
        (tx, dt, lt)
    }

    #[tokio::test]
    async fn test_device_not_found() {
        let mut registry = DeviceRegistry::new();
        let (tx, dt, lt) = harness();
        let (reply_tx, reply_rx) = oneshot::channel();

        handle_command(
            EngineCommand::GetKeycode { device_id: "test1".into(), layer: 0, row: 0, col: 0, reply: reply_tx },
            &mut registry, &tx, &dt, &lt,
        ).await;

        assert!(matches!(reply_rx.await.unwrap(), Err(DaemonError::DeviceNotFound { .. })));
    }

    #[tokio::test]
    async fn test_out_of_bounds() {
        let mut registry = DeviceRegistry::new();
        let (tx, dt, lt) = harness();
        let (worker_tx, _) = mpsc::channel(1);

        registry.devices.insert("dev1".into(), DeviceHandle {
            topology: DeviceTopology { matrix: topology::KeyMatrix { rows: 2, cols: 2 }, layer_count: 1 },
            model: "test_model".to_string(),
            tx: worker_tx, vendor_id: 0, product_id: 0, hidraw_path: PathBuf::new(),
        });

        let (reply_tx, reply_rx) = oneshot::channel();
        handle_command(
            EngineCommand::GetKeycode { device_id: "dev1".into(), layer: 1, row: 0, col: 0, reply: reply_tx },
            &mut registry, &tx, &dt, &lt,
        ).await;

        assert!(matches!(reply_rx.await.unwrap(), Err(DaemonError::DbusRouting(_))));
    }

    #[tokio::test]
    async fn test_duplicate_device_connected() {
        let mut registry = DeviceRegistry::new();
        let (tx, dt, lt) = harness();
        let (worker_tx, _) = mpsc::channel(1);

        registry.devices.insert("dev1".into(), DeviceHandle {
            topology: DeviceTopology { matrix: topology::KeyMatrix { rows: 2, cols: 2 }, layer_count: 1 },
            model: "test_model".to_string(),
            tx: worker_tx, vendor_id: 0, product_id: 0, hidraw_path: PathBuf::new(),
        });

        handle_command(
            EngineCommand::DeviceConnected { device_id: "dev1".into(), vendor_id: 0, product_id: 0, hidraw_path: PathBuf::new() },
            &mut registry, &tx, &dt, &lt,
        ).await;

        assert_eq!(registry.len(), 1);
    }

    #[tokio::test]
    async fn test_device_disconnected_unregistered() {
        let mut registry = DeviceRegistry::new();
        let (tx, dt, lt) = harness();

        handle_command(
            EngineCommand::DeviceDisconnected { device_id: "dev1".into() },
            &mut registry, &tx, &dt, &lt,
        ).await;
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn test_reattach_token_matching() {
        let mut registry = DeviceRegistry::new();
        let (tx, dt, lt) = harness();

        // 1. Setup a reconnect state with a token.
        let (cancel_tx, _cancel_rx) = oneshot::channel();
        registry.reconnecting.insert("dev1".into(), (cancel_tx, 42));

        // Create a dummy DeviceHandle to use for reattachment.
        let (worker_tx, _) = mpsc::channel(1);
        let handle = DeviceHandle {
            topology: DeviceTopology { matrix: topology::KeyMatrix { rows: 2, cols: 2 }, layer_count: 1 },
            model: "test_model".to_string(),
            tx: worker_tx,
            vendor_id: 0,
            product_id: 0,
            hidraw_path: PathBuf::new(),
        };

        // 2. Dispatch DeviceReattached with a mismatching token (e.g. 41).
        handle_command(
            EngineCommand::DeviceReattached {
                device_id: "dev1".into(),
                handle: handle.clone(),
                token: 41,
            },
            &mut registry, &tx, &dt, &lt,
        ).await;

        // Verify it was ignored: device not added, and reconnect state is still present.
        assert!(registry.devices.is_empty());
        assert!(registry.reconnecting.contains_key("dev1"));
        assert_eq!(registry.reconnecting.get("dev1").unwrap().1, 42);

        // 3. Dispatch DeviceReattached with matching token (42).
        handle_command(
            EngineCommand::DeviceReattached {
                device_id: "dev1".into(),
                handle,
                token: 42,
            },
            &mut registry, &tx, &dt, &lt,
        ).await;

        // Verify it succeeded: device added, reconnect state removed.
        assert_eq!(registry.len(), 1);
        assert!(registry.devices.contains_key("dev1"));
        assert!(!registry.reconnecting.contains_key("dev1"));
    }

    fn permission_denied_factory(path: &Path) -> Result<Box<dyn KeyboardDriver>, DaemonError> {
        Err(DaemonError::Driver(DriverError::PermissionDenied { path: path.to_path_buf() }))
    }

    fn protocol_violation_factory(_path: &Path) -> Result<Box<dyn KeyboardDriver>, DaemonError> {
        Err(DaemonError::Driver(DriverError::ProtocolViolation {
            reason: "no VIA Usage Page (0xFF60) in HID descriptor",
        }))
    }

    #[tokio::test]
    async fn test_attach_permission_denied_starts_backoff() {
        // A PermissionDenied at attach is transient (uaccess ACL race at
        // session start) — the engine must schedule a backoff retry instead
        // of abandoning the device.
        let mut registry = DeviceRegistry::new();
        let (tx, _, lt) = harness();
        let mut by_vid_pid = std::collections::HashMap::new();
        by_vid_pid.insert((0x05AC, 0x024F), permission_denied_factory as DriverFactory);
        let dt = std::sync::Arc::new(DriverTable { by_vid_pid, via_fallback: None });

        handle_command(
            EngineCommand::DeviceConnected {
                device_id: "dev1".into(),
                vendor_id: 0x05AC,
                product_id: 0x024F,
                hidraw_path: PathBuf::from("/dev/hidraw7"),
            },
            &mut registry, &tx, &dt, &lt,
        ).await;

        assert!(registry.devices.is_empty());
        assert!(registry.reconnecting.contains_key("dev1"));
    }

    #[tokio::test]
    async fn test_attach_protocol_violation_does_not_retry() {
        // A probe rejection is deterministic for the node — no backoff.
        let mut registry = DeviceRegistry::new();
        let (tx, _, lt) = harness();
        let mut by_vid_pid = std::collections::HashMap::new();
        by_vid_pid.insert((0x05AC, 0x024F), protocol_violation_factory as DriverFactory);
        let dt = std::sync::Arc::new(DriverTable { by_vid_pid, via_fallback: None });

        handle_command(
            EngineCommand::DeviceConnected {
                device_id: "dev1".into(),
                vendor_id: 0x05AC,
                product_id: 0x024F,
                hidraw_path: PathBuf::from("/dev/hidraw7"),
            },
            &mut registry, &tx, &dt, &lt,
        ).await;

        assert!(registry.devices.is_empty());
        assert!(!registry.reconnecting.contains_key("dev1"));
    }

    #[tokio::test]
    async fn test_reattach_without_reconnect_state() {
        let mut registry = DeviceRegistry::new();
        let (tx, dt, lt) = harness();

        // DeviceReattached arrives but there is no entry in reconnecting.
        let (worker_tx, _) = mpsc::channel(1);
        let handle = DeviceHandle {
            topology: DeviceTopology { matrix: topology::KeyMatrix { rows: 2, cols: 2 }, layer_count: 1 },
            model: "test_model".to_string(),
            tx: worker_tx,
            vendor_id: 0,
            product_id: 0,
            hidraw_path: PathBuf::new(),
        };

        handle_command(
            EngineCommand::DeviceReattached {
                device_id: "dev1".into(),
                handle,
                token: 0,
            },
            &mut registry, &tx, &dt, &lt,
        ).await;

        // Verify it was ignored.
        assert!(registry.devices.is_empty());
    }
}
