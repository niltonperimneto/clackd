#![allow(dead_code)]
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! VIA native raw-HID driver.
//!
//! # Protocol Overview
//!
//! VIA firmware exposes a single 32-byte raw HID endpoint on Usage Page
//! `0xFF60`, Usage `0x61`. Each operation is a request frame the host
//! writes to that endpoint followed by a response frame the device sends on
//! the matching interrupt-in. The first byte of every frame is the command
//! ID; the firmware echoes it on the response so the host can confirm the
//! command-id state machine has not desynchronized.
//!
//! All command IDs implemented here are from QMK's `quantum/via.h`:
//!
//! | ID    | Constant                              | Purpose                           |
//! |-------|---------------------------------------|-----------------------------------|
//! | 0x04  | `id_dynamic_keymap_get_keycode`       | Read a keycode at `(L, R, C)`     |
//! | 0x05  | `id_dynamic_keymap_set_keycode`       | Write a keycode at `(L, R, C)`    |
//! | 0x11  | `id_dynamic_keymap_get_layer_count`   | Query layer-count                 |
//!
//! # Why Matrix Dimensions Are Not Probed
//!
//! VIA does **not** standardize a "get matrix dimensions" command. In the
//! upstream VIA-the-application, dimensions live in a per-device JSON
//! "keyboard definition" file. For the daemon we adopt the same model: the
//! [`ViaDriver`] takes `(rows, cols)` at construction time and returns them
//! from [`KeyboardDriver::get_matrix_dimensions`]. The
//! [`crate::engine::DriverTable`] entry for each known device is
//! responsible for passing the correct values; a permissive
//! `via_fallback` factory in `main.rs` uses a `(16, 16)` ceiling for
//! unknown devices and accepts the loose bounds checking that implies.
//!
//! # IO Discipline
//!
//! Per CLAUDE.md §4.1: hidraw character devices **must not** be opened
//! through `tokio::fs::File` — that path delegates to the blocking pool
//! and will deadlock the runtime under multi-device load. This driver
//! uses [`tokio::io::unix::AsyncFd`] over a raw fd opened with
//! `O_RDWR | O_NONBLOCK | O_CLOEXEC` via `nix::fcntl::open`.
//!
//! Every round-trip is wrapped in `tokio::time::timeout` with a
//! per-driver-configurable deadline (default 1000 ms). On timeout the
//! driver surfaces [`DriverError::Timeout`] carrying the device id and
//! the operation name so the engine's tracing spans can correlate.
//!
//! # Write-Coalescing Buffer (Opt-In)
//!
//! Off by default per CLAUDE.md §4.1. When constructed via
//! [`ViaDriver::with_coalescing`], the driver holds a short-lived
//! `HashMap<(layer, row, col), u16>` of pending writes; `set_keycode`
//! updates the map and returns immediately, and a single
//! `tokio::time::sleep` deadline flushes the consolidated set to
//! hardware on expiry. `get_keycode` calls during the buffered window
//! check the map first and only fall through to a hidraw round-trip on
//! miss. `commit_to_nvram` forces an immediate flush.
//!
//! Activation is logged at INFO level on construction; the deviation
//! from Principle 1.1 is recorded in tracing so audit replays can
//! attribute any observed staleness to this mode.
//!
//! The buffer-mode driver is structured as a background actor task —
//! the [`ViaTransport`] (which owns the `AsyncFd`) is moved into the
//! actor at spawn time, and the public driver becomes a thin
//! `mpsc::Sender<CoalescerOp>` facade. Per SKILLS.md §1.1 this avoids
//! `Arc<Mutex>` over device state.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use tokio::io::unix::AsyncFd;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};
use tracing::{debug, info, trace, warn};

use crate::engine::LifecycleEvent;
use crate::hal::{DriverError, KeyboardDriver};

/// VIA wire-frame length. Fixed by the protocol; never changes between
/// firmware versions.
const VIA_FRAME_LEN: usize = 32;

/// HID report ID prefix written ahead of every request frame.
///
/// **Context:** VIA's raw HID endpoint is descriptor-declared without an
/// explicit Report ID. Linux `hidraw` writes still require the leading
/// byte; the kernel ignores it on devices without report IDs.
const VIA_HIDRAW_REPORT_ID: u8 = 0x00;

// --- VIA command IDs (from QMK `quantum/via.h`) ------------------------------
const ID_DYNAMIC_KEYMAP_GET_KEYCODE: u8 = 0x04;
const ID_DYNAMIC_KEYMAP_SET_KEYCODE: u8 = 0x05;
const ID_DYNAMIC_KEYMAP_GET_LAYER_COUNT: u8 = 0x11;
const ID_LIGHTING_GET_VALUE: u8 = 0x07;
const ID_LIGHTING_SET_VALUE: u8 = 0x08;
const ID_MACRO_GET_BUFFER: u8 = 0x0B;
const ID_MACRO_SET_BUFFER: u8 = 0x0C;

/// Default per-round-trip deadline.
///
/// **Context:** 1000 ms matches the figure documented in CLAUDE.md §4.1.
/// A working VIA round-trip on a healthy bus is typically sub-millisecond;
/// the deadline exists to bound recovery time after a stalled USB
/// interrupt-in transfer, not to be approached in normal operation.
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

/// VIA Usage Page as declared in HID report descriptors.
///
/// **Context:** The VIA protocol operates over a vendor-specific HID interface
/// whose descriptor declares Usage Page `0xFF60`. Devices whose descriptors
/// do not include this page are not VIA-capable and must be rejected at
/// attach time to avoid noisy `ProtocolViolation` errors.
const VIA_USAGE_PAGE: u16 = 0xFF60;

/// Maximum HID report descriptor size per `linux/hid.h`.
const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;

/// Bound on the coalescer's command channel.
///
/// **Context:** Bigger than the per-device worker channel because the
/// actor processes both keymap operations and the periodic flush tick,
/// and the worker may dispatch a burst of `Set` ops during a profile
/// load. A saturated channel surfaces as
/// `mpsc::error::TrySendError::Full` to the engine dispatcher, which
/// already maps it to a "device busy" reply.
const COALESCER_CHANNEL_CAPACITY: usize = 64;

// -- HIDIOCGRDESCSIZE / HIDIOCGRDESC ioctl wrappers --
//
// The hidraw ioctls use `_IOR('H', 0x01, int)` and
// `_IOR('H', 0x02, struct hidraw_report_descriptor)` respectively.
// We use nix's ioctl macros to generate type-safe wrappers.

/// Kernel `struct hidraw_report_descriptor` layout.
#[repr(C)]
struct HidrawReportDescriptor {
    size: u32,
    value: [u8; HID_MAX_DESCRIPTOR_SIZE],
}

nix::ioctl_read!(hidiocgrdescsize, b'H', 0x01, std::os::raw::c_int);
nix::ioctl_read!(hidiocgrdesc, b'H', 0x02, HidrawReportDescriptor);

