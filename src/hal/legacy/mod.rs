// SPDX-License-Identifier: GPL-3.0-or-later
//
//! Legacy polyfill driver infrastructure — shadow state machine.
//!
//! # Purpose
//!
//! Non-VIA keyboards (GMK67-family, and future Logitech/Razer backends) cannot
//! be queried for their current keymap over USB. Instead, the daemon maintains
//! a **shadow state** — a JSON-serialized cache of every keycode the host has
//! written — and compiles it into a vendor-specific binary blob on commit.
//!
//! This module provides the shared infrastructure that every legacy driver
//! delegates to:
//!
//! - [`CacheStatus`] — tracks whether the shadow matches confirmed hardware
//!   state, is pending a push, or failed and was rolled back.
//! - [`ShadowState`] — the keymap cache with JSON persistence, dirty tracking,
//!   snapshot/rollback, and atomic file writes.
//!
//! # Persistence Format
//!
//! Shadow state is serialized to `$XDG_DATA_HOME/clackd/<device_id>.json`
//! (resolved via the `directories` crate; falls back to
//! `$HOME/.local/share/clackd/` per XDG spec). The JSON schema is versioned
//! (`"version": 1`) for forward compatibility; the keymap is stored as an
//! array of `{layer, row, col, keycode}` entries, not as a map with tuple
//! keys, to keep `serde` straightforward.
//!
//! # Rollback Semantics (CLAUDE.md §4.2)
//!
//! Before each `commit_to_nvram`, the driver calls [`ShadowState::prepare_commit`]
//! to snapshot the current keymap. If the vendor blob push succeeds, the driver
//! calls [`ShadowState::confirm_commit`] to persist the JSON and clear the
//! dirty set. If the push fails, the driver calls [`ShadowState::rollback`] to
//! restore the keymap to the last-known-good snapshot and set the status to
//! `Failed`. The engine never observes stale state through `get_keycode`.

pub mod gmk67;

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

// ─────────────────────────────────────────────────────────────────────────────
// CacheStatus
// ─────────────────────────────────────────────────────────────────────────────

/// Tracks whether the shadow keymap matches confirmed hardware state.
///
/// **Context:** Internal to legacy drivers. Not exposed through the
/// [`KeyboardDriver`](super::KeyboardDriver) trait — that would require a
/// trait-level change affecting VIA. A future trait extension
/// (`fn cache_status()`) can surface this to frontends via D-Bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    /// Hardware state matches the shadow cache. The JSON file on disk is
    /// authoritative.
    Confirmed,
    /// A `set_keycode` has been issued but `commit_to_nvram` has not yet
    /// completed. The shadow reflects *intended* state, not confirmed state.
    Pending,
    /// The last `commit_to_nvram` failed and the shadow was rolled back to
    /// the last-known-good state.
    Failed,
}

// ─────────────────────────────────────────────────────────────────────────────
// JSON schema (versioned for forward compat)
// ─────────────────────────────────────────────────────────────────────────────

/// On-disk representation of the shadow state.
#[derive(Debug, Serialize, Deserialize)]
struct ShadowFile {
    version: u32,
    #[serde(default)]
    keymap: Vec<KeymapEntry>,
    #[serde(default)]
    lighting: Option<LightingState>,
}

/// A single remapped key in the persisted keymap.
#[derive(Debug, Serialize, Deserialize)]
struct KeymapEntry {
    layer: u8,
    row: u8,
    col: u8,
    keycode: u16,
}

/// Lighting state persisted alongside the keymap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightingState {
    pub mode: u8,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub brightness: u8,
    pub speed: u8,
    pub direction: u8,
    pub random: bool,
}

