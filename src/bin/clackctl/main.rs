// SPDX-License-Identifier: GPL-3.0-or-later
//
//! `clackctl` — command-line control for the `clackd` keyboard daemon.
//!
//! A thin D-Bus client of `io.github.clackd`, modelled on `ratbagctl`
//! (libratbag's CLI). It owns no device state — every subcommand maps to
//! one method on the `io.github.clackd.Device` interface and prints the
//! reply. The daemon must be running (or D-Bus-activatable) on the
//! session bus.
//!
//! # Command summary
//!
//! ```text
//! clackctl list                                # connected keyboards
//! clackctl info    <device>                    # identity, matrix, layers
//! clackctl keymap  <device> [--layer N]        # whole layer(s) as a grid
//! clackctl get     <device> <layer> <row> <col>
//! clackctl set     <device> <layer> <row> <col> <key>
//! clackctl keys    [filter]                    # known key names
//! clackctl lighting <device>                   # show current lighting
//! clackctl lighting <device> color red         # …and change it
//! clackctl commit  <device>                    # save changes to the keyboard
//! clackctl monitor                             # stream lifecycle events
//! ```
//!
//! `<device>` accepts a device id as printed by `list` (e.g. `hidraw0`),
//! its numeric index in the `list` output, or a unique case-insensitive
//! substring of the id or product name (`clackctl info ek68`).
//!
//! `<key>` accepts a QMK key name in any of its usual spellings (`KC_A`,
//! `a`, `esc`, `page-up`, `lshift`) as well as raw decimal (`76`) or hex
//! (`0x004C`) for keycodes outside the name table. Bare digits `0`–`9`
//! mean the number-row keys, not raw codes.
//!
//! `get-macro` / `set-macro` and `lighting … get-raw` / `set-raw` remain
//! deliberately raw (hex in, hex out) — they exist for protocol work and
//! scripting, not day-to-day use.

mod keycodes;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tokio_stream::StreamExt;

/// D-Bus proxy for `io.github.clackd.Device`.
///
/// **Note:** This mirrors the server-side interface in
/// `src/ipc/frontend.rs`. The two definitions are independent (zbus
/// generates a client from `#[proxy]` and a server from `#[interface]`)
/// but must agree on method signatures and the interface name.
#[zbus::proxy(
    interface = "io.github.clackd.Device",
    default_service = "io.github.clackd",
    default_path = "/io/github/clackd"
)]
trait Clackd {
    /// Enumerate connected device ids.
    fn list_devices(&self) -> zbus::Result<Vec<String>>;

    /// `(model, rows, cols, layer_count)` for a device.
    fn get_device_info(&self, device_id: &str) -> zbus::Result<(String, u8, u8, u8)>;
    /// `(vendor_id, product_id, product_name)` for a device.
    fn get_device_identity(&self, device_id: &str) -> zbus::Result<(u16, u16, String)>;
    /// Read the keycode at `(layer, row, col)`.
    fn get_keycode(&self, device_id: &str, layer: u8, row: u8, col: u8) -> zbus::Result<u16>;
    /// Write a keycode to `(layer, row, col)`.
    fn set_keycode(
        &self,
        device_id: &str,
        layer: u8,
        row: u8,
        col: u8,
        keycode: u16,
    ) -> zbus::Result<()>;
    /// Flush pending writes to NVRAM.
    fn commit(&self, device_id: &str) -> zbus::Result<()>;

    /// Read macro buffer bytes starting at `offset` for `length` bytes.
    fn get_macro(&self, device_id: &str, offset: u16, length: u8) -> zbus::Result<Vec<u8>>;
    /// Write macro buffer bytes at `offset`.
    fn set_macro(&self, device_id: &str, offset: u16, data: Vec<u8>) -> zbus::Result<()>;
    /// Read a lighting/RGB configuration value.
    fn get_lighting(&self, device_id: &str, channel: u8, value_id: u8) -> zbus::Result<Vec<u8>>;
    /// Write a lighting/RGB configuration value.
    fn set_lighting(
        &self,
        device_id: &str,
        channel: u8,
        value_id: u8,
        data: Vec<u8>,
    ) -> zbus::Result<()>;

    /// Emitted when a layout change is finalized.
    #[zbus(signal)]
    fn layout_updated(&self, device_id: &str) -> zbus::Result<()>;
    /// Emitted when a device is attached / reattached.
    #[zbus(signal)]
    fn device_added(&self, device_id: &str) -> zbus::Result<()>;
    /// Emitted when a device is detached / evicted.
    #[zbus(signal)]
    fn device_removed(&self, device_id: &str) -> zbus::Result<()>;
}

