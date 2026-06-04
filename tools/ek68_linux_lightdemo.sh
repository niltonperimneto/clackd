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
# Lighting payload = [mode, R, G, B, brightness(0x00-0x0F), random, speed, dir];
# modes: 01 static .. 13 effects, 80 = off (see docs section 4). The clackctl
# 'command' byte is unused by the EK68 driver (pass 0).
set -euo pipefail

CTL="${CLACKCTL:-./target/debug/clackctl}"
DEV="${1:?usage: $0 <device-id from 'clackctl list'>}"

step() { printf '>> %-14s (%s)\n' "$1" "$2"; "$CTL" set-lighting "$DEV" 0 "$2"; sleep 1.5; }

echo "Driving EK68 '$DEV' through clackd. Watch the keyboard."
step "RED"          01ff00000f00
step "GREEN"        0100ff000f00
step "BLUE"         010000ff0f00
step "dim white"    01ffffff0400
step "bright white" 01ffffff0f00
step "spectrum"     08ff00000f01
step "LED off"      80000000
step "white"        01ffffff0f00
echo "Done. If the colors matched the labels, the driver works end-to-end."