impl Default for LightingState {
    fn default() -> Self {
        Self {
            mode: 0x01, // MODE_STATIC
            r: 0xFF,
            g: 0xFF,
            b: 0xFF,
            brightness: 0x0F,
            speed: 0x00,
            direction: 0x00,
            random: false,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ShadowState
// ─────────────────────────────────────────────────────────────────────────────

/// Shadow keymap cache with JSON persistence, dirty tracking, and rollback.
///
/// **Ownership:** Embedded inside each legacy driver struct. One `ShadowState`
/// per device, exclusively owned by the driver's worker task.
pub struct ShadowState {
    /// Current intended keymap: `(layer, row, col) -> keycode`.
    keymap: BTreeMap<(u8, u8, u8), u16>,
    /// Last-known-good keymap snapshot, taken at `prepare_commit()`.
    confirmed: BTreeMap<(u8, u8, u8), u16>,
    /// Positions changed since the last successful commit.
    dirty: BTreeMap<(u8, u8, u8), u16>,
    /// Current lighting state (persisted alongside the keymap).
    lighting: LightingState,
    /// Whether the shadow matches confirmed hardware state.
    status: CacheStatus,
    /// Resolved path to the JSON file. `None` if XDG resolution failed.
    data_path: Option<PathBuf>,
}

impl ShadowState {
    /// Constructs a new `ShadowState`, loading from the JSON file if it exists.
    ///
    /// `device_id` is used as the JSON filename stem (e.g. `"05ac_024f"` →
    /// `$XDG_DATA_HOME/clackd/05ac_024f.json`). The `directories` crate
    /// resolves `XDG_DATA_HOME` with the XDG spec fallback.
    ///
    /// If the file does not exist or cannot be parsed, the shadow starts empty
    /// (all positions report `KC_NO`). This is intentional: the factory
    /// keymap is owned by the device firmware, and the shadow only tracks
    /// host-initiated remaps.
    pub fn load(device_id: &str) -> Self {
        let data_path = resolve_data_path(device_id);

        let (keymap, lighting) = match &data_path {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(contents) => match serde_json::from_str::<ShadowFile>(&contents) {
                    Ok(sf) => {
                        let mut km = BTreeMap::new();
                        for e in &sf.keymap {
                            km.insert((e.layer, e.row, e.col), e.keycode);
                        }
                        let lighting = sf.lighting.unwrap_or_default();
                        info!(
                            path = %path.display(),
                            keys = km.len(),
                            "loaded shadow state from disk",
                        );
                        (km, lighting)
                    }
                    Err(e) => {
                        warn!(
                            path = %path.display(),
                            error = %e,
                            "failed to parse shadow state JSON — starting fresh",
                        );
                        (BTreeMap::new(), LightingState::default())
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    debug!(path = %path.display(), "no shadow state file — starting fresh");
                    (BTreeMap::new(), LightingState::default())
                }
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to read shadow state file — starting fresh",
                    );
                    (BTreeMap::new(), LightingState::default())
                }
            },
            None => {
                warn!("XDG data directory could not be resolved — shadow state will not persist");
                (BTreeMap::new(), LightingState::default())
            }
        };

        let confirmed = keymap.clone();
        Self {
            keymap,
            confirmed,
            dirty: BTreeMap::new(),
            lighting,
            status: CacheStatus::Confirmed,
            data_path,
        }
    }

    /// Constructs an in-memory-only `ShadowState` (no disk persistence).
    /// Used in tests and test seams.
    pub fn ephemeral() -> Self {
        Self {
            keymap: BTreeMap::new(),
            confirmed: BTreeMap::new(),
            dirty: BTreeMap::new(),
            lighting: LightingState::default(),
            status: CacheStatus::Confirmed,
            data_path: None,
        }
    }

    // ── Accessors ────────────────────────────────────────────────────────

    /// Reads a keycode from the shadow cache. Returns `0x0000` (`KC_NO`) for
    /// positions that have never been remapped by the host.
    pub fn get_keycode(&self, layer: u8, row: u8, col: u8) -> u16 {
        self.keymap.get(&(layer, row, col)).copied().unwrap_or(0)
    }

    /// Updates a keycode in the shadow cache and marks the position dirty.
    pub fn set_keycode(&mut self, layer: u8, row: u8, col: u8, keycode: u16) {
        self.keymap.insert((layer, row, col), keycode);
        self.dirty.insert((layer, row, col), keycode);
        self.status = CacheStatus::Pending;
    }

    /// Returns whether any positions have been modified since the last commit.
    pub fn is_dirty(&self) -> bool {
        !self.dirty.is_empty()
    }

    /// Returns the distinct layers that have pending changes.
    pub fn dirty_layers(&self) -> Vec<u8> {
        let mut layers: Vec<u8> = self.dirty.keys().map(|&k| k.0).collect();
        layers.sort_unstable();
        layers.dedup();
        layers
    }

    /// Returns an iterator over all keymap entries for a given layer.
    pub fn layer_entries(&self, layer: u8) -> impl Iterator<Item = (u8, u8, u16)> + '_ {
        self.keymap
            .iter()
            .filter(move |&(&(l, _, _), _)| l == layer)
            .map(|(&(_, row, col), &keycode)| (row, col, keycode))
    }