// --- VIA custom-value addressing (QMK `via.h`) -------------------------------
//
// The friendly `lighting` subcommands speak the RGB-matrix channel that
// both the native VIA driver and the GMK67 compatibility layer accept.
// Raw channel/value access stays available via `lighting … get-raw`.
const CH_RGB_MATRIX: u8 = 3; // id_qmk_rgb_matrix_channel
const VAL_BRIGHTNESS: u8 = 1; // id_qmk_rgb_matrix_brightness (0–255)
const VAL_EFFECT: u8 = 2; // id_qmk_rgb_matrix_effect (0 = off)
const VAL_SPEED: u8 = 3; // id_qmk_rgb_matrix_effect_speed (0–255)
const VAL_COLOR: u8 = 4; // id_qmk_rgb_matrix_color ([hue, sat], 0–255)

/// Effect names for the GMK67 family (`docs/protocol/epomaker-ek68.md`
/// section 4). Native VIA firmwares compile their own effect list, so on
/// other boards these names are a best-effort guess — the numeric id is
/// always printed alongside and always accepted as input.
const EFFECTS: &[(u8, &str)] = &[
    (0x01, "static"),
    (0x02, "keystroke-light-up"),
    (0x03, "keystroke-dim"),
    (0x04, "sparkle"),
    (0x05, "rain"),
    (0x06, "random-colors"),
    (0x07, "breathing"),
    (0x08, "spectrum-cycle"),
    (0x09, "ring-gradient"),
    (0x0A, "vertical-gradient"),
    (0x0B, "rainbow-wave"),
    (0x0C, "around-edges"),
    (0x0D, "keystroke-horizontal-lines"),
    (0x0E, "keystroke-tilted-lines"),
    (0x0F, "keystroke-ripples"),
    (0x10, "sequence"),
    (0x11, "wave-line"),
    (0x12, "tilted-lines"),
    (0x13, "back-and-forth"),
];

/// Named colours accepted by `lighting … color`.
const COLORS: &[(&str, [u8; 3])] = &[
    ("red", [0xFF, 0x00, 0x00]),
    ("orange", [0xFF, 0x80, 0x00]),
    ("yellow", [0xFF, 0xFF, 0x00]),
    ("green", [0x00, 0xFF, 0x00]),
    ("cyan", [0x00, 0xFF, 0xFF]),
    ("blue", [0x00, 0x00, 0xFF]),
    ("purple", [0x80, 0x00, 0xFF]),
    ("magenta", [0xFF, 0x00, 0xFF]),
    ("pink", [0xFF, 0x69, 0xB4]),
    ("white", [0xFF, 0xFF, 0xFF]),
];

#[derive(Parser)]
#[command(
    name = "clackctl",
    version,
    about = "Control keyboards managed by the clackd daemon",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List connected keyboards.
    List,
    /// Show one keyboard's identity, matrix size, and layer count.
    Info {
        /// Device id, list index, or name fragment (e.g. `ek68`).
        device: String,
    },
    /// Print a whole layer (or all layers) as a key-name grid.
    Keymap {
        /// Device id, list index, or name fragment.
        device: String,
        /// Only this layer (default: all layers).
        #[arg(long)]
        layer: Option<u8>,
    },
    /// Read the key at one (layer, row, col) position.
    Get {
        /// Device id, list index, or name fragment.
        device: String,
        /// Zero-indexed layer.
        layer: u8,
        /// Zero-indexed matrix row.
        row: u8,
        /// Zero-indexed matrix column.
        col: u8,
    },
    /// Assign a key to one (layer, row, col) position.
    Set {
        /// Device id, list index, or name fragment.
        device: String,
        /// Zero-indexed layer.
        layer: u8,
        /// Zero-indexed matrix row.
        row: u8,
        /// Zero-indexed matrix column.
        col: u8,
        /// Key name (`a`, `esc`, `KC_ENTER`, `page-up`), or a raw
        /// keycode in decimal (`76`) or hex (`0x004C`).
        #[arg(allow_hyphen_values = true)]
        key: String,
    },
    /// List the key names accepted by `set` (optionally filtered).
    Keys {
        /// Show only names containing this text.
        filter: Option<String>,
    },
    /// Save pending changes to the keyboard's memory.
    Commit {
        /// Device id, list index, or name fragment.
        device: String,
    },
    /// Show or change RGB lighting.
    Lighting {
        /// Device id, list index, or name fragment.
        device: String,
        /// What to do; omit to show the current lighting.
        #[command(subcommand)]
        action: Option<LightingAction>,
    },
    /// Read macro storage bytes (raw hex, for protocol work).
    GetMacro {
        /// Device id, list index, or name fragment.
        device: String,
        /// Byte offset into the macro buffer (decimal or 0x hex).
        offset: String,
        /// Number of bytes to read (1–28).
        length: u8,
    },
    /// Write macro storage bytes (raw hex, for protocol work).
    SetMacro {
        /// Device id, list index, or name fragment.
        device: String,
        /// Byte offset into the macro buffer (decimal or 0x hex).
        offset: String,
        /// Hex-encoded data bytes (e.g. `0102030405`).
        data: String,
    },
    /// Stream device and layout lifecycle events until interrupted.
    Monitor,
}

