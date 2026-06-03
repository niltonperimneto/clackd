#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# EK68 lighting demo driven through the running clackd daemon via clackctl.
# Confirms the Epomaker driver end-to-end (D-Bus -> engine -> hal::epomaker
# -> HIDIOCSFEATURE). Lighting is volatile (no EEPROM writes).
#
# Prereqs: clackd running on the session bus, EK68 attached (see clackctl list).
#
# Usage:
#   ./tools/ek68_linux_lightdemo.sh <device-id>     # e.g. hidraw3
#   CLACKCTL=/path/to/clackctl ./tools/ek68_linux_lightdemo.sh hidraw3
#
# Lighting payload = [mode, R, G, B, brightness(0x01-0x10), rainbow]; the
# clackctl 'command' byte is unused by the EK68 driver (pass 0).
set -euo pipefail

CTL="${CLACKCTL:-./target/debug/clackctl}"
DEV="${1:?usage: $0 <device-id from 'clackctl list'>}"

step() { printf '>> %-14s (%s)\n' "$1" "$2"; "$CTL" set-lighting "$DEV" 0 "$2"; sleep 1.5; }

echo "Driving EK68 '$DEV' through clackd. Watch the keyboard."
step "RED"          01ff00001000
step "GREEN"        0100ff001000
step "BLUE"         010000ff1000
step "dim white"    01ffffff0400
step "bright white" 01ffffff1000
step "spectrum"     08ff00001001
step "LED off"      000000000100
step "white"        01ffffff1000
echo "Done. If the colors matched the labels, the driver works end-to-end. ✅"