// ─────────────────────────────────────────────────────────────────────────────
// ViaTransport — the hardware-touching component
// ─────────────────────────────────────────────────────────────────────────────

/// Hidraw-level VIA transport.
///
/// **Context:** Owns the `AsyncFd` and performs the actual 32-byte frame
/// round-trips. Wrapped by [`ViaDriver`] (directly in unbuffered mode, via
/// the coalescer actor in coalescing mode). Not `Clone`.
///
/// Methods take `&mut self` so the type satisfies
/// [`CoalescerTransport`] — the underlying I/O is fine with `&self`
/// (`AsyncFd` has interior mutability), the `mut` is purely a trait
/// requirement.
pub(crate) struct ViaTransport {
    fd: AsyncFd<OwnedFd>,
    device_id: Arc<str>,
    timeout: Duration,
}

impl ViaTransport {
    fn new(fd: OwnedFd, device_id: Arc<str>, timeout: Duration) -> Result<Self, DriverError> {
        let fd = AsyncFd::new(fd).map_err(DriverError::Io)?;
        Ok(Self { fd, device_id, timeout })
    }

    /// Issues one request frame and reads one response frame, bounded by
    /// [`Self::timeout`].
    ///
    /// **Cancel-safety:** Cancel-safe at the *frame* boundary only. If the
    /// caller drops the returned future after the write has happened but
    /// before the read completes, the firmware command-id state machine
    /// stays out of sync with the host until the device is reset. Neither
    /// the worker nor the coalescer ever drop a round-trip future mid-flight.
    async fn roundtrip(
        &self,
        request: [u8; VIA_FRAME_LEN],
        op: &'static str,
    ) -> Result<[u8; VIA_FRAME_LEN], DriverError> {
        let dur = self.timeout;
        let device_id = self.device_id.clone();
        timeout(dur, async {
            self.write_frame(&request).await?;
            self.read_frame().await
        })
        .await
        .map_err(|_| DriverError::Timeout { device: device_id.to_string(), op })?
    }

    async fn write_frame(&self, frame: &[u8; VIA_FRAME_LEN]) -> Result<(), DriverError> {
        let mut buf = [0u8; VIA_FRAME_LEN + 1];
        buf[0] = VIA_HIDRAW_REPORT_ID;
        buf[1..].copy_from_slice(frame);

        loop {
            let mut guard = self.fd.writable().await.map_err(DriverError::Io)?;
            let write_result = guard.try_io(|inner| {
                let borrowed = inner.get_ref().as_fd();
                nix::unistd::write(borrowed, &buf).map_err(io_from_errno)
            });
            match write_result {
                Ok(Ok(n)) if n == buf.len() => return Ok(()),
                Ok(Ok(_)) => {
                    return Err(DriverError::ProtocolViolation {
                        reason: "short write to hidraw node",
                    })
                }
                Ok(Err(e)) => return Err(classify_io_error(e)),
                Err(_would_block) => continue,
            }
        }
    }

