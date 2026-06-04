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
//! `GET_REPORT(Feature)` — there is no OUT endpoint.
//!
//! The EK68 is firmware-identical to the **Zuoya GMK67** (same `05ac:024f`,
//! interface 0, usage `0001:0006`), reverse-engineered for OpenRGB by Aubry
//! Flora. The full record lives in `docs/protocol/epomaker-ek68.md` §3.6; the
//! load-bearing facts this module encodes:
//!
//! ## Lighting — a multi-frame *transaction* (the crucial point)
//!
//! A colour/effect does **not** render from a single frame. The render is a
//! transaction whose framing commands carry `PACKET_HEADER = 0x04` in byte 0
//! (these are the frames we long mistook for "heartbeats"). For an effect or
//! static colour ([`EpomakerDriver::render_effect`]):
//!
//! ```text
//! 04 18                          SetCustomization(ON)     Send → Read
//! 04 13  [8]=01                  StartEffectPage          Send → Read
//! <mode> [1..3]=RGB [8]=random   the MODE FRAME           Send → Read
//!        [9]=brightness [10]=speed [11]=direction [14..15]=AA 55
//! 04 02                          EndCommunication         Send → Read
//! 04 F0                          StartEffectCommand       Send
//! ```
//!
//! Each `Send` (`SET_REPORT`) is paced by a `Read` (`GET_REPORT`) — the app
//! interleaves them and the firmware appears to expect it. Sending only the
//! middle mode frame (as an earlier revision did) renders nothing. There is
//! **no payload checksum**; `AA 55` at bytes 14–15 is a fixed page-check code.
//!
//! Per-key colour (`Direct` mode `0x20` / `Custom` `0x23`) pushes a 128-slot
//! `index,R,G,B` framebuffer and, for `Direct`, needs a ≤2 s keepalive. That
//! path is not wired through the VIA-shaped [`KeyboardDriver`] trait yet (it
//! needs an engine-side refresh task) and is deferred; see the protocol doc.
//!
//! ## Keymap / remap (format decoded; addressing pending hardware test)
//!
//! A remap is a *select* frame (`02 00 <source-key default scancode>`)
//! followed by a *write* frame (`02 00 <keycode-lo> <keycode-hi>`), keycodes
//! being standard VIA/QMK values. The keyboard identifies the source key by
//! its factory scancode (the write offset is a per-row column position and is
//! **not** a globally-unique slot). The exact offset/select requirement is
//! finalized against real hardware; this module emits the documented frames
//! and keeps a host-side shadow keymap (the legacy-polyfill shadow model,
//! CLAUDE.md §4.2). The GMK67 source names the key-definition area commands
//! (`0x10` read / `0x11` write) as a future lead for read-back.
//!
//! ## Macros
//!
//! Not yet decoded (deferred). The macro methods operate on the shadow buffer
//! only. The GMK67 source names the macro area (`0x14` read / `0x15` write).
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
/// the real maker string is `hfd.cn`, product `EK68`. Firmware-identical to
/// the Zuoya GMK67, which shares this exact VID/PID.
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

// --- Transaction framing (byte 0 = PACKET_HEADER, byte 1 = command) ---------
//
// These are the frames previously misread as "heartbeats". See §3.6.
const PACKET_HEADER: u8 = 0x04;
const CMD_COMMUNICATION_END: u8 = 0x02;
const CMD_WRITE_LED_SPECIAL_EFFECT_AREA: u8 = 0x13;
const CMD_TURN_ON_CUSTOMIZATION: u8 = 0x18;
const CMD_TURN_OFF_CUSTOMIZATION: u8 = 0x19;
const CMD_LED_EFFECT_START: u8 = 0xF0;
/// Number of effect-page packets that follow `StartEffectPage` for a mode write.
const LED_SPECIAL_EFFECT_PACKETS: u8 = 0x01;

// --- Mode-frame layout (the middle frame of the transaction) -----------------
const MF_MODE: usize = 0;
const MF_R: usize = 1;
const MF_G: usize = 2;
const MF_B: usize = 3;
const MF_RANDOM: usize = 8;
const MF_BRIGHTNESS: usize = 9;
const MF_SPEED: usize = 10;
const MF_DIRECTION: usize = 11;
const MF_FOOTER: usize = 14;
const FOOTER_MAGIC: [u8; 2] = [0xAA, 0x55];

