#![allow(dead_code)]
// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Logitech HID++ 2.0 legacy polyfill driver (G915 / Lightspeed family).
//!
//! # Protocol Overview
//!
//! Logitech gaming keyboards do not speak VIA. Their configuration channel is
//! **HID++ 2.0**: numbered interrupt reports (`0x10` short / `0x11` long)
//! exchanged over the hidraw node of either the keyboard itself (wired) or
//! its **Lightspeed receiver**. Every request addresses a *device index*
//! (`0xFF` wired, `0x01`–`0x06` for a receiver slot) and a *feature index*
//! resolved at attach time through the IRoot feature (`0x0000`).
//!
//! The full reverse-engineering record for this module lives in
//! `docs/protocol/logitech-g915-hidpp.md`. Nothing in it has been confirmed
//! against physical hardware yet — the wire formats are assembled from the
//! public libratbag and Solaar reimplementations and are flagged accordingly;
//! the provisional G-key sector layout (doc §5.3) sits behind pure functions
//! so a corrected offset table is a constants-only change.
//!
//! ## Keymap — an Onboard Profiles blob (the crucial point)
//!
//! Remaps are not per-key writes. The board stores *profiles* as binary
//! sectors in onboard flash (feature `0x8100`); the host compiles the whole
//! profile image — G-key bindings patched into the baseline sector read at
//! attach, CRC-CCITT recomputed over the tail — and pushes it monolithically:
//!
//! ```text
//! memoryAddrWrite(sector, 0, len)      open write session
//! memoryWrite(16 bytes) × n            sector payload in 16-byte chunks
//! memoryWriteEnd()                     commit
//! ```
//!
//! This is exactly the shadow-state compile-and-push model (README §4,
//! CLAUDE.md §4.2): `set_keycode` only marks the [`ShadowState`] dirty, the
//! engine worker debounces for 500 ms, and [`KeyboardDriver::commit_to_nvram`]
//! performs the blob push with rollback on failure.
//!
//! The matrix model is `layers × 1 row × G-key columns`: clackd layers 0–2
//! map to the M1/M2/M3 G-key banks, columns to G1…Gn. The rest of the
//! keyboard is not remappable through onboard profiles.
//!
//! ## Lighting — RGB Effects (`0x8071`)
//!
//! Whole-cluster effects are applied live (RAM-only persistence flag) on
//! every `set_lighting` and re-sent with the flash flag from
//! `commit_to_nvram` — the same wear-levelling split the VIA driver gets
//! from `id_custom_save`. The VIA `(channel, value_id)` compatibility layer
//! matches the GMK67 driver; shared helpers live in the `legacy` parent
//! module.
//!
//! ## Macros
//!
//! Deferred. The macro methods return [`DriverError::Unsupported`] (D-Bus
//! `NotSupported`) rather than pretending to store data.
//!
//! # IO Discipline
//!
//! HID++ frames ride the interrupt pipe, so the transport is
//! [`tokio::io::unix::AsyncFd`] over an `O_NONBLOCK` fd — the same pattern as
//! the VIA driver, **not** the GMK67 feature-report ioctls, and never
//! `tokio::fs` (CLAUDE.md §4.1). The receiver multiplexes unsolicited
//! notifications (device arrival/departure, battery…) onto the same node;
//! the read loop skips frames that do not match the pending request's
//! `(device_index, feature_index, function, sw_id)` and the whole exchange
//! is bounded by a per-call 1000 ms `tokio::time::timeout`.
//!
//! # Shadow State (CLAUDE.md §4.2)
//!
//! Uses the [`ShadowState`](super::ShadowState) infrastructure from the
//! `legacy` parent module: JSON persistence at
//! `$XDG_DATA_HOME/clackd/046d_g915.json`, dirty tracking,
//! snapshot/rollback on commit, `CacheStatus` tracking. At attach the driver
//! reads the active profile sector back and — when its CRC checks out —
//! adopts the decoded G-key bindings as the confirmed baseline, mirroring
//! the GMK67 `sync_eeprom` parity.
//!
//! # Stability — EXPERIMENTAL, opt-in only
//!
//! Nothing in this module has been exercised against physical hardware.
//! The backend is therefore **gated**: `build_driver_table` (src/main.rs)
//! only selects it when the `devices.toml` entry sets both
//! `driver = "logitech"` and `experimental = true`; without the flag the
//! entry falls back to VIA (whose usage-page probe rejects the node,
//! leaving the device untouched). Two safety rails limit the blast radius
//! for testers: commits are *refused* (`VendorCompile`) unless a
//! CRC-valid baseline sector was read from the device — the driver never
//! fabricates a profile image — and a failed push rolls the shadow back to
//! last-known-good. Remove the gate only once the doc's status table has
//! flipped to hardware-confirmed.
//!
//! # Future Work (leads for the next iterations)
//!
//! Ranked roughly by value; the protocol doc §8 carries the wire-level
//! detail for each:
//!
//! 1. **Hardware confirmation.** Capture a G HUB G-key remap and a
//!    lighting change on a real G915; the G-key slot layout
//!    ([`GKEY_BANK_BASE`]/[`GKEY_BANK_STRIDE`]) and the
//!    `setClusterEffect` parameter order are the two provisional guesses.
//!    Both sit behind pure functions, so corrections are constants-only.
//! 2. **Macros.** Onboard profiles reserve macro sectors and the binding
//!    table has a macro type; wiring them to
//!    `get_macro_buffer`/`set_macro_buffer` (currently `NotSupported`)
//!    would complete the VIA surface.
//! 3. **Effect-table discovery.** `getInfo` (`0x8071` fn `0x0`) enumerates
//!    each cluster's real effect list; today the `mode` byte is a raw
//!    effect index. Discovering the table would let the driver map VIA
//!    effect ids robustly and address clusters beyond
//!    [`RGB_CLUSTER_PRIMARY`].
//! 4. **Receiver arrival notifications.** Device-arrival (`0x41`) frames
//!    are currently skipped by the reply matcher; surfacing them to the
//!    supervisor would re-attach a keyboard the moment it powers on
//!    instead of waiting out the exponential backoff.
//! 5. **Battery surface.** Features `0x1000`/`0x1004` are one call each;
//!    needs a trait-level home first (the VIA contract has none).
//! 6. **Profile management.** Only the *active* profile sector is edited.
//!    The profile directory (sector 0) plus `setCurrentProfile` are the
//!    natural backend for mission 6 (named profiles / hot-swap).
//! 7. **Very-long reports (`0x12`).** Some newer firmwares prefer the
//!    64-byte report; the transport reads it but never transmits one.

use std::collections::BTreeMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use tokio::io::unix::AsyncFd;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::{
    LightingState, ShadowState, VIA_CHANNEL_PACKED, VIA_CHANNEL_RGB_MATRIX, VIA_VALUE_BRIGHTNESS,
    VIA_VALUE_COLOR, VIA_VALUE_EFFECT, VIA_VALUE_SPEED, hs_to_rgb, rgb_to_hs,
};
use crate::hal::via::hid_descriptor_has_usage_page;
use crate::hal::{DriverError, KeyboardDriver};

/// USB vendor id shared by every Logitech device.
pub const LOGITECH_VID: u16 = 0x046D;
/// Lightspeed receiver PID shipped with the G915. Receiver PIDs vary per
/// bundle/revision — the actual binding is config-driven (`devices.toml`).
pub const G915_RECEIVER_PID: u16 = 0xC541;

/// Stable stem for the shadow JSON file. Deliberately not the hidraw sysname
/// (session-scoped) and not the PID (differs between the receiver and a
/// cable connection to the *same* keyboard).
const PERSIST_ID: &str = "046d_g915";

// --- HID++ framing (doc §2) --------------------------------------------------

const REPORT_ID_SHORT: u8 = 0x10;
const REPORT_ID_LONG: u8 = 0x11;
const SHORT_LEN: usize = 7;
const LONG_LEN: usize = 20;
/// Parameter bytes in a long frame.
const LONG_PARAMS: usize = 16;
/// Host-chosen 4-bit software id echoed by the firmware; distinguishes our
/// replies from other HID++ clients and from notifications.
const SW_ID: u8 = 0x0A;

/// Device index of a cable-connected device.
const DEV_IDX_WIRED: u8 = 0xFF;
/// Receiver pairing slots.
const RECEIVER_SLOT_FIRST: u8 = 0x01;
const RECEIVER_SLOT_LAST: u8 = 0x06;

/// HID++ 2.0 error marker (third byte of the error reply).
const ERR_HIDPP2_MARKER: u8 = 0xFF;
/// HID++ 1.0 error sub-id (receiver-generated errors).
const ERR_HIDPP1_SUBID: u8 = 0x8F;

// HID++ 2.0 error codes (doc §2.1).
const HIDPP2_ERR_INVALID_ARGUMENT: u8 = 2;
const HIDPP2_ERR_OUT_OF_RANGE: u8 = 3;
const HIDPP2_ERR_INVALID_FEATURE_INDEX: u8 = 6;
const HIDPP2_ERR_INVALID_FUNCTION_ID: u8 = 7;
const HIDPP2_ERR_BUSY: u8 = 8;
const HIDPP2_ERR_UNSUPPORTED: u8 = 9;

// HID++ 1.0 error codes (doc §2.1) — the "device unreachable" family.
const HIDPP1_ERR_CONNECT_FAIL: u8 = 4;
const HIDPP1_ERR_UNKNOWN_DEVICE: u8 = 8;
const HIDPP1_ERR_RESOURCE_ERROR: u8 = 9;

/// Per-call deadline for a HID++ round trip. Matches the VIA/GMK67 figure
/// (CLAUDE.md §4.1).
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

/// Maximum HID report descriptor size per `linux/hid.h`.
const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;

/// HID++ vendor usage pages: `0xFF00` on receivers, `0xFF43` on modern
/// wired devices. Either marks the configuration interface.
const HIDPP_USAGE_PAGE_RECEIVER: u16 = 0xFF00;
const HIDPP_USAGE_PAGE_WIRED: u16 = 0xFF43;

// --- Features and functions (doc §3–§6) --------------------------------------

const FEAT_ROOT: u16 = 0x0000;
const FEAT_DEVICE_NAME: u16 = 0x0005;
const FEAT_RGB_EFFECTS: u16 = 0x8071;
const FEAT_ONBOARD_PROFILES: u16 = 0x8100;

/// IRoot is always at feature index 0.
const FEAT_IDX_ROOT: u8 = 0x00;
const ROOT_GET_FEATURE: u8 = 0x0;
const ROOT_PING: u8 = 0x1;
/// Arbitrary byte echoed by a HID++ 2.0 ping reply (third param).
const PING_MAGIC: u8 = 0x5A;

