// SPDX-License-Identifier: GPL-3.0-or-later
//
//! `clackd` — entry point.
//!
//! # Boot Sequence
//!
//! 1. Install the tracing subscriber (honours `RUST_LOG`, default `info`).
//! 2. Construct the bounded engine command channel
//!    ([`engine::ENGINE_CHANNEL_CAPACITY`]).
//! 3. `tokio::spawn` the engine supervisor.
//! 4. Enumerate already-attached `hidraw` devices via `tokio-udev` —
//!    enumeration is synchronous in libudev, so it runs inside
//!    `tokio::task::spawn_blocking` per CLAUDE.md §2.
//! 5. Subscribe to live `hidraw` udev events through an async netlink
//!    socket; forward `Add` / `Remove` events to the engine as
//!    [`EngineCommand::DeviceConnected`] / `DeviceDisconnected`.
//! 6. Await SIGTERM or SIGINT. On signal: drop the engine sender (which
//!    drains the supervisor cleanly), abort the udev pump, and exit.
//!
//! # What This File Is Not
//!
//! The IPC frontend (`src/ipc/`) is not yet wired here; that arrives in
//! Mission 3 alongside the zbus connection. Without an IPC layer, the
//! daemon currently only reacts to udev — there is no way to issue
//! `GetKeycode` / `SetKeycode` commands. This is intentional: Mission 2
//! delivers the supervisor scaffolding only.

mod engine;
mod hal;
mod ipc;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sd_notify::NotifyState;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::StreamExt;
use tokio_udev::{AsyncMonitorSocket, Device, EventType, MonitorBuilder};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use engine::messages::EngineCommand;
use engine::{
    DaemonError, DriverTable, LifecycleEvent, ENGINE_CHANNEL_CAPACITY, LIFECYCLE_CHANNEL_CAPACITY,
};
use hal::legacy::gmk67::Gmk67Driver;
use hal::via::ViaDriver;
use hal::KeyboardDriver;

/// udev subsystem we monitor. Every keyboard the daemon supports — VIA and
/// vendor-proprietary alike — exposes its control endpoint as a `hidraw`
/// node.
const HIDRAW_SUBSYSTEM: &str = "hidraw";

/// Boxed error type for the boot path.
///
/// **Context:** `main` is the one place a coarse error type is appropriate —
/// startup failures are heterogeneous (udev unavailable, kernel feature
/// missing, signal handler installation refused) and the supervisor and
/// worker code paths under it use the typed [`engine::DaemonError`] /
/// [`hal::DriverError`] hierarchy instead.
type BootError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), BootError> {
    init_tracing();
    init_coalesce_config();
    info!(version = env!("CARGO_PKG_VERSION"), "clackd starting");

    let (engine_tx, engine_rx) = mpsc::channel::<EngineCommand>(ENGINE_CHANNEL_CAPACITY);
    let (lifecycle_tx, lifecycle_rx) = broadcast::channel::<LifecycleEvent>(LIFECYCLE_CHANNEL_CAPACITY);
    let driver_table = Arc::new(build_driver_table());
    let supervisor = tokio::spawn(engine::run_supervisor(
        engine_tx.clone(),
        engine_rx,
        driver_table,
        lifecycle_tx.clone(),
    ));

    let ipc_engine_tx = engine_tx.clone();
    let (ipc_ready_tx, ipc_ready_rx) = oneshot::channel::<()>();
    let ipc = tokio::spawn(async move {
        if let Err(e) = ipc::run_ipc(ipc_engine_tx, lifecycle_rx, ipc_ready_tx).await {
            error!(error = %e, "IPC frontend exited with error");
        }
    });

    // Wait for the IPC layer to acquire its bus name and stand up the
    // object server before announcing readiness to systemd. If the IPC
    // never reports ready (it crashed before signaling), we still send
    // READY=1 so a downstream `Type=notify` unit doesn't hang the boot
    // sequence indefinitely — the supervisor itself is already running
    // and will keep retrying.
    match tokio::time::timeout(Duration::from_secs(5), ipc_ready_rx).await {
        Ok(Ok(())) => notify_systemd_ready(),
        Ok(Err(_)) => {
            warn!("IPC ready channel dropped before signal — proceeding without IPC");
            notify_systemd_ready();
        }
        Err(_) => {
            warn!("IPC did not report ready within 5s — proceeding anyway");
            notify_systemd_ready();
        }
    }

    // Seed the engine with devices that are already plugged in at startup.
    // libudev's enumeration is synchronous; isolate it in spawn_blocking so
    // it never parks the runtime.
    match scan_initial_devices().await {
        Ok(seeded) => {
            info!(count = seeded.len(), "seeding engine with attached hidraw nodes");
            for cmd in seeded {
                if engine_tx.send(cmd).await.is_err() {
                    warn!("engine receiver closed during initial scan — aborting boot");
                    break;
                }
            }
        }
        Err(e) => {
            // Non-fatal: hot-plug events via the live monitor still work.
            warn!(error = %e, "initial udev enumeration failed — continuing on live events only");
        }
    }

    // Live hot-plug monitoring.
    let monitor = build_udev_monitor()?;
    
    let local = tokio::task::LocalSet::new();
    let pump = local.spawn_local(run_udev_pump(monitor, engine_tx.clone()));

    local.run_until(async move {
        wait_for_shutdown().await?;
        info!("shutdown signal received");
        notify_systemd_stopping();

        // Signal the engine supervisor to gracefully shut down. It clears
        // its registry (dropping every worker's command channel, which
        // terminates the workers) and breaks its loop — this is what
        // resolves the circular sender ownership (the supervisor holds an
        // engine_tx clone, as do the workers), which channel-closure alone
        // cannot break.
        let _ = engine_tx.send(EngineCommand::Shutdown).await;

        // Drop the last sender clones we hold so the IPC signal pump's
        // broadcast receiver sees `Closed`.
        drop(engine_tx);
        drop(lifecycle_tx);
        pump.abort();
        let _ = pump.await;
        ipc.abort();
        let _ = ipc.await;
        if let Err(e) = supervisor.await {
            error!(error = %e, "supervisor join failed");
        }

        info!("clackd exited");
        Ok(())
    }).await
}