#[derive(Subcommand)]
enum LightingAction {
    /// Show the current lighting (the default when no action is given).
    Show,
    /// Set brightness: a percentage (`80%`) or a raw 0–255 value.
    Brightness {
        /// `0%`–`100%`, or `0`–`255`.
        value: String,
    },
    /// Set the animation effect, by name or number.
    Effect {
        /// Effect name from `lighting <device> effects`, or a numeric id.
        effect: String,
    },
    /// Set the effect animation speed: a percentage or a raw 0–255 value.
    Speed {
        /// `0%`–`100%`, or `0`–`255`.
        value: String,
    },
    /// Set the lighting colour.
    Color {
        /// A colour name (`red`, `cyan`, …), or hex `rrggbb` / `#rrggbb`.
        color: String,
    },
    /// Turn the lighting off.
    Off,
    /// List the effect names accepted by `effect`.
    Effects,
    /// Read a raw VIA custom value: `<channel> <value_id>` (hex out).
    GetRaw {
        /// VIA custom channel (RGB-matrix=3, RGBLIGHT=2, backlight=1).
        channel: String,
        /// Value id within the channel.
        value_id: String,
    },
    /// Write a raw VIA custom value: `<channel> <value_id> <hexdata>`.
    SetRaw {
        /// VIA custom channel (RGB-matrix=3, RGBLIGHT=2, backlight=1).
        channel: String,
        /// Value id within the channel.
        value_id: String,
        /// Hex-encoded value bytes (e.g. `FF`, or `AABB` for hue+sat).
        data: String,
    },
}

/// Boxed error type for the CLI. Coarse on purpose — failures are
/// reported to the user as a single stderr line, not handled
/// programmatically.
type CliError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Rust ignores SIGPIPE by default, turning `clackctl keys | head`
    // into a "failed printing to stdout" panic when the pipe closes.
    // Restore the conventional Unix behaviour: die quietly.
    // SAFETY: SigDfl installs the default disposition; no handler code
    // runs, and this happens before any other thread exists.
    unsafe {
        let _ = nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGPIPE,
            nix::sys::signal::SigHandler::SigDfl,
        );
    }

    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("clackctl: {}", friendly_error(&e));
            ExitCode::FAILURE
        }
    }
}

/// Rewrites D-Bus transport errors into plain language. The daemon maps
/// `DaemonError` onto well-known `org.freedesktop.DBus.Error.*` names
/// (see `src/ipc/frontend.rs`); this is the inverse mapping, for humans.
fn friendly_error(e: &CliError) -> String {
    let Some(zbus::Error::MethodError(name, detail, _)) = e.downcast_ref::<zbus::Error>() else {
        if let Some(zbus::Error::InterfaceNotFound) = e.downcast_ref::<zbus::Error>() {
            return "clackd does not expose the expected interface — version mismatch between clackctl and clackd?".into();
        }
        return e.to_string();
    };
    let detail = detail.as_deref().unwrap_or("no detail");
    match name.as_str() {
        "org.freedesktop.DBus.Error.NotSupported" => {
            format!("this keyboard cannot do that: {detail}")
        }
        "org.freedesktop.DBus.Error.UnknownObject" => {
            format!("keyboard not found: {detail} (see `clackctl list`)")
        }
        "org.freedesktop.DBus.Error.AccessDenied" => {
            format!("access denied: {detail} (check the udev uaccess rule for /dev/hidraw*)")
        }
        "org.freedesktop.DBus.Error.ServiceUnknown" => {
            "clackd is not running on the session bus — start it and retry".into()
        }
        _ => format!("{detail} [{name}]"),
    }
}

async fn run(cli: Cli) -> Result<(), CliError> {
    let connection = zbus::Connection::session().await?;
    let proxy = ClackdProxy::new(&connection).await?;

    match cli.command {
        Command::List => cmd_list(&proxy).await,
        Command::Info { device } => cmd_info(&proxy, &device).await,
        Command::Keymap { device, layer } => cmd_keymap(&proxy, &device, layer).await,
        Command::Get { device, layer, row, col } => cmd_get(&proxy, &device, layer, row, col).await,
        Command::Set { device, layer, row, col, key } => {
            cmd_set(&proxy, &device, layer, row, col, &key).await
        }
        Command::Keys { filter } => cmd_keys(filter.as_deref()),
        Command::Commit { device } => cmd_commit(&proxy, &device).await,
        Command::Lighting { device, action } => {
            cmd_lighting(&proxy, &device, action.unwrap_or(LightingAction::Show)).await
        }
        Command::GetMacro { device, offset, length } => {
            cmd_get_macro(&proxy, &device, &offset, length).await
        }
        Command::SetMacro { device, offset, data } => {
            cmd_set_macro(&proxy, &device, &offset, &data).await
        }
        Command::Monitor => cmd_monitor(&proxy).await,
    }
}

