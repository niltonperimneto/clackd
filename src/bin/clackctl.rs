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
//! clackctl list                              # enumerate connected devices
//! clackctl info   <device>                   # matrix dimensions + layer count
//! clackctl get    <device> <layer> <row> <col>
//! clackctl set    <device> <layer> <row> <col> <keycode>
//! clackctl commit <device>                   # flush pending writes to NVRAM
//! clackctl monitor                           # stream lifecycle/layout events
//! ```
//!
//! `<device>` accepts either a device id as printed by `list` (e.g.
//! `hidraw0`) or its numeric index in the `list` output.
//!
//! `<keycode>` accepts decimal (`76`) or hex (`0x004C`). Output is always
//! 4-digit hex. A symbolic VIA keycode table (`KC_A` …) is a future
//! enhancement — for now this is a raw u16 tool.

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
    /// `(rows, cols, layer_count)` for a device.
    fn get_device_info(&self, device_id: &str) -> zbus::Result<(u8, u8, u8)>;
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
    fn set_lighting(&self, device_id: &str, channel: u8, value_id: u8, data: Vec<u8>) -> zbus::Result<()>;

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

#[derive(Parser)]
#[command(
    name = "clackctl",
    version,
    about = "Control the clackd VIA keyboard daemon",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List connected devices, one `index: id` per line.
    List,
    /// Show a device's matrix dimensions and layer count.
    Info {
        /// Device id (e.g. `hidraw0`) or numeric index from `list`.
        device: String,
    },
    /// Show a device's USB identity (vendor id, product id, name).
    Identity {
        /// Device id (e.g. `hidraw0`) or numeric index from `list`.
        device: String,
    },
    /// Read the keycode at a `(layer, row, col)` slot.
    Get {
        /// Device id or numeric index.
        device: String,
        /// Zero-indexed layer.
        layer: u8,
        /// Zero-indexed matrix row.
        row: u8,
        /// Zero-indexed matrix column.
        col: u8,
    },
    /// Write a keycode to a `(layer, row, col)` slot.
    Set {
        /// Device id or numeric index.
        device: String,
        /// Zero-indexed layer.
        layer: u8,
        /// Zero-indexed matrix row.
        row: u8,
        /// Zero-indexed matrix column.
        col: u8,
        /// Keycode — decimal (`76`) or hex (`0x004C`).
        keycode: String,
    },
    /// Flush pending writes to the device's NVRAM.
    Commit {
        /// Device id or numeric index.
        device: String,
    },
    /// Read macro buffer bytes from the device.
    GetMacro {
        /// Device id or numeric index.
        device: String,
        /// Byte offset into the macro buffer (decimal or 0x hex).
        offset: String,
        /// Number of bytes to read (1–28).
        length: u8,
    },
    /// Write macro buffer bytes to the device.
    SetMacro {
        /// Device id or numeric index.
        device: String,
        /// Byte offset into the macro buffer (decimal or 0x hex).
        offset: String,
        /// Hex-encoded data bytes (e.g. `"0102030405"`).
        data: String,
    },
    /// Read a lighting/RGB configuration value.
    GetLighting {
        /// Device id or numeric index.
        device: String,
        /// VIA custom channel (decimal or 0x hex): RGB-matrix=3, RGBLIGHT=2, backlight=1.
        channel: String,
        /// Value id within the channel: brightness=1, effect=2, speed=3, colour=4.
        value_id: String,
    },
    /// Write a lighting/RGB configuration value.
    SetLighting {
        /// Device id or numeric index.
        device: String,
        /// VIA custom channel (decimal or 0x hex): RGB-matrix=3, RGBLIGHT=2, backlight=1.
        channel: String,
        /// Value id within the channel: brightness=1, effect=2, speed=3, colour=4.
        value_id: String,
        /// Hex-encoded value bytes (e.g. `"FF"` brightness, `"AABB"` hue+sat).
        data: String,
    },
    /// Stream device and layout lifecycle events until interrupted.
    Monitor,
}

/// Boxed error type for the CLI. Coarse on purpose — failures are
/// reported to the user as a single stderr line, not handled
/// programmatically.
type CliError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("clackctl: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), CliError> {
    let connection = zbus::Connection::session().await?;
    let proxy = ClackdProxy::new(&connection).await?;

    match cli.command {
        Command::List => cmd_list(&proxy).await,
        Command::Info { device } => cmd_info(&proxy, &device).await,
        Command::Identity { device } => cmd_identity(&proxy, &device).await,
        Command::Get { device, layer, row, col } => cmd_get(&proxy, &device, layer, row, col).await,
        Command::Set { device, layer, row, col, keycode } => {
            cmd_set(&proxy, &device, layer, row, col, &keycode).await
        }
        Command::Commit { device } => cmd_commit(&proxy, &device).await,
        Command::GetMacro { device, offset, length } => {
            cmd_get_macro(&proxy, &device, &offset, length).await
        }
        Command::SetMacro { device, offset, data } => {
            cmd_set_macro(&proxy, &device, &offset, &data).await
        }
        Command::GetLighting { device, channel, value_id } => {
            cmd_get_lighting(&proxy, &device, &channel, &value_id).await
        }
        Command::SetLighting { device, channel, value_id, data } => {
            cmd_set_lighting(&proxy, &device, &channel, &value_id, &data).await
        }
        Command::Monitor => cmd_monitor(&proxy).await,
    }
}

