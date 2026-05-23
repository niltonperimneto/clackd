// SPDX-License-Identifier: GPL-3.0-or-later
//
//! D-Bus interface exposed at `io.github.clackd` / `/io/github/clackd`.
//!
//! # Contract
//!
//! Methods map one-to-one onto [`EngineCommand`] variants — each method
//! constructs the command, sends it on the bounded engine channel, and
//! awaits the `oneshot` reply. Signals are emitted by the IPC pump in
//! [`super::run_signal_pump`] in response to
//! [`crate::engine::LifecycleEvent`]s; the interface methods do not emit
//! signals directly.
//!
//! # Error Translation
//!
//! [`DaemonError`] becomes [`zbus::fdo::Error`] via the `From` impl at
//! the bottom of this file, per CLAUDE.md §3.3:
//!
//! | DaemonError variant                          | zbus::fdo::Error    |
//! |----------------------------------------------|---------------------|
//! | `DeviceNotFound`                             | `UnknownObject`     |
//! | `DeviceUnauthorized`                         | `AccessDenied`      |
//! | `Driver(DriverError::PermissionDenied { })`  | `AccessDenied`      |
//! | everything else                              | `Failed`            |

use tokio::sync::{mpsc, oneshot};
use zbus::object_server::SignalEmitter;

use crate::engine::messages::EngineCommand;
use crate::engine::DaemonError;
use crate::hal::DriverError;

/// The `io.github.clackd.Device` interface instance.
///
/// **Context:** Holds the bounded sender to the engine; nothing else.
/// State lives in the engine's [`crate::engine::DeviceRegistry`].
/// `Send + Sync` because zbus serves this behind an internal RwLock and
/// dispatches methods across the runtime's worker threads.
pub struct ClackdInterface {
    /// Sender into the engine's command channel.
    pub engine_tx: mpsc::Sender<EngineCommand>,
}

