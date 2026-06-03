#![allow(dead_code)]
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Epomaker EK68 (non-VIA) proprietary HID driver.
//!
//! # Protocol Overview
//!
//! Unlike [`crate::hal::via`], the EK68 (the non-VIA / "driver" model,
//! USB `05ac:024f`, maker `hfd.cn`) does **not** expose the VIA Usage Page.
//! Its configuration channel is a **64-byte HID *Feature* report** on
//! **interface 0**, driven over the control pipe via `SET_REPORT(Feature)` /
//! `GET_REPORT(Feature)` — there is no OUT endpoint. The full reverse-
//! engineering record lives in `docs/protocol/epomaker-ek68.md`; the
//! load-bearing facts this module encodes:
//!
//! ## Lighting (fully decoded)
//!
//! A single 64-byte frame sets the whole-keyboard lighting:
//!
//! | Offset | Field                                            |
//! |--------|--------------------------------------------------|
//! | 0      | effect/mode id (`0x00` off, `0x01` static … `0x13` shuttle) |
//! | 1..3   | R, G, B                                          |
//! | 8      | rainbow / multicolor flag (`0x01` on Colourful/Spectrum) |
//! | 9      | brightness `0x01`–`0x10`                          |
//! | 10     | `0x0B` when lit, `0x01` when off                 |
//! | 14..15 | `AA 55` footer magic                             |
//!
//! There is **no payload checksum** (confirmed by diffing red/green/blue —
//! only the RGB bytes changed).
//!
//! ## Keymap / remap (format decoded; addressing pending hardware test)
//!
//! A remap is a *select* frame (`02 00 <source-key default scancode>`)
//! followed by a *write* frame (`02 00 <keycode-lo> <keycode-hi>`), keycodes
//! being standard VIA/QMK values. The keyboard identifies the source key by
//! its factory scancode (the write offset is a per-row column position and
//! is **not** a globally-unique slot — `E` and `5` produce byte-identical
//! writes). Because that disambiguation is not observable in passive
//! captures, the exact offset/select requirement is finalized by testing
//! against real hardware; this module emits the documented frames and keeps
//! a host-side shadow keymap so the D-Bus API is coherent meanwhile
//! (the legacy-polyfill shadow model, CLAUDE.md §4.2).
//!
//! ## Macros
//!
//! Not yet decoded (deliberately deferred). The macro methods operate on the
//! shadow buffer only and are no-ops on the wire until the format is reversed.
//!
//! # IO Discipline
//!
//! Feature reports are issued with the `HIDIOCSFEATURE` / `HIDIOCGFEATURE`
//! ioctls — **not** `read`/`write`, and **not** `tokio::fs::File` (CLAUDE.md
//! §4.1). The ioctl is a synchronous USB control transfer, so it is run on
//! [`tokio::task::spawn_blocking`] and bounded by a per-call
//! `tokio::time::timeout`, mirroring the VIA driver's deadline discipline.

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nix::fcntl::{open, OFlag};
use nix::sys::stat::Mode;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::hal::{DriverError, KeyboardDriver};

/// USB identifiers for the EK68 (non-VIA). Reports as "Apple" — a spoof;
/// the real maker string is `hfd.cn`, product `EK68`.
pub const EK68_VID: u16 = 0x05AC;
pub const EK68_PID: u16 = 0x024F;

/// Epomaker vendor Feature-report payload length.
const FRAME_LEN: usize = 64;

/// Report ID prefix for the Feature report (none declared → 0x00).
const REPORT_ID: u8 = 0x00;

/// Per-call deadline for a feature-report round trip. Matches the VIA
/// driver's 1000 ms figure (CLAUDE.md §4.1).
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

/// Maximum HID report descriptor size per `linux/hid.h`.
const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;

// --- Lighting frame layout (see module docs / docs/protocol/epomaker-ek68.md) ---
const LIGHT_OFF_MODE: usize = 0;
const LIGHT_OFF_R: usize = 1;
const LIGHT_OFF_G: usize = 2;
const LIGHT_OFF_B: usize = 3;
const LIGHT_OFF_RAINBOW: usize = 8;
const LIGHT_OFF_BRIGHTNESS: usize = 9;
const LIGHT_OFF_MARKER: usize = 10;
const LIGHT_OFF_FOOTER: usize = 14;
const LIGHT_MARKER_LIT: u8 = 0x0B;
const LIGHT_MARKER_OFF: u8 = 0x01;
const FOOTER_MAGIC: [u8; 2] = [0xAA, 0x55];