    /// The current cache status.
    pub fn status(&self) -> CacheStatus {
        self.status
    }

    /// Returns the current lighting state.
    pub fn lighting(&self) -> &LightingState {
        &self.lighting
    }

    /// Updates the lighting state in the shadow.
    pub fn set_lighting(&mut self, lighting: LightingState) {
        self.lighting = lighting;
    }

    // ── Commit lifecycle ─────────────────────────────────────────────────

    /// Phase 1 of commit: sets status to `Pending`.
    ///
    /// Call this *before* starting the vendor blob push. The `confirmed`
    /// snapshot is NOT updated here — it still reflects the last successful
    /// commit (or the initial load), so `rollback()` always reverts to
    /// known-good state.
    pub fn prepare_commit(&mut self) {
        self.status = CacheStatus::Pending;
    }

    /// Phase 2a of commit (success): snapshots the current keymap as
    /// confirmed, clears the dirty set, sets status to `Confirmed`, and
    /// persists the shadow to JSON.
    ///
    /// Call this *after* the vendor blob push succeeds.
    pub fn confirm_commit(&mut self) {
        self.confirmed = self.keymap.clone();
        self.dirty.clear();
        self.status = CacheStatus::Confirmed;
        self.save();
    }

    /// Phase 2b of commit (failure): restores the keymap to the last
    /// successfully committed state, clears the dirty set, and sets status
    /// to `Failed`.
    ///
    /// Call this if the vendor blob push fails. Protects against partial
    /// commits (e.g. layer 0 pushed but layer 1 failed).
    pub fn rollback(&mut self) {
        self.keymap = self.confirmed.clone();
        self.dirty.clear();
        self.status = CacheStatus::Failed;
        warn!("shadow state rolled back to last-known-good after commit failure");
    }

    // ── Persistence ──────────────────────────────────────────────────────

    /// Writes the current shadow state to the JSON file atomically (write to
    /// `.tmp`, then rename). Errors are logged and swallowed — persistence
    /// failure must not abort a successful hardware commit.
    fn save(&self) {
        let Some(path) = &self.data_path else {
            return;
        };

        let sf = ShadowFile {
            version: 1,
            keymap: self
                .keymap
                .iter()
                .map(|(&(layer, row, col), &keycode)| KeymapEntry {
                    layer,
                    row,
                    col,
                    keycode,
                })
                .collect(),
            lighting: Some(self.lighting.clone()),
        };

        let json = match serde_json::to_string_pretty(&sf) {
            Ok(j) => j,
            Err(e) => {
                warn!(error = %e, "failed to serialize shadow state");
                return;
            }
        };

        // Ensure the parent directory exists.
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(
                    path = %parent.display(),
                    error = %e,
                    "failed to create shadow state directory",
                );
                return;
            }
        }

        // Atomic write: write to .tmp, then rename.
        let tmp_path = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp_path, &json) {
            warn!(
                path = %tmp_path.display(),
                error = %e,
                "failed to write shadow state temp file",
            );
            return;
        }
        if let Err(e) = std::fs::rename(&tmp_path, path) {
            warn!(
                src = %tmp_path.display(),
                dst = %path.display(),
                error = %e,
                "failed to rename shadow state temp file",
            );
            return;
        }

        debug!(path = %path.display(), keys = self.keymap.len(), "shadow state persisted");
    }
}