// 0x0005 Device Name.
const NAME_GET_COUNT: u8 = 0x0;
const NAME_GET_CHUNK: u8 = 0x1;
/// Sanity cap on the name length the driver will read (3 chunks).
const NAME_MAX_LEN: usize = 48;

// 0x8100 Onboard Profiles.
const ONB_GET_DESCRIPTION: u8 = 0x0;
const ONB_SET_MODE: u8 = 0x1;
const ONB_GET_MODE: u8 = 0x2;
const ONB_SET_CURRENT_PROFILE: u8 = 0x3;
const ONB_GET_CURRENT_PROFILE: u8 = 0x4;
const ONB_MEMORY_READ: u8 = 0x5;
const ONB_MEMORY_ADDR_WRITE: u8 = 0x6;
const ONB_MEMORY_WRITE: u8 = 0x7;
const ONB_MEMORY_WRITE_END: u8 = 0x8;
/// `setOnboardMode` argument: profiles applied from onboard flash.
const ONBOARD_MODE_ONBOARD: u8 = 0x01;
/// Onboard memory moves in 16-byte chunks.
const MEM_CHUNK: usize = 16;
/// Fallback profile sector when `getCurrentProfile` reports none active.
const PROFILE_SECTOR_DEFAULT: u16 = 0x0001;
/// Upper bound accepted from `getDescription` before we call the topology
/// implausible (largest sector libratbag has observed is well below this).
const SECTOR_SIZE_MAX: usize = 4096;

// 0x8071 RGB Effects.
const RGB_GET_INFO: u8 = 0x0;
const RGB_SET_CLUSTER_EFFECT: u8 = 0x1;
/// Primary cluster (whole keyboard). Per-cluster addressing is a later stage.
const RGB_CLUSTER_PRIMARY: u8 = 0x00;
/// Persistence flag values for `setClusterEffect`.
const RGB_PERSIST_RAM: u8 = 0x00;
const RGB_PERSIST_FLASH: u8 = 0x01;
/// Effect index 0 is "off" on the boards inspected by libratbag/Solaar.
pub const EFFECT_OFF: u8 = 0x00;
/// Effect index 1 is fixed-colour.
pub const EFFECT_FIXED: u8 = 0x01;

// --- G-key binding layout inside the profile sector (doc §5.3, PROVISIONAL) --

/// First G-key binding slot in the profile sector.
const GKEY_BANK_BASE: usize = 0x20;
/// Stride between M-key banks.
const GKEY_BANK_STRIDE: usize = 0x40;
/// Bytes per binding entry.
const GKEY_ENTRY_LEN: usize = 4;
/// M-key banks the layout provides for (M1/M2/M3 = clackd layers 0–2).
const GKEY_BANKS: u8 = 3;
/// Binding slots a bank can hold (`GKEY_BANK_STRIDE / GKEY_ENTRY_LEN`).
const GKEY_SLOTS_PER_BANK: u8 = (GKEY_BANK_STRIDE / GKEY_ENTRY_LEN) as u8;
/// Binding entry type: key/button press.
const BIND_TYPE_KEY: u8 = 0x80;
/// Binding entry subtype: keyboard HID usage with modifier mask.
const BIND_SUBTYPE_HID_KEY: u8 = 0x02;

// ─────────────────────────────────────────────────────────────────────────────
// Pure frame helpers (unit tested)
// ─────────────────────────────────────────────────────────────────────────────

/// The `(device, feature, function|sw_id)` triple a reply must echo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingRequest {
    device_idx: u8,
    feature_idx: u8,
    func_sw: u8,
}

impl PendingRequest {
    fn new(device_idx: u8, feature_idx: u8, func: u8) -> Self {
        Self {
            device_idx,
            feature_idx,
            func_sw: (func << 4) | SW_ID,
        }
    }
}

/// What an incoming frame means relative to the pending request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplyKind {
    /// The matching reply; carries the (zero-padded) parameter bytes.
    Match([u8; LONG_PARAMS]),
    /// HID++ 2.0 error from the device, with its error code.
    Hidpp2Error(u8),
    /// HID++ 1.0 error from the receiver, with its error code.
    Hidpp1Error(u8),
    /// Unrelated traffic (notifications, other clients) — keep reading.
    Ignore,
}

/// Builds the long request frame for `(device, feature, function, params)`.
/// `params` longer than a long frame's payload is a caller bug surfaced as a
/// protocol violation rather than silent truncation.
fn encode_long_request(
    device_idx: u8,
    feature_idx: u8,
    func: u8,
    params: &[u8],
) -> Result<[u8; LONG_LEN], DriverError> {
    if params.len() > LONG_PARAMS {
        return Err(DriverError::ProtocolViolation {
            reason: "HID++ request parameters exceed a long frame",
        });
    }
    let mut f = [0u8; LONG_LEN];
    f[0] = REPORT_ID_LONG;
    f[1] = device_idx;
    f[2] = feature_idx;
    f[3] = (func << 4) | SW_ID;
    f[4..4 + params.len()].copy_from_slice(params);
    Ok(f)
}

/// Classifies one incoming frame against the pending request (doc §2.2).
/// Anything that is not our reply or our error — notifications, other
/// clients' `sw_id`s, other device indexes — is [`ReplyKind::Ignore`].
fn classify_reply(req: &PendingRequest, frame: &[u8]) -> ReplyKind {
    if frame.len() < SHORT_LEN {
        return ReplyKind::Ignore;
    }
    if frame[0] != REPORT_ID_SHORT && frame[0] != REPORT_ID_LONG {
        return ReplyKind::Ignore;
    }
    if frame[1] != req.device_idx {
        return ReplyKind::Ignore;
    }
    // Error shapes carry the echoed (feature, function|sw_id) one byte later.
    if frame[2] == ERR_HIDPP2_MARKER && frame[3] == req.feature_idx && frame[4] == req.func_sw {
        return ReplyKind::Hidpp2Error(frame[5]);
    }
    if frame[2] == ERR_HIDPP1_SUBID && frame[3] == req.feature_idx && frame[4] == req.func_sw {
        return ReplyKind::Hidpp1Error(frame[5]);
    }
    if frame[2] == req.feature_idx && frame[3] == req.func_sw {
        let mut params = [0u8; LONG_PARAMS];
        let avail = (frame.len() - 4).min(LONG_PARAMS);
        params[..avail].copy_from_slice(&frame[4..4 + avail]);
        return ReplyKind::Match(params);
    }
    ReplyKind::Ignore
}

/// Maps a HID++ 2.0 error code onto the typed [`DriverError`].
fn hidpp2_error_to_driver(code: u8) -> DriverError {
    match code {
        HIDPP2_ERR_INVALID_FEATURE_INDEX
        | HIDPP2_ERR_INVALID_FUNCTION_ID
        | HIDPP2_ERR_UNSUPPORTED => DriverError::Unsupported {
            op: "HID++ function not supported by this firmware",
        },
        HIDPP2_ERR_INVALID_ARGUMENT | HIDPP2_ERR_OUT_OF_RANGE => DriverError::ProtocolViolation {
            reason: "device rejected a HID++ argument",
        },
        HIDPP2_ERR_BUSY => DriverError::Io(std::io::Error::other("HID++ device busy")),
        _ => DriverError::ProtocolViolation {
            reason: "HID++ 2.0 error reply",
        },
    }
}