/// Effect/mode ids (byte 0 of the lighting frame).
pub const MODE_LED_OFF: u8 = 0x00;
pub const MODE_STATIC: u8 = 0x01;
/// Highest known effect id (`Shuttle`). Ids `0x00..=0x13` are valid.
pub const MODE_MAX: u8 = 0x13;

const BRIGHTNESS_MIN: u8 = 0x01;
const BRIGHTNESS_MAX: u8 = 0x10;

// --- Remap frame layout ---
//
// `02 00 <scancode>`           = select (which key)
// `02 00 <kc_lo> <kc_hi>`      = write  (new keycode, 16-bit little-endian)
//
// The select/write offsets observed in the Windows driver are row-dependent
// (a per-row column position, not a global slot). These constants are the
// best-known starting point and are the single thing to confirm on hardware.
const REMAP_MARKER: u8 = 0x02;
const REMAP_SELECT_OFFSET: usize = 20;
const REMAP_WRITE_OFFSET: usize = 4;

// -- HIDIOCSFEATURE / HIDIOCGFEATURE ioctl wrappers --
//
// Both are `_IOC(_IOC_WRITE|_IOC_READ, 'H', nr, len)`; the buffer is
// `[report_id, payload...]`. nix's `*_buf!` variant encodes the length from
// the slice at call time.
nix::ioctl_readwrite_buf!(hidiocsfeature, b'H', 0x06, u8);
nix::ioctl_readwrite_buf!(hidiocgfeature, b'H', 0x07, u8);

// HIDIOCGRDESCSIZE / HIDIOCGRDESC — used to confirm we bound interface 0
// (the one carrying the 64-byte vendor Feature report).
#[repr(C)]
struct HidrawReportDescriptor {
    size: u32,
    value: [u8; HID_MAX_DESCRIPTOR_SIZE],
}
nix::ioctl_read!(hidiocgrdescsize, b'H', 0x01, std::os::raw::c_int);
nix::ioctl_read!(hidiocgrdesc, b'H', 0x02, HidrawReportDescriptor);

// ─────────────────────────────────────────────────────────────────────────────
// Transport abstraction
// ─────────────────────────────────────────────────────────────────────────────

/// The minimal hardware surface the driver needs. Implemented by
/// [`EpomakerTransport`] in production and by a mock in tests, so the frame
/// encoding can be asserted without a device.
#[async_trait]
pub(crate) trait EpomakerIo: Send + Sync + 'static {
    /// Sends one 64-byte Feature report (`SET_REPORT`).
    async fn set_feature(&self, payload: [u8; FRAME_LEN]) -> Result<(), DriverError>;
    /// Reads one 64-byte Feature report (`GET_REPORT`).
    async fn get_feature(&self, report_id: u8) -> Result<[u8; FRAME_LEN], DriverError>;
}

/// Hidraw feature-report transport for the EK68.
///
/// Owns the fd behind an `Arc` so each ioctl can be moved into a
/// `spawn_blocking` closure (the control transfer is synchronous).
pub(crate) struct EpomakerTransport {
    fd: Arc<OwnedFd>,
    device_id: Arc<str>,
    timeout: Duration,
}

impl EpomakerTransport {
    fn new(fd: OwnedFd, device_id: Arc<str>, timeout: Duration) -> Self {
        Self { fd: Arc::new(fd), device_id, timeout }
    }
}

#[async_trait]
impl EpomakerIo for EpomakerTransport {
    async fn set_feature(&self, payload: [u8; FRAME_LEN]) -> Result<(), DriverError> {
        let fd = self.fd.clone();
        let dev = self.device_id.clone();
        let fut = tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 1 + FRAME_LEN];
            buf[0] = REPORT_ID;
            buf[1..].copy_from_slice(&payload);
            // SAFETY: `hidiocsfeature` writes `buf` (report id + payload) to a
            // valid hidraw fd owned for the lifetime of this call via the Arc.
            unsafe { hidiocsfeature(fd.as_raw_fd(), &mut buf) }
                .map(|_| ())
                .map_err(io_from_errno)
        });
        join_with_timeout(fut, self.timeout, &dev, "set_feature").await
    }

    async fn get_feature(&self, report_id: u8) -> Result<[u8; FRAME_LEN], DriverError> {
        let fd = self.fd.clone();
        let dev = self.device_id.clone();
        let fut = tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 1 + FRAME_LEN];
            buf[0] = report_id;
            // SAFETY: `hidiocgfeature` fills `buf` from the device's feature
            // report; buf[0] selects the report id.
            unsafe { hidiocgfeature(fd.as_raw_fd(), &mut buf) }
                .map(|_| {
                    let mut out = [0u8; FRAME_LEN];
                    out.copy_from_slice(&buf[1..]);
                    out
                })
                .map_err(io_from_errno)
        });
        join_with_timeout(fut, self.timeout, &dev, "get_feature").await
    }
}