#[zbus::interface(name = "io.github.clackd.Device")]
impl ClackdInterface {
    /// Fetches the keycode at a `(layer, row, col)` slot.
    ///
    /// **Context:** For VIA, this queries hardware (or the coalescing
    /// buffer if enabled). For legacy backends (Mission 4), this reads
    /// the shadow cache.
    ///
    /// # Errors
    /// - `UnknownObject` if no device is registered under `device_id`.
    /// - `AccessDenied` on permission failures (udev or policy).
    /// - `Failed` for any other transport, protocol, or timeout error.
    async fn get_keycode(
        &self,
        device_id: &str,
        layer: u8,
        row: u8,
        col: u8,
    ) -> zbus::fdo::Result<u16> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.engine_tx
            .send(EngineCommand::GetKeycode {
                device_id: device_id.to_owned(),
                layer,
                row,
                col,
                reply: reply_tx,
            })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine command channel closed".to_owned()))?;
        let inner = reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine reply channel dropped".to_owned()))?;
        inner.map_err(zbus::fdo::Error::from)
    }

    /// Writes a keycode to a `(layer, row, col)` slot.
    ///
    /// **Hardware Quirk (EEPROM):** On the VIA unbuffered path this
    /// triggers an immediate flash-cell program; ATmega32U4 EEPROM is
    /// rated for ~100k cycles per cell. Clients should debounce rapid
    /// changes locally — see [`crate::hal`] module docs.
    ///
    /// # Errors
    /// Same surface as [`Self::get_keycode`].
    async fn set_keycode(
        &self,
        device_id: &str,
        layer: u8,
        row: u8,
        col: u8,
        keycode: u16,
    ) -> zbus::fdo::Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.engine_tx
            .send(EngineCommand::SetKeycode {
                device_id: device_id.to_owned(),
                layer,
                row,
                col,
                keycode,
                reply: reply_tx,
            })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine command channel closed".to_owned()))?;
        let inner = reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine reply channel dropped".to_owned()))?;
        inner.map_err(zbus::fdo::Error::from)
    }

    /// Lists the IDs of all currently-connected devices.
    ///
    /// **Context:** Registry metadata — does not touch hardware. Drives
    /// `clackctl list`. Returns an empty array when no devices are
    /// attached (not an error).
    async fn list_devices(&self) -> zbus::fdo::Result<Vec<String>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.engine_tx
            .send(EngineCommand::ListDevices { reply: reply_tx })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine command channel closed".to_owned()))?;
        reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine reply channel dropped".to_owned()))
    }

    /// Returns the `(rows, cols, layer_count)` topology of a device.
    ///
    /// **Context:** Cached at attach time; no hardware round-trip. Drives
    /// `clackctl info`.
    ///
    /// # Errors
    /// - `UnknownObject` if no device is registered under `device_id`.
    /// - `Failed` for engine-channel failures.
    async fn get_device_info(&self, device_id: &str) -> zbus::fdo::Result<(u8, u8, u8)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.engine_tx
            .send(EngineCommand::GetDeviceInfo {
                device_id: device_id.to_owned(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine command channel closed".to_owned()))?;
        let inner = reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine reply channel dropped".to_owned()))?;
        inner.map_err(zbus::fdo::Error::from)
    }

    /// Returns the `(vendor_id, product_id, product_name)` USB identity of a device.
    ///
    /// **Context:** Cached at attach time; no hardware round-trip. Lets a
    /// frontend match the device against an external layout definition keyed on
    /// `vid:pid` (e.g. a VIA/KLE definition). `product_name` is best-effort and
    /// may be empty.
    ///
    /// # Errors
    /// - `UnknownObject` if no device is registered under `device_id`.
    /// - `Failed` for engine-channel failures.
    async fn get_device_identity(&self, device_id: &str) -> zbus::fdo::Result<(u16, u16, String)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.engine_tx
            .send(EngineCommand::GetDeviceIdentity {
                device_id: device_id.to_owned(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine command channel closed".to_owned()))?;
        let inner = reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine reply channel dropped".to_owned()))?;
        inner.map_err(zbus::fdo::Error::from)
    }

    /// Forces a commit / NVRAM flush for a device.
    ///
    /// **Context:** No-op for unbuffered VIA; flushes the write-coalescing
    /// buffer for coalescing VIA; compiles and pushes the vendor blob for
    /// legacy backends. Drives `clackctl commit`.
    ///
    /// # Errors
    /// Same surface as [`Self::set_keycode`].
    async fn commit(&self, device_id: &str) -> zbus::fdo::Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.engine_tx
            .send(EngineCommand::CommitToNvram {
                device_id: device_id.to_owned(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine command channel closed".to_owned()))?;
        let inner = reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine reply channel dropped".to_owned()))?;
        inner.map_err(zbus::fdo::Error::from)
    }

    /// Macro operations
    async fn get_macro(&self, device_id: &str, offset: u16, length: u8) -> zbus::fdo::Result<Vec<u8>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.engine_tx
            .send(EngineCommand::GetMacro {
                device_id: device_id.to_owned(),
                offset,
                length,
                reply: reply_tx,
            })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine command channel closed".to_owned()))?;
        let inner = reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine reply channel dropped".to_owned()))?;
        inner.map_err(zbus::fdo::Error::from)
    }

    async fn set_macro(&self, device_id: &str, offset: u16, data: Vec<u8>) -> zbus::fdo::Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.engine_tx
            .send(EngineCommand::SetMacro {
                device_id: device_id.to_owned(),
                offset,
                data,
                reply: reply_tx,
            })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine command channel closed".to_owned()))?;
        let inner = reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine reply channel dropped".to_owned()))?;
        inner.map_err(zbus::fdo::Error::from)
    }

    /// Reads a VIA custom value `(channel, value_id)` — RGB/backlight config.
    ///
    /// **Context:** `channel` is the VIA custom channel (QMK RGB-matrix = 3,
    /// RGBLIGHT = 2, backlight = 1); `value_id` selects the field
    /// (brightness = 1, effect = 2, speed = 3, colour = 4). Returns the
    /// raw value bytes.
    async fn get_lighting(
        &self,
        device_id: &str,
        channel: u8,
        value_id: u8,
    ) -> zbus::fdo::Result<Vec<u8>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.engine_tx
            .send(EngineCommand::GetLighting {
                device_id: device_id.to_owned(),
                channel,
                value_id,
                reply: reply_tx,
            })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine command channel closed".to_owned()))?;
        let inner = reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine reply channel dropped".to_owned()))?;
        inner.map_err(zbus::fdo::Error::from)
    }

    /// Writes a VIA custom value `(channel, value_id)` — RGB/backlight config.
    ///
    /// **Context:** Applies live; not persisted to EEPROM unless a separate
    /// save is issued (not currently exposed). See [`Self::get_lighting`]
    /// for the channel/value_id meanings.
    async fn set_lighting(
        &self,
        device_id: &str,
        channel: u8,
        value_id: u8,
        data: Vec<u8>,
    ) -> zbus::fdo::Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.engine_tx
            .send(EngineCommand::SetLighting {
                device_id: device_id.to_owned(),
                channel,
                value_id,
                data,
                reply: reply_tx,
            })
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine command channel closed".to_owned()))?;
        let inner = reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("engine reply channel dropped".to_owned()))?;
        inner.map_err(zbus::fdo::Error::from)
    }

    /// Emitted when a layout change has been finalized on the device.
    ///
    /// **Emission semantics:** For unbuffered VIA, fires on every
    /// successful `set_keycode`. For coalescing VIA (Mission 4), fires
    /// on flush. For legacy backends, fires after the debounced compile-
    /// and-push completes.
    #[zbus(signal)]
    pub async fn layout_updated(emitter: &SignalEmitter<'_>, device_id: &str) -> zbus::Result<()>;

    /// Emitted when a device is attached or successfully reattached.
    #[zbus(signal)]
    pub async fn device_added(emitter: &SignalEmitter<'_>, device_id: &str) -> zbus::Result<()>;

    /// Emitted when a device is detached or its worker has been evicted.
    #[zbus(signal)]
    pub async fn device_removed(emitter: &SignalEmitter<'_>, device_id: &str) -> zbus::Result<()>;
}

/// Final conversion at the D-Bus boundary per CLAUDE.md §3.3.
impl From<DaemonError> for zbus::fdo::Error {
    fn from(e: DaemonError) -> Self {
        match &e {
            DaemonError::DeviceNotFound { device_id } => {
                zbus::fdo::Error::UnknownObject(device_id.clone())
            }
            DaemonError::DeviceUnauthorized { .. }
            | DaemonError::Driver(DriverError::PermissionDenied { .. }) => {
                zbus::fdo::Error::AccessDenied(e.to_string())
            }
            _ => zbus::fdo::Error::Failed(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- From<DaemonError> for zbus::fdo::Error mapping tests ----

    #[test]
    fn device_not_found_maps_to_unknown_object() {
        let err = DaemonError::DeviceNotFound {
            device_id: "hidraw0".into(),
        };
        let zbus_err: zbus::fdo::Error = err.into();
        assert!(
            matches!(zbus_err, zbus::fdo::Error::UnknownObject(ref s) if s == "hidraw0"),
            "expected UnknownObject, got {zbus_err:?}"
        );
    }

    #[test]
    fn device_unauthorized_maps_to_access_denied() {
        let err = DaemonError::DeviceUnauthorized {
            device_id: "hidraw1".into(),
        };
        let zbus_err: zbus::fdo::Error = err.into();
        assert!(
            matches!(zbus_err, zbus::fdo::Error::AccessDenied(_)),
            "expected AccessDenied, got {zbus_err:?}"
        );
    }

    #[test]
    fn permission_denied_maps_to_access_denied() {
        let err = DaemonError::Driver(DriverError::PermissionDenied {
            path: PathBuf::from("/dev/hidraw2"),
        });
        let zbus_err: zbus::fdo::Error = err.into();
        assert!(
            matches!(zbus_err, zbus::fdo::Error::AccessDenied(_)),
            "expected AccessDenied, got {zbus_err:?}"
        );
    }

    #[test]
    fn driver_io_maps_to_failed() {
        let err = DaemonError::Driver(DriverError::Io(
            std::io::Error::other("test"),
        ));
        let zbus_err: zbus::fdo::Error = err.into();
        assert!(
            matches!(zbus_err, zbus::fdo::Error::Failed(_)),
            "expected Failed, got {zbus_err:?}"
        );
    }

    #[test]
    fn driver_disconnected_maps_to_failed() {
        let err = DaemonError::Driver(DriverError::Disconnected);
        let zbus_err: zbus::fdo::Error = err.into();
        assert!(
            matches!(zbus_err, zbus::fdo::Error::Failed(_)),
            "expected Failed, got {zbus_err:?}"
        );
    }

    // ---- Method dispatch tests ----

    #[tokio::test]
    async fn get_keycode_happy_path() {
        let (engine_tx, mut engine_rx) = mpsc::channel(1);
        let iface = ClackdInterface { engine_tx };

        // Spawn a mock engine that replies with Ok(0x004C).
        tokio::spawn(async move {
            if let Some(EngineCommand::GetKeycode { reply, .. }) = engine_rx.recv().await {
                let _ = reply.send(Ok(0x004C));
            }
        });

        let result = iface.get_keycode("dev0", 0, 1, 2).await;
        assert_eq!(result.unwrap(), 0x004C);
    }

    #[tokio::test]
    async fn get_device_identity_happy_path() {
        let (engine_tx, mut engine_rx) = mpsc::channel(1);
        let iface = ClackdInterface { engine_tx };

        tokio::spawn(async move {
            if let Some(EngineCommand::GetDeviceIdentity { reply, .. }) = engine_rx.recv().await {
                let _ = reply.send(Ok((0x1234, 0xabcd, "Test Board".to_owned())));
            }
        });

        let (vid, pid, name) = iface.get_device_identity("dev0").await.unwrap();
        assert_eq!(vid, 0x1234);
        assert_eq!(pid, 0xabcd);
        assert_eq!(name, "Test Board");
    }

    #[tokio::test]
    async fn set_keycode_happy_path() {
        let (engine_tx, mut engine_rx) = mpsc::channel(1);
        let iface = ClackdInterface { engine_tx };

        tokio::spawn(async move {
            if let Some(EngineCommand::SetKeycode { reply, .. }) = engine_rx.recv().await {
                let _ = reply.send(Ok(()));
            }
        });

        let result = iface.set_keycode("dev0", 0, 1, 2, 0x004C).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn get_keycode_engine_closed() {
        let (engine_tx, engine_rx) = mpsc::channel(1);
        let iface = ClackdInterface { engine_tx };
        drop(engine_rx); // simulate engine shutdown

        let result = iface.get_keycode("dev0", 0, 0, 0).await;
        let err = result.unwrap_err();
        assert!(
            matches!(err, zbus::fdo::Error::Failed(ref msg) if msg.contains("engine command channel closed")),
            "expected Failed(engine command channel closed), got {err:?}"
        );
    }

    #[tokio::test]
    async fn get_keycode_reply_dropped() {
        let (engine_tx, mut engine_rx) = mpsc::channel(1);
        let iface = ClackdInterface { engine_tx };

        // Engine receives the command but drops the reply sender without sending.
        tokio::spawn(async move {
            if let Some(EngineCommand::GetKeycode { reply, .. }) = engine_rx.recv().await {
                drop(reply);
            }
        });

        let result = iface.get_keycode("dev0", 0, 0, 0).await;
        let err = result.unwrap_err();
        assert!(
            matches!(err, zbus::fdo::Error::Failed(ref msg) if msg.contains("engine reply channel dropped")),
            "expected Failed(engine reply channel dropped), got {err:?}"
        );
    }
}