// --- Device enumeration -------------------------------------------------------

async fn cmd_list(proxy: &ClackdProxy<'_>) -> Result<(), CliError> {
    let devices = proxy.list_devices().await?;
    if devices.is_empty() {
        println!("No keyboards connected (or none that clackd manages).");
        return Ok(());
    }
    for (idx, id) in devices.iter().enumerate() {
        println!("{idx}: {}", describe_device(proxy, id).await);
    }
    Ok(())
}

/// One-line human summary of a device; degrades to the bare id when the
/// detail queries fail (e.g. worker mid-restart).
async fn describe_device(proxy: &ClackdProxy<'_>, id: &str) -> String {
    let identity = proxy.get_device_identity(id).await;
    let info = proxy.get_device_info(id).await;
    match (identity, info) {
        (Ok((vid, pid, name)), Ok((_, rows, cols, layers))) => {
            let name = if name.is_empty() { "unnamed".to_owned() } else { name };
            format!("{id} — {name} [{vid:04x}:{pid:04x}], {rows}×{cols} keys, {layers} layers")
        }
        _ => id.to_owned(),
    }
}

async fn cmd_info(proxy: &ClackdProxy<'_>, device: &str) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let (model, rows, cols, layers) = proxy.get_device_info(&id).await?;
    let (vid, pid, name) = proxy.get_device_identity(&id).await?;
    println!("{id}:");
    println!("  name:    {}", if name.is_empty() { "(unknown)" } else { &name });
    println!("  usb id:  {vid:04x}:{pid:04x}");
    println!("  driver:  {}", if model.is_empty() { "(unknown)" } else { &model });
    println!("  matrix:  {rows} rows × {cols} columns");
    println!("  layers:  {layers}");
    Ok(())
}

// --- Keymap -------------------------------------------------------------------

async fn cmd_get(
    proxy: &ClackdProxy<'_>,
    device: &str,
    layer: u8,
    row: u8,
    col: u8,
) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let keycode = proxy.get_keycode(&id, layer, row, col).await?;
    println!("{}", keycodes::describe(keycode));
    Ok(())
}

async fn cmd_set(
    proxy: &ClackdProxy<'_>,
    device: &str,
    layer: u8,
    row: u8,
    col: u8,
    key: &str,
) -> Result<(), CliError> {
    // Validate the key before touching the bus — a typo'd name should be
    // reported even when the daemon or device is unavailable.
    let keycode = parse_key(key)?;
    let id = resolve_device(proxy, device).await?;
    proxy.set_keycode(&id, layer, row, col, keycode).await?;
    println!(
        "{id}: layer {layer}, row {row}, col {col} ← {}",
        keycodes::describe(keycode)
    );
    Ok(())
}

async fn cmd_keymap(
    proxy: &ClackdProxy<'_>,
    device: &str,
    layer: Option<u8>,
) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let (_, rows, cols, layer_count) = proxy.get_device_info(&id).await?;
    let layers: Vec<u8> = match layer {
        Some(l) => vec![l],
        None => (0..layer_count).collect(),
    };

    for &l in &layers {
        // Collect the whole layer first so column widths can be computed
        // from actual content (raw hex for unnamed codes is 6 chars wide).
        let mut grid: Vec<Vec<String>> = Vec::with_capacity(rows as usize);
        for r in 0..rows {
            let mut row_labels = Vec::with_capacity(cols as usize);
            for c in 0..cols {
                let code = proxy.get_keycode(&id, l, r, c).await?;
                row_labels.push(keycodes::short_label(code));
            }
            grid.push(row_labels);
        }
        let width = grid
            .iter()
            .flatten()
            .map(|s| s.chars().count())
            .max()
            .unwrap_or(2);

        println!("{id} layer {l}:");
        print!("     ");
        for c in 0..cols {
            print!(" {:>width$}", format!("c{c}"));
        }
        println!();
        for (r, row_labels) in grid.iter().enumerate() {
            print!("  r{r} ");
            for label in row_labels {
                print!(" {label:>width$}");
            }
            println!();
        }
        println!();
    }
    println!("-- = no key assigned · TRNS = falls through to the layer below");
    Ok(())
}