/// Effect/mode ids (byte 0 of the mode frame). Effects span `0x01..=0x13`;
/// `0x80` is the dedicated lights-off mode. (`0x20`/`0x23` are the per-key
/// Direct/Custom modes, handled by a separate framebuffer path — not here.)
pub const MODE_STATIC: u8 = 0x01;
/// Highest known effect id (`Back-and-forth`).
pub const MODE_EFFECT_MAX: u8 = 0x13;
/// Dedicated lights-off mode.
pub const MODE_LIGHTS_OFF: u8 = 0x80;

const BRIGHTNESS_MIN: u8 = 0x00;
const BRIGHTNESS_MAX: u8 = 0x0F;
const SPEED_MAX: u8 = 0x0F;

/// Whether `mode` is a renderable whole-keyboard mode this driver accepts.
fn is_renderable_mode(mode: u8) -> bool {
    (MODE_STATIC..=MODE_EFFECT_MAX).contains(&mode) || mode == MODE_LIGHTS_OFF
}

// --- Key-definition (remap) area (confirmed on hardware — see §3.8) ---
//
// A remap writes the 9-page (576-byte) key-definition area (cmd `0x11`), wrapped
// in the same transaction as lighting. Each key's 4-byte entry sits at absolute
// offset `key_index * 4`: `[0x02, 0x00, kc_lo, kc_hi]` (16-bit LE VIA/QMK code).
// Zero entries keep the factory default, so a commit sends the *full* keymap.
const CMD_WRITE_KEY_DEFINITION_AREA: u8 = 0x11;
const KEY_DEF_PAGES: usize = 9;
const KEY_DEF_BUF_LEN: usize = KEY_DEF_PAGES * FRAME_LEN; // 576
const KEY_DEF_ENTRY_MARKER: u8 = 0x02;

/// Sentinel for a matrix cell with no key.
const NA: u8 = 0xFF;
const EK68_ROWS: usize = 5;
const EK68_COLS: usize = 15;

/// Physical `(row, col)` → firmware `key_index`, from the vendor
/// `KeyboardLayout.xml` (docs/protocol/epomaker-ek68.md §3.7). Rows are
/// top→bottom, cols left→right; `NA` marks an empty cell. The same index is the
/// LED slot (`key_index == light_index` on this board).
const KEY_INDEX: [[u8; EK68_COLS]; EK68_ROWS] = [
    [1, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 103, NA],
    [37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 67, 119],
    [55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 85, 118, NA],
    [73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 101, 121, NA],
    [91, 92, 93, 94, 95, 96, 99, 100, 102, NA, NA, NA, NA, NA, NA],
];

/// Maps a matrix position to the firmware `key_index`, or `None` for an empty
/// cell / out-of-bounds.
fn key_index_for(row: u8, col: u8) -> Option<u8> {
    let ki = *KEY_INDEX.get(row as usize)?.get(col as usize)?;
    (ki != NA).then_some(ki)
}

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

/// Whole-keyboard lighting settings carried by the transaction's mode frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lighting {
    pub mode: u8,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    /// `0x00`–`0x0F`; clamped into range on encode.
    pub brightness: u8,
    /// `0x00`–`0x0F`; only meaningful for animated effects.
    pub speed: u8,
    /// Per-mode direction selector (gradient / sequence / back-and-forth).
    pub direction: u8,
    /// Random-colour flag (byte 8); replaces the old "rainbow" name.
    pub random: bool,
}

impl Default for Lighting {
    fn default() -> Self {
        Self {
            mode: MODE_STATIC,
            r: 0xFF,
            g: 0xFF,
            b: 0xFF,
            brightness: BRIGHTNESS_MAX,
            speed: 0x00,
            direction: 0x00,
            random: false,
        }
    }
}

impl Lighting {
    /// Builds the 64-byte mode frame (the middle of the render transaction).
    fn encode(&self) -> [u8; FRAME_LEN] {
        let mut f = [0u8; FRAME_LEN];
        f[MF_MODE] = self.mode;
        f[MF_R] = self.r;
        f[MF_G] = self.g;
        f[MF_B] = self.b;
        f[MF_RANDOM] = self.random as u8;
        f[MF_BRIGHTNESS] = self.brightness.clamp(BRIGHTNESS_MIN, BRIGHTNESS_MAX);
        f[MF_SPEED] = self.speed.min(SPEED_MAX);
        f[MF_DIRECTION] = self.direction;
        f[MF_FOOTER..MF_FOOTER + 2].copy_from_slice(&FOOTER_MAGIC);
        f
    }