/// Per-device configuration entry from `devices.toml`.
///
/// **Context:** Maps a `(vid, pid)` to known matrix dimensions so the engine
/// can reject out-of-bounds requests before they hit the firmware.
#[derive(Debug, serde::Deserialize)]
struct DeviceConfig {
    vid: u16,
    pid: u16,
    rows: u8,
    cols: u8,
    /// If set, overrides the layer count queried from firmware.
    #[allow(dead_code)]
    layer_count_override: Option<u8>,
    /// Driver backend for this device: `"via"` (default) or `"gmk67"`.
    /// Selects the vendor HAL driver in [`build_driver_table`].
    #[serde(default)]
    driver: Option<String>,
}

/// Top-level structure for `$XDG_CONFIG_HOME/clackd/devices.toml`.
#[derive(Debug, serde::Deserialize)]
struct DevicesConfig {
    #[serde(default)]
    device: Vec<DeviceConfig>,
}

/// Constructs the daemon's driver dispatch table.
///
/// **Context:** Reads `$XDG_CONFIG_HOME/clackd/devices.toml` if it exists.
/// Each `[[device]]` entry maps a `(vid, pid)` to a factory that constructs
/// a [`ViaDriver`] with the specified `(rows, cols)`. Devices not in the
/// config fall through to the `via_fallback` which uses a permissive
/// `(16, 16)` ceiling.
///
/// If the config file is absent or unreadable, the table is empty (fallback
/// only) and a WARN is logged so the operator knows bounds checking is loose.
fn build_driver_table() -> DriverTable {
    let mut by_vid_pid = HashMap::new();

    if let Some(proj_dirs) = directories::ProjectDirs::from("io", "github", "clackd") {
        let config_path = proj_dirs.config_dir().join("devices.toml");
        match std::fs::read_to_string(&config_path) {
            Ok(contents) => {
                match toml::from_str::<DevicesConfig>(&contents) {
                    Ok(cfg) => {
                        for dev in &cfg.device {
                            let backend = dev.driver.as_deref().unwrap_or("via");
                            info!(
                                vid = format!("{:04x}", dev.vid),
                                pid = format!("{:04x}", dev.pid),
                                rows = dev.rows,
                                cols = dev.cols,
                                driver = backend,
                                "registered device from config",
                            );
                            // Select the vendor backend. Unknown values fall
                            // back to VIA with a warning rather than dropping
                            // the device entirely.
                            let factory: engine::DriverFactory = match backend {
                                "gmk67" => gmk67_factory,
                                "epomaker" => {
                                    warn!("driver = \"epomaker\" is deprecated; rename it to \"gmk67\" in devices.toml");
                                    gmk67_factory
                                }
                                "via" => via_fallback_factory,
                                other => {
                                    warn!(driver = other, "unknown driver backend — defaulting to via");
                                    via_fallback_factory
                                }
                            };
                            by_vid_pid.insert((dev.vid, dev.pid), factory);
                        }
                    }
                    Err(e) => {
                        warn!(path = %config_path.display(), error = %e, "failed to parse devices.toml");
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!("no devices.toml at {} — using (16,16) ceiling for all devices", config_path.display());
            }
            Err(e) => {
                warn!(path = %config_path.display(), error = %e, "failed to read devices.toml");
            }
        }
    }

    DriverTable {
        by_vid_pid,
        via_fallback: Some(via_fallback_factory),
    }
}

/// Process-wide opt-in for the VIA write-coalescing buffer.
///
/// **Context:** Populated by [`init_coalesce_config`] once at startup from
/// the `CLACKD_VIA_COALESCE_MS` environment variable. `None` (the default)
/// means every device factory builds an unbuffered [`ViaDriver`] per
/// CLAUDE.md §4.1's "off by default" mandate. `Some(interval)` means every
/// device factory builds a coalescing driver with that flush interval.
///
/// Read by [`via_fallback_factory`] (and by the per-device factories
/// populated from `devices.toml`, when those are added).
static COALESCE_INTERVAL: OnceLock<Option<Duration>> = OnceLock::new();

/// Parses `CLACKD_VIA_COALESCE_MS` once at startup.
///
/// **Context:** Accepted values:
/// - unset, empty, malformed, or `0` → coalescing disabled (default).
/// - any positive integer N → coalescing enabled with N-millisecond flush
///   interval.
///
/// The env-var name is INFO-logged on enable so the audit trail is clear.
/// Per CLAUDE.md §4.1 the driver itself also logs activation per device
/// when [`ViaDriver::with_coalescing`] is invoked.
fn init_coalesce_config() {
    let interval = std::env::var("CLACKD_VIA_COALESCE_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_millis);
    if let Some(d) = interval {
        info!(
            flush_interval_ms = d.as_millis() as u64,
            "CLACKD_VIA_COALESCE_MS set — VIA write-coalescing will be enabled per device",
        );
    }
    let _ = COALESCE_INTERVAL.set(interval);
}

/// Factory for the permissive VIA fallback.
///
/// **Context:** Attaches a [`ViaDriver`] to any hidraw node that passes the
/// Usage Page `0xFF60` probe (performed inside [`ViaDriver::new`] /
/// [`ViaDriver::with_coalescing`]), declaring a `(16, 16)` matrix ceiling.
/// This is a deliberate over-estimate — most real keyboards are smaller
/// (a 60% is 5×14, a TKL is 6×17). The firmware's command-id echo check
/// will reject out-of-physical-range writes with
/// [`crate::hal::DriverError::ProtocolViolation`], so the loose bounds
/// are a safety net, not an authorization mechanism.
///
/// **Mode selection:** Inspects [`COALESCE_INTERVAL`]. Unset → unbuffered
/// driver (VIA-first default). Set → coalescing driver with the
/// configured flush interval.
///
/// For precise bounds, add a `[[device]]` entry to
/// `$XDG_CONFIG_HOME/clackd/devices.toml` with the device's VID, PID,
/// and physical `(rows, cols)`.
fn via_fallback_factory(hidraw_path: &Path) -> Result<Box<dyn KeyboardDriver>, DaemonError> {
    let driver: Box<dyn KeyboardDriver> = match COALESCE_INTERVAL.get().copied().flatten() {
        Some(interval) => Box::new(ViaDriver::with_coalescing(hidraw_path, (16, 16), interval, None)?),
        None => Box::new(ViaDriver::new(hidraw_path, (16, 16))?),
    };
    Ok(driver)
}

/// EK68 matrix bounds and layer count for the GMK67-family driver. The keymap
/// matrix (5 rows × 15 cols) comes from the vendor `KeyboardLayout.xml` key
/// positions (docs/protocol/epomaker-ek68.md section 6); the driver's `KEY_INDEX`
/// table maps each `(row, col)` to the firmware `key_index`. Two layers
/// (base + Fn) per the approved plan; only base-layer remap is wired so far
/// (doc section 5). The device offers no topology query.
const EK68_MATRIX: (u8, u8) = (5, 15);
const EK68_LAYERS: u8 = 2;

/// Factory for the GMK67-family (non-VIA) vendor driver (Epomaker EK68 /
/// Zuoya GMK67).
///
/// **Context:** Selected for a `(vid, pid)` whose `devices.toml` entry sets
/// `driver = "gmk67"`. Binds the vendor Feature-report interface (the
/// `Gmk67Driver::new` probe rejects the non-vendor hidraw node of the
/// composite device).
fn gmk67_factory(hidraw_path: &Path) -> Result<Box<dyn KeyboardDriver>, DaemonError> {
    Ok(Box::new(Gmk67Driver::new(hidraw_path, EK68_MATRIX, EK68_LAYERS)?))
}

/// Emits `READY=1` to the systemd notification socket.
///
/// **Context:** No-op when `NOTIFY_SOCKET` is unset (i.e. the daemon was
/// not started by systemd). When set, this is what flips the unit from
/// `activating` to `active` under `Type=notify`. Called once IPC has
/// acquired its bus name so the unit is only marked ready when its
/// public interface is actually reachable.
fn notify_systemd_ready() {
    match sd_notify::notify(&[NotifyState::Ready]) {
        Ok(()) => debug!("sd_notify: READY=1 sent"),
        Err(e) => debug!(error = %e, "sd_notify READY failed (likely not under systemd)"),
    }
}

/// Emits `STOPPING=1` to the systemd notification socket.
///
/// **Context:** Sent on receipt of SIGTERM/SIGINT, before the supervisor
/// tear-down begins. systemd uses this to transition the unit state
/// from `active` to `deactivating` and to suppress restart-on-failure
/// during the orderly shutdown window. No-op without systemd.
fn notify_systemd_stopping() {
    match sd_notify::notify(&[NotifyState::Stopping]) {
        Ok(()) => debug!("sd_notify: STOPPING=1 sent"),
        Err(e) => debug!(error = %e, "sd_notify STOPPING failed (likely not under systemd)"),
    }
}

/// Installs the global tracing subscriber.
///
/// **Context:** `RUST_LOG` honoured if set; otherwise default to `info`.
/// Idempotent — only meaningful to call once per process, but a second
/// call is rejected by `subscriber::set_global_default` rather than
/// panicking the daemon.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Builds the async udev `hidraw` monitor.
///
/// **Context:** `MonitorBuilder::listen` returns the raw socket; the
/// `try_into` converts it to an `AsyncMonitorSocket` backed by `AsyncFd`
/// over the udev netlink fd. The blocking `udev` crate is not on this
/// path (CLAUDE.md §2).
///
/// # Errors
/// Surfaces any `io::Error` from socket construction or subsystem-filter
/// installation; both indicate either a missing kernel feature or a
/// permission problem that the daemon cannot recover from in-process.
fn build_udev_monitor() -> Result<AsyncMonitorSocket, BootError> {
    let monitor = MonitorBuilder::new()?
        .match_subsystem(HIDRAW_SUBSYSTEM)?
        .listen()?;
    Ok(AsyncMonitorSocket::new(monitor)?)
}

/// Enumerates already-attached `hidraw` nodes and returns
/// [`EngineCommand::DeviceConnected`] for each.
///
/// **Context:** `tokio_udev::Enumerator` is a synchronous libudev wrapper;
/// per CLAUDE.md §2 we issue it inside `spawn_blocking`. The closure is
/// pure side-effect-free enumeration and bounded in latency by the number
/// of attached USB HID devices (small in practice).
///
/// # Errors
/// Propagates any `io::Error` from the enumeration call or the
/// `JoinError` if the blocking task itself was cancelled.
async fn scan_initial_devices() -> Result<Vec<EngineCommand>, BootError> {
    let cmds = tokio::task::spawn_blocking(|| -> std::io::Result<Vec<EngineCommand>> {
        let mut enumerator = tokio_udev::Enumerator::new()?;
        enumerator.match_subsystem(HIDRAW_SUBSYSTEM)?;
        let devices = enumerator.scan_devices()?;
        Ok(devices.filter_map(|d| device_to_connect_command(&d)).collect())
    })
    .await??;
    Ok(cmds)
}

/// Drives the live udev event stream and translates events into commands.
///
/// **Context:** Stops on three conditions — stream end (kernel closed the
/// netlink socket), engine receiver closed (supervisor shut down), or the
/// task being aborted by `main`. Non-keyboard events and unhandled event
/// types (`Change`, `Bind`, `Unbind`, `Online`/`Offline`) are dropped.
async fn run_udev_pump(mut socket: AsyncMonitorSocket, engine_tx: mpsc::Sender<EngineCommand>) {
    debug!("udev pump started");
    while let Some(event) = socket.next().await {
        match event {
            Ok(ev) => {
                let Some(cmd) = event_to_command(&ev) else {
                    continue;
                };
                if engine_tx.send(cmd).await.is_err() {
                    info!("engine receiver closed — udev pump exiting");
                    break;
                }
            }
            Err(e) => {
                warn!(error = %e, "udev event error — continuing");
            }
        }
    }
    debug!("udev pump exiting");
}

/// Translates a single udev event into an engine command, or `None` if the
/// event has no engine-side effect.
fn event_to_command(event: &tokio_udev::Event) -> Option<EngineCommand> {
    match event.event_type() {
        EventType::Add => device_to_connect_command(&event.device()),
        EventType::Remove => {
            let device_id = stable_device_id(&event.device())?;
            Some(EngineCommand::DeviceDisconnected { device_id })
        }
        _ => None,
    }
}

/// Builds a [`EngineCommand::DeviceConnected`] from a fully-resolved
/// `Device`, or returns `None` if any required attribute is missing.
///
/// **Context:** Required attributes are the devnode path and the
/// `idVendor` / `idProduct` USB attributes from the device's USB parent.
/// Devices without a USB parent (Bluetooth HID, virtual nodes) are
/// intentionally skipped at this layer — the daemon only supports wired
/// VIA keyboards in Mission 2.
fn device_to_connect_command(d: &Device) -> Option<EngineCommand> {
    let devnode: PathBuf = d.devnode()?.to_path_buf();
    let device_id = stable_device_id(d)?;

    let usb_parent = d.parent_with_subsystem_devtype("usb", "usb_device").ok()??;
    let vendor_id = parse_usb_id(&usb_parent, "idVendor")?;
    let product_id = parse_usb_id(&usb_parent, "idProduct")?;

    Some(EngineCommand::DeviceConnected {
        device_id,
        vendor_id,
        product_id,
        hidraw_path: devnode,
    })
}

/// Returns a session-stable identifier for the device.
///
/// **Context:** Uses the udev `sysname` (e.g. `"hidraw0"`) which is unique
/// within the running kernel session. Not stable across reboots, but the
/// daemon does not persist device-keyed state across restarts, so the
/// session-scoped guarantee is sufficient.
fn stable_device_id(d: &Device) -> Option<String> {
    Some(d.sysname().to_string_lossy().into_owned())
}

/// Reads a 4-digit hex USB attribute (`idVendor` / `idProduct`) off the
/// USB parent device.
fn parse_usb_id(usb_parent: &Device, attr: &str) -> Option<u16> {
    let raw = usb_parent.attribute_value(attr)?.to_str()?;
    u16::from_str_radix(raw.trim(), 16).ok()
}

/// Awaits either SIGTERM or SIGINT.
///
/// **Context:** Installs handlers for both standard shutdown signals. The
/// first one fired wins; the other handler is dropped on return. Returns
/// `Err` only if signal-handler installation itself fails (e.g. the
/// process was started with signal-handler restrictions), which is a
/// hard-fail condition for an unattended daemon.
async fn wait_for_shutdown() -> std::io::Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => debug!("SIGTERM received"),
        _ = sigint.recv() => debug!("SIGINT received"),
    }
    Ok(())
}