fn cmd_keys(filter: Option<&str>) -> Result<(), CliError> {
    let filter = filter.map(str::to_ascii_lowercase);
    let mut shown = 0usize;
    for k in keycodes::ALL {
        let line = if k.aliases.is_empty() {
            format!("{:#06x}  {}", k.code, k.name)
        } else {
            format!("{:#06x}  {}  (also: {})", k.code, k.name, k.aliases.join(", "))
        };
        if filter
            .as_deref()
            .is_none_or(|f| line.to_ascii_lowercase().contains(f))
        {
            println!("{line}");
            shown += 1;
        }
    }
    if shown == 0 {
        println!("No key names match — try `clackctl keys` for the full list.");
    }
    Ok(())
}

async fn cmd_commit(proxy: &ClackdProxy<'_>, device: &str) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    proxy.commit(&id).await?;
    println!("{id}: pending changes saved to the keyboard");
    Ok(())
}

// --- Lighting -------------------------------------------------------------------

async fn cmd_lighting(
    proxy: &ClackdProxy<'_>,
    device: &str,
    action: LightingAction,
) -> Result<(), CliError> {
    // The effect listing is static — print it without requiring the
    // device (or the daemon) to be reachable.
    if let LightingAction::Effects = action {
        for (idx, name) in EFFECTS {
            println!("{idx:#04x}  {name}");
        }
        println!("(names are for the GMK67 family; other keyboards may map ids differently)");
        return Ok(());
    }

    let id = resolve_device(proxy, device).await?;
    match action {
        LightingAction::Show => lighting_show(proxy, &id).await,
        LightingAction::Brightness { value } => {
            let v = parse_level(&value)?;
            proxy.set_lighting(&id, CH_RGB_MATRIX, VAL_BRIGHTNESS, vec![v]).await?;
            println!("{id}: brightness set to {} ({v}/255)", percent(v));
            Ok(())
        }
        LightingAction::Speed { value } => {
            let v = parse_level(&value)?;
            proxy.set_lighting(&id, CH_RGB_MATRIX, VAL_SPEED, vec![v]).await?;
            println!("{id}: effect speed set to {} ({v}/255)", percent(v));
            Ok(())
        }
        LightingAction::Effect { effect } => {
            let e = parse_effect(&effect)?;
            proxy.set_lighting(&id, CH_RGB_MATRIX, VAL_EFFECT, vec![e]).await?;
            println!("{id}: effect set to {}", effect_label(e));
            Ok(())
        }
        LightingAction::Off => {
            // VIA convention: effect 0 is "off".
            proxy.set_lighting(&id, CH_RGB_MATRIX, VAL_EFFECT, vec![0]).await?;
            println!("{id}: lighting turned off");
            Ok(())
        }
        LightingAction::Color { color } => {
            let [r, g, b] = parse_color(&color)?;
            let (h, s) = rgb_to_hs(r, g, b);
            proxy.set_lighting(&id, CH_RGB_MATRIX, VAL_COLOR, vec![h, s]).await?;
            println!("{id}: colour set to #{r:02x}{g:02x}{b:02x} (hue {h}, sat {s})");
            Ok(())
        }
        // INVARIANT: handled by the early return above.
        LightingAction::Effects => Ok(()),
        LightingAction::GetRaw { channel, value_id } => {
            let data = proxy
                .get_lighting(&id, parse_u8(&channel)?, parse_u8(&value_id)?)
                .await?;
            println!("{}", to_hex(&data));
            Ok(())
        }
        LightingAction::SetRaw { channel, value_id, data } => {
            proxy
                .set_lighting(&id, parse_u8(&channel)?, parse_u8(&value_id)?, parse_hex_bytes(&data)?)
                .await?;
            println!("ok");
            Ok(())
        }
    }
}

/// Reads the four RGB-matrix values and prints them in human terms. Each
/// read degrades independently — a keyboard that answers brightness but
/// not colour still gets a useful report.
async fn lighting_show(proxy: &ClackdProxy<'_>, id: &str) -> Result<(), CliError> {
    println!("{id} lighting:");

    match proxy.get_lighting(id, CH_RGB_MATRIX, VAL_EFFECT).await {
        Ok(v) if !v.is_empty() => {
            if v[0] == 0 {
                println!("  effect:      off");
            } else {
                println!("  effect:      {}", effect_label(v[0]));
            }
        }
        Ok(_) => println!("  effect:      (empty reply)"),
        Err(e) => println!("  effect:      {}", unsupported_note(&e)),
    }
    match proxy.get_lighting(id, CH_RGB_MATRIX, VAL_BRIGHTNESS).await {
        Ok(v) if !v.is_empty() => {
            println!("  brightness:  {} ({}/255)", percent(v[0]), v[0]);
        }
        Ok(_) => println!("  brightness:  (empty reply)"),
        Err(e) => println!("  brightness:  {}", unsupported_note(&e)),
    }
    match proxy.get_lighting(id, CH_RGB_MATRIX, VAL_SPEED).await {
        Ok(v) if !v.is_empty() => {
            println!("  speed:       {} ({}/255)", percent(v[0]), v[0]);
        }
        Ok(_) => println!("  speed:       (empty reply)"),
        Err(e) => println!("  speed:       {}", unsupported_note(&e)),
    }
    match proxy.get_lighting(id, CH_RGB_MATRIX, VAL_COLOR).await {
        Ok(v) if v.len() >= 2 => {
            let (r, g, b) = hs_to_rgb(v[0], v[1]);
            println!("  colour:      #{r:02x}{g:02x}{b:02x} (hue {}, sat {})", v[0], v[1]);
        }
        Ok(_) => println!("  colour:      (empty reply)"),
        Err(e) => println!("  colour:      {}", unsupported_note(&e)),
    }
    Ok(())
}