async fn cmd_list(proxy: &ClackdProxy<'_>) -> Result<(), CliError> {
    let devices = proxy.list_devices().await?;
    if devices.is_empty() {
        eprintln!("no devices connected");
        return Ok(());
    }
    for (idx, id) in devices.iter().enumerate() {
        println!("{idx}:\t{id}");
    }
    Ok(())
}

async fn cmd_info(proxy: &ClackdProxy<'_>, device: &str) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let (rows, cols, layers) = proxy.get_device_info(&id).await?;
    println!("device {id}:");
    println!("  matrix:  {rows} rows × {cols} cols");
    println!("  layers:  {layers}");
    Ok(())
}

async fn cmd_identity(proxy: &ClackdProxy<'_>, device: &str) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let (vid, pid, name) = proxy.get_device_identity(&id).await?;
    println!("device {id}:");
    println!("  vendor:   0x{vid:04x}");
    println!("  product:  0x{pid:04x}");
    println!("  name:     {}", if name.is_empty() { "(unknown)" } else { &name });
    Ok(())
}

async fn cmd_get(
    proxy: &ClackdProxy<'_>,
    device: &str,
    layer: u8,
    row: u8,
    col: u8,
) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let keycode = proxy.get_keycode(&id, layer, row, col).await?;
    println!("{keycode:#06x}");
    Ok(())
}

async fn cmd_set(
    proxy: &ClackdProxy<'_>,
    device: &str,
    layer: u8,
    row: u8,
    col: u8,
    keycode: &str,
) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let keycode = parse_keycode(keycode)?;
    proxy.set_keycode(&id, layer, row, col, keycode).await?;
    Ok(())
}

async fn cmd_commit(proxy: &ClackdProxy<'_>, device: &str) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    proxy.commit(&id).await?;
    Ok(())
}

async fn cmd_monitor(proxy: &ClackdProxy<'_>) -> Result<(), CliError> {
    let mut added = proxy.receive_device_added().await?;
    let mut removed = proxy.receive_device_removed().await?;
    let mut layout = proxy.receive_layout_updated().await?;

    eprintln!("monitoring clackd events — Ctrl-C to stop");
    loop {
        tokio::select! {
            Some(sig) = added.next() => {
                let args = sig.args()?;
                println!("device-added    {}", args.device_id());
            }
            Some(sig) = removed.next() => {
                let args = sig.args()?;
                println!("device-removed  {}", args.device_id());
            }
            Some(sig) = layout.next() => {
                let args = sig.args()?;
                println!("layout-updated  {}", args.device_id());
            }
            else => break,
        }
    }
    Ok(())
}

/// Resolves a user-supplied device spec to a concrete device id.
///
/// Accepts an exact id match first, then a numeric index into the
/// `list_devices` output. Errors list the available devices so the user
/// can correct the spec.
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
    let available = if devices.is_empty() {
        "none connected".to_owned()
    } else {
        devices.join(", ")
    };
    Err(format!("no device matching '{spec}' (available: {available})").into())
}

/// Parses a keycode in decimal or `0x`-prefixed hex.
fn parse_keycode(s: &str) -> Result<u16, CliError> {
    let s = s.trim();
    let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16),
        None => s.parse::<u16>(),
    };
    parsed.map_err(|_| format!("invalid keycode '{s}' (use decimal or 0xHEX)").into())
}

/// Parses a u8 in decimal or `0x`-prefixed hex.
fn parse_u8(s: &str) -> Result<u8, CliError> {
    let s = s.trim();
    let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16),
        None => s.parse::<u8>(),
    };
    parsed.map_err(|_| format!("invalid byte '{s}' (use decimal or 0xHEX)").into())
}

/// Parses a hex string (e.g. "0102AABB") into a `Vec<u8>`.
fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, CliError> {
    let s = s.trim();
    // Strip optional "0x" prefix.
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

async fn cmd_get_macro(
    proxy: &ClackdProxy<'_>,
    device: &str,
    offset: &str,
    length: u8,
) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let off = parse_keycode(offset)? ; // u16
    let data = proxy.get_macro(&id, off, length).await?;
    // Print as hex dump
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
    let off = parse_keycode(offset)?; // u16
    let bytes = parse_hex_bytes(data)?;
    proxy.set_macro(&id, off, bytes).await?;
    println!("ok");
    Ok(())
}

async fn cmd_get_lighting(
    proxy: &ClackdProxy<'_>,
    device: &str,
    channel: &str,
    value_id: &str,
) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let ch = parse_u8(channel)?;
    let vid = parse_u8(value_id)?;
    let data = proxy.get_lighting(&id, ch, vid).await?;
    let hex: Vec<String> = data.iter().map(|b| format!("{b:02x}")).collect();
    println!("{}", hex.join(" "));
    Ok(())
}

async fn cmd_set_lighting(
    proxy: &ClackdProxy<'_>,
    device: &str,
    channel: &str,
    value_id: &str,
    data: &str,
) -> Result<(), CliError> {
    let id = resolve_device(proxy, device).await?;
    let ch = parse_u8(channel)?;
    let vid = parse_u8(value_id)?;
    let bytes = parse_hex_bytes(data)?;
    proxy.set_lighting(&id, ch, vid, bytes).await?;
    println!("ok");
    Ok(())
}
