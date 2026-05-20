// SPDX-License-Identifier: GPL-3.0-or-later
//
//! IPC frontend — session-bus D-Bus interface for `clackd`.
//!
//! # Boundary
//!
//! This module owns the zbus session-bus connection and the lifecycle of
//! the [`frontend::ClackdInterface`] object. It is the **only** module
//! that observes [`crate::engine::LifecycleEvent`] and translates it into
//! D-Bus signals. Conversely, every method on `ClackdInterface` produces
//! exactly one [`crate::engine::messages::EngineCommand`] and awaits the
//! reply — the IPC layer holds no per-device state of its own.
//!
//! # Bus Scoping
//!
//! Per CLAUDE.md §6, the daemon is session-scoped. The interface is
//! exposed at object path `/io/github/clackd` on the well-known name
//! `io.github.clackd`. A future system-bus migration requires a polkit
//! policy file and an authorization model update; do not transition
//! without both.

use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, info, warn};
use zbus::object_server::InterfaceRef;

use crate::engine::messages::EngineCommand;
use crate::engine::LifecycleEvent;

pub mod frontend;

/// D-Bus object path the interface is exposed at.
pub const CLACKD_OBJECT_PATH: &str = "/io/github/clackd";

/// D-Bus well-known name the daemon requests.
pub const CLACKD_BUS_NAME: &str = "io.github.clackd";

/// Builds the session-bus connection and runs the signal pump.
///
/// **Context:** Resolves once the lifecycle broadcast receiver is closed
/// (i.e. the supervisor has shut down) or the bus connection drops. Holds
/// the [`zbus::Connection`] for the duration — dropping it would tear
/// down the service.
///
/// # Arguments
/// * `engine_tx` — sender into the engine command channel; cloned onto
///   the [`frontend::ClackdInterface`] for method dispatch.
/// * `lifecycle_rx` — broadcast receiver subscribed to the engine's
///   [`LifecycleEvent`] stream; consumed by the signal pump.
/// * `ready` — fired once the bus name is acquired and the object
///   server is reachable. The boot path waits on this before emitting
///   the systemd `READY=1` notification (sd_notify); without this
///   handshake a `Type=notify` unit would be marked ready before its
///   D-Bus interface is actually callable.
///
/// # Errors
/// Propagates any zbus connection error from name acquisition or object
/// registration. These are fatal at boot but recoverable on hot-reload
/// (not implemented in Mission 3).
pub async fn run_ipc(
    engine_tx: mpsc::Sender<EngineCommand>,
    lifecycle_rx: broadcast::Receiver<LifecycleEvent>,
    ready: oneshot::Sender<()>,
) -> zbus::Result<()> {
    let interface = frontend::ClackdInterface { engine_tx };

    let connection = zbus::connection::Builder::session()?
        .name(CLACKD_BUS_NAME)?
        .serve_at(CLACKD_OBJECT_PATH, interface)?
        .build()
        .await?;

    info!(bus = CLACKD_BUS_NAME, path = CLACKD_OBJECT_PATH, "IPC frontend ready");

    // Notify the boot path that the bus name is owned and the object
    // server is up. main.rs uses this to gate the systemd READY=1
    // notification — a Type=notify unit must not signal ready before its
    // public interface is reachable.
    let _ = ready.send(());

    let path = zbus::zvariant::ObjectPath::try_from(CLACKD_OBJECT_PATH)?;
    let iface_ref: InterfaceRef<frontend::ClackdInterface> = connection
        .object_server()
        .interface(path)
        .await?;

    run_signal_pump(iface_ref, lifecycle_rx).await;

    // Hold the connection until the pump returns; dropping it ends the
    // bus service.
    drop(connection);
    info!("IPC frontend shutting down");
    Ok(())
}

/// Translates [`LifecycleEvent`]s into the matching zbus signals.
///
/// **Context:** Subscribes to the supervisor's broadcast channel. A lagged
/// receiver (the IPC pump fell behind a burst of layout updates) is
/// logged but not fatal — D-Bus signals are advisory, and the IPC layer
/// can resubscribe via a property re-read if a client cares about exact
/// fidelity.
async fn run_signal_pump(
    iface_ref: InterfaceRef<frontend::ClackdInterface>,
    mut lifecycle_rx: broadcast::Receiver<LifecycleEvent>,
) {
    debug!("lifecycle signal pump started");
    loop {
        match lifecycle_rx.recv().await {
            Ok(LifecycleEvent::DeviceAdded(device_id)) => {
                let emitter = iface_ref.signal_emitter();
                if let Err(e) = frontend::ClackdInterface::device_added(emitter, &device_id).await {
                    warn!(error = %e, "failed to emit DeviceAdded signal");
                }
            }
            Ok(LifecycleEvent::DeviceRemoved(device_id)) => {
                let emitter = iface_ref.signal_emitter();
                if let Err(e) = frontend::ClackdInterface::device_removed(emitter, &device_id).await {
                    warn!(error = %e, "failed to emit DeviceRemoved signal");
                }
            }
            Ok(LifecycleEvent::LayoutUpdated(device_id)) => {
                let emitter = iface_ref.signal_emitter();
                if let Err(e) = frontend::ClackdInterface::layout_updated(emitter, &device_id).await {
                    warn!(error = %e, "failed to emit LayoutUpdated signal");
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(skipped, "lifecycle signal pump lagged — some signals dropped");
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!("lifecycle channel closed — signal pump exiting");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_ipc_ready_handshake() {
        let (engine_tx, _) = mpsc::channel(1);
        let (_, lifecycle_rx) = broadcast::channel(1);
        let (ready_tx, ready_rx) = oneshot::channel();

        let ipc_task = tokio::spawn(run_ipc(engine_tx, lifecycle_rx, ready_tx));

        // The IPC task will either succeed in connecting to the session bus and send(),
        // or it will fail (e.g., in CI without dbus) and drop the sender.
        // We test that it resolves promptly and doesn't deadlock.
        let handshake_result = tokio::time::timeout(Duration::from_secs(2), ready_rx).await;

        match handshake_result {
            Ok(Ok(())) => {
                // Connected and signaled ready
            }
            Ok(Err(_)) => {
                // Failed to connect, sender was dropped
            }
            Err(_) => {
                panic!("IPC handshake timed out - neither succeeded nor failed");
            }
        }
        
        ipc_task.abort();
    }
}