/// Short in-table note for a failed lighting read.
fn unsupported_note(e: &zbus::Error) -> String {
    if let zbus::Error::MethodError(name, _, _) = e
        && name.as_str() == "org.freedesktop.DBus.Error.NotSupported"
    {
        return "not supported by this keyboard".into();
    }
    format!("read failed ({e})")
}

/// `id (name)` when the effect id is in the GMK67 table, bare hex otherwise.
fn effect_label(id: u8) -> String {
    match EFFECTS.iter().find(|(i, _)| *i == id) {
        Some((_, name)) => format!("{name} ({id:#04x})"),
        None => format!("{id:#04x} (no name for this keyboard)"),
    }
}

fn percent(v: u8) -> String {
    format!("{}%", (v as u16 * 100 + 127) / 255)
}

// --- Macros (raw) ---------------------------------------------------------------

async fn cmd_get_macro(
    proxy: &ClackdProxy<'_>,
    device: &str,
    offset: &str,
    length: u8,
) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let off = parse_u16(offset)?;
    let data = proxy.get_macro(&id, off, length).await?;
    for (i, chunk) in data.chunks(16).enumerate() {
        let addr = off as usize + i * 16;
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' })
            .collect();
        println!("{addr:04x}  {:<48}  |{ascii}|", hex.join(" "));
    }
    Ok(())
}

async fn cmd_set_macro(
    proxy: &ClackdProxy<'_>,
    device: &str,
    offset: &str,
    data: &str,
) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let off = parse_u16(offset)?;
    let bytes = parse_hex_bytes(data)?;
    let len = bytes.len();
    proxy.set_macro(&id, off, bytes).await?;
    println!("{id}: wrote {len} bytes at offset {off:#06x}");
    Ok(())
}

// --- Monitor --------------------------------------------------------------------

async fn cmd_monitor(proxy: &ClackdProxy<'_>) -> Result<(), CliError> {
    let mut added = proxy.receive_device_added().await?;
    let mut removed = proxy.receive_device_removed().await?;
    let mut layout = proxy.receive_layout_updated().await?;

    eprintln!("Watching for keyboard events — press Ctrl-C to stop.");
    loop {
        tokio::select! {
            Some(sig) = added.next() => {
                let args = sig.args()?;
                println!("keyboard connected:    {}", args.device_id());
            }
            Some(sig) = removed.next() => {
                let args = sig.args()?;
                println!("keyboard disconnected: {}", args.device_id());
            }
            Some(sig) = layout.next() => {
                let args = sig.args()?;
                println!("layout saved:          {}", args.device_id());
            }
            else => break,
        }
    }
    Ok(())
}

// --- Device resolution ------------------------------------------------------------

/// Resolves a user-supplied device spec to a concrete device id.
///
/// Tries, in order: exact id match, numeric index into the `list`
/// output, then a unique case-insensitive substring of the id or the
/// USB product name. Errors list the available devices so the user can
/// correct the spec.
async fn resolve_device(proxy: &ClackdProxy<'_>, spec: &str) -> Result<String, CliError> {
    let devices = proxy.list_devices().await?;
    if devices.iter().any(|d| d == spec) {
        return Ok(spec.to_owned());
    }
    if let Ok(idx) = spec.parse::<usize>()
        && let Some(id) = devices.get(idx)
    {
        return Ok(id.clone());
    }

    // Substring match over id and product name; ambiguity is an error.
    let needle = spec.to_ascii_lowercase();
    let mut matches: Vec<String> = Vec::new();
    for id in &devices {
        let name = proxy
            .get_device_identity(id)
            .await
            .map(|(_, _, name)| name)
            .unwrap_or_default();
        if id.to_ascii_lowercase().contains(&needle)
            || name.to_ascii_lowercase().contains(&needle)
        {
            matches.push(id.clone());
        }
    }
    match matches.as_slice() {
        [only] => return Ok(only.clone()),
        [] => {}
        many => {
            return Err(format!(
                "'{spec}' matches several keyboards: {} — be more specific",
                many.join(", ")
            )
            .into());
        }
    }

    let available = if devices.is_empty() {
        "none connected".to_owned()
    } else {
        devices.join(", ")
    };
    Err(format!("no keyboard matching '{spec}' (available: {available})").into())
}