/// Awaits a blocking ioctl task under a deadline, flattening the
/// timeout / join-error / ioctl-error layers into a single [`DriverError`].
async fn join_with_timeout<T>(
    fut: tokio::task::JoinHandle<Result<T, std::io::Error>>,
    dur: Duration,
    device_id: &str,
    op: &'static str,
) -> Result<T, DriverError> {
    match timeout(dur, fut).await {
        Err(_) => Err(DriverError::Timeout { device: device_id.to_string(), op }),
        Ok(Err(_join)) => Err(DriverError::Io(std::io::Error::other("feature ioctl task panicked"))),
        Ok(Ok(Err(e))) => Err(classify_io_error(e)),
        Ok(Ok(Ok(v))) => Ok(v),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Frame encoders (pure functions — unit tested)
// ─────────────────────────────────────────────────────────────────────────────

/// Whole-keyboard lighting settings carried by one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lighting {
    pub mode: u8,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    /// `0x01`–`0x10`; clamped into range on encode.
    pub brightness: u8,
    pub rainbow: bool,
}

impl Default for Lighting {
    fn default() -> Self {
        Self { mode: MODE_STATIC, r: 0xFF, g: 0xFF, b: 0xFF, brightness: BRIGHTNESS_MAX, rainbow: false }
    }
}

impl Lighting {
    /// Builds the 64-byte lighting payload per the decoded layout.
    fn encode(&self) -> [u8; FRAME_LEN] {
        let mut f = [0u8; FRAME_LEN];
        f[LIGHT_OFF_MODE] = self.mode;
        f[LIGHT_OFF_R] = self.r;
        f[LIGHT_OFF_G] = self.g;
        f[LIGHT_OFF_B] = self.b;
        f[LIGHT_OFF_RAINBOW] = self.rainbow as u8;
        f[LIGHT_OFF_BRIGHTNESS] = self.brightness.clamp(BRIGHTNESS_MIN, BRIGHTNESS_MAX);
        f[LIGHT_OFF_MARKER] = if self.mode == MODE_LED_OFF { LIGHT_MARKER_OFF } else { LIGHT_MARKER_LIT };
        f[LIGHT_OFF_FOOTER..LIGHT_OFF_FOOTER + 2].copy_from_slice(&FOOTER_MAGIC);
        f
    }

    /// Parses a `set_lighting` D-Bus payload: `[mode, r, g, b, brightness?, rainbow?]`.
    /// Missing trailing fields fall back to defaults.
    fn from_bytes(data: &[u8]) -> Lighting {
        let d = Lighting::default();
        Lighting {
            mode: *data.first().unwrap_or(&d.mode),
            r: *data.get(1).unwrap_or(&d.r),
            g: *data.get(2).unwrap_or(&d.g),
            b: *data.get(3).unwrap_or(&d.b),
            brightness: *data.get(4).unwrap_or(&d.brightness),
            rainbow: data.get(5).map(|&x| x != 0).unwrap_or(d.rainbow),
        }
    }

    fn to_bytes(self) -> Vec<u8> {
        vec![self.mode, self.r, self.g, self.b, self.brightness, self.rainbow as u8]
    }
}

/// Encodes the *select* frame that identifies the source key by its factory
/// scancode.
fn encode_select(scancode: u8) -> [u8; FRAME_LEN] {
    let mut f = [0u8; FRAME_LEN];
    f[REMAP_SELECT_OFFSET] = REMAP_MARKER;
    f[REMAP_SELECT_OFFSET + 2] = scancode;
    f
}

/// Encodes the *write* frame carrying the new 16-bit little-endian keycode.
fn encode_remap_write(keycode: u16) -> [u8; FRAME_LEN] {
    let [lo, hi] = keycode.to_le_bytes();
    let mut f = [0u8; FRAME_LEN];
    f[REMAP_WRITE_OFFSET] = REMAP_MARKER;
    f[REMAP_WRITE_OFFSET + 2] = lo;
    f[REMAP_WRITE_OFFSET + 3] = hi;
    f
}

// ─────────────────────────────────────────────────────────────────────────────
// Driver
// ─────────────────────────────────────────────────────────────────────────────

/// Epomaker EK68 driver. Lighting is pushed live; keymap/macros are held in a
/// host-side shadow and flushed on [`KeyboardDriver::commit_to_nvram`]
/// (CLAUDE.md §4.2 legacy-polyfill model), since the device offers no
/// keymap read-back and writes its EEPROM on every change.
pub struct EpomakerDriver {
    io: Box<dyn EpomakerIo>,
    device_id: Arc<str>,
    matrix: (u8, u8),
    layers: u8,
    /// Host-side keymap shadow: `(layer, row, col) -> keycode`.
    keymap: BTreeMap<(u8, u8, u8), u16>,
    /// Positions changed since the last commit.
    dirty: BTreeMap<(u8, u8, u8), u16>,
    lighting: Lighting,
}

impl EpomakerDriver {
    /// Opens interface 0 of an EK68 hidraw node and constructs the driver.
    ///
    /// # Errors
    /// - [`DriverError::PermissionDenied`] / [`DriverError::Disconnected`] from `open(2)`.
    /// - [`DriverError::ProtocolViolation`] if the node is not the vendor
    ///   (64-byte Feature report) interface.
    pub fn new(hidraw_path: &Path, matrix: (u8, u8), layers: u8) -> Result<Self, DriverError> {
        let flags = OFlag::O_RDWR | OFlag::O_CLOEXEC;
        let owned = open(hidraw_path, flags, Mode::empty())
            .map_err(|errno| classify_open_errno(errno, hidraw_path))?;
        probe_vendor_feature_interface(&owned)?;

        let device_id: Arc<str> = hidraw_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown-hidraw")
            .into();
        info!(device_id = %device_id, "attached Epomaker EK68 (vendor feature-report) driver");
        let transport = EpomakerTransport::new(owned, device_id.clone(), DEFAULT_TIMEOUT);
        Ok(Self::with_io(Box::new(transport), device_id, matrix, layers))
    }

    /// Test/seam constructor over an arbitrary [`EpomakerIo`].
    pub(crate) fn with_io(
        io: Box<dyn EpomakerIo>,
        device_id: Arc<str>,
        matrix: (u8, u8),
        layers: u8,
    ) -> Self {
        Self {
            io,
            device_id,
            matrix,
            layers,
            keymap: BTreeMap::new(),
            dirty: BTreeMap::new(),
            lighting: Lighting::default(),
        }
    }

    /// Maps a matrix position to the source key's factory scancode used by
    /// the select frame. Returns `None` for positions whose scancode is not
    /// yet known (the slot→key map is partial — see docs).
    fn scancode_for(&self, _layer: u8, row: u8, col: u8) -> Option<u8> {
        // The reverse-engineering effort confirmed the keyboard addresses keys
        // by their default scancode; the full (row,col)->scancode table is to
        // be completed against hardware. Until then, a frontend may address by
        // scancode directly by encoding it as col on row 0.
        if row == 0 { Some(col) } else { None }
    }
}

#[async_trait]
impl KeyboardDriver for EpomakerDriver {
    async fn get_matrix_dimensions(&mut self) -> Result<(u8, u8), DriverError> {
        Ok(self.matrix)
    }

    async fn get_layer_count(&mut self) -> Result<u8, DriverError> {
        Ok(self.layers)
    }

    async fn get_keycode(&mut self, layer: u8, row: u8, col: u8) -> Result<u16, DriverError> {
        // Shadow read — the device exposes no keymap read-back. Unwritten
        // positions report KC_NO (0x0000); a frontend overlays factory defaults.
        Ok(self.keymap.get(&(layer, row, col)).copied().unwrap_or(0))
    }

    async fn set_keycode(&mut self, layer: u8, row: u8, col: u8, keycode: u16) -> Result<(), DriverError> {
        self.keymap.insert((layer, row, col), keycode);
        self.dirty.insert((layer, row, col), keycode);
        Ok(())
    }

    async fn get_macro_buffer(&mut self, _offset: u16, length: u8) -> Result<Vec<u8>, DriverError> {
        // Macro format not yet decoded — shadow only.
        Ok(vec![0u8; length as usize])
    }

    async fn set_macro_buffer(&mut self, _offset: u16, _data: &[u8]) -> Result<(), DriverError> {
        warn!(device_id = %self.device_id, "EK68 macro write ignored — wire format not yet decoded");
        Ok(())
    }

    /// `command` is unused for the EK68; `data` is `[mode, r, g, b, brightness?, rainbow?]`.
    async fn set_lighting(&mut self, _command: u8, data: &[u8]) -> Result<(), DriverError> {
        let lighting = Lighting::from_bytes(data);
        if lighting.mode > MODE_MAX {
            return Err(DriverError::ProtocolViolation { reason: "EK68 lighting mode out of range (0x00..=0x13)" });
        }
        self.io.set_feature(lighting.encode()).await?;
        self.lighting = lighting;
        debug!(device_id = %self.device_id, mode = lighting.mode, "set EK68 lighting");
        Ok(())
    }

    async fn get_lighting(&mut self, _command: u8) -> Result<Vec<u8>, DriverError> {
        Ok(self.lighting.to_bytes())
    }

    /// Flushes pending keymap edits to the device as `select + write` frame
    /// pairs, then clears the dirty set.
    async fn commit_to_nvram(&mut self) -> Result<(), DriverError> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let pending: Vec<((u8, u8, u8), u16)> = self.dirty.iter().map(|(k, v)| (*k, *v)).collect();
        for ((layer, row, col), keycode) in pending {
            match self.scancode_for(layer, row, col) {
                Some(scancode) => {
                    self.io.set_feature(encode_select(scancode)).await?;
                    self.io.set_feature(encode_remap_write(keycode)).await?;
                }
                None => {
                    warn!(
                        device_id = %self.device_id,
                        layer, row, col,
                        "skipping remap — no known scancode for matrix position (EK68 slot map incomplete)",
                    );
                }
            }
        }
        self.dirty.clear();
        Ok(())
    }

    fn emits_layout_event_per_set(&self) -> bool {
        // Writes are batched until commit, so the event fires once per flush.
        false
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Open / probe helpers
// ─────────────────────────────────────────────────────────────────────────────

fn classify_open_errno(errno: nix::errno::Errno, path: &Path) -> DriverError {
    use nix::errno::Errno;
    match errno {
        Errno::EACCES | Errno::EPERM => DriverError::PermissionDenied { path: path.to_path_buf() },
        Errno::ENODEV | Errno::ENOENT => DriverError::Disconnected,
        other => DriverError::Io(std::io::Error::from_raw_os_error(other as i32)),
    }
}

fn classify_io_error(e: std::io::Error) -> DriverError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::PermissionDenied => DriverError::PermissionDenied { path: PathBuf::new() },
        ErrorKind::NotFound | ErrorKind::BrokenPipe => DriverError::Disconnected,
        _ => DriverError::Io(e),
    }
}