    /// Parses a `set_lighting` D-Bus payload:
    /// `[mode, r, g, b, brightness?, random?, speed?, direction?]`.
    /// Missing trailing fields fall back to defaults.
    fn from_bytes(data: &[u8]) -> Lighting {
        let d = Lighting::default();
        Lighting {
            mode: *data.first().unwrap_or(&d.mode),
            r: *data.get(1).unwrap_or(&d.r),
            g: *data.get(2).unwrap_or(&d.g),
            b: *data.get(3).unwrap_or(&d.b),
            brightness: *data.get(4).unwrap_or(&d.brightness),
            random: data.get(5).map(|&x| x != 0).unwrap_or(d.random),
            speed: *data.get(6).unwrap_or(&d.speed),
            direction: *data.get(7).unwrap_or(&d.direction),
        }
    }

    fn to_bytes(self) -> Vec<u8> {
        vec![
            self.mode,
            self.r,
            self.g,
            self.b,
            self.brightness,
            self.random as u8,
            self.speed,
            self.direction,
        ]
    }
}

/// A framing command frame: byte 0 = [`PACKET_HEADER`], byte 1 = `command`.
fn encode_cmd(command: u8) -> [u8; FRAME_LEN] {
    let mut f = [0u8; FRAME_LEN];
    f[0] = PACKET_HEADER;
    f[1] = command;
    f
}

/// `StartEffectPage`: announces `LED_SPECIAL_EFFECT_PACKETS` follow-on packets.
fn encode_start_effect_page() -> [u8; FRAME_LEN] {
    let mut f = encode_cmd(CMD_WRITE_LED_SPECIAL_EFFECT_AREA);
    f[8] = LED_SPECIAL_EFFECT_PACKETS;
    f
}

/// `StartKeyDefPage`: announces the 9 key-definition packets that follow.
fn encode_start_keydef_page() -> [u8; FRAME_LEN] {
    let mut f = encode_cmd(CMD_WRITE_KEY_DEFINITION_AREA);
    f[8] = KEY_DEF_PAGES as u8; // 0x09
    f
}

/// Builds the 9-page key-definition buffer from `entries` (`key_index ->
/// keycode`). Each entry is `[0x02, 0x00, kc_lo, kc_hi]` at offset
/// `key_index * 4`; the last page carries the `AA 55` check code.
fn encode_keydef_buffer(entries: &[(u8, u16)]) -> [u8; KEY_DEF_BUF_LEN] {
    let mut buf = [0u8; KEY_DEF_BUF_LEN];
    for &(ki, kc) in entries {
        let o = ki as usize * 4;
        if o + 3 < KEY_DEF_BUF_LEN {
            let [lo, hi] = kc.to_le_bytes();
            buf[o] = KEY_DEF_ENTRY_MARKER;
            buf[o + 2] = lo;
            buf[o + 3] = hi;
        }
    }
    buf[KEY_DEF_BUF_LEN - 2] = FOOTER_MAGIC[0]; // page 8, byte 62
    buf[KEY_DEF_BUF_LEN - 1] = FOOTER_MAGIC[1]; // page 8, byte 63
    buf
}

// ─────────────────────────────────────────────────────────────────────────────
// Driver
// ─────────────────────────────────────────────────────────────────────────────