// --- Input parsing ----------------------------------------------------------------

/// Parses a key argument: a name from the keycode table first, then raw
/// decimal or `0x`-prefixed hex. Names win over numbers so that a bare
/// `1` means the number-row key, not keycode 0x0001.
fn parse_key(s: &str) -> Result<u16, CliError> {
    if let Some(code) = keycodes::by_name(s) {
        return Ok(code);
    }
    parse_u16(s).map_err(|_| {
        format!("unknown key '{s}' — try a name from `clackctl keys`, or a 0xHEX keycode").into()
    })
}

/// Parses a `u16` in decimal or `0x`-prefixed hex.
fn parse_u16(s: &str) -> Result<u16, CliError> {
    let s = s.trim();
    let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16),
        None => s.parse::<u16>(),
    };
    parsed.map_err(|_| format!("invalid number '{s}' (use decimal or 0xHEX)").into())
}

/// Parses a `u8` in decimal or `0x`-prefixed hex.
fn parse_u8(s: &str) -> Result<u8, CliError> {
    let s = s.trim();
    let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16),
        None => s.parse::<u8>(),
    };
    parsed.map_err(|_| format!("invalid byte '{s}' (use decimal or 0xHEX)").into())
}

/// Parses a brightness/speed level: `NN%` (0–100, scaled onto 0–255,
/// rounding to nearest) or a raw 0–255 value.
fn parse_level(s: &str) -> Result<u8, CliError> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        let pct: u16 = pct
            .trim()
            .parse()
            .map_err(|_| format!("invalid percentage '{s}'"))?;
        if pct > 100 {
            return Err(format!("percentage out of range '{s}' (0%–100%)").into());
        }
        return Ok(((pct * 255 + 50) / 100) as u8);
    }
    parse_u8(s).map_err(|_| format!("invalid level '{s}' (use 0–255 or 0%–100%)").into())
}

/// Parses an effect: a name from [`EFFECTS`] (case-insensitive, spaces
/// and underscores accepted for dashes), `off`, or a numeric id.
fn parse_effect(s: &str) -> Result<u8, CliError> {
    let norm = s.trim().to_ascii_lowercase().replace([' ', '_'], "-");
    if norm == "off" {
        return Ok(0);
    }
    if let Some((id, _)) = EFFECTS.iter().find(|(_, name)| *name == norm) {
        return Ok(*id);
    }
    parse_u8(s).map_err(|_| {
        format!("unknown effect '{s}' — see `clackctl lighting <device> effects`").into()
    })
}

/// Parses a colour: a name from [`COLORS`] or hex `rrggbb` / `#rrggbb`.
fn parse_color(s: &str) -> Result<[u8; 3], CliError> {
    let raw = s.trim();
    if let Some((_, rgb)) = COLORS.iter().find(|(name, _)| name.eq_ignore_ascii_case(raw)) {
        return Ok(*rgb);
    }
    let hex = raw.strip_prefix('#').unwrap_or(raw);
    if hex.len() == 6
        && let Ok(v) = u32::from_str_radix(hex, 16)
    {
        return Ok([(v >> 16) as u8, (v >> 8) as u8, v as u8]);
    }
    let names: Vec<&str> = COLORS.iter().map(|(n, _)| *n).collect();
    Err(format!("unknown colour '{s}' (use #rrggbb, or one of: {})", names.join(", ")).into())
}

/// Parses a hex string (e.g. `0102AABB`) into a `Vec<u8>`.
fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, CliError> {
    let s = s.trim();
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if !hex.len().is_multiple_of(2) {
        return Err(format!("hex data must have even length, got '{s}'").into());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|_| format!("invalid hex byte at position {i} in '{s}'").into())
        })
        .collect()
}

fn to_hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

// --- Colour conversion -------------------------------------------------------------
//
// Mirrors the integer HSV↔RGB in `src/hal/legacy/gmk67.rs` (which mirrors
// QMK) so the colour printed by `lighting show` matches the colour the
// user set. Duplicated because bins cannot import the daemon's modules.