fn io_from_errno(errno: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(errno as i32)
}

/// Confirms the opened hidraw node is the EK68 vendor interface (interface 0),
/// identified by the presence of a 64-byte HID **Feature** report in its
/// descriptor. Interface 1 (consumer/mouse/NKRO) has no such report.
fn probe_vendor_feature_interface(fd: &OwnedFd) -> Result<(), DriverError> {
    let raw_fd = fd.as_raw_fd();
    let mut desc_size: std::os::raw::c_int = 0;
    // SAFETY: reads a single c_int from a valid hidraw fd.
    unsafe { hidiocgrdescsize(raw_fd, &mut desc_size).map_err(|e| DriverError::Io(io_from_errno(e)))? };

    let mut rpt = HidrawReportDescriptor { size: desc_size as u32, value: [0u8; HID_MAX_DESCRIPTOR_SIZE] };
    // SAFETY: fills the struct, size field set from the query above.
    unsafe { hidiocgrdesc(raw_fd, &mut rpt).map_err(|e| DriverError::Io(io_from_errno(e)))? };

    let bytes = &rpt.value[..(rpt.size as usize).min(HID_MAX_DESCRIPTOR_SIZE)];
    if descriptor_has_feature_report(bytes, FRAME_LEN as u32) {
        Ok(())
    } else {
        Err(DriverError::ProtocolViolation {
            reason: "not the EK68 vendor interface (no 64-byte Feature report in HID descriptor)",
        })
    }
}