    async fn read_frame(&self) -> Result<[u8; VIA_FRAME_LEN], DriverError> {
        let mut buf = [0u8; VIA_FRAME_LEN];
        loop {
            let mut guard = self.fd.readable().await.map_err(DriverError::Io)?;
            let read_result = guard.try_io(|inner| {
                let borrowed = inner.get_ref().as_fd();
                nix::unistd::read(borrowed, &mut buf).map_err(io_from_errno)
            });
            match read_result {
                Ok(Ok(n)) if n == VIA_FRAME_LEN => return Ok(buf),
                Ok(Ok(_)) => {
                    return Err(DriverError::ProtocolViolation {
                        reason: "short read from hidraw node",
                    })
                }
                Ok(Err(e)) => return Err(classify_io_error(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

#[async_trait]
impl CoalescerTransport for ViaTransport {
    async fn get_keycode(&mut self, layer: u8, row: u8, col: u8) -> Result<u16, DriverError> {
        let mut req = [0u8; VIA_FRAME_LEN];
        req[0] = ID_DYNAMIC_KEYMAP_GET_KEYCODE;
        req[1] = layer;
        req[2] = row;
        req[3] = col;
        let resp = self.roundtrip(req, "get_keycode").await?;
        if resp[0] != ID_DYNAMIC_KEYMAP_GET_KEYCODE
            || resp[1] != layer
            || resp[2] != row
            || resp[3] != col
        {
            return Err(DriverError::ProtocolViolation {
                reason: "(cmd, layer, row, col) echo mismatch on get_keycode",
            });
        }
        let keycode = u16::from_be_bytes([resp[4], resp[5]]);
        trace!(device_id = %self.device_id, layer, row, col, keycode, "get_keycode");
        Ok(keycode)
    }

    async fn set_keycode(
        &mut self,
        layer: u8,
        row: u8,
        col: u8,
        keycode: u16,
    ) -> Result<(), DriverError> {
        let [kc_hi, kc_lo] = keycode.to_be_bytes();
        let mut req = [0u8; VIA_FRAME_LEN];
        req[0] = ID_DYNAMIC_KEYMAP_SET_KEYCODE;
        req[1] = layer;
        req[2] = row;
        req[3] = col;
        req[4] = kc_hi;
        req[5] = kc_lo;
        let resp = self.roundtrip(req, "set_keycode").await?;
        if resp[0] != ID_DYNAMIC_KEYMAP_SET_KEYCODE
            || resp[1] != layer
            || resp[2] != row
            || resp[3] != col
            || resp[4] != kc_hi
            || resp[5] != kc_lo
        {
            return Err(DriverError::ProtocolViolation {
                reason: "(cmd, layer, row, col, keycode) echo mismatch on set_keycode",
            });
        }
        trace!(device_id = %self.device_id, layer, row, col, keycode, "set_keycode");
        Ok(())
    }

    async fn get_layer_count(&mut self) -> Result<u8, DriverError> {
        let mut req = [0u8; VIA_FRAME_LEN];
        req[0] = ID_DYNAMIC_KEYMAP_GET_LAYER_COUNT;
        let resp = self.roundtrip(req, "get_layer_count").await?;
        if resp[0] != ID_DYNAMIC_KEYMAP_GET_LAYER_COUNT {
            warn!(
                device_id = %self.device_id,
                got = resp[0],
                expected = ID_DYNAMIC_KEYMAP_GET_LAYER_COUNT,
                "command-id mismatch on get_layer_count",
            );
            return Err(DriverError::ProtocolViolation {
                reason: "command-id echo mismatch on get_layer_count",
            });
        }
        Ok(resp[1])
    }

    async fn get_macro_buffer(&mut self, offset: u16, length: u8) -> Result<Vec<u8>, DriverError> {
        let mut req = [0u8; VIA_FRAME_LEN];
        req[0] = ID_MACRO_GET_BUFFER;
        let [off_hi, off_lo] = offset.to_be_bytes();
        req[1] = off_hi;
        req[2] = off_lo;
        req[3] = length;
        let resp = self.roundtrip(req, "get_macro_buffer").await?;
        // Protocol specifies we read `length` bytes after command echoes
        // Actually VIA just returns the buffer starting at byte 4.
        let mut result = Vec::with_capacity(length as usize);
        let max_len = (length as usize).min(VIA_FRAME_LEN - 4);
        result.extend_from_slice(&resp[4..4 + max_len]);
        Ok(result)
    }

    async fn set_macro_buffer(&mut self, offset: u16, data: &[u8]) -> Result<(), DriverError> {
        let mut req = [0u8; VIA_FRAME_LEN];
        req[0] = ID_MACRO_SET_BUFFER;
        let [off_hi, off_lo] = offset.to_be_bytes();
        req[1] = off_hi;
        req[2] = off_lo;
        let max_len = data.len().min(VIA_FRAME_LEN - 4);
        req[3] = max_len as u8;
        req[4..4 + max_len].copy_from_slice(&data[..max_len]);
        self.roundtrip(req, "set_macro_buffer").await?;
        Ok(())
    }

    async fn get_lighting(&mut self, lighting_cmd: u8) -> Result<Vec<u8>, DriverError> {
        let mut req = [0u8; VIA_FRAME_LEN];
        req[0] = ID_LIGHTING_GET_VALUE;
        req[1] = lighting_cmd;
        let resp = self.roundtrip(req, "get_lighting").await?;
        // Return the whole frame minus the report ID
        Ok(resp[1..].to_vec())
    }

    async fn set_lighting(&mut self, lighting_cmd: u8, data: &[u8]) -> Result<(), DriverError> {
        let mut req = [0u8; VIA_FRAME_LEN];
        req[0] = ID_LIGHTING_SET_VALUE;
        req[1] = lighting_cmd;
        let max_len = data.len().min(VIA_FRAME_LEN - 2);
        req[2..2 + max_len].copy_from_slice(&data[..max_len]);
        self.roundtrip(req, "set_lighting").await?;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Coalescer — actor that owns the transport when coalescing is enabled
// ─────────────────────────────────────────────────────────────────────────────

/// Hardware operations the coalescer can issue.
///
/// **Context:** Decoupled from [`KeyboardDriver`] because the trait is the
/// engine-facing surface and includes operations (matrix dimensions) that
/// don't touch the bus. This trait is the *minimum* set the coalescer
/// needs to delegate to. Implemented by [`ViaTransport`] in production
/// and by mock transports in tests.
#[async_trait]
pub(crate) trait CoalescerTransport: Send + Sync + 'static {
    async fn get_keycode(&mut self, layer: u8, row: u8, col: u8) -> Result<u16, DriverError>;
    async fn set_keycode(
        &mut self,
        layer: u8,
        row: u8,
        col: u8,
        keycode: u16,
    ) -> Result<(), DriverError>;
    async fn get_layer_count(&mut self) -> Result<u8, DriverError>;
    async fn get_macro_buffer(&mut self, offset: u16, length: u8) -> Result<Vec<u8>, DriverError>;
    async fn set_macro_buffer(&mut self, offset: u16, data: &[u8]) -> Result<(), DriverError>;
    async fn get_lighting(&mut self, lighting_cmd: u8) -> Result<Vec<u8>, DriverError>;
    async fn set_lighting(&mut self, lighting_cmd: u8, data: &[u8]) -> Result<(), DriverError>;
}

/// Commands sent into the coalescer actor.
#[derive(Debug)]
enum CoalescerOp {
    GetKeycode {
        layer: u8,
        row: u8,
        col: u8,
        reply: oneshot::Sender<Result<u16, DriverError>>,
    },
    SetKeycode {
        layer: u8,
        row: u8,
        col: u8,
        keycode: u16,
        reply: oneshot::Sender<Result<(), DriverError>>,
    },
    GetLayerCount {
        reply: oneshot::Sender<Result<u8, DriverError>>,
    },
    Flush {
        reply: oneshot::Sender<Result<(), DriverError>>,
    },
    GetMacro {
        offset: u16,
        length: u8,
        reply: oneshot::Sender<Result<Vec<u8>, DriverError>>,
    },
    SetMacro {
        offset: u16,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<(), DriverError>>,
    },
    GetLighting {
        command: u8,
        reply: oneshot::Sender<Result<Vec<u8>, DriverError>>,
    },
    SetLighting {
        command: u8,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<(), DriverError>>,
    },
}

/// Coalescer actor loop.
///
/// **Context:** Owns the [`CoalescerTransport`] and the in-flight buffer
/// for the duration of the device session. Single-task; commands are
/// processed strictly in receive order. The auto-flush deadline is **not
/// sliding** — it is set when the buffer first becomes non-empty and is
/// not extended by subsequent writes. This bounds the worst-case write
/// latency at `flush_interval` regardless of write pressure.
///
/// **Lifecycle emission:** After each successful flush (auto-flush,
/// explicit `Flush` command, or final shutdown flush), exactly one
/// [`LifecycleEvent::LayoutUpdated`] is emitted on the lifecycle
/// broadcast channel. This is the only correct emission point for
/// coalesced writes — the engine worker suppresses its per-`set_keycode`
/// emission when the driver reports `emits_layout_event_per_set() == false`.
///
/// **Final flush:** When the command channel closes (the driver was
/// dropped), the loop drains the buffer once more before exiting so any
/// in-flight writes are not lost.
async fn run_coalescer<T: CoalescerTransport>(
    mut transport: T,
    mut rx: mpsc::Receiver<CoalescerOp>,
    flush_interval: Duration,
    device_id: Arc<str>,
    lifecycle_tx: Option<broadcast::Sender<LifecycleEvent>>,
) {
    let mut buffer: HashMap<(u8, u8, u8), u16> = HashMap::new();
    let mut deadline: Option<Instant> = None;
    debug!(device_id = %device_id, ?flush_interval, "coalescer actor started");

    /// Emits a single `LayoutUpdated` event after a successful flush.
    /// Best-effort: a closed or lagged broadcast receiver does not
    /// affect the flush outcome.
    fn emit_layout_updated(
        lifecycle_tx: &Option<broadcast::Sender<LifecycleEvent>>,
        device_id: &str,
    ) {
        if let Some(tx) = lifecycle_tx {
            let _ = tx.send(LifecycleEvent::LayoutUpdated(device_id.to_owned()));
        }
    }

    loop {
        // Construct the sleep future fresh each iteration. When deadline
        // is None we await a never-firing future, leaving rx.recv() as
        // the only wake source.
        let sleep_fut = async {
            match deadline {
                Some(d) => sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            maybe_op = rx.recv() => {
                let Some(op) = maybe_op else { break; };
                match op {
                    CoalescerOp::SetKeycode { layer, row, col, keycode, reply } => {
                        buffer.insert((layer, row, col), keycode);
                        if deadline.is_none() {
                            deadline = Some(Instant::now() + flush_interval);
                        }
                        let _ = reply.send(Ok(()));
                    }
                    CoalescerOp::GetKeycode { layer, row, col, reply } => {
                        let result = match buffer.get(&(layer, row, col)) {
                            Some(&kc) => Ok(kc),
                            None => transport.get_keycode(layer, row, col).await,
                        };
                        let _ = reply.send(result);
                    }
                    CoalescerOp::GetLayerCount { reply } => {
                        let _ = reply.send(transport.get_layer_count().await);
                    }
                    CoalescerOp::Flush { reply } => {
                        let result = flush_buffer(&mut transport, &mut buffer).await;
                        if result.is_ok() {
                            emit_layout_updated(&lifecycle_tx, &device_id);
                        }
                        deadline = None;
                        let _ = reply.send(result);
                    }
                    CoalescerOp::GetMacro { offset, length, reply } => {
                        let _ = reply.send(transport.get_macro_buffer(offset, length).await);
                    }
                    CoalescerOp::SetMacro { offset, data, reply } => {
                        let _ = reply.send(transport.set_macro_buffer(offset, &data).await);
                    }
                    CoalescerOp::GetLighting { command, reply } => {
                        let _ = reply.send(transport.get_lighting(command).await);
                    }
                    CoalescerOp::SetLighting { command, data, reply } => {
                        let _ = reply.send(transport.set_lighting(command, &data).await);
                    }
                }
            }
            _ = sleep_fut => {
                match flush_buffer(&mut transport, &mut buffer).await {
                    Ok(()) => emit_layout_updated(&lifecycle_tx, &device_id),
                    Err(e) => warn!(device_id = %device_id, error = %e, "auto-flush failed"),
                }
                deadline = None;
            }
        }
    }

    // Final flush on shutdown — best-effort; we can't reply because the
    // sender side is gone.
    if !buffer.is_empty() {
        match flush_buffer(&mut transport, &mut buffer).await {
            Ok(()) => emit_layout_updated(&lifecycle_tx, &device_id),
            Err(e) => warn!(device_id = %device_id, error = %e, "final flush on shutdown failed"),
        }
    }
    debug!(device_id = %device_id, "coalescer actor exiting");
}

/// Drains every buffered `(layer, row, col) -> keycode` entry through
/// the transport, in arbitrary order. On the first error, the remaining
/// entries are retained in the buffer so a subsequent flush can retry
/// them.
async fn flush_buffer<T: CoalescerTransport>(
    transport: &mut T,
    buffer: &mut HashMap<(u8, u8, u8), u16>,
) -> Result<(), DriverError> {
    if buffer.is_empty() {
        return Ok(());
    }
    // Collect keys up front so we can mutate `buffer` while iterating.
    let keys: Vec<(u8, u8, u8)> = buffer.keys().copied().collect();
    for key in keys {
        let Some(&keycode) = buffer.get(&key) else { continue };
        let (layer, row, col) = key;
        transport.set_keycode(layer, row, col, keycode).await?;
        buffer.remove(&key);
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// ViaDriver — the public facade
// ─────────────────────────────────────────────────────────────────────────────

/// Native VIA driver.
///
/// **Context:** Constructed by a factory in
/// [`crate::engine::DriverTable`] and moved into a per-device worker task
/// per SKILLS.md §1.1. Not `Clone` — there is exactly one driver per
/// device per session.
///
/// **Modes:**
/// - [`Self::new`] — direct hidraw round-trips on every call; the VIA-first
///   default per CLAUDE.md §1.1.
/// - [`Self::with_coalescing`] — opt-in write-coalescing buffer per
///   CLAUDE.md §4.1. INFO-logged on construction.
pub struct ViaDriver {
    inner: ViaInner,
    matrix: (u8, u8),
    device_id: Arc<str>,
}

enum ViaInner {
    /// VIA-first default: every call performs a fresh hidraw round-trip.
    Direct(ViaTransport),
    /// Opt-in coalescing mode: commands are forwarded to a background
    /// actor that owns the transport and buffers writes.
    Coalesced {
        op_tx: mpsc::Sender<CoalescerOp>,
        // JoinHandle is held so the actor doesn't drop in a detached
        // state. When ViaDriver is dropped, `op_tx` closes, the actor
        // sees `rx.recv() == None`, performs its final flush, and
        // exits. The JoinHandle then resolves and is dropped naturally.
        _actor: JoinHandle<()>,
    },
}

impl ViaDriver {
    /// Opens the hidraw node in **unbuffered** mode.
    ///
    /// **Context:** Synchronous — performs `open(2)` directly. The fd is
    /// opened `O_NONBLOCK`; all subsequent I/O is async via
    /// [`tokio::io::unix::AsyncFd`].
    ///
    /// # Errors
    /// - [`DriverError::PermissionDenied`] on `EACCES` / `EPERM`.
    /// - [`DriverError::Disconnected`] on `ENOENT` / `ENODEV`.
    /// - [`DriverError::ProtocolViolation`] if the HID descriptor does
    ///   not declare Usage Page `0xFF60`.
    /// - [`DriverError::Io`] for any other `errno`.
    pub fn new(hidraw_path: &Path, matrix: (u8, u8)) -> Result<Self, DriverError> {
        let (transport, device_id) = open_transport(hidraw_path)?;
        Ok(Self {
            inner: ViaInner::Direct(transport),
            matrix,
            device_id,
        })
    }

    /// Opens the hidraw node in **coalescing** mode.
    ///
    /// **Context:** Per CLAUDE.md §4.1, this is an explicit, documented
    /// deviation from the VIA-first invariant and must be off by default.
    /// Activation is logged at INFO so audit replays can correlate any
    /// observed read-after-write staleness with this mode.
    ///
    /// # Errors
    /// Same surface as [`Self::new`].
    ///
    /// # Panics
    /// Does not panic — but `flush_interval` of zero will cause every
    /// `set_keycode` to flush immediately, behaviorally identical to
    /// [`Self::new`]. Callers wanting unbuffered behavior should use
    /// [`Self::new`] directly to avoid the actor overhead.
    pub fn with_coalescing(
        hidraw_path: &Path,
        matrix: (u8, u8),
        flush_interval: Duration,
        lifecycle_tx: Option<broadcast::Sender<LifecycleEvent>>,
    ) -> Result<Self, DriverError> {
        let (transport, device_id) = open_transport(hidraw_path)?;
        let (op_tx, op_rx) = mpsc::channel(COALESCER_CHANNEL_CAPACITY);
        info!(
            device_id = %device_id,
            flush_interval_ms = flush_interval.as_millis() as u64,
            "VIA write-coalescing buffer activated (CLAUDE.md §4.1 opt-in deviation from Principle 1.1)",
        );
        let actor_device_id = device_id.clone();
        let actor = tokio::spawn(run_coalescer(
            transport,
            op_rx,
            flush_interval,
            actor_device_id,
            lifecycle_tx,
        ));
        Ok(Self {
            inner: ViaInner::Coalesced { op_tx, _actor: actor },
            matrix,
            device_id,
        })
    }
}

#[async_trait]
impl KeyboardDriver for ViaDriver {
    async fn get_matrix_dimensions(&mut self) -> Result<(u8, u8), DriverError> {
        // INVARIANT: matrix dimensions are immutable for the session and
        // were validated by the factory when ViaDriver was constructed.
        Ok(self.matrix)
    }

    async fn get_layer_count(&mut self) -> Result<u8, DriverError> {
        match &mut self.inner {
            ViaInner::Direct(t) => t.get_layer_count().await,
            ViaInner::Coalesced { op_tx, .. } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                op_tx
                    .send(CoalescerOp::GetLayerCount { reply: reply_tx })
                    .await
                    .map_err(|_| DriverError::Disconnected)?;
                reply_rx.await.map_err(|_| DriverError::Disconnected)?
            }
        }
    }

    async fn get_keycode(&mut self, layer: u8, row: u8, col: u8) -> Result<u16, DriverError> {
        match &mut self.inner {
            ViaInner::Direct(t) => t.get_keycode(layer, row, col).await,
            ViaInner::Coalesced { op_tx, .. } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                op_tx
                    .send(CoalescerOp::GetKeycode { layer, row, col, reply: reply_tx })
                    .await
                    .map_err(|_| DriverError::Disconnected)?;
                reply_rx.await.map_err(|_| DriverError::Disconnected)?
            }
        }
    }

    async fn set_keycode(
        &mut self,
        layer: u8,
        row: u8,
        col: u8,
        keycode: u16,
    ) -> Result<(), DriverError> {
        match &mut self.inner {
            ViaInner::Direct(t) => t.set_keycode(layer, row, col, keycode).await,
            ViaInner::Coalesced { op_tx, .. } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                op_tx
                    .send(CoalescerOp::SetKeycode { layer, row, col, keycode, reply: reply_tx })
                    .await
                    .map_err(|_| DriverError::Disconnected)?;
                reply_rx.await.map_err(|_| DriverError::Disconnected)?
            }
        }
    }

    async fn get_macro_buffer(&mut self, offset: u16, length: u8) -> Result<Vec<u8>, DriverError> {
        match &mut self.inner {
            ViaInner::Direct(t) => t.get_macro_buffer(offset, length).await,
            ViaInner::Coalesced { op_tx, .. } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                op_tx
                    .send(CoalescerOp::GetMacro { offset, length, reply: reply_tx })
                    .await
                    .map_err(|_| DriverError::Disconnected)?;
                reply_rx.await.map_err(|_| DriverError::Disconnected)?
            }
        }
    }

    async fn set_macro_buffer(&mut self, offset: u16, data: &[u8]) -> Result<(), DriverError> {
        match &mut self.inner {
            ViaInner::Direct(t) => t.set_macro_buffer(offset, data).await,
            ViaInner::Coalesced { op_tx, .. } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                op_tx
                    .send(CoalescerOp::SetMacro { offset, data: data.to_vec(), reply: reply_tx })
                    .await
                    .map_err(|_| DriverError::Disconnected)?;
                reply_rx.await.map_err(|_| DriverError::Disconnected)?
            }
        }
    }

    async fn get_lighting(&mut self, lighting_cmd: u8) -> Result<Vec<u8>, DriverError> {
        match &mut self.inner {
            ViaInner::Direct(t) => t.get_lighting(lighting_cmd).await,
            ViaInner::Coalesced { op_tx, .. } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                op_tx
                    .send(CoalescerOp::GetLighting { command: lighting_cmd, reply: reply_tx })
                    .await
                    .map_err(|_| DriverError::Disconnected)?;
                reply_rx.await.map_err(|_| DriverError::Disconnected)?
            }
        }
    }

    async fn set_lighting(&mut self, lighting_cmd: u8, data: &[u8]) -> Result<(), DriverError> {
        match &mut self.inner {
            ViaInner::Direct(t) => t.set_lighting(lighting_cmd, data).await,
            ViaInner::Coalesced { op_tx, .. } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                op_tx
                    .send(CoalescerOp::SetLighting { command: lighting_cmd, data: data.to_vec(), reply: reply_tx })
                    .await
                    .map_err(|_| DriverError::Disconnected)?;
                reply_rx.await.map_err(|_| DriverError::Disconnected)?
            }
        }
    }

    async fn commit_to_nvram(&mut self) -> Result<(), DriverError> {
        match &mut self.inner {
            // Unbuffered VIA: every `set_keycode` already persisted via
            // the firmware's `eeprom_update_byte` call. Nothing to flush.
            ViaInner::Direct(_) => Ok(()),
            // Coalescing VIA: drain the buffer through the transport.
            ViaInner::Coalesced { op_tx, .. } => {
                let (reply_tx, reply_rx) = oneshot::channel();
                op_tx
                    .send(CoalescerOp::Flush { reply: reply_tx })
                    .await
                    .map_err(|_| DriverError::Disconnected)?;
                reply_rx.await.map_err(|_| DriverError::Disconnected)?
            }
        }
    }

    fn emits_layout_event_per_set(&self) -> bool {
        matches!(self.inner, ViaInner::Direct(_))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Open / probe helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Shared open-and-probe routine for both [`ViaDriver::new`] and
/// [`ViaDriver::with_coalescing`]. Returns the constructed transport
/// alongside the derived device id (also stored on the outer facade for
/// log span enrichment).
fn open_transport(hidraw_path: &Path) -> Result<(ViaTransport, Arc<str>), DriverError> {
    let flags = OFlag::O_RDWR | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    let owned = open(hidraw_path, flags, Mode::empty())
        .map_err(|errno| classify_open_errno(errno, hidraw_path))?;

    // Probe the HID report descriptor for the VIA Usage Page before
    // committing to this fd. Non-VIA devices are rejected early to
    // avoid noisy ProtocolViolation errors on every frame.
    probe_via_usage_page(&owned)?;

    let device_id: Arc<str> = hidraw_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown-hidraw")
        .into();

    let transport = ViaTransport::new(owned, device_id.clone(), DEFAULT_TIMEOUT)?;
    Ok((transport, device_id))
}

/// Maps a `nix::Errno` from `open(2)` onto the typed `DriverError`.
fn classify_open_errno(errno: nix::errno::Errno, path: &Path) -> DriverError {
    use nix::errno::Errno;
    match errno {
        Errno::EACCES | Errno::EPERM => DriverError::PermissionDenied {
            path: path.to_path_buf(),
        },
        Errno::ENODEV | Errno::ENOENT => DriverError::Disconnected,
        other => DriverError::Io(std::io::Error::from_raw_os_error(other as i32)),
    }
}

/// Maps a generic `std::io::Error` from `read`/`write` onto the typed
/// `DriverError`.
fn classify_io_error(e: std::io::Error) -> DriverError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::PermissionDenied => DriverError::PermissionDenied {
            path: PathBuf::new(),
        },
        ErrorKind::NotFound | ErrorKind::BrokenPipe => DriverError::Disconnected,
        _ => DriverError::Io(e),
    }
}

/// Converts a `nix::Errno` into `std::io::Error` for try_io's required type.
fn io_from_errno(errno: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(errno as i32)
}

/// Reads the HID report descriptor via ioctl and validates that it declares
/// Usage Page `0xFF60` (VIA).
///
/// **Context:** Called once at `ViaDriver::new` time, before the fd is
/// wrapped in `AsyncFd`. The ioctl is blocking but instantaneous — it
/// reads kernel-cached data, not bus traffic.
///
/// # Errors
/// - [`DriverError::ProtocolViolation`] if Usage Page `0xFF60` is absent.
/// - [`DriverError::Io`] if either ioctl fails.
fn probe_via_usage_page(fd: &OwnedFd) -> Result<(), DriverError> {
    let raw_fd = fd.as_raw_fd();
    let mut desc_size: std::os::raw::c_int = 0;

    // SAFETY: `hidiocgrdescsize` reads a single `c_int` from a valid fd.
    unsafe {
        hidiocgrdescsize(raw_fd, &mut desc_size)
            .map_err(|e| DriverError::Io(io_from_errno(e)))?;
    }

    let mut rpt_desc = HidrawReportDescriptor {
        size: desc_size as u32,
        value: [0u8; HID_MAX_DESCRIPTOR_SIZE],
    };

    // SAFETY: `hidiocgrdesc` reads into the struct with the size field set.
    unsafe {
        hidiocgrdesc(raw_fd, &mut rpt_desc)
            .map_err(|e| DriverError::Io(io_from_errno(e)))?;
    }

    let desc_bytes = &rpt_desc.value[..rpt_desc.size as usize];
    if hid_descriptor_has_usage_page(desc_bytes, VIA_USAGE_PAGE) {
        Ok(())
    } else {
        Err(DriverError::ProtocolViolation {
            reason: "no VIA Usage Page (0xFF60) in HID descriptor",
        })
    }
}

/// Scans a raw HID report descriptor for a Usage Page item matching `target`.
///
/// **Context:** HID report descriptors are a sequence of variable-length
/// items. A Usage Page item is a "Global" item with tag `0x05` (short) or
/// `0x04` (long, not used here). The data payload encodes the page number
/// as either 1 or 2 bytes for short items.
///
/// Per the HID specification §6.2.2, short items have a 1-byte prefix:
///   bits [7:4] = tag+type, bits [1:0] = size (0,1,2,3 where 3 means 4 bytes).
///
/// Usage Page has tag=0000, type=01 (Global), so the prefix byte is:
///   - `0x05` for 1-byte data
///   - `0x06` for 2-byte data
///   - `0x07` for 4-byte data (rare, for 32-bit extended pages)
pub(crate) fn hid_descriptor_has_usage_page(descriptor: &[u8], target: u16) -> bool {
    let mut i = 0;
    while i < descriptor.len() {
        let prefix = descriptor[i];

        // Long items: prefix == 0xFE, next byte is data length, then tag.
        if prefix == 0xFE {
            if i + 1 >= descriptor.len() {
                break;
            }
            let data_len = descriptor[i + 1] as usize;
            i += 3 + data_len; // prefix + size + tag + data
            continue;
        }

        // Short item: bits [1:0] encode data size.
        let size_code = prefix & 0x03;
        let data_len = match size_code {
            0 => 0,
            1 => 1,
            2 => 2,
            3 => 4, // size code 3 means 4 bytes per HID spec
            _ => unreachable!(),
        };

        // Tag+type is bits [7:2]. Usage Page prefix values:
        //   0x05 = tag 0000 | type 01 | size 01 (1-byte data)
        //   0x06 = tag 0000 | type 01 | size 10 (2-byte data)
        //   0x07 = tag 0000 | type 01 | size 11 (4-byte data)
        let tag_type = prefix & 0xFC; // mask off size bits
        if tag_type == 0x04 {
            // This is a Usage Page item.
            let page = match data_len {
                1 if i + 1 < descriptor.len() => {
                    descriptor[i + 1] as u16
                }
                2 if i + 2 < descriptor.len() => {
                    u16::from_le_bytes([descriptor[i + 1], descriptor[i + 2]])
                }
                4 if i + 4 < descriptor.len() => {
                    // 32-bit page — only compare low 16 bits.
                    u16::from_le_bytes([descriptor[i + 1], descriptor[i + 2]])
                }
                _ => {
                    i += 1 + data_len;
                    continue;
                }
            };
            if page == target {
                return true;
            }
        }

        i += 1 + data_len;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal HID descriptor that declares Usage Page 0xFF60 (VIA)
    /// followed by Usage 0x61 and a Collection(Application).
    fn via_descriptor() -> Vec<u8> {
        vec![
            0x06, 0x60, 0xFF, // Usage Page (0xFF60) — 2-byte little-endian
            0x09, 0x61,       // Usage (0x61)
            0xA1, 0x01,       // Collection (Application)
            0xC0,             // End Collection
        ]
    }

    /// A minimal HID descriptor for a generic mouse (Usage Page 0x01).
    fn mouse_descriptor() -> Vec<u8> {
        vec![
            0x05, 0x01,       // Usage Page (Generic Desktop) — 1-byte
            0x09, 0x02,       // Usage (Mouse)
            0xA1, 0x01,       // Collection (Application)
            0xC0,             // End Collection
        ]
    }

    /// A descriptor with multiple usage pages, including VIA.
    fn multi_page_descriptor() -> Vec<u8> {
        vec![
            0x05, 0x01,       // Usage Page (Generic Desktop)
            0x09, 0x06,       // Usage (Keyboard)
            0xA1, 0x01,       // Collection (Application)
            0xC0,             // End Collection
            0x06, 0x60, 0xFF, // Usage Page (0xFF60) — VIA
            0x09, 0x61,       // Usage (0x61)
            0xA1, 0x01,       // Collection (Application)
            0xC0,             // End Collection
        ]
    }

    #[test]
    fn test_via_descriptor_matches() {
        assert!(hid_descriptor_has_usage_page(&via_descriptor(), VIA_USAGE_PAGE));
    }

    #[test]
    fn test_mouse_descriptor_does_not_match() {
        assert!(!hid_descriptor_has_usage_page(&mouse_descriptor(), VIA_USAGE_PAGE));
    }

    #[test]
    fn test_multi_page_descriptor_matches() {
        assert!(hid_descriptor_has_usage_page(&multi_page_descriptor(), VIA_USAGE_PAGE));
    }

    #[test]
    fn test_empty_descriptor_does_not_match() {
        assert!(!hid_descriptor_has_usage_page(&[], VIA_USAGE_PAGE));
    }

    #[test]
    fn test_truncated_descriptor_does_not_panic() {
        // Prefix says 2-byte data but only 1 byte follows.
        assert!(!hid_descriptor_has_usage_page(&[0x06, 0x60], VIA_USAGE_PAGE));
    }

    // ─────────────────────────────────────────────────────────────────────
    // Coalescer tests — exercise the actor loop against an in-memory mock
    // transport. No real hidraw access; no #[tokio::test] is conditional on
    // libudev or kernel features.
    // ─────────────────────────────────────────────────────────────────────

    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::time::Duration;

    /// In-memory CoalescerTransport that records every hardware call.
    ///
    /// **Note:** uses `std::sync::Mutex` because the lock is never held
    /// across an `.await` boundary — each trait method grabs the lock,
    /// performs synchronous bookkeeping, and drops it before returning.
    /// CLAUDE.md SKILLS.md §1.1 forbids `Arc<Mutex>` over *device state*;
    /// this is *test fixture* state and the prohibition does not apply.
    /// See the production driver — it uses the actor pattern with no
    /// mutexes.
    #[derive(Default)]
    struct MockTransport {
        /// Simulated firmware storage: `(layer, row, col) -> keycode`.
        storage: HashMap<(u8, u8, u8), u16>,
        /// Call recorder, ordered.
        calls: Vec<MockCall>,
        /// Layer count returned by `get_layer_count`.
        layer_count: u8,
        /// If set, `set_keycode` calls matching these coordinates will fail
        /// with a Disconnected error to simulate a partial hardware failure.
        fail_on_set: Option<(u8, u8, u8)>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum MockCall {
        Get(u8, u8, u8),
        Set(u8, u8, u8, u16),
        LayerCount,
    }

    /// Adapter so the test can hand the mock to `run_coalescer` while
    /// retaining a handle for assertions.
    struct SharedMock(Arc<Mutex<MockTransport>>);

    #[async_trait]
    impl CoalescerTransport for SharedMock {
        async fn get_keycode(&mut self, layer: u8, row: u8, col: u8) -> Result<u16, DriverError> {
            let mut m = self.0.lock().unwrap();
            m.calls.push(MockCall::Get(layer, row, col));
            Ok(m.storage.get(&(layer, row, col)).copied().unwrap_or(0))
        }
        async fn set_keycode(
            &mut self,
            layer: u8,
            row: u8,
            col: u8,
            keycode: u16,
        ) -> Result<(), DriverError> {
            let mut m = self.0.lock().unwrap();
            if m.fail_on_set == Some((layer, row, col)) {
                return Err(DriverError::Disconnected);
            }
            m.calls.push(MockCall::Set(layer, row, col, keycode));
            m.storage.insert((layer, row, col), keycode);
            Ok(())
        }
        async fn get_layer_count(&mut self) -> Result<u8, DriverError> {
            let mut m = self.0.lock().unwrap();
            m.calls.push(MockCall::LayerCount);
            Ok(m.layer_count)
        }
        async fn get_macro_buffer(&mut self, _offset: u16, length: u8) -> Result<Vec<u8>, DriverError> {
            Ok(vec![0; length as usize])
        }
        async fn set_macro_buffer(&mut self, _offset: u16, _data: &[u8]) -> Result<(), DriverError> {
            Ok(())
        }
        async fn get_lighting(&mut self, _lighting_cmd: u8) -> Result<Vec<u8>, DriverError> {
            Ok(vec![0; 4])
        }
        async fn set_lighting(&mut self, _lighting_cmd: u8, _data: &[u8]) -> Result<(), DriverError> {
            Ok(())
        }
    }

    fn spawn_test_coalescer(
        flush_interval: Duration,
    ) -> (
        mpsc::Sender<CoalescerOp>,
        Arc<Mutex<MockTransport>>,
        JoinHandle<()>,
        broadcast::Receiver<LifecycleEvent>,
    ) {
        let mock = Arc::new(Mutex::new(MockTransport {
            layer_count: 4,
            ..Default::default()
        }));
        let (tx, rx) = mpsc::channel(16);
        let (lifecycle_tx, lifecycle_rx) = broadcast::channel(16);
        let device_id: Arc<str> = "test-hidraw".into();
        let actor = tokio::spawn(run_coalescer(
            SharedMock(Arc::clone(&mock)),
            rx,
            flush_interval,
            device_id,
            Some(lifecycle_tx),
        ));
        (tx, mock, actor, lifecycle_rx)
    }

    /// Helper: send a Set op and await the reply.
    async fn send_set(
        tx: &mpsc::Sender<CoalescerOp>,
        layer: u8,
        row: u8,
        col: u8,
        keycode: u16,
    ) -> Result<(), DriverError> {
        let (rtx, rrx) = oneshot::channel();
        tx.send(CoalescerOp::SetKeycode { layer, row, col, keycode, reply: rtx })
            .await
            .unwrap();
        rrx.await.unwrap()
    }

    /// Helper: send a Get op and await the reply.
    async fn send_get(
        tx: &mpsc::Sender<CoalescerOp>,
        layer: u8,
        row: u8,
        col: u8,
    ) -> Result<u16, DriverError> {
        let (rtx, rrx) = oneshot::channel();
        tx.send(CoalescerOp::GetKeycode { layer, row, col, reply: rtx })
            .await
            .unwrap();
        rrx.await.unwrap()
    }

    /// Helper: send a Flush op and await the reply.
    async fn send_flush(tx: &mpsc::Sender<CoalescerOp>) -> Result<(), DriverError> {
        let (rtx, rrx) = oneshot::channel();
        tx.send(CoalescerOp::Flush { reply: rtx }).await.unwrap();
        rrx.await.unwrap()
    }

    #[tokio::test]
    async fn set_is_buffered_no_transport_call() {
        // A very long flush interval guarantees no auto-flush during the test.
        let (tx, mock, _actor, _rx) = spawn_test_coalescer(Duration::from_secs(60));
        send_set(&tx, 0, 1, 2, 0x004C).await.unwrap();

        // Storage must still be empty; only the buffer holds the write.
        let m = mock.lock().unwrap();
        assert!(m.storage.is_empty(), "transport should not have been written to");
        assert!(
            !m.calls.iter().any(|c| matches!(c, MockCall::Set(..))),
            "no Set call should reach the transport before flush"
        );
    }

    #[tokio::test]
    async fn get_on_buffered_key_returns_from_buffer() {
        let (tx, mock, _actor, _rx) = spawn_test_coalescer(Duration::from_secs(60));
        send_set(&tx, 0, 1, 2, 0x1234).await.unwrap();
        let got = send_get(&tx, 0, 1, 2).await.unwrap();
        assert_eq!(got, 0x1234);

        // The transport must NOT have been read — the buffer satisfied the
        // request.
        let m = mock.lock().unwrap();
        assert!(
            !m.calls.iter().any(|c| matches!(c, MockCall::Get(..))),
            "buffered Get should not reach the transport, calls={:?}",
            m.calls,
        );
    }

    #[tokio::test]
    async fn get_on_unbuffered_key_falls_through_to_transport() {
        let (tx, mock, _actor, _rx) = spawn_test_coalescer(Duration::from_secs(60));
        // Pre-populate the transport's storage to distinguish hardware
        // reads from buffer reads.
        mock.lock().unwrap().storage.insert((3, 4, 5), 0xBEEF);

        let got = send_get(&tx, 3, 4, 5).await.unwrap();
        assert_eq!(got, 0xBEEF);

        let m = mock.lock().unwrap();
        assert_eq!(
            m.calls.iter().filter(|c| matches!(c, MockCall::Get(3, 4, 5))).count(),
            1,
            "expected exactly one hardware Get on buffer miss",
        );
    }

    #[tokio::test]
    async fn flush_drains_buffer_to_transport() {
        let (tx, mock, _actor, _rx) = spawn_test_coalescer(Duration::from_secs(60));
        send_set(&tx, 0, 0, 0, 0x01).await.unwrap();
        send_set(&tx, 1, 0, 0, 0x02).await.unwrap();
        send_set(&tx, 2, 0, 0, 0x03).await.unwrap();
        send_flush(&tx).await.unwrap();

        {
            let m = mock.lock().unwrap();
            assert_eq!(m.storage.get(&(0, 0, 0)), Some(&0x01));
            assert_eq!(m.storage.get(&(1, 0, 0)), Some(&0x02));
            assert_eq!(m.storage.get(&(2, 0, 0)), Some(&0x03));
        }

        // After flush, a subsequent Get should fall through to hardware
        // (buffer is empty).
        let _ = send_get(&tx, 0, 0, 0).await.unwrap();
        let m = mock.lock().unwrap();
        assert!(
            m.calls.iter().any(|c| matches!(c, MockCall::Get(0, 0, 0))),
            "post-flush Get should hit the transport, calls={:?}",
            m.calls,
        );
    }

    #[tokio::test]
    async fn auto_flush_fires_after_deadline() {
        // Short interval so the test resolves quickly.
        let (tx, mock, _actor, _rx) = spawn_test_coalescer(Duration::from_millis(50));
        send_set(&tx, 7, 7, 7, 0xABCD).await.unwrap();

        // Storage is empty immediately after Set returns.
        assert!(mock.lock().unwrap().storage.is_empty());

        // Wait past the deadline.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let m = mock.lock().unwrap();
        assert_eq!(
            m.storage.get(&(7, 7, 7)),
            Some(&0xABCD),
            "auto-flush should have drained the buffer; calls={:?}",
            m.calls,
        );
    }

    #[tokio::test]
    async fn final_flush_on_channel_close() {
        let (tx, mock, actor, _rx) = spawn_test_coalescer(Duration::from_secs(60));
        send_set(&tx, 5, 5, 5, 0xDEAD).await.unwrap();
        // Closing the only sender triggers the actor's `rx.recv() == None`
        // branch, which performs the final flush before exit.
        drop(tx);
        actor.await.unwrap();

        let m = mock.lock().unwrap();
        assert_eq!(
            m.storage.get(&(5, 5, 5)),
            Some(&0xDEAD),
            "shutdown flush should have drained the buffer; calls={:?}",
            m.calls,
        );
    }

    #[tokio::test]
    async fn deadline_not_extended_by_subsequent_writes() {
        // Bounded worst-case latency: the deadline is set on the first write
        // into an empty buffer and is NOT extended. After flush, subsequent
        // writes start a fresh window.
        let (tx, mock, _actor, _rx) = spawn_test_coalescer(Duration::from_millis(80));
        send_set(&tx, 0, 0, 0, 1).await.unwrap();
        // Subsequent writes inside the window: the deadline must not slide.
        tokio::time::sleep(Duration::from_millis(20)).await;
        send_set(&tx, 0, 0, 1, 2).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        send_set(&tx, 0, 0, 2, 3).await.unwrap();

        // Total elapsed ~40ms; deadline still 80ms from first write (~60ms remaining).
        // Wait past the original deadline only.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let m = mock.lock().unwrap();
        let written: HashSet<_> = m
            .calls
            .iter()
            .filter_map(|c| match c {
                MockCall::Set(l, r, col, _) => Some((*l, *r, *col)),
                _ => None,
            })
            .collect();
        assert!(written.contains(&(0, 0, 0)));
        assert!(written.contains(&(0, 0, 1)));
        assert!(written.contains(&(0, 0, 2)));
    }

    #[tokio::test]
    async fn get_layer_count_routes_through_actor() {
        let (tx, mock, _actor, _rx) = spawn_test_coalescer(Duration::from_secs(60));
        let (rtx, rrx) = oneshot::channel();
        tx.send(CoalescerOp::GetLayerCount { reply: rtx }).await.unwrap();
        assert_eq!(rrx.await.unwrap().unwrap(), 4);

        let m = mock.lock().unwrap();
        assert_eq!(
            m.calls.iter().filter(|c| matches!(c, MockCall::LayerCount)).count(),
            1,
        );
    }

    #[tokio::test]
    async fn flush_lifecycle_event_emission() {
        let (tx, _mock, _actor, mut rx) = spawn_test_coalescer(Duration::from_secs(60));

        // Send multiple writes. None of them should trigger an event yet.
        send_set(&tx, 0, 0, 0, 0x01).await.unwrap();
        send_set(&tx, 0, 0, 1, 0x02).await.unwrap();
        
        // Assert channel is still empty
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "no event should be emitted before flush"
        );

        // Explicit flush
        send_flush(&tx).await.unwrap();

        // Exactly one event should have been emitted
        let event = rx.try_recv().expect("expected a lifecycle event after flush");
        assert!(matches!(event, LifecycleEvent::LayoutUpdated(_)));
        
        assert!(
            matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)),
            "only one event should be emitted per flush"
        );
    }

    #[tokio::test]
    async fn partial_flush_retains_unflushed_entries() {
        let (tx, mock, _actor, _rx) = spawn_test_coalescer(Duration::from_secs(60));
        
        // Queue three writes
        send_set(&tx, 0, 0, 0, 0x01).await.unwrap();
        send_set(&tx, 0, 0, 1, 0x02).await.unwrap();
        send_set(&tx, 0, 0, 2, 0x03).await.unwrap();

        // Configure the transport to fail on the middle coordinate
        mock.lock().unwrap().fail_on_set = Some((0, 0, 1));

        // Flush will fail because of the injected error
        let err = send_flush(&tx).await.unwrap_err();
        assert!(matches!(err, DriverError::Disconnected));

        // Now clear the error so we can retry
        mock.lock().unwrap().fail_on_set = None;

        // Send another flush
        send_flush(&tx).await.unwrap();

        // The remaining entries should have been successfully flushed this time.
        // We know (0,0,1) failed the first time. The other two may or may not have 
        // been attempted during the first flush because HashMap iteration order is random.
        // But after the second successful flush, ALL THREE MUST be in storage.
        let m = mock.lock().unwrap();
        assert_eq!(m.storage.get(&(0, 0, 0)), Some(&0x01));
        assert_eq!(m.storage.get(&(0, 0, 1)), Some(&0x02));
        assert_eq!(m.storage.get(&(0, 0, 2)), Some(&0x03));
    }
}