/// Integer HSV→RGB at full value, hue and saturation in VIA's 0–255 range.
fn hs_to_rgb(h: u8, s: u8) -> (u8, u8, u8) {
    if s == 0 {
        return (255, 255, 255);
    }
    let region = h / 43;
    let remainder = (h as u16 - region as u16 * 43) * 6;
    let s = s as u16;
    let p = (255 * (255 - s) / 255) as u8;
    let q = (255 * (255 - (s * remainder) / 255) / 255) as u8;
    let t = (255 * (255 - (s * (255 - remainder)) / 255) / 255) as u8;
    match region {
        0 => (255, t, p),
        1 => (q, 255, p),
        2 => (p, 255, t),
        3 => (p, q, 255),
        4 => (t, p, 255),
        _ => (255, p, q),
    }
}

/// Integer RGB→(hue, sat) in VIA's 0–255 range.
fn rgb_to_hs(r: u8, g: u8, b: u8) -> (u8, u8) {
    let max = r.max(g).max(b) as i32;
    let min = r.min(g).min(b) as i32;
    let delta = max - min;
    if max == 0 || delta == 0 {
        return (0, 0);
    }
    let s = (delta * 255 / max) as u8;
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let h = if r == max {
        43 * (g - b) / delta
    } else if g == max {
        85 + 43 * (b - r) / delta
    } else {
        171 + 43 * (r - g) / delta
    };
    (h.rem_euclid(256) as u8, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_prefers_names_over_numbers() {
        // Bare "1" is the number-row key, not raw keycode 0x0001.
        assert_eq!(parse_key("1").unwrap(), 0x001E);
        // Values with no name fall through to numeric parsing.
        assert_eq!(parse_key("76").unwrap(), 76);
        assert_eq!(parse_key("0x004C").unwrap(), 0x004C);
        assert_eq!(parse_key("esc").unwrap(), 0x0029);
        assert!(parse_key("notakey").is_err());
    }

    #[test]
    fn parse_level_accepts_percent_and_raw() {
        assert_eq!(parse_level("0%").unwrap(), 0);
        assert_eq!(parse_level("100%").unwrap(), 255);
        assert_eq!(parse_level("50%").unwrap(), 128);
        assert_eq!(parse_level("255").unwrap(), 255);
        assert_eq!(parse_level("0x80").unwrap(), 0x80);
        assert!(parse_level("101%").is_err());
        assert!(parse_level("256").is_err());
        assert!(parse_level("bright").is_err());
    }

    #[test]
    fn parse_effect_accepts_names_off_and_ids() {
        assert_eq!(parse_effect("static").unwrap(), 0x01);
        assert_eq!(parse_effect("Spectrum Cycle").unwrap(), 0x08);
        assert_eq!(parse_effect("spectrum_cycle").unwrap(), 0x08);
        assert_eq!(parse_effect("off").unwrap(), 0);
        assert_eq!(parse_effect("0x13").unwrap(), 0x13);
        assert!(parse_effect("disco").is_err());
    }

    #[test]
    fn parse_color_accepts_names_and_hex() {
        assert_eq!(parse_color("red").unwrap(), [0xFF, 0x00, 0x00]);
        assert_eq!(parse_color("RED").unwrap(), [0xFF, 0x00, 0x00]);
        assert_eq!(parse_color("#00ff00").unwrap(), [0x00, 0xFF, 0x00]);
        assert_eq!(parse_color("0000ff").unwrap(), [0x00, 0x00, 0xFF]);
        assert!(parse_color("beige").is_err());
        assert!(parse_color("#12345").is_err());
    }

    #[test]
    fn color_round_trips_through_hue_sat() {
        // QMK's integer HSV math is not exact (green comes back as
        // (3,255,0)), so `lighting show` after `lighting color X` is
        // correct to within a few steps per component — assert that.
        for &(name, [r, g, b]) in COLORS.iter().filter(|(n, _)| {
            matches!(*n, "red" | "green" | "blue" | "white")
        }) {
            let (h, s) = rgb_to_hs(r, g, b);
            let (r2, g2, b2) = hs_to_rgb(h, s);
            for (a, b) in [(r, r2), (g, g2), (b, b2)] {
                assert!(a.abs_diff(b) <= 3, "{name}: {a} vs {b} off by more than 3");
            }
        }
    }

    #[test]
    fn parse_hex_bytes_round_trips() {
        assert_eq!(parse_hex_bytes("01ff00").unwrap(), vec![0x01, 0xFF, 0x00]);
        assert_eq!(parse_hex_bytes("0x01FF").unwrap(), vec![0x01, 0xFF]);
        assert!(parse_hex_bytes("abc").is_err());
        assert!(parse_hex_bytes("zz").is_err());
    }

    #[test]
    fn percent_rounds_to_nearest() {
        assert_eq!(percent(255), "100%");
        assert_eq!(percent(0), "0%");
        assert_eq!(percent(128), "50%");
    }
}