/// Maps a HID++ 1.0 (receiver) error code onto the typed [`DriverError`].
/// The unreachable-device family maps to [`DriverError::Disconnected`] so
/// the supervisor's backoff covers "keyboard switched off / out of range".
fn hidpp1_error_to_driver(code: u8) -> DriverError {
    match code {
        HIDPP1_ERR_CONNECT_FAIL | HIDPP1_ERR_UNKNOWN_DEVICE | HIDPP1_ERR_RESOURCE_ERROR => {
            DriverError::Disconnected
        }
        _ => DriverError::ProtocolViolation {
            reason: "HID++ 1.0 error reply from receiver",
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Transport abstraction
// ─────────────────────────────────────────────────────────────────────────────

/// The minimal hardware surface the driver needs: one addressed HID++
/// function call. Implemented by [`HidppTransport`] in production and by a
/// mock in tests, so blob compilation and command sequencing can be asserted
/// without a device.
#[async_trait]
pub(crate) trait LogitechIo: Send + Sync + 'static {
    /// Issues one HID++ 2.0 call and returns the reply's parameter bytes
    /// (zero-padded to a long frame's 16).
    async fn call(
        &self,
        device_idx: u8,
        feature_idx: u8,
        func: u8,
        params: &[u8],
        op: &'static str,
    ) -> Result<[u8; LONG_PARAMS], DriverError>;
}

/// Hidraw interrupt-report transport for HID++.
pub(crate) struct HidppTransport {
    fd: AsyncFd<OwnedFd>,
    device_id: Arc<str>,
    timeout: Duration,
}

impl HidppTransport {
    fn new(fd: OwnedFd, device_id: Arc<str>, timeout: Duration) -> Result<Self, DriverError> {
        let fd = AsyncFd::new(fd).map_err(DriverError::Io)?;
        Ok(Self {
            fd,
            device_id,
            timeout,
        })
    }

    async fn write_frame(&self, frame: &[u8; LONG_LEN]) -> Result<(), DriverError> {
        loop {
            let mut guard = self.fd.writable().await.map_err(DriverError::Io)?;
            let write_result = guard.try_io(|inner| {
                let borrowed = inner.get_ref().as_fd();
                nix::unistd::write(borrowed, frame).map_err(io_from_errno)
            });
            match write_result {
                Ok(Ok(n)) if n == frame.len() => return Ok(()),
                Ok(Ok(_)) => {
                    return Err(DriverError::ProtocolViolation {
                        reason: "short write to hidraw node",
                    });
                }
                Ok(Err(e)) => return Err(classify_io_error(e)),
                Err(_would_block) => continue,
            }
        }
    }

    /// Reads one report. The buffer is sized for the largest HID++ report
    /// (`0x12` very-long, 64 bytes) so oversized frames don't error — they
    /// just classify as [`ReplyKind::Ignore`].
    async fn read_frame(&self) -> Result<([u8; 64], usize), DriverError> {
        let mut buf = [0u8; 64];
        loop {
            let mut guard = self.fd.readable().await.map_err(DriverError::Io)?;
            let read_result = guard.try_io(|inner| {
                let borrowed = inner.get_ref().as_fd();
                nix::unistd::read(borrowed, &mut buf).map_err(io_from_errno)
            });
            match read_result {
                Ok(Ok(n)) => return Ok((buf, n)),
                Ok(Err(e)) => return Err(classify_io_error(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

#[async_trait]
impl LogitechIo for HidppTransport {
    /// **Cancel-safety:** Cancel-safe at the *call* boundary only — the
    /// worker never drops a round-trip future mid-flight (trait-level doc in
    /// `hal/mod.rs`).
    async fn call(
        &self,
        device_idx: u8,
        feature_idx: u8,
        func: u8,
        params: &[u8],
        op: &'static str,
    ) -> Result<[u8; LONG_PARAMS], DriverError> {
        let req = encode_long_request(device_idx, feature_idx, func, params)?;
        let pending = PendingRequest::new(device_idx, feature_idx, func);
        let dur = self.timeout;
        let device = self.device_id.clone();
        timeout(dur, async {
            self.write_frame(&req).await?;
            loop {
                let (buf, n) = self.read_frame().await?;
                match classify_reply(&pending, &buf[..n]) {
                    ReplyKind::Match(p) => return Ok(p),
                    ReplyKind::Hidpp2Error(code) => return Err(hidpp2_error_to_driver(code)),
                    ReplyKind::Hidpp1Error(code) => return Err(hidpp1_error_to_driver(code)),
                    ReplyKind::Ignore => continue,
                }
            }
        })
        .await
        .map_err(|_| DriverError::Timeout {
            device: device.to_string(),
            op,
        })?
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Blob compiler (pure functions — unit tested)
// ─────────────────────────────────────────────────────────────────────────────

/// CRC-CCITT ("CCITT-FALSE"): polynomial `0x1021`, init `0xFFFF`, no final
/// XOR. The onboard-profiles sector checksum (doc §5.2).
fn crc_ccitt(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Byte offset of a G-key binding slot, or `None` when the (bank, slot) is
/// outside the provisional layout (doc §5.3).
fn gkey_binding_offset(layer: u8, gkey: u8) -> Option<usize> {
    if layer >= GKEY_BANKS || gkey >= GKEY_SLOTS_PER_BANK {
        return None;
    }
    Some(GKEY_BANK_BASE + layer as usize * GKEY_BANK_STRIDE + gkey as usize * GKEY_ENTRY_LEN)
}

fn vendor_compile(detail: String) -> DriverError {
    DriverError::VendorCompile {
        vendor: "logitech",
        detail,
    }
}

/// Translates a 16-bit QMK keycode into a 4-byte G-key binding entry
/// (doc §5.4).
///
/// # Errors
/// [`DriverError::VendorCompile`] for keycodes with no Logitech equivalent
/// (layer-taps, macros, consumer keys, mixed-hand modifier combinations…).
fn encode_gkey_binding(keycode: u16) -> Result<[u8; GKEY_ENTRY_LEN], DriverError> {
    match keycode {
        // KC_NO — unassigned; the factory behavior stays.
        0x0000 => Ok([0, 0, 0, 0]),
        // Basic keycodes are HID usages verbatim.
        0x0004..=0x00A4 => Ok([BIND_TYPE_KEY, BIND_SUBTYPE_HID_KEY, 0, keycode as u8]),
        // Bare modifiers: HID boot-keyboard modifier mask, no usage.
        0x00E0..=0x00E7 => Ok([
            BIND_TYPE_KEY,
            BIND_SUBTYPE_HID_KEY,
            1u8 << (keycode - 0x00E0),
            0,
        ]),
        // QMK modified keycodes: 5-bit mod field over a basic code
        // (bits 8–11 = ctrl/shift/alt/gui, bit 12 = right hand).
        0x0100..=0x1FFF => {
            let base = (keycode & 0x00FF) as u8;
            let mods = ((keycode >> 8) & 0x1F) as u8;
            let hand_mask = mods & 0x0F;
            if !(0x04..=0xA4).contains(&base) || hand_mask == 0 {
                return Err(vendor_compile(format!(
                    "QMK modified keycode 0x{keycode:04x} has no Logitech G-key binding equivalent"
                )));
            }
            let mask = if mods & 0x10 != 0 {
                hand_mask << 4
            } else {
                hand_mask
            };
            Ok([BIND_TYPE_KEY, BIND_SUBTYPE_HID_KEY, mask, base])
        }
        _ => Err(vendor_compile(format!(
            "QMK keycode 0x{keycode:04x} has no Logitech G-key binding equivalent"
        ))),
    }
}

/// Inverse of [`encode_gkey_binding`] for the attach-time baseline parse.
/// Returns `None` for unassigned slots and for binding types the QMK
/// keycode space cannot express (those stay device-owned).
fn decode_gkey_binding(entry: &[u8]) -> Option<u16> {
    let &[t, sub, mask, usage] = entry.first_chunk::<GKEY_ENTRY_LEN>()?;
    if [t, sub, mask, usage] == [0, 0, 0, 0] {
        return None;
    }
    if t != BIND_TYPE_KEY || sub != BIND_SUBTYPE_HID_KEY {
        return None;
    }
    match (mask, usage) {
        (0, 0x04..=0xA4) => Some(usage as u16),
        (m, 0) if m.count_ones() == 1 => Some(0x00E0 + m.trailing_zeros() as u16),
        (m, 0x04..=0xA4) => {
            let left = m & 0x0F;
            let right = m >> 4;
            if left != 0 && right == 0 {
                Some(((left as u16) << 8) | usage as u16)
            } else if left == 0 && right != 0 {
                Some((0x1000 | ((right as u16) << 8)) | usage as u16)
            } else {
                // Mixed-hand mask — not expressible as one QMK code.
                None
            }
        }
        _ => None,
    }
}

/// Compiles the profile sector image: the attach-time `baseline` with every
/// shadow entry's binding patched in and the CRC tail recomputed.
///
/// `entries` is `(layer, gkey, keycode)` — matrix row is always 0.
///
/// # Errors
/// [`DriverError::VendorCompile`] when the baseline is too small for the
/// G-key area or an entry cannot be translated.
fn compile_profile_image(
    baseline: &[u8],
    entries: &[(u8, u8, u16)],
) -> Result<Vec<u8>, DriverError> {
    // The full bank area plus the 2-byte CRC must fit.
    let min_len = GKEY_BANK_BASE + GKEY_BANKS as usize * GKEY_BANK_STRIDE + 2;
    if baseline.len() < min_len {
        return Err(vendor_compile(format!(
            "profile sector too small for the G-key area ({} < {min_len} bytes)",
            baseline.len()
        )));
    }
    let mut image = baseline.to_vec();
    for &(layer, gkey, keycode) in entries {
        let offset = gkey_binding_offset(layer, gkey).ok_or_else(|| {
            vendor_compile(format!("no G-key binding slot for layer {layer} col {gkey}"))
        })?;
        let binding = encode_gkey_binding(keycode)?;
        image[offset..offset + GKEY_ENTRY_LEN].copy_from_slice(&binding);
    }
    let crc_at = image.len() - 2;
    let crc = crc_ccitt(&image[..crc_at]);
    image[crc_at..].copy_from_slice(&crc.to_be_bytes());
    Ok(image)
}

/// Whether a sector image carries a valid CRC tail.
fn sector_crc_valid(image: &[u8]) -> bool {
    if image.len() < 2 {
        return false;
    }
    let crc_at = image.len() - 2;
    crc_ccitt(&image[..crc_at]).to_be_bytes() == image[crc_at..]
}

/// Decodes every recognizable G-key binding out of a profile image into
/// shadow-keymap entries (`(layer, row=0, col) -> keycode`).
fn parse_gkey_bindings(image: &[u8], layers: u8, gkeys: u8) -> BTreeMap<(u8, u8, u8), u16> {
    let mut entries = BTreeMap::new();
    for layer in 0..layers.min(GKEY_BANKS) {
        for gkey in 0..gkeys.min(GKEY_SLOTS_PER_BANK) {
            let Some(offset) = gkey_binding_offset(layer, gkey) else {
                continue;
            };
            let Some(entry) = image.get(offset..offset + GKEY_ENTRY_LEN) else {
                continue;
            };
            if let Some(keycode) = decode_gkey_binding(entry) {
                entries.insert((layer, 0, gkey), keycode);
            }
        }
    }
    entries
}

// ─────────────────────────────────────────────────────────────────────────────
// Lighting encoders (pure functions — unit tested)
// ─────────────────────────────────────────────────────────────────────────────

/// Builds the `setClusterEffect` parameter block (doc §6).
fn encode_cluster_effect(cluster: u8, l: &LightingState, persist: u8) -> [u8; LONG_PARAMS] {
    let mut p = [0u8; LONG_PARAMS];
    p[0] = cluster;
    p[1] = l.mode;
    p[2] = l.r;
    p[3] = l.g;
    p[4] = l.b;
    let period_ms = effect_period_ms(l.speed);
    p[5..7].copy_from_slice(&period_ms.to_be_bytes());
    p[7] = l.brightness;
    p[9] = persist;
    p
}

/// Maps the VIA 0–255 speed onto an effect period in milliseconds
/// (higher speed → shorter period).
fn effect_period_ms(speed: u8) -> u16 {
    2000u16.saturating_sub(speed as u16 * 7)
}

/// Parses a `set_lighting` D-Bus packed payload (channel 0):
/// `[mode, r, g, b, brightness?, random?, speed?, direction?]` — the same
/// field order as the GMK67 driver's packed channel. Missing trailing
/// fields fall back to the current state's values.
fn lighting_from_bytes(current: &LightingState, data: &[u8]) -> LightingState {
    LightingState {
        mode: data.first().copied().unwrap_or(current.mode),
        r: data.get(1).copied().unwrap_or(current.r),
        g: data.get(2).copied().unwrap_or(current.g),
        b: data.get(3).copied().unwrap_or(current.b),
        brightness: data.get(4).copied().unwrap_or(current.brightness),
        random: data.get(5).map(|&x| x != 0).unwrap_or(current.random),
        speed: data.get(6).copied().unwrap_or(current.speed),
        direction: data.get(7).copied().unwrap_or(current.direction),
    }
}

fn lighting_to_bytes(l: &LightingState) -> Vec<u8> {
    vec![
        l.mode,
        l.r,
        l.g,
        l.b,
        l.brightness,
        l.random as u8,
        l.speed,
        l.direction,
    ]
}

// ─────────────────────────────────────────────────────────────────────────────
// Driver
// ─────────────────────────────────────────────────────────────────────────────

/// Attach-time facts resolved once per session: where the keyboard lives on
/// the HID++ channel and how its onboard memory is shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttachInfo {
    /// Bound device index (`0xFF` wired, or a receiver slot).
    device_idx: u8,
    /// Resolved feature index of Onboard Profiles (`0x8100`).
    feat_onboard: u8,
    /// Resolved feature index of RGB Effects (`0x8071`), if present.
    feat_rgb: Option<u8>,
    /// Sector size from `getDescription`.
    sector_size: u16,
    /// The active profile's sector.
    profile_sector: u16,
}

/// Logitech G915 / Lightspeed driver. G-key remaps are held in the
/// [`ShadowState`] and flushed as a compiled onboard-profile sector on
/// [`KeyboardDriver::commit_to_nvram`]; lighting is pushed live (RAM
/// persistence) and re-sent with the flash flag on commit.
pub struct LogitechDriver {
    io: Box<dyn LogitechIo>,
    device_id: Arc<str>,
    matrix: (u8, u8),
    layers: u8,
    /// Shadow keymap with JSON persistence and rollback support.
    shadow: ShadowState,
    /// Live lighting state (pushed immediately, persisted on commit).
    lighting: LightingState,
    /// Whether lighting changed since the last flash persist.
    lighting_dirty: bool,
    /// Product name from HID++ `0x0005`, or the fallback.
    model: String,
    /// Resolved at first use (the factory contract keeps `new()` sync).
    attach: Option<AttachInfo>,
    /// The active profile sector read at attach — the compile baseline.
    /// `None` when the read failed or its CRC was invalid; commits are
    /// refused rather than pushing a fabricated profile image.
    baseline: Option<Vec<u8>>,
}

impl LogitechDriver {
    /// Opens the HID++ interface of a Logitech receiver or wired keyboard
    /// and constructs the driver.
    ///
    /// Loads any previously persisted shadow state from
    /// `$XDG_DATA_HOME/clackd/046d_g915.json`. The HID++ handshake (device
    /// index discovery, feature resolution, baseline sector read) is
    /// deferred to the engine's attach-time topology query — `new()` must
    /// stay synchronous per the factory contract.
    ///
    /// # Errors
    /// - [`DriverError::PermissionDenied`] / [`DriverError::Disconnected`]
    ///   from `open(2)`.
    /// - [`DriverError::ProtocolViolation`] if the node's descriptor lacks
    ///   the HID++ vendor usage page (`0xFF00` / `0xFF43`) — i.e. this is
    ///   one of the composite device's other interfaces.
    pub fn new(hidraw_path: &Path, matrix: (u8, u8), layers: u8) -> Result<Self, DriverError> {
        let flags = OFlag::O_RDWR | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
        let owned = open(hidraw_path, flags, Mode::empty())
            .map_err(|errno| classify_open_errno(errno, hidraw_path))?;
        probe_hidpp_interface(&owned)?;

        let device_id: Arc<str> = hidraw_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown-hidraw")
            .into();

        let shadow = ShadowState::load(PERSIST_ID);
        let lighting = shadow.lighting().clone();

        info!(device_id = %device_id, "attached Logitech HID++ driver (handshake deferred to first topology query)");
        let transport = HidppTransport::new(owned, device_id.clone(), DEFAULT_TIMEOUT)?;
        Ok(Self {
            io: Box::new(transport),
            device_id,
            matrix,
            layers,
            shadow,
            lighting,
            lighting_dirty: false,
            model: "Logitech G915".to_string(),
            attach: None,
            baseline: None,
        })
    }

    /// Test/seam constructor over an arbitrary [`LogitechIo`]; the HID++
    /// handshake still runs on first use.
    pub(crate) fn with_io(
        io: Box<dyn LogitechIo>,
        device_id: Arc<str>,
        matrix: (u8, u8),
        layers: u8,
    ) -> Self {
        Self {
            io,
            device_id,
            matrix,
            layers,
            shadow: ShadowState::ephemeral(),
            lighting: LightingState::default(),
            lighting_dirty: false,
            model: "Logitech G915".to_string(),
            attach: None,
            baseline: None,
        }
    }

    /// Test constructor with the handshake pre-resolved.
    #[cfg(test)]
    pub(crate) fn with_io_attached(
        io: Box<dyn LogitechIo>,
        attach: AttachInfo,
        baseline: Option<Vec<u8>>,
    ) -> Self {
        let mut d = Self::with_io(io, "g915-test".into(), (1, 5), 3);
        d.attach = Some(attach);
        d.baseline = baseline;
        d
    }

    /// Resolves the HID++ handshake exactly once per session and returns
    /// the (small, `Copy`) attach facts.
    async fn ensure_attached(&mut self) -> Result<AttachInfo, DriverError> {
        if let Some(att) = self.attach {
            return Ok(att);
        }
        let att = self.attach_device().await?;
        self.attach = Some(att);
        Ok(att)
    }

    /// The full attach sequence (doc §3.1): device-index discovery, feature
    /// resolution, onboard-memory description, and the best-effort baseline
    /// sector read.
    async fn attach_device(&mut self) -> Result<AttachInfo, DriverError> {
        let (device_idx, feat_onboard) = self.discover_device_index().await?;

        let feat_rgb = self
            .feature_index(device_idx, FEAT_RGB_EFFECTS)
            .await
            .unwrap_or(None);
        if feat_rgb.is_none() {
            info!(device_id = %self.device_id, "device lacks RGB Effects (0x8071) — lighting will report NotSupported");
        }

        // Display name (best-effort; the fallback stays otherwise).
        if let Ok(Some(feat_name)) = self.feature_index(device_idx, FEAT_DEVICE_NAME).await {
            match self.read_device_name(device_idx, feat_name).await {
                Ok(name) if !name.is_empty() => self.model = name,
                Ok(_) => {}
                Err(e) => debug!(device_id = %self.device_id, error = %e, "device name read failed (ignored)"),
            }
        }

        // Onboard memory topology.
        let desc = self
            .io
            .call(device_idx, feat_onboard, ONB_GET_DESCRIPTION, &[], "onboard_get_description")
            .await?;
        let sector_size = u16::from_be_bytes([desc[7], desc[8]]);
        if !(sector_size as usize).is_multiple_of(MEM_CHUNK)
            || sector_size == 0
            || sector_size as usize > SECTOR_SIZE_MAX
        {
            return Err(DriverError::ProtocolViolation {
                reason: "implausible onboard-profiles sector size in getDescription",
            });
        }

        // Profiles must come from onboard flash for a pushed blob to take
        // effect. Best-effort: a failure is logged, not fatal — the memory
        // write path itself will surface real trouble.
        if let Err(e) = self
            .io
            .call(
                device_idx,
                feat_onboard,
                ONB_SET_MODE,
                &[ONBOARD_MODE_ONBOARD],
                "onboard_set_mode",
            )
            .await
        {
            warn!(device_id = %self.device_id, error = %e, "setOnboardMode(onboard) failed — continuing");
        }

        // The active profile's sector is our compile target.
        let cur = self
            .io
            .call(
                device_idx,
                feat_onboard,
                ONB_GET_CURRENT_PROFILE,
                &[],
                "onboard_get_current_profile",
            )
            .await?;
        let mut profile_sector = u16::from_be_bytes([cur[0], cur[1]]);
        if profile_sector == 0 {
            warn!(device_id = %self.device_id, "no active onboard profile reported — assuming sector 0x0001");
            profile_sector = PROFILE_SECTOR_DEFAULT;
        }

        let att = AttachInfo {
            device_idx,
            feat_onboard,
            feat_rgb,
            sector_size,
            profile_sector,
        };

        // Baseline read + shadow adoption (best-effort — a failed read keeps
        // the persisted shadow but refuses future blob compiles).
        match self.read_sector(att, profile_sector).await {
            Ok(image) if sector_crc_valid(&image) => {
                let entries = parse_gkey_bindings(&image, self.layers, self.matrix.1);
                info!(
                    device_id = %self.device_id,
                    sector = profile_sector,
                    bindings = entries.len(),
                    "adopted device profile as shadow baseline",
                );
                self.shadow.adopt_confirmed(entries);
                self.baseline = Some(image);
            }
            Ok(_) => {
                warn!(
                    device_id = %self.device_id,
                    sector = profile_sector,
                    "profile sector CRC invalid — keeping persisted shadow; commits refused until a valid baseline exists",
                );
            }
            Err(e) => {
                warn!(
                    device_id = %self.device_id,
                    error = %e,
                    "profile sector read failed — keeping persisted shadow; commits refused until a valid baseline exists",
                );
            }
        }

        Ok(att)
    }

    /// Probes `0xFF` (wired) then receiver slots `1..=6` for a HID++ 2.0
    /// device exposing Onboard Profiles (doc §3.1). Per-index failures are
    /// expected traffic (empty slots error, powered-off devices time out)
    /// and only demote to DEBUG logs.
    async fn discover_device_index(&self) -> Result<(u8, u8), DriverError> {
        let candidates = std::iter::once(DEV_IDX_WIRED).chain(RECEIVER_SLOT_FIRST..=RECEIVER_SLOT_LAST);
        for idx in candidates {
            match self.probe_index(idx).await {
                Ok(Some(feat_onboard)) => {
                    info!(
                        device_id = %self.device_id,
                        device_idx = format!("0x{idx:02x}"),
                        feat_onboard,
                        "bound HID++ device with Onboard Profiles",
                    );
                    return Ok((idx, feat_onboard));
                }
                Ok(None) => {
                    debug!(device_id = %self.device_id, device_idx = format!("0x{idx:02x}"), "HID++ device without Onboard Profiles — skipping");
                }
                Err(e) => {
                    debug!(device_id = %self.device_id, device_idx = format!("0x{idx:02x}"), error = %e, "index probe failed — skipping");
                }
            }
        }
        warn!(device_id = %self.device_id, "no HID++ device with Onboard Profiles reachable (keyboard off?) — supervisor will retry");
        Err(DriverError::Disconnected)
    }

    /// Pings one device index and, if it answers HID++ 2.0, resolves the
    /// Onboard Profiles feature. `Ok(None)` = reachable but not eligible.
    async fn probe_index(&self, idx: u8) -> Result<Option<u8>, DriverError> {
        let pong = self
            .io
            .call(idx, FEAT_IDX_ROOT, ROOT_PING, &[0, 0, PING_MAGIC], "root_ping")
            .await?;
        if pong[2] != PING_MAGIC {
            // Not a HID++ 2.0 ping echo (e.g. the receiver itself).
            return Ok(None);
        }
        self.feature_index(idx, FEAT_ONBOARD_PROFILES).await
    }

    /// IRoot `getFeature`: feature id → feature index (`None` when absent).
    async fn feature_index(&self, idx: u8, feature: u16) -> Result<Option<u8>, DriverError> {
        let [hi, lo] = feature.to_be_bytes();
        let reply = self
            .io
            .call(idx, FEAT_IDX_ROOT, ROOT_GET_FEATURE, &[hi, lo], "root_get_feature")
            .await?;
        Ok((reply[0] != 0).then_some(reply[0]))
    }

    /// Reads the product name via `0x0005` (doc §4), sanitized to printable
    /// ASCII and capped at [`NAME_MAX_LEN`].
    async fn read_device_name(&self, idx: u8, feat_name: u8) -> Result<String, DriverError> {
        let count = self
            .io
            .call(idx, feat_name, NAME_GET_COUNT, &[], "name_get_count")
            .await?[0] as usize;
        let count = count.min(NAME_MAX_LEN);
        let mut name = String::new();
        let mut offset = 0usize;
        while offset < count {
            let chunk = self
                .io
                .call(idx, feat_name, NAME_GET_CHUNK, &[offset as u8], "name_get_chunk")
                .await?;
            for &b in chunk.iter() {
                if name.len() >= count || b == 0 {
                    break;
                }
                if b.is_ascii_graphic() || b == b' ' {
                    name.push(b as char);
                }
            }
            offset += LONG_PARAMS;
        }
        Ok(name.trim().to_string())
    }

    /// Reads one onboard sector in 16-byte chunks (doc §5.2).
    async fn read_sector(&self, att: AttachInfo, sector: u16) -> Result<Vec<u8>, DriverError> {
        let size = att.sector_size as usize;
        let mut image = vec![0u8; size];
        let [s_hi, s_lo] = sector.to_be_bytes();
        let mut offset = 0usize;
        while offset < size {
            let [o_hi, o_lo] = (offset as u16).to_be_bytes();
            let chunk = self
                .io
                .call(
                    att.device_idx,
                    att.feat_onboard,
                    ONB_MEMORY_READ,
                    &[s_hi, s_lo, o_hi, o_lo],
                    "onboard_memory_read",
                )
                .await?;
            image[offset..offset + MEM_CHUNK].copy_from_slice(&chunk[..MEM_CHUNK]);
            offset += MEM_CHUNK;
        }
        Ok(image)
    }

    /// Writes one onboard sector as a full write session (doc §5.2):
    /// `memoryAddrWrite` → n × `memoryWrite` → `memoryWriteEnd`.
    async fn write_sector(
        &self,
        att: AttachInfo,
        sector: u16,
        image: &[u8],
    ) -> Result<(), DriverError> {
        let [s_hi, s_lo] = sector.to_be_bytes();
        let [n_hi, n_lo] = (image.len() as u16).to_be_bytes();
        self.io
            .call(
                att.device_idx,
                att.feat_onboard,
                ONB_MEMORY_ADDR_WRITE,
                &[s_hi, s_lo, 0, 0, n_hi, n_lo],
                "onboard_memory_addr_write",
            )
            .await?;
        for chunk in image.chunks(MEM_CHUNK) {
            self.io
                .call(
                    att.device_idx,
                    att.feat_onboard,
                    ONB_MEMORY_WRITE,
                    chunk,
                    "onboard_memory_write",
                )
                .await?;
        }
        self.io
            .call(
                att.device_idx,
                att.feat_onboard,
                ONB_MEMORY_WRITE_END,
                &[],
                "onboard_memory_write_end",
            )
            .await?;
        Ok(())
    }

    /// Compiles the shadow keymap onto the baseline and pushes the sector.
    /// On success the pushed image becomes the new baseline.
    async fn push_keymap(&mut self, att: AttachInfo) -> Result<(), DriverError> {
        let Some(baseline) = self.baseline.as_deref() else {
            return Err(vendor_compile(
                "no profile baseline from device — refusing to fabricate a profile sector".into(),
            ));
        };
        let mut entries: Vec<(u8, u8, u16)> = Vec::new();
        for layer in 0..self.layers {
            for (row, col, keycode) in self.shadow.layer_entries(layer) {
                if row != 0 {
                    // set_keycode validates row 0; a nonzero row can only come
                    // from a hand-edited shadow file. Skip, don't corrupt.
                    warn!(device_id = %self.device_id, layer, row, col, "skipping shadow entry with nonzero row");
                    continue;
                }
                entries.push((layer, col, keycode));
            }
        }
        let image = compile_profile_image(baseline, &entries)?;
        self.write_sector(att, att.profile_sector, &image).await?;
        self.baseline = Some(image);
        Ok(())
    }

    /// Applies the given lighting to the primary cluster with the given
    /// persistence flag.
    async fn push_lighting(
        &self,
        att: AttachInfo,
        lighting: &LightingState,
        persist: u8,
    ) -> Result<(), DriverError> {
        let Some(feat_rgb) = att.feat_rgb else {
            return Err(DriverError::Unsupported {
                op: "lighting (device lacks HID++ RGB Effects 0x8071)",
            });
        };
        let params = encode_cluster_effect(RGB_CLUSTER_PRIMARY, lighting, persist);
        self.io
            .call(
                att.device_idx,
                feat_rgb,
                RGB_SET_CLUSTER_EFFECT,
                &params,
                "rgb_set_cluster_effect",
            )
            .await?;
        Ok(())
    }

    /// Builds a [`LightingState`] by patching one VIA RGB-matrix value onto
    /// the current state. VIA encodings: brightness/speed are 0–255
    /// (passed through), effect is the raw cluster effect index (0 = off),
    /// colour is `[hue, sat]` at full value.
    fn lighting_with_via_value(
        &self,
        value_id: u8,
        data: &[u8],
    ) -> Result<LightingState, DriverError> {
        let missing = DriverError::ProtocolViolation {
            reason: "short data for VIA RGB-matrix lighting value",
        };
        let mut lighting = self.lighting.clone();
        match value_id {
            VIA_VALUE_BRIGHTNESS => {
                lighting.brightness = *data.first().ok_or(missing)?;
            }
            VIA_VALUE_EFFECT => {
                lighting.mode = *data.first().ok_or(missing)?;
            }
            VIA_VALUE_SPEED => {
                lighting.speed = *data.first().ok_or(missing)?;
            }
            VIA_VALUE_COLOR => {
                let [h, s] = *data.first_chunk::<2>().ok_or(missing)?;
                (lighting.r, lighting.g, lighting.b) = hs_to_rgb(h, s);
                lighting.random = false;
            }
            _ => {
                return Err(DriverError::Unsupported {
                    op: "RGB-matrix value id (Logitech supports brightness/effect/speed/color)",
                });
            }
        }
        Ok(lighting)
    }
}

#[async_trait]
impl KeyboardDriver for LogitechDriver {
    async fn get_matrix_dimensions(&mut self) -> Result<(u8, u8), DriverError> {
        // Attach-time seam (the engine queries topology once per session):
        // run the HID++ handshake here, where async round-trips are possible
        // — `new()` is synchronous by the factory contract. Unlike the GMK67
        // read-back this is load-bearing (device index + feature indexes),
        // so a failure fails the attach and the supervisor retries with
        // backoff.
        self.ensure_attached().await?;
        Ok(self.matrix)
    }

    async fn get_layer_count(&mut self) -> Result<u8, DriverError> {
        Ok(self.layers)
    }

    async fn get_keycode(&mut self, layer: u8, row: u8, col: u8) -> Result<u16, DriverError> {
        // Shadow read — onboard profiles expose no per-key query, and the
        // attach-time sector read already seeded the shadow. Unwritten
        // positions report KC_NO (0x0000).
        Ok(self.shadow.get_keycode(layer, row, col))
    }

    async fn set_keycode(
        &mut self,
        layer: u8,
        row: u8,
        col: u8,
        keycode: u16,
    ) -> Result<(), DriverError> {
        // Validate up front instead of shadowing a value the commit would
        // reject: only the G-key row exists, only the M-key banks are
        // layers, and only HID-usage-shaped keycodes are expressible.
        if layer >= self.layers || gkey_binding_offset(layer, 0).is_none() {
            return Err(DriverError::Unsupported {
                op: "keymap layer beyond the M1/M2/M3 G-key banks on Logitech",
            });
        }
        if row != 0 || col >= self.matrix.1 {
            return Err(DriverError::Unsupported {
                op: "no G-key at this matrix position on Logitech",
            });
        }
        // Surface unmappable keycodes now, not at commit time where the
        // whole batch would roll back.
        encode_gkey_binding(keycode)?;
        self.shadow.set_keycode(layer, row, col, keycode);
        Ok(())
    }

    async fn get_macro_buffer(&mut self, _offset: u16, _length: u8) -> Result<Vec<u8>, DriverError> {
        // Onboard macro sectors exist but their format is deferred;
        // reporting NotSupported beats returning fabricated zeros.
        Err(DriverError::Unsupported {
            op: "macro storage on Logitech (onboard macro sectors not yet implemented)",
        })
    }

    async fn set_macro_buffer(&mut self, _offset: u16, _data: &[u8]) -> Result<(), DriverError> {
        Err(DriverError::Unsupported {
            op: "macro storage on Logitech (onboard macro sectors not yet implemented)",
        })
    }

    /// Two addressing modes (shared VIA-compatibility constants in the
    /// `legacy` parent module):
    ///
    /// - `channel` 0: `data` is the driver-native packed payload
    ///   `[mode, r, g, b, brightness?, random?, speed?, direction?]`.
    /// - `channel` 3 (VIA RGB-matrix): `value_id` selects brightness /
    ///   effect / speed / colour, patching only that field.
    ///
    /// Either way the effect is applied live (RAM persistence) and flagged
    /// for a flash persist on the next `commit_to_nvram`.
    async fn set_lighting(
        &mut self,
        channel: u8,
        value_id: u8,
        data: &[u8],
    ) -> Result<(), DriverError> {
        let att = self.ensure_attached().await?;
        let lighting = match channel {
            VIA_CHANNEL_PACKED => lighting_from_bytes(&self.lighting, data),
            VIA_CHANNEL_RGB_MATRIX => self.lighting_with_via_value(value_id, data)?,
            _ => {
                return Err(DriverError::Unsupported {
                    op: "lighting channel (Logitech supports packed channel 0 and RGB-matrix channel 3)",
                });
            }
        };
        self.push_lighting(att, &lighting, RGB_PERSIST_RAM).await?;
        self.lighting = lighting.clone();
        self.shadow.set_lighting(lighting);
        // Persist the shadow copy now so lighting survives a daemon restart
        // even before the flash-persist commit.
        self.shadow.persist_lighting();
        self.lighting_dirty = true;
        debug!(device_id = %self.device_id, mode = self.lighting.mode, "set Logitech lighting (RAM)");
        Ok(())
    }

    async fn get_lighting(&mut self, channel: u8, value_id: u8) -> Result<Vec<u8>, DriverError> {
        match channel {
            VIA_CHANNEL_PACKED => Ok(lighting_to_bytes(&self.lighting)),
            VIA_CHANNEL_RGB_MATRIX => match value_id {
                VIA_VALUE_BRIGHTNESS => Ok(vec![self.lighting.brightness]),
                VIA_VALUE_EFFECT => Ok(vec![self.lighting.mode]),
                VIA_VALUE_SPEED => Ok(vec![self.lighting.speed]),
                VIA_VALUE_COLOR => {
                    let (h, s) = rgb_to_hs(self.lighting.r, self.lighting.g, self.lighting.b);
                    Ok(vec![h, s])
                }
                _ => Err(DriverError::Unsupported {
                    op: "RGB-matrix value id (Logitech supports brightness/effect/speed/color)",
                }),
            },
            _ => Err(DriverError::Unsupported {
                op: "lighting channel (Logitech supports packed channel 0 and RGB-matrix channel 3)",
            }),
        }
    }

    /// Flushes pending state to onboard flash: the compiled profile sector
    /// for keymap edits (doc §5), then a flash-persist `setClusterEffect`
    /// for lighting touched since the last commit (doc §6).
    ///
    /// On keymap success the shadow is confirmed and persisted to JSON; on
    /// failure it rolls back to last-known-good (CLAUDE.md §4.2) and the
    /// lighting persist is not attempted.
    async fn commit_to_nvram(&mut self) -> Result<(), DriverError> {
        let att = self.ensure_attached().await?;

        if self.shadow.is_dirty() {
            // Phase 1: prepare rollback target (confirmed = last successful
            // commit).
            self.shadow.prepare_commit();
            match self.push_keymap(att).await {
                Ok(()) => {
                    // Phase 2a: success — persist and mark confirmed.
                    self.shadow.confirm_commit();
                }
                Err(e) => {
                    // Phase 2b: failure — roll back to last-known-good.
                    self.shadow.rollback();
                    return Err(e);
                }
            }
        }

        if self.lighting_dirty {
            let lighting = self.lighting.clone();
            self.push_lighting(att, &lighting, RGB_PERSIST_FLASH).await?;
            self.lighting_dirty = false;
        }

        Ok(())
    }

    fn emits_layout_event_per_set(&self) -> bool {
        // Writes are batched until commit, so the event fires once per flush.
        false
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Open / probe helpers
// ─────────────────────────────────────────────────────────────────────────────

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

fn io_from_errno(errno: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(errno as i32)
}

// HIDIOCGRDESCSIZE / HIDIOCGRDESC — used to confirm we bound the HID++
// interface of the composite device (each driver keeps its own wrappers;
// the nix macros generate module-private fns).
#[repr(C)]
struct HidrawReportDescriptor {
    size: u32,
    value: [u8; HID_MAX_DESCRIPTOR_SIZE],
}
nix::ioctl_read!(hidiocgrdescsize, b'H', 0x01, std::os::raw::c_int);
nix::ioctl_read!(hidiocgrdesc, b'H', 0x02, HidrawReportDescriptor);

/// Confirms the opened hidraw node is the HID++ configuration interface,
/// identified by the vendor usage page (`0xFF00` receiver / `0xFF43`
/// wired) in its report descriptor. The composite device's boot-keyboard
/// and consumer interfaces carry neither.
fn probe_hidpp_interface(fd: &OwnedFd) -> Result<(), DriverError> {
    let raw_fd = fd.as_raw_fd();
    let mut desc_size: std::os::raw::c_int = 0;
    // SAFETY: reads a single c_int from a valid hidraw fd.
    unsafe {
        hidiocgrdescsize(raw_fd, &mut desc_size).map_err(|e| DriverError::Io(io_from_errno(e)))?
    };
    if desc_size <= 0 {
        return Err(DriverError::ProtocolViolation {
            reason: "invalid or empty HID report descriptor size",
        });
    }

    let size = (desc_size as usize).min(HID_MAX_DESCRIPTOR_SIZE);
    let mut rpt = HidrawReportDescriptor {
        size: size as u32,
        value: [0u8; HID_MAX_DESCRIPTOR_SIZE],
    };
    // SAFETY: fills the struct, size field set from the query above.
    unsafe { hidiocgrdesc(raw_fd, &mut rpt).map_err(|e| DriverError::Io(io_from_errno(e)))? };

    let bytes = &rpt.value[..(rpt.size as usize).min(HID_MAX_DESCRIPTOR_SIZE)];
    if hid_descriptor_has_usage_page(bytes, HIDPP_USAGE_PAGE_RECEIVER)
        || hid_descriptor_has_usage_page(bytes, HIDPP_USAGE_PAGE_WIRED)
    {
        Ok(())
    } else {
        Err(DriverError::ProtocolViolation {
            reason: "not a HID++ interface (no 0xFF00/0xFF43 usage page in HID descriptor)",
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::Mutex;

    const FEAT_ONB_IDX: u8 = 0x10;
    const FEAT_RGB_IDX: u8 = 0x11;
    const SECTOR: u16 = 0x0001;
    const SECTOR_SIZE: u16 = 0x0100;

    /// `(device_idx, feature_idx, func)` — the address of one HID++ call.
    type CallKey = (u8, u8, u8);
    /// One recorded call: its address plus the sent parameter bytes.
    type RecordedCall = (u8, u8, u8, Vec<u8>);

    /// Records every call and serves scripted replies keyed by [`CallKey`].
    struct MockIo {
        calls: Mutex<Vec<RecordedCall>>,
        replies: Mutex<HashMap<CallKey, VecDeque<[u8; LONG_PARAMS]>>>,
        fail_on: Mutex<HashSet<CallKey>>,
    }
    impl MockIo {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(HashMap::new()),
                fail_on: Mutex::new(HashSet::new()),
            }
        }
        fn script(&self, key: (u8, u8, u8), params: [u8; LONG_PARAMS]) {
            self.replies.lock().unwrap().entry(key).or_default().push_back(params);
        }
        fn fail_on(&self, key: (u8, u8, u8)) {
            self.fail_on.lock().unwrap().insert(key);
        }
        fn calls(&self) -> Vec<(u8, u8, u8, Vec<u8>)> {
            self.calls.lock().unwrap().clone()
        }
        fn calls_for(&self, feature_idx: u8, func: u8) -> Vec<Vec<u8>> {
            self.calls()
                .into_iter()
                .filter(|&(_, f, fun, _)| f == feature_idx && fun == func)
                .map(|(_, _, _, p)| p)
                .collect()
        }
    }
    #[async_trait]
    impl LogitechIo for Arc<MockIo> {
        async fn call(
            &self,
            device_idx: u8,
            feature_idx: u8,
            func: u8,
            params: &[u8],
            _op: &'static str,
        ) -> Result<[u8; LONG_PARAMS], DriverError> {
            let key = (device_idx, feature_idx, func);
            if self.fail_on.lock().unwrap().contains(&key) {
                return Err(DriverError::Disconnected);
            }
            self.calls
                .lock()
                .unwrap()
                .push((device_idx, feature_idx, func, params.to_vec()));
            let scripted = self.replies.lock().unwrap().get_mut(&key).and_then(|q| q.pop_front());
            Ok(scripted.unwrap_or([0u8; LONG_PARAMS]))
        }
    }

    fn attach_info() -> AttachInfo {
        AttachInfo {
            device_idx: 0x02,
            feat_onboard: FEAT_ONB_IDX,
            feat_rgb: Some(FEAT_RGB_IDX),
            sector_size: SECTOR_SIZE,
            profile_sector: SECTOR,
        }
    }

    /// A baseline image with a valid CRC and no bindings.
    fn empty_baseline() -> Vec<u8> {
        let mut image = vec![0u8; SECTOR_SIZE as usize];
        let crc_at = image.len() - 2;
        let crc = crc_ccitt(&image[..crc_at]);
        image[crc_at..].copy_from_slice(&crc.to_be_bytes());
        image
    }

    fn driver_with(io: Arc<MockIo>) -> LogitechDriver {
        LogitechDriver::with_io_attached(Box::new(io), attach_info(), Some(empty_baseline()))
    }

    // ── Pure helpers ─────────────────────────────────────────────────────

    #[test]
    fn crc_ccitt_known_vectors() {
        // The canonical CCITT-FALSE check value.
        assert_eq!(crc_ccitt(b"123456789"), 0x29B1);
        assert_eq!(crc_ccitt(&[]), 0xFFFF);
    }

    #[test]
    fn long_request_layout() {
        let f = encode_long_request(0xFF, 0x10, 0x5, &[0xAA, 0xBB]).unwrap();
        assert_eq!(f[0], REPORT_ID_LONG);
        assert_eq!(f[1], 0xFF);
        assert_eq!(f[2], 0x10);
        assert_eq!(f[3], (0x5 << 4) | SW_ID);
        assert_eq!(&f[4..6], &[0xAA, 0xBB]);
        assert_eq!(f.len(), LONG_LEN);
        assert!(encode_long_request(0xFF, 0x10, 0x5, &[0u8; 17]).is_err());
    }

    #[test]
    fn classify_matches_and_zero_pads_short_replies() {
        let req = PendingRequest::new(0x02, 0x10, 0x5);
        // Short reply: 3 param bytes, rest must read as zero.
        let frame = [REPORT_ID_SHORT, 0x02, 0x10, (0x5 << 4) | SW_ID, 1, 2, 3];
        match classify_reply(&req, &frame) {
            ReplyKind::Match(p) => {
                assert_eq!(&p[..3], &[1, 2, 3]);
                assert!(p[3..].iter().all(|&b| b == 0));
            }
            other => panic!("expected Match, got {other:?}"),
        }
    }

    #[test]
    fn classify_skips_notifications_and_other_devices() {
        let req = PendingRequest::new(0x02, 0x10, 0x5);
        // Device-arrival notification (sub-id 0x41) on our index.
        let notif = [REPORT_ID_SHORT, 0x02, 0x41, 0x04, 0x00, 0x00, 0x00];
        assert_eq!(classify_reply(&req, &notif), ReplyKind::Ignore);
        // Correct echo but a different device index.
        let other_dev = [REPORT_ID_SHORT, 0x03, 0x10, (0x5 << 4) | SW_ID, 0, 0, 0];
        assert_eq!(classify_reply(&req, &other_dev), ReplyKind::Ignore);
        // Same feature/function but another client's sw_id.
        let other_sw = [REPORT_ID_SHORT, 0x02, 0x10, (0x5 << 4) | 0x01, 0, 0, 0];
        assert_eq!(classify_reply(&req, &other_sw), ReplyKind::Ignore);
        // Runt frame.
        assert_eq!(classify_reply(&req, &[0x10, 0x02]), ReplyKind::Ignore);
    }

    #[test]
    fn classify_detects_both_error_shapes() {
        let req = PendingRequest::new(0x02, 0x10, 0x5);
        let e2 = [
            REPORT_ID_LONG,
            0x02,
            ERR_HIDPP2_MARKER,
            0x10,
            (0x5 << 4) | SW_ID,
            HIDPP2_ERR_INVALID_FUNCTION_ID,
            0,
        ];
        assert_eq!(
            classify_reply(&req, &e2),
            ReplyKind::Hidpp2Error(HIDPP2_ERR_INVALID_FUNCTION_ID)
        );
        let e1 = [
            REPORT_ID_SHORT,
            0x02,
            ERR_HIDPP1_SUBID,
            0x10,
            (0x5 << 4) | SW_ID,
            HIDPP1_ERR_RESOURCE_ERROR,
            0,
        ];
        assert_eq!(
            classify_reply(&req, &e1),
            ReplyKind::Hidpp1Error(HIDPP1_ERR_RESOURCE_ERROR)
        );
    }

    #[test]
    fn error_code_mapping() {
        assert!(matches!(
            hidpp2_error_to_driver(HIDPP2_ERR_INVALID_FUNCTION_ID),
            DriverError::Unsupported { .. }
        ));
        assert!(matches!(
            hidpp2_error_to_driver(HIDPP2_ERR_INVALID_ARGUMENT),
            DriverError::ProtocolViolation { .. }
        ));
        assert!(matches!(
            hidpp1_error_to_driver(HIDPP1_ERR_RESOURCE_ERROR),
            DriverError::Disconnected
        ));
        assert!(matches!(
            hidpp1_error_to_driver(0x01),
            DriverError::ProtocolViolation { .. }
        ));
    }

    #[test]
    fn gkey_binding_encoding() {
        // KC_A → plain usage.
        assert_eq!(encode_gkey_binding(0x0004).unwrap(), [0x80, 0x02, 0x00, 0x04]);
        // KC_NO → unassigned.
        assert_eq!(encode_gkey_binding(0x0000).unwrap(), [0, 0, 0, 0]);
        // Bare left-shift → modifier mask only.
        assert_eq!(encode_gkey_binding(0x00E1).unwrap(), [0x80, 0x02, 0x02, 0x00]);
        // LCTL(KC_C) = 0x0106 → ctrl mask + usage.
        assert_eq!(encode_gkey_binding(0x0106).unwrap(), [0x80, 0x02, 0x01, 0x06]);
        // RALT(KC_A): right-hand bit set → mask shifted to the high nibble.
        assert_eq!(encode_gkey_binding(0x1404).unwrap(), [0x80, 0x02, 0x40, 0x04]);
        // Layer-tap and friends are not expressible.
        assert!(matches!(
            encode_gkey_binding(0x5C00),
            Err(DriverError::VendorCompile { vendor: "logitech", .. })
        ));
        // KC_TRNS has no meaning on a bank-based device.
        assert!(encode_gkey_binding(0x0001).is_err());
    }

    #[test]
    fn gkey_binding_round_trip() {
        for kc in [0x0004u16, 0x0052, 0x00A4, 0x00E1, 0x0106, 0x1404, 0x0204] {
            let enc = encode_gkey_binding(kc).unwrap();
            assert_eq!(decode_gkey_binding(&enc), Some(kc), "kc=0x{kc:04x}");
        }
        // Unassigned decodes to no entry.
        assert_eq!(decode_gkey_binding(&[0, 0, 0, 0]), None);
        // Non-key binding types stay device-owned.
        assert_eq!(decode_gkey_binding(&[0x90, 0x01, 0, 0]), None);
        // Mixed-hand modifier masks are not one QMK code.
        assert_eq!(decode_gkey_binding(&[0x80, 0x02, 0x11, 0x04]), None);
    }

    #[test]
    fn compile_patches_slot_and_recomputes_crc() {
        let baseline = empty_baseline();
        let image = compile_profile_image(&baseline, &[(0, 0, 0x0004), (2, 4, 0x0106)]).unwrap();
        assert_eq!(image.len(), baseline.len());
        // Layer 0, G1 at 0x20.
        assert_eq!(&image[0x20..0x24], &[0x80, 0x02, 0x00, 0x04]);
        // Layer 2, G5 at 0x20 + 2*0x40 + 4*4 = 0xB0.
        assert_eq!(&image[0xB0..0xB4], &[0x80, 0x02, 0x01, 0x06]);
        assert!(sector_crc_valid(&image));
        // And it differs from the (also valid) empty baseline CRC.
        assert_ne!(image, baseline);
    }

    #[test]
    fn compile_rejects_small_baseline_and_bad_entries() {
        assert!(matches!(
            compile_profile_image(&[0u8; 16], &[]),
            Err(DriverError::VendorCompile { vendor: "logitech", .. })
        ));
        let baseline = empty_baseline();
        assert!(compile_profile_image(&baseline, &[(3, 0, 0x0004)]).is_err());
        assert!(compile_profile_image(&baseline, &[(0, 0, 0x5C00)]).is_err());
    }

    #[test]
    fn parse_recovers_bindings_from_image() {
        let baseline = empty_baseline();
        let image = compile_profile_image(&baseline, &[(0, 1, 0x0052), (1, 0, 0x00E1)]).unwrap();
        let entries = parse_gkey_bindings(&image, 3, 5);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries.get(&(0, 0, 1)), Some(&0x0052));
        assert_eq!(entries.get(&(1, 0, 0)), Some(&0x00E1));
    }

    #[test]
    fn cluster_effect_frame_layout() {
        let l = LightingState {
            mode: EFFECT_FIXED,
            r: 0xFF,
            g: 0x20,
            b: 0x00,
            brightness: 0x80,
            speed: 0,
            direction: 0,
            random: false,
        };
        let p = encode_cluster_effect(RGB_CLUSTER_PRIMARY, &l, RGB_PERSIST_FLASH);
        assert_eq!(p[0], 0x00);
        assert_eq!(p[1], EFFECT_FIXED);
        assert_eq!(&p[2..5], &[0xFF, 0x20, 0x00]);
        assert_eq!(u16::from_be_bytes([p[5], p[6]]), 2000);
        assert_eq!(p[7], 0x80);
        assert_eq!(p[9], RGB_PERSIST_FLASH);
    }

    #[test]
    fn effect_period_scales_with_speed() {
        assert_eq!(effect_period_ms(0), 2000);
        assert!(effect_period_ms(255) < effect_period_ms(0));
    }

    // ── Driver behavior ──────────────────────────────────────────────────

    #[tokio::test]
    async fn set_keycode_is_buffered_until_commit() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        d.set_keycode(0, 0, 0, 0x0004).await.unwrap();
        // Nothing on the wire yet.
        assert!(io.calls().is_empty());
        assert_eq!(d.get_keycode(0, 0, 0).await.unwrap(), 0x0004);
        assert_eq!(d.shadow.status(), super::super::CacheStatus::Pending);

        d.commit_to_nvram().await.unwrap();
        // One full write session: addr-write, 16 chunks of a 256-byte
        // sector, write-end.
        assert_eq!(io.calls_for(FEAT_ONB_IDX, ONB_MEMORY_ADDR_WRITE).len(), 1);
        assert_eq!(
            io.calls_for(FEAT_ONB_IDX, ONB_MEMORY_WRITE).len(),
            SECTOR_SIZE as usize / MEM_CHUNK
        );
        assert_eq!(io.calls_for(FEAT_ONB_IDX, ONB_MEMORY_WRITE_END).len(), 1);
        assert_eq!(d.shadow.status(), super::super::CacheStatus::Confirmed);
    }

    #[tokio::test]
    async fn commit_session_addresses_the_profile_sector() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        d.set_keycode(1, 0, 2, 0x0052).await.unwrap();
        d.commit_to_nvram().await.unwrap();

        let addr = &io.calls_for(FEAT_ONB_IDX, ONB_MEMORY_ADDR_WRITE)[0];
        // [sector_hi, sector_lo, offset_hi, offset_lo, count_hi, count_lo]
        assert_eq!(&addr[..6], &[0x00, 0x01, 0x00, 0x00, 0x01, 0x00]);

        // The pushed chunks reassemble into the compiled image.
        let chunks = io.calls_for(FEAT_ONB_IDX, ONB_MEMORY_WRITE);
        let pushed: Vec<u8> = chunks.concat();
        let expected = compile_profile_image(&empty_baseline(), &[(1, 2, 0x0052)]).unwrap();
        assert_eq!(pushed, expected);
    }

    #[tokio::test]
    async fn failed_push_rolls_back_to_last_known_good() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());

        // Commit 0x0004 as known-good.
        d.set_keycode(0, 0, 0, 0x0004).await.unwrap();
        d.commit_to_nvram().await.unwrap();

        // Now fail the next push mid-session.
        io.fail_on((0x02, FEAT_ONB_IDX, ONB_MEMORY_WRITE));
        d.set_keycode(0, 0, 0, 0x0052).await.unwrap();
        let err = d.commit_to_nvram().await;
        assert!(err.is_err());

        // Shadow reverted to the last successful commit.
        assert_eq!(d.get_keycode(0, 0, 0).await.unwrap(), 0x0004);
        assert_eq!(d.shadow.status(), super::super::CacheStatus::Failed);
        assert!(!d.shadow.is_dirty());
    }

    #[tokio::test]
    async fn commit_without_baseline_is_refused() {
        let io = Arc::new(MockIo::new());
        let mut d = LogitechDriver::with_io_attached(Box::new(io.clone()), attach_info(), None);
        d.set_keycode(0, 0, 0, 0x0004).await.unwrap();
        assert!(matches!(
            d.commit_to_nvram().await,
            Err(DriverError::VendorCompile { vendor: "logitech", .. })
        ));
        // Refused before anything touched the wire.
        assert!(io.calls_for(FEAT_ONB_IDX, ONB_MEMORY_ADDR_WRITE).is_empty());
    }

    #[tokio::test]
    async fn set_keycode_rejects_invalid_positions_and_keycodes_early() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io);
        // Layer beyond the M-key banks.
        assert!(matches!(
            d.set_keycode(3, 0, 0, 0x0004).await,
            Err(DriverError::Unsupported { .. })
        ));
        // Row 1 does not exist; column beyond the G-keys.
        assert!(d.set_keycode(0, 1, 0, 0x0004).await.is_err());
        assert!(d.set_keycode(0, 0, 5, 0x0004).await.is_err());
        // Unmappable keycode fails at set time, not commit time.
        assert!(matches!(
            d.set_keycode(0, 0, 0, 0x5C00).await,
            Err(DriverError::VendorCompile { vendor: "logitech", .. })
        ));
        assert!(!d.shadow.is_dirty());
    }

    #[tokio::test]
    async fn macros_report_unsupported() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io);
        assert!(matches!(
            d.get_macro_buffer(0, 16).await,
            Err(DriverError::Unsupported { .. })
        ));
        assert!(matches!(
            d.set_macro_buffer(0, &[1, 2, 3]).await,
            Err(DriverError::Unsupported { .. })
        ));
    }

    #[tokio::test]
    async fn set_lighting_pushes_ram_effect_and_commit_persists_flash() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        d.set_lighting(VIA_CHANNEL_PACKED, 0, &[EFFECT_FIXED, 0xFF, 0x00, 0x00])
            .await
            .unwrap();

        let effects = io.calls_for(FEAT_RGB_IDX, RGB_SET_CLUSTER_EFFECT);
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0][1], EFFECT_FIXED);
        assert_eq!(&effects[0][2..5], &[0xFF, 0x00, 0x00]);
        assert_eq!(effects[0][9], RGB_PERSIST_RAM);

        // Commit re-sends with the flash flag (no keymap edits pending).
        d.commit_to_nvram().await.unwrap();
        let effects = io.calls_for(FEAT_RGB_IDX, RGB_SET_CLUSTER_EFFECT);
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[1][9], RGB_PERSIST_FLASH);
        // And the flag clears: a second commit sends nothing.
        d.commit_to_nvram().await.unwrap();
        assert_eq!(io.calls_for(FEAT_RGB_IDX, RGB_SET_CLUSTER_EFFECT).len(), 2);
    }

    #[tokio::test]
    async fn via_channel_patches_single_lighting_field() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        d.set_lighting(VIA_CHANNEL_RGB_MATRIX, VIA_VALUE_BRIGHTNESS, &[0x42])
            .await
            .unwrap();
        let effects = io.calls_for(FEAT_RGB_IDX, RGB_SET_CLUSTER_EFFECT);
        assert_eq!(effects[0][7], 0x42);
        // Other fields kept their defaults.
        assert_eq!(effects[0][1], LightingState::default().mode);
        // Readback reflects the patch.
        assert_eq!(
            d.get_lighting(VIA_CHANNEL_RGB_MATRIX, VIA_VALUE_BRIGHTNESS).await.unwrap(),
            vec![0x42]
        );
    }

    #[tokio::test]
    async fn via_color_sets_rgb_from_hue_sat() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io.clone());
        // Hue 0, full saturation → pure red.
        d.set_lighting(VIA_CHANNEL_RGB_MATRIX, VIA_VALUE_COLOR, &[0, 255])
            .await
            .unwrap();
        let effects = io.calls_for(FEAT_RGB_IDX, RGB_SET_CLUSTER_EFFECT);
        assert_eq!(&effects[0][2..5], &[255, 0, 0]);
        let hs = d.get_lighting(VIA_CHANNEL_RGB_MATRIX, VIA_VALUE_COLOR).await.unwrap();
        assert_eq!(hs, vec![0, 255]);
    }

    #[tokio::test]
    async fn lighting_without_rgb_feature_reports_unsupported() {
        let io = Arc::new(MockIo::new());
        let att = AttachInfo {
            feat_rgb: None,
            ..attach_info()
        };
        let mut d =
            LogitechDriver::with_io_attached(Box::new(io), att, Some(empty_baseline()));
        assert!(matches!(
            d.set_lighting(VIA_CHANNEL_PACKED, 0, &[EFFECT_FIXED]).await,
            Err(DriverError::Unsupported { .. })
        ));
        // Reads still serve the shadow (display-only).
        assert!(d.get_lighting(VIA_CHANNEL_PACKED, 0).await.is_ok());
    }

    #[tokio::test]
    async fn unsupported_lighting_channels_and_values_rejected() {
        let io = Arc::new(MockIo::new());
        let mut d = driver_with(io);
        assert!(d.set_lighting(7, 0, &[1]).await.is_err());
        assert!(d.get_lighting(7, 0).await.is_err());
        assert!(d.set_lighting(VIA_CHANNEL_RGB_MATRIX, 9, &[1]).await.is_err());
        assert!(d.get_lighting(VIA_CHANNEL_RGB_MATRIX, 9).await.is_err());
    }

    // ── Attach flow ──────────────────────────────────────────────────────

    /// Scripts a full successful handshake on receiver slot 2, with a
    /// baseline image containing one binding.
    fn script_attach(io: &MockIo, image: &[u8]) {
        let dev = 0x02;
        // Ping succeeds only on slot 2 (unscripted indexes return zeros,
        // which fail the magic-echo check like a receiver would).
        let mut pong = [0u8; LONG_PARAMS];
        pong[0] = 4; // protocol major
        pong[2] = PING_MAGIC;
        io.script((dev, FEAT_IDX_ROOT, ROOT_PING), pong);
        // getFeature order: 0x8100, then 0x8071, then 0x0005 (name absent).
        let mut feat = [0u8; LONG_PARAMS];
        feat[0] = FEAT_ONB_IDX;
        io.script((dev, FEAT_IDX_ROOT, ROOT_GET_FEATURE), feat);
        feat[0] = FEAT_RGB_IDX;
        io.script((dev, FEAT_IDX_ROOT, ROOT_GET_FEATURE), feat);
        io.script((dev, FEAT_IDX_ROOT, ROOT_GET_FEATURE), [0u8; LONG_PARAMS]);
        // getDescription: sector size at params[7..9].
        let mut desc = [0u8; LONG_PARAMS];
        desc[3] = 3; // profile count
        desc[7..9].copy_from_slice(&SECTOR_SIZE.to_be_bytes());
        io.script((dev, FEAT_ONB_IDX, ONB_GET_DESCRIPTION), desc);
        // getCurrentProfile → sector 1.
        let mut cur = [0u8; LONG_PARAMS];
        cur[0..2].copy_from_slice(&SECTOR.to_be_bytes());
        io.script((dev, FEAT_ONB_IDX, ONB_GET_CURRENT_PROFILE), cur);
        // memoryRead chunks.
        for chunk in image.chunks(MEM_CHUNK) {
            let mut p = [0u8; LONG_PARAMS];
            p[..chunk.len()].copy_from_slice(chunk);
            io.script((dev, FEAT_ONB_IDX, ONB_MEMORY_READ), p);
        }
    }

    #[tokio::test]
    async fn attach_discovers_slot_and_adopts_device_keymap() {
        let io = Arc::new(MockIo::new());
        let image = compile_profile_image(&empty_baseline(), &[(0, 0, 0x0004)]).unwrap();
        script_attach(&io, &image);

        let mut d = LogitechDriver::with_io(Box::new(io.clone()), "g915-test".into(), (1, 5), 3);
        assert_eq!(d.get_matrix_dimensions().await.unwrap(), (1, 5));

        // Wired 0xFF and slot 1 were probed and skipped before slot 2 bound.
        let pings = io.calls();
        let ping_idxs: Vec<u8> = pings
            .iter()
            .filter(|&&(_, f, fun, _)| f == FEAT_IDX_ROOT && fun == ROOT_PING)
            .map(|&(d, _, _, _)| d)
            .collect();
        assert_eq!(ping_idxs, vec![0xFF, 0x01, 0x02]);

        // Device keymap adopted as confirmed baseline.
        assert_eq!(d.get_keycode(0, 0, 0).await.unwrap(), 0x0004);
        assert_eq!(d.shadow.status(), super::super::CacheStatus::Confirmed);
        assert!(d.baseline.is_some());

        // Onboard mode was requested.
        assert_eq!(io.calls_for(FEAT_ONB_IDX, ONB_SET_MODE)[0][0], ONBOARD_MODE_ONBOARD);
    }

    #[tokio::test]
    async fn attach_with_bad_crc_keeps_shadow_and_refuses_commit() {
        let io = Arc::new(MockIo::new());
        // Valid handshake, but the sector content has a broken CRC.
        let mut image = empty_baseline();
        let len = image.len();
        image[len - 1] ^= 0xFF;
        script_attach(&io, &image);

        let mut d = LogitechDriver::with_io(Box::new(io.clone()), "g915-test".into(), (1, 5), 3);
        d.get_matrix_dimensions().await.unwrap();
        assert!(d.baseline.is_none());

        d.set_keycode(0, 0, 0, 0x0004).await.unwrap();
        assert!(matches!(
            d.commit_to_nvram().await,
            Err(DriverError::VendorCompile { .. })
        ));
    }

    #[tokio::test]
    async fn attach_fails_disconnected_when_nothing_answers() {
        // All probes return zeros → ping magic never echoes → no device.
        let io = Arc::new(MockIo::new());
        let mut d = LogitechDriver::with_io(Box::new(io), "g915-test".into(), (1, 5), 3);
        assert!(matches!(
            d.get_matrix_dimensions().await,
            Err(DriverError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn attach_rejects_implausible_sector_size() {
        let io = Arc::new(MockIo::new());
        let dev = 0x02;
        let mut pong = [0u8; LONG_PARAMS];
        pong[2] = PING_MAGIC;
        io.script((dev, FEAT_IDX_ROOT, ROOT_PING), pong);
        let mut feat = [0u8; LONG_PARAMS];
        feat[0] = FEAT_ONB_IDX;
        io.script((dev, FEAT_IDX_ROOT, ROOT_GET_FEATURE), feat);
        // 0x8071 and 0x0005 absent.
        io.script((dev, FEAT_IDX_ROOT, ROOT_GET_FEATURE), [0u8; LONG_PARAMS]);
        io.script((dev, FEAT_IDX_ROOT, ROOT_GET_FEATURE), [0u8; LONG_PARAMS]);
        // getDescription with a zero sector size.
        io.script((dev, FEAT_ONB_IDX, ONB_GET_DESCRIPTION), [0u8; LONG_PARAMS]);

        let mut d = LogitechDriver::with_io(Box::new(io), "g915-test".into(), (1, 5), 3);
        assert!(matches!(
            d.get_matrix_dimensions().await,
            Err(DriverError::ProtocolViolation { .. })
        ));
    }

    #[tokio::test]
    async fn model_name_defaults_without_name_feature() {
        let io = Arc::new(MockIo::new());
        let d = driver_with(io);
        assert_eq!(d.model_name(), "Logitech G915");
    }
}