/// Epomaker EK68 driver. Lighting is pushed live as the full render
/// transaction; keymap/macros are held in a host-side shadow and flushed on
/// [`KeyboardDriver::commit_to_nvram`] (CLAUDE.md §4.2 legacy-polyfill model),
/// since the device offers no keymap read-back and writes its EEPROM on every
/// change.
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

    /// Issues the paced `GET_REPORT` that the app interleaves between writes.
    /// Best-effort: the firmware appears to want the control-read to advance
    /// the transaction, but its contents are not acted on, so a failed read is
    /// logged and ignored rather than aborting a half-applied transaction.
    async fn read_ack(&self) {
        if let Err(e) = self.io.get_feature(REPORT_ID).await {
            debug!(device_id = %self.device_id, error = %e, "EK68 transaction read-ack failed (ignored)");
        }
    }

    /// Runs the full effect/static render transaction (§3.6 path A). Without
    /// the surrounding `0x04`-header framing the mode frame renders nothing.
    async fn render_effect(&mut self, lighting: Lighting) -> Result<(), DriverError> {
        self.io.set_feature(encode_cmd(CMD_TURN_ON_CUSTOMIZATION)).await?;
        self.read_ack().await;
        self.io.set_feature(encode_start_effect_page()).await?;
        self.read_ack().await;
        self.io.set_feature(lighting.encode()).await?;
        self.read_ack().await;
        self.io.set_feature(encode_cmd(CMD_COMMUNICATION_END)).await?;
        self.read_ack().await;
        self.io.set_feature(encode_cmd(CMD_LED_EFFECT_START)).await?;
        Ok(())
    }

    /// Sends the full key-definition (remap) transaction for `entries`
    /// (`key_index -> keycode`): customization-on → start-keydef-page → 9 pages
    /// → end → effect-start, each `Send` paced by a `Read` (§3.8).
    async fn commit_keymap(&self, entries: &[(u8, u16)]) -> Result<(), DriverError> {
        let buf = encode_keydef_buffer(entries);
        self.io.set_feature(encode_cmd(CMD_TURN_ON_CUSTOMIZATION)).await?;
        self.read_ack().await;
        self.io.set_feature(encode_start_keydef_page()).await?;
        self.read_ack().await;
        for p in 0..KEY_DEF_PAGES {
            let mut page = [0u8; FRAME_LEN];
            page.copy_from_slice(&buf[p * FRAME_LEN..(p + 1) * FRAME_LEN]);
            self.io.set_feature(page).await?;
            self.read_ack().await;
        }
        self.io.set_feature(encode_cmd(CMD_COMMUNICATION_END)).await?;
        self.read_ack().await;
        self.io.set_feature(encode_cmd(CMD_LED_EFFECT_START)).await?;
        Ok(())
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

    /// `command` is unused for the EK68; `data` is
    /// `[mode, r, g, b, brightness?, random?, speed?, direction?]`. The frame
    /// is rendered via the full transaction (not a single write).
    async fn set_lighting(&mut self, _command: u8, data: &[u8]) -> Result<(), DriverError> {
        let lighting = Lighting::from_bytes(data);
        if !is_renderable_mode(lighting.mode) {
            return Err(DriverError::ProtocolViolation {
                reason: "EK68 lighting mode out of range (effects 0x01..=0x13, or 0x80 off)",
            });
        }
        self.render_effect(lighting).await?;
        self.lighting = lighting;
        debug!(device_id = %self.device_id, mode = lighting.mode, "set EK68 lighting");
        Ok(())
    }

    async fn get_lighting(&mut self, _command: u8) -> Result<Vec<u8>, DriverError> {
        Ok(self.lighting.to_bytes())
    }

    /// Flushes the keymap to the device's key-definition area (§3.8). Sends the
    /// **full base-layer keymap** (not just the dirty set): zero entries keep the
    /// factory default, so re-sending everything avoids resetting previously
    /// remapped keys.
    async fn commit_to_nvram(&mut self) -> Result<(), DriverError> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        let mut entries: Vec<(u8, u16)> = Vec::new();
        let mut fn_layer_skips: u32 = 0;
        for (&(layer, row, col), &keycode) in self.keymap.iter() {
            if layer != 0 {
                // Fn-layer placement in the key-def area is not yet decoded (§3.8).
                fn_layer_skips += 1;
                continue;
            }
            match key_index_for(row, col) {
                Some(ki) => entries.push((ki, keycode)),
                None => warn!(
                    device_id = %self.device_id,
                    row, col, "skipping remap — no key at matrix position",
                ),
            }
        }
        if fn_layer_skips > 0 {
            warn!(
                device_id = %self.device_id,
                count = fn_layer_skips,
                "EK68 Fn-layer edits not flushed — key-def placement for layer 1 is undecoded",
            );
        }
        if entries.is_empty() {
            self.dirty.clear();
            return Ok(());
        }
        self.commit_keymap(&entries).await?;
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
        EpomakerDriver::with_io(Box::new(Shim(io)), "ek68".into(), (5, 15), 2)
    }

    #[test]
    fn lighting_static_red_layout() {
        let f = Lighting {
            mode: MODE_STATIC,
            r: 0xFF,
            g: 0,
            b: 0,
            brightness: 0x0F,
            speed: 0x08,
            direction: 0x01,
            random: false,
        }
        .encode();
        assert_eq!(f[MF_MODE], 0x01);
        assert_eq!(f[MF_R], 0xFF);
        assert_eq!(f[MF_G], 0x00);
        assert_eq!(f[MF_B], 0x00);
        assert_eq!(f[MF_RANDOM], 0x00);
        assert_eq!(f[MF_BRIGHTNESS], 0x0F);
        assert_eq!(f[MF_SPEED], 0x08);
        assert_eq!(f[MF_DIRECTION], 0x01);
        assert_eq!(&f[MF_FOOTER..MF_FOOTER + 2], &FOOTER_MAGIC);
    }

    #[test]
    fn lighting_off_mode_is_0x80() {
        let f = Lighting { mode: MODE_LIGHTS_OFF, ..Lighting::default() }.encode();
        assert_eq!(f[MF_MODE], 0x80);
        assert_eq!(&f[MF_FOOTER..MF_FOOTER + 2], &FOOTER_MAGIC);
    }

    #[test]
    fn brightness_and_speed_are_clamped() {
        let f = Lighting { brightness: 0xFF, speed: 0xFF, ..Lighting::default() }.encode();
        assert_eq!(f[MF_BRIGHTNESS], BRIGHTNESS_MAX);
        assert_eq!(f[MF_SPEED], SPEED_MAX);
        let f0 = Lighting { brightness: 0x00, ..Lighting::default() }.encode();
        assert_eq!(f0[MF_BRIGHTNESS], BRIGHTNESS_MIN);
    }

    #[test]
    fn key_index_for_maps_matrix() {
        assert_eq!(key_index_for(0, 0), Some(1)); // Esc
        assert_eq!(key_index_for(2, 7), Some(62)); // J
        assert_eq!(key_index_for(4, 3), Some(94)); // Space
        assert_eq!(key_index_for(4, 9), None); // empty cell
        assert_eq!(key_index_for(0, 14), None); // empty cell
        assert_eq!(key_index_for(9, 9), None); // out of bounds
    }

    #[test]
    fn keydef_buffer_places_entry_at_index_times_4() {
        // Esc (key_index 1) and Space (key_index 94) -> KC_A (little-endian).
        let buf = encode_keydef_buffer(&[(1, 0x0004), (94, 0x0004)]);
        assert_eq!(buf[1 * 4], KEY_DEF_ENTRY_MARKER);
        assert_eq!(buf[1 * 4 + 2], 0x04);
        assert_eq!(buf[1 * 4 + 3], 0x00);
        assert_eq!(buf[94 * 4], KEY_DEF_ENTRY_MARKER);
        assert_eq!(buf[94 * 4 + 2], 0x04);
        // Check code lives on the last page (bytes 62/63 of page 8).
        assert_eq!(buf[KEY_DEF_BUF_LEN - 2], FOOTER_MAGIC[0]);
        assert_eq!(buf[KEY_DEF_BUF_LEN - 1], FOOTER_MAGIC[1]);
        assert_eq!(buf.len(), 9 * FRAME_LEN);
    }

    #[tokio::test]
    async fn set_lighting_runs_full_transaction() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        d.set_lighting(0, &[MODE_STATIC, 0x00, 0xFF, 0x00, 0x08, 0]).await.unwrap();

        let sent = io.sent();
        assert_eq!(sent.len(), 5, "transaction = customization, page, mode, end, start");
        // 1. SetCustomization(ON)
        assert_eq!(sent[0][0], PACKET_HEADER);
        assert_eq!(sent[0][1], CMD_TURN_ON_CUSTOMIZATION);
        // 2. StartEffectPage
        assert_eq!(sent[1][0], PACKET_HEADER);
        assert_eq!(sent[1][1], CMD_WRITE_LED_SPECIAL_EFFECT_AREA);
        assert_eq!(sent[1][8], LED_SPECIAL_EFFECT_PACKETS);
        // 3. the mode frame
        assert_eq!(sent[2][MF_MODE], MODE_STATIC);
        assert_eq!(sent[2][MF_G], 0xFF);
        assert_eq!(sent[2][MF_BRIGHTNESS], 0x08);
        assert_eq!(&sent[2][MF_FOOTER..MF_FOOTER + 2], &FOOTER_MAGIC);
        // 4. EndCommunication
        assert_eq!(sent[3][0], PACKET_HEADER);
        assert_eq!(sent[3][1], CMD_COMMUNICATION_END);
        // 5. StartEffectCommand
        assert_eq!(sent[4][0], PACKET_HEADER);
        assert_eq!(sent[4][1], CMD_LED_EFFECT_START);

        // get_lighting reflects the last set.
        assert_eq!(d.get_lighting(0).await.unwrap()[2], 0xFF);
    }

    #[tokio::test]
    async fn out_of_range_mode_rejected() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        // 0x14 is just past the effect range and is not the 0x80 off mode.
        let err = d.set_lighting(0, &[0x14, 0, 0, 0]).await.unwrap_err();
        assert!(matches!(err, DriverError::ProtocolViolation { .. }));
        assert!(io.sent().is_empty());
    }

    #[tokio::test]
    async fn lighting_transaction_propagates_failure() {
        let io = Arc::new(MockIo::failing());
        let mut d = driver_with(io.clone());
        assert!(matches!(
            d.set_lighting(0, &[MODE_STATIC, 0xFF, 0, 0]).await,
            Err(DriverError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn set_keycode_is_buffered_until_commit() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        d.set_keycode(0, 0, 0, 0x0004).await.unwrap(); // Esc (key_index 1) -> A
        assert!(io.sent().is_empty(), "writes must not hit the wire before commit");
        assert_eq!(d.get_keycode(0, 0, 0).await.unwrap(), 0x0004);

        d.commit_to_nvram().await.unwrap();
        let sent = io.sent();
        // transaction = 04 18, 04 11, 9 pages, 04 02, 04 f0 = 13 frames.
        assert_eq!(sent.len(), 2 + KEY_DEF_PAGES + 2);
        assert_eq!((sent[0][0], sent[0][1]), (PACKET_HEADER, CMD_TURN_ON_CUSTOMIZATION));
        assert_eq!((sent[1][0], sent[1][1]), (PACKET_HEADER, CMD_WRITE_KEY_DEFINITION_AREA));
        assert_eq!(sent[1][8], KEY_DEF_PAGES as u8);
        // Esc entry at offset 4 lives in page 0 (frame index 2).
        assert_eq!(sent[2][4], KEY_DEF_ENTRY_MARKER);
        assert_eq!(sent[2][6], 0x04);
        // Check code on the last page (frame index 2+8 = 10).
        assert_eq!(sent[2 + KEY_DEF_PAGES - 1][62], 0xAA);
        assert_eq!(sent[2 + KEY_DEF_PAGES - 1][63], 0x55);
        // Trailer.
        assert_eq!((sent[11][0], sent[11][1]), (PACKET_HEADER, CMD_COMMUNICATION_END));
        assert_eq!((sent[12][0], sent[12][1]), (PACKET_HEADER, CMD_LED_EFFECT_START));

        // Buffer cleared — a second commit is a no-op.
        d.commit_to_nvram().await.unwrap();
        assert_eq!(io.sent().len(), 13);
    }

    #[tokio::test]
    async fn commit_sends_full_keymap_not_just_dirty() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        d.set_keycode(0, 0, 0, 0x0004).await.unwrap(); // Esc -> A
        d.set_keycode(0, 2, 7, 0x0005).await.unwrap(); // J (key_index 62) -> B
        d.commit_to_nvram().await.unwrap();
        let sent = io.sent();
        // page 0 has Esc; page for J (62*4=248 -> page 3, offset 56) has J.
        assert_eq!(sent[2][4], KEY_DEF_ENTRY_MARKER); // Esc in page 0
        assert_eq!(sent[2 + 3][56], KEY_DEF_ENTRY_MARKER); // J in page 3
        assert_eq!(sent[2 + 3][58], 0x05); // KC_B low byte
    }

    #[tokio::test]
    async fn commit_skips_when_only_unknown_positions() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        d.set_keycode(0, 4, 9, 0x0004).await.unwrap(); // empty matrix cell
        d.commit_to_nvram().await.unwrap();
        assert!(io.sent().is_empty());
    }

    #[tokio::test]
    async fn commit_propagates_transport_failure() {
        let io = Arc::new(MockIo::failing());
        let mut d = driver_with(io.clone());
        d.set_keycode(0, 0, 0, 0x0004).await.unwrap(); // Esc -> A (known position)
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