/// Walks a HID report descriptor and reports whether it declares a **Feature**
/// main item whose Report Count is at least `min_count`.
///
/// HID short items: prefix bits `[1:0]` = data size code (0,1,2 → 0,1,2 bytes;
/// 3 → 4 bytes); `[7:2]` = tag+type. Report Count (global) has tag+type
/// `0x94`; Feature (main) has `0xB0`.
pub(crate) fn descriptor_has_feature_report(descriptor: &[u8], min_count: u32) -> bool {
    let mut i = 0;
    let mut report_count: u32 = 0;
    while i < descriptor.len() {
        let prefix = descriptor[i];
        if prefix == 0xFE {
            // Long item: prefix + size + tag + data.
            if i + 1 >= descriptor.len() {
                break;
            }
            i += 3 + descriptor[i + 1] as usize;
            continue;
        }
        let data_len = match prefix & 0x03 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let read_data = |n: usize| -> u32 {
            let mut v: u32 = 0;
            for k in 0..n {
                if i + 1 + k < descriptor.len() {
                    v |= (descriptor[i + 1 + k] as u32) << (8 * k);
                }
            }
            v
        };
        match prefix & 0xFC {
            0x94 => report_count = read_data(data_len), // Report Count (global)
            0xB0 => {
                // Feature (main)
                if report_count >= min_count {
                    return true;
                }
            }
            _ => {}
        }
        i += 1 + data_len;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records every frame sent, and serves canned get_feature responses.
    struct MockIo {
        sent: Mutex<Vec<[u8; FRAME_LEN]>>,
        fail: bool,
    }
    impl MockIo {
        fn new() -> Self {
            Self { sent: Mutex::new(Vec::new()), fail: false }
        }
        fn failing() -> Self {
            Self { sent: Mutex::new(Vec::new()), fail: true }
        }
        fn sent(&self) -> Vec<[u8; FRAME_LEN]> {
            self.sent.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl EpomakerIo for MockIo {
        async fn set_feature(&self, payload: [u8; FRAME_LEN]) -> Result<(), DriverError> {
            if self.fail {
                return Err(DriverError::Disconnected);
            }
            self.sent.lock().unwrap().push(payload);
            Ok(())
        }
        async fn get_feature(&self, _report_id: u8) -> Result<[u8; FRAME_LEN], DriverError> {
            Ok([0u8; FRAME_LEN])
        }
    }

    fn driver_with(io: Arc<MockIo>) -> EpomakerDriver {
        // Box a thin shim so the test keeps a handle to the Arc for assertions.
        struct Shim(Arc<MockIo>);
        #[async_trait]
        impl EpomakerIo for Shim {
            async fn set_feature(&self, p: [u8; FRAME_LEN]) -> Result<(), DriverError> {
                self.0.set_feature(p).await
            }
            async fn get_feature(&self, r: u8) -> Result<[u8; FRAME_LEN], DriverError> {
                self.0.get_feature(r).await
            }
        }
        EpomakerDriver::with_io(Box::new(Shim(io)), "ek68".into(), (5, 15), 1)
    }

    #[test]
    fn lighting_static_red_layout() {
        let f = Lighting { mode: MODE_STATIC, r: 0xFF, g: 0, b: 0, brightness: 0x10, rainbow: false }.encode();
        assert_eq!(f[LIGHT_OFF_MODE], 0x01);
        assert_eq!(f[LIGHT_OFF_R], 0xFF);
        assert_eq!(f[LIGHT_OFF_G], 0x00);
        assert_eq!(f[LIGHT_OFF_B], 0x00);
        assert_eq!(f[LIGHT_OFF_BRIGHTNESS], 0x10);
        assert_eq!(f[LIGHT_OFF_MARKER], LIGHT_MARKER_LIT);
        assert_eq!(&f[LIGHT_OFF_FOOTER..LIGHT_OFF_FOOTER + 2], &FOOTER_MAGIC);
    }

    #[test]
    fn lighting_off_uses_off_marker() {
        let f = Lighting { mode: MODE_LED_OFF, r: 0, g: 0, b: 0, brightness: 1, rainbow: false }.encode();
        assert_eq!(f[LIGHT_OFF_MARKER], LIGHT_MARKER_OFF);
    }

    #[test]
    fn brightness_is_clamped() {
        let f = Lighting { mode: MODE_STATIC, r: 1, g: 2, b: 3, brightness: 0xFF, rainbow: false }.encode();
        assert_eq!(f[LIGHT_OFF_BRIGHTNESS], BRIGHTNESS_MAX);
        let f0 = Lighting { brightness: 0x00, ..Lighting::default() }.encode();
        assert_eq!(f0[LIGHT_OFF_BRIGHTNESS], BRIGHTNESS_MIN);
    }

    #[test]
    fn remap_write_is_little_endian() {
        let f = encode_remap_write(0x0004); // KC_A
        assert_eq!(f[REMAP_WRITE_OFFSET], REMAP_MARKER);
        assert_eq!(f[REMAP_WRITE_OFFSET + 2], 0x04);
        assert_eq!(f[REMAP_WRITE_OFFSET + 3], 0x00);
    }

    #[test]
    fn select_carries_scancode() {
        let f = encode_select(0x29); // Esc
        assert_eq!(f[REMAP_SELECT_OFFSET], REMAP_MARKER);
        assert_eq!(f[REMAP_SELECT_OFFSET + 2], 0x29);
    }

    #[tokio::test]
    async fn set_lighting_sends_one_frame() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        d.set_lighting(0, &[MODE_STATIC, 0x00, 0xFF, 0x00, 0x08, 0]).await.unwrap();
        let sent = io.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0][LIGHT_OFF_G], 0xFF);
        assert_eq!(sent[0][LIGHT_OFF_BRIGHTNESS], 0x08);
        // get_lighting reflects last set
        assert_eq!(d.get_lighting(0).await.unwrap()[2], 0xFF);
    }

    #[tokio::test]
    async fn out_of_range_mode_rejected() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        let err = d.set_lighting(0, &[0x14, 0, 0, 0]).await.unwrap_err();
        assert!(matches!(err, DriverError::ProtocolViolation { .. }));
        assert!(io.sent().is_empty());
    }

    #[tokio::test]
    async fn set_keycode_is_buffered_until_commit() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        d.set_keycode(0, 0, 0x29, 0x0004).await.unwrap(); // row0,col=scancode Esc -> A
        assert!(io.sent().is_empty(), "writes must not hit the wire before commit");
        assert_eq!(d.get_keycode(0, 0, 0x29).await.unwrap(), 0x0004);

        d.commit_to_nvram().await.unwrap();
        let sent = io.sent();
        assert_eq!(sent.len(), 2, "expected select + write");
        assert_eq!(sent[0][REMAP_SELECT_OFFSET + 2], 0x29); // select Esc scancode
        assert_eq!(sent[1][REMAP_WRITE_OFFSET + 2], 0x04); // write KC_A low byte

        // Buffer cleared — a second commit is a no-op.
        d.commit_to_nvram().await.unwrap();
        assert_eq!(io.sent().len(), 2);
    }

    #[tokio::test]
    async fn commit_skips_unknown_positions() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        d.set_keycode(0, 3, 5, 0x0004).await.unwrap(); // row!=0 -> no known scancode
        d.commit_to_nvram().await.unwrap();
        assert!(io.sent().is_empty());
    }

    #[tokio::test]
    async fn commit_propagates_transport_failure() {
        let io = Arc::new(MockIo::failing());
        let mut d = driver_with(io.clone());
        d.set_keycode(0, 0, 0x29, 0x0004).await.unwrap();
        assert!(matches!(d.commit_to_nvram().await, Err(DriverError::Disconnected)));
    }

    #[test]
    fn descriptor_feature_report_detection() {
        // Report Count 64 (95 40), Report Size 8 (75 08), Feature Var (B1 02).
        let if0 = [0x95u8, 0x40, 0x75, 0x08, 0xB1, 0x02];
        assert!(descriptor_has_feature_report(&if0, 64));
        // No feature item present (just an input report).
        let if1 = [0x95u8, 0x08, 0x75, 0x08, 0x81, 0x02];
        assert!(!descriptor_has_feature_report(&if1, 64));
        // Feature present but smaller than 64.
        let small = [0x95u8, 0x03, 0x75, 0x08, 0xB1, 0x02];
        assert!(!descriptor_has_feature_report(&small, 64));
    }
}
