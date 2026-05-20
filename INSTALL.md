# Installing clackd

`clackd` runs as a **session-scoped systemd user service**. It connects to the
session D-Bus (`io.github.clackd`) and reads/writes `/dev/hidrawX` directly
under the invoking user's ACL — there is no system service, no root, and no
polkit policy.

## Requirements

- Linux ≥ 5.4 (for `hidraw` and `uaccess` tag handling in udev)
- systemd ≥ 240 (for the `NotifyAccess=main` + `Type=notify` pair on user
  units)
- Rust ≥ 1.95 (MSRV declared in `Cargo.toml`)
- A working session D-Bus (`dbus-broker` or `dbus-daemon`)

## Build

```sh
cargo build --release
```

This produces two binaries:
- `target/release/clackd` — the daemon.
- `target/release/clackctl` — the command-line control tool (a D-Bus
  client of the daemon; see "Controlling devices with clackctl" below).

## Install

The four artifacts produced by the project are:

| Source                                          | Destination                                            |
|-------------------------------------------------|--------------------------------------------------------|
| `target/release/clackd`                         | `/usr/bin/clackd`                                      |
| `target/release/clackctl`                       | `/usr/bin/clackctl`                                   |
| `dist/systemd/clackd.service`                   | `/usr/lib/systemd/user/clackd.service`                 |
| `dist/dbus/io.github.clackd.service`            | `/usr/share/dbus-1/services/io.github.clackd.service`  |
| `dist/udev/60-clackd-via.rules`                 | `/etc/udev/rules.d/60-clackd-via.rules`                |

System-wide install (one shot):

```sh
sudo install -Dm755 target/release/clackd                /usr/bin/clackd
sudo install -Dm755 target/release/clackctl              /usr/bin/clackctl
sudo install -Dm644 dist/systemd/clackd.service          /usr/lib/systemd/user/clackd.service
sudo install -Dm644 dist/dbus/io.github.clackd.service   /usr/share/dbus-1/services/io.github.clackd.service
sudo install -Dm644 dist/udev/60-clackd-via.rules        /etc/udev/rules.d/60-clackd-via.rules
```

Per-user install (no root needed except for the udev rule):

```sh
install -Dm755 target/release/clackd                ~/.local/bin/clackd
install -Dm644 dist/systemd/clackd.service          ~/.config/systemd/user/clackd.service
install -Dm644 dist/dbus/io.github.clackd.service   ~/.local/share/dbus-1/services/io.github.clackd.service
sudo install -Dm644 dist/udev/60-clackd-via.rules   /etc/udev/rules.d/60-clackd-via.rules
# Adjust the ExecStart= path in the unit file if you used ~/.local/bin.
```

## Activate

After install, reload the daemon-side machinery:

```sh
sudo udevadm control --reload
sudo udevadm trigger
systemctl --user daemon-reload
```

Edit `/etc/udev/rules.d/60-clackd-via.rules` and add one
`SUBSYSTEM=="hidraw", ATTRS{idVendor}=="...", ATTRS{idProduct}=="..."` line
per VIA keyboard you want managed. `lsusb` or
`udevadm info /dev/hidraw0` will give you the IDs. Re-trigger udev after
editing:

```sh
sudo udevadm control --reload
sudo udevadm trigger
```

Enable and start the service:

```sh
systemctl --user enable --now clackd.service
```

Verify:

```sh
systemctl --user status clackd
journalctl --user -u clackd -f
busctl --user introspect io.github.clackd /io/github/clackd
```

The introspect call also lazily activates the daemon if you skipped
`enable --now` (the D-Bus activation file handles it).

## Configuration

### Per-device matrix dimensions

By default the daemon attaches to any VIA-capable hidraw node with a
permissive `(16, 16)` matrix ceiling. To get tight bounds checking, drop
a TOML file at `~/.config/clackd/devices.toml`:

```toml
[[device]]
vid = 0x04d8
pid = 0xeed3
rows = 5
cols = 15
# layer_count_override = 4  # optional; overrides firmware report
```

### Write-coalescing buffer

Off by default per the VIA-first invariant. To enable, set
`CLACKD_VIA_COALESCE_MS` in the unit file (uncomment the line at the
bottom of `clackd.service`) or in your shell environment before
launching:

```ini
Environment=CLACKD_VIA_COALESCE_MS=250
```

Trade-off: every `set_keycode` returns immediately; writes are batched
and flushed after the configured interval. Reduces EEPROM wear under a
rapidly-changing UI (e.g. dragging a slider), at the cost of a brief
window where `get_keycode` and `set_keycode` see different states.
See `CLAUDE.md` §4.1.

### Log level

`RUST_LOG=debug systemctl --user restart clackd` — or edit the
`Environment=RUST_LOG=info` line in the unit.

## Uninstall

```sh
systemctl --user disable --now clackd.service
sudo rm /usr/bin/clackd \
        /usr/bin/clackctl \
        /usr/lib/systemd/user/clackd.service \
        /usr/share/dbus-1/services/io.github.clackd.service \
        /etc/udev/rules.d/60-clackd-via.rules
sudo udevadm control --reload
systemctl --user daemon-reload
```

## Controlling devices with clackctl

`clackctl` is a `ratbagctl`-style command-line client. It talks to the
running daemon over the session bus; the daemon must be active (or
D-Bus-activatable).

```sh
# Enumerate connected devices ("index: id" per line).
clackctl list

# Show a device's matrix dimensions and layer count.
clackctl info hidraw0
clackctl info 0            # by index from `list`

# Read the keycode at (layer, row, col). Output is 4-digit hex.
clackctl get hidraw0 0 2 5
# => 0x004c

# Write a keycode. Accepts decimal or 0x-prefixed hex. Silent on success.
clackctl set hidraw0 0 2 5 0x004c
clackctl set 0 0 2 5 76

# Flush pending writes to NVRAM (meaningful with the coalescing buffer).
clackctl commit hidraw0

# Stream device + layout events until Ctrl-C.
clackctl monitor
```

The `<device>` argument accepts either a device id as printed by
`clackctl list` or its numeric index. Keycodes are raw VIA u16 values;
a symbolic name table (`KC_A` …) is a planned enhancement.

## Running without systemd

The daemon does not require systemd. Just run the binary:

```sh
./target/release/clackd
```

`sd_notify` calls are no-ops when `NOTIFY_SOCKET` is unset, so the
boot path is identical. You still need the udev rules (or `sudo`) for
hidraw access, and a session D-Bus must be available.