/// Resolves the XDG data path for a device's shadow state file.
///
/// Uses `directories::ProjectDirs` (already a dependency) to find
/// `$XDG_DATA_HOME/clackd/`. Falls back per XDG spec. Returns `None`
/// if the home directory cannot be determined (e.g. containerized
/// environment without `HOME` set).
fn resolve_data_path(device_id: &str) -> Option<PathBuf> {
    let proj = directories::ProjectDirs::from("io", "github", "clackd")?;
    let data_dir = proj.data_dir();
    Some(data_dir.join(format!("{device_id}.json")))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Creates a `ShadowState` backed by a temporary directory.
    fn shadow_with_tmpdir(dir: &TempDir, filename: &str) -> ShadowState {
        let path = dir.path().join(filename);
        ShadowState {
            keymap: BTreeMap::new(),
            confirmed: BTreeMap::new(),
            dirty: BTreeMap::new(),
            lighting: LightingState::default(),
            status: CacheStatus::Confirmed,
            data_path: Some(path),
        }
    }

    #[test]
    fn ephemeral_starts_empty_and_confirmed() {
        let s = ShadowState::ephemeral();
        assert!(s.keymap.is_empty());
        assert!(!s.is_dirty());
        assert_eq!(s.status(), CacheStatus::Confirmed);
    }

    #[test]
    fn set_keycode_marks_dirty_and_pending() {
        let mut s = ShadowState::ephemeral();
        s.set_keycode(0, 0, 0, 0x0004);
        assert!(s.is_dirty());
        assert_eq!(s.status(), CacheStatus::Pending);
        assert_eq!(s.get_keycode(0, 0, 0), 0x0004);
    }

    #[test]
    fn get_keycode_returns_zero_for_unset() {
        let s = ShadowState::ephemeral();
        assert_eq!(s.get_keycode(0, 5, 5), 0);
    }

    #[test]
    fn dirty_layers_deduplicates() {
        let mut s = ShadowState::ephemeral();
        s.set_keycode(0, 0, 0, 4);
        s.set_keycode(0, 1, 1, 5);
        s.set_keycode(1, 0, 0, 6);
        assert_eq!(s.dirty_layers(), vec![0, 1]);
    }

    #[test]
    fn prepare_and_confirm_clears_dirty() {
        let mut s = ShadowState::ephemeral();
        s.set_keycode(0, 0, 0, 0x0004);
        s.prepare_commit();
        assert_eq!(s.status(), CacheStatus::Pending);
        s.confirm_commit();
        assert!(!s.is_dirty());
        assert_eq!(s.status(), CacheStatus::Confirmed);
        // Keymap preserved.
        assert_eq!(s.get_keycode(0, 0, 0), 0x0004);
    }

    #[test]
    fn rollback_restores_last_committed_state() {
        let mut s = ShadowState::ephemeral();
        // Commit 0x0004 as the known-good state.
        s.set_keycode(0, 0, 0, 0x0004);
        s.prepare_commit();
        s.confirm_commit();

        // Change to a new value and attempt to commit (but it fails).
        s.set_keycode(0, 0, 0, 0x0005);
        assert_eq!(s.get_keycode(0, 0, 0), 0x0005);

        s.prepare_commit();
        s.rollback();

        // Rollback restores to the last successfully committed state (0x0004),
        // not the pending value (0x0005).
        assert_eq!(s.get_keycode(0, 0, 0), 0x0004);
        assert_eq!(s.status(), CacheStatus::Failed);
        assert!(!s.is_dirty());
    }

    #[test]
    fn rollback_with_no_prior_commit_clears_everything() {
        let mut s = ShadowState::ephemeral();
        // Set a value without ever committing.
        s.set_keycode(0, 0, 0, 0x0005);
        s.prepare_commit();
        s.rollback();

        // With no prior commit, confirmed is empty, so rollback clears.
        assert_eq!(s.get_keycode(0, 0, 0), 0);
        assert_eq!(s.status(), CacheStatus::Failed);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = TempDir::new().unwrap();
        let mut s = shadow_with_tmpdir(&dir, "test_device.json");
        s.set_keycode(0, 0, 0, 0x0004);
        s.set_keycode(1, 2, 3, 0x0042);
        s.prepare_commit();
        s.confirm_commit(); // saves

        // Read back.
        let path = dir.path().join("test_device.json");
        let contents = std::fs::read_to_string(&path).unwrap();
        let sf: ShadowFile = serde_json::from_str(&contents).unwrap();
        assert_eq!(sf.version, 1);
        assert_eq!(sf.keymap.len(), 2);
        assert!(sf.keymap.iter().any(|e| e.layer == 0
            && e.row == 0
            && e.col == 0
            && e.keycode == 0x0004));
        assert!(sf.keymap.iter().any(|e| e.layer == 1
            && e.row == 2
            && e.col == 3
            && e.keycode == 0x0042));
    }

    #[test]
    fn load_from_existing_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("existing.json");
        let json = r#"{
            "version": 1,
            "keymap": [
                {"layer": 0, "row": 1, "col": 2, "keycode": 99}
            ],
            "lighting": {
                "mode": 5, "r": 128, "g": 64, "b": 32,
                "brightness": 8, "speed": 3, "direction": 1, "random": true
            }
        }"#;
        std::fs::write(&path, json).unwrap();

        let mut s = ShadowState {
            keymap: BTreeMap::new(),
            confirmed: BTreeMap::new(),
            dirty: BTreeMap::new(),
            lighting: LightingState::default(),
            status: CacheStatus::Confirmed,
            data_path: Some(path.clone()),
        };

        // Simulate load by parsing the file.
        let contents = std::fs::read_to_string(&path).unwrap();
        let sf: ShadowFile = serde_json::from_str(&contents).unwrap();
        for e in &sf.keymap {
            s.keymap.insert((e.layer, e.row, e.col), e.keycode);
        }
        s.confirmed = s.keymap.clone();
        if let Some(l) = sf.lighting {
            s.lighting = l;
        }

        assert_eq!(s.get_keycode(0, 1, 2), 99);
        assert_eq!(s.lighting().mode, 5);
        assert_eq!(s.lighting().r, 128);
        assert!(s.lighting().random);
    }

    #[test]
    fn save_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("deep").join("nested");
        let mut s = ShadowState {
            keymap: BTreeMap::new(),
            confirmed: BTreeMap::new(),
            dirty: BTreeMap::new(),
            lighting: LightingState::default(),
            status: CacheStatus::Confirmed,
            data_path: Some(nested.join("device.json")),
        };
        s.set_keycode(0, 0, 0, 42);
        s.prepare_commit();
        s.confirm_commit();
        assert!(nested.join("device.json").exists());
    }

    #[test]
    fn layer_entries_filters_correctly() {
        let mut s = ShadowState::ephemeral();
        s.set_keycode(0, 0, 0, 4);
        s.set_keycode(0, 1, 1, 5);
        s.set_keycode(1, 0, 0, 6);

        let l0: Vec<_> = s.layer_entries(0).collect();
        assert_eq!(l0.len(), 2);
        let l1: Vec<_> = s.layer_entries(1).collect();
        assert_eq!(l1.len(), 1);
        assert_eq!(l1[0], (0, 0, 6));
    }
}
