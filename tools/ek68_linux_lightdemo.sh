#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# EK68 lighting demo driven through the running clackd daemon via clackctl.
# Confirms the GMK67-family driver end-to-end (D-Bus -> engine -> hal::gmk67
# -> HIDIOCSFEATURE). Lighting applies live; nothing is saved until
# 'clackctl commit'.
#
# Prereqs: clackd running on the session bus, EK68 attached (see clackctl list).
#
# Usage:
#   ./tools/ek68_linux_lightdemo.sh <device-id>     # e.g. hidraw3
#   CLACKCTL=/path/to/clackctl ./tools/ek68_linux_lightdemo.sh hidraw3
#
# Uses the friendly 'clackctl lighting' interface (VIA RGB-matrix channel);
# see 'clackctl lighting <device> effects' for the effect names.
set -euo pipefail

CTL="${CLACKCTL:-./target/debug/clackctl}"
DEV="${1:?usage: $0 <device-id from 'clackctl list'>}"

step() { desc="$1"; shift; printf '>> %s\n' "$desc"; "$CTL" lighting "$DEV" "$@"; sleep 1.5; }

echo "Driving EK68 '$DEV' through clackd. Watch the keyboard."
step "static effect"  effect static
step "RED"            color red
step "GREEN"          color green
step "BLUE"           color blue
step "dim white"      color white
step "               " brightness 25%
step "bright white"   brightness 100%
step "spectrum cycle" effect spectrum-cycle
step "LED off"        off
step "white again"    effect static
step "               " color white
echo "Done. If the colors matched the labels, the driver works end-to-end."
echo "Current state:"
"$CTL" lighting "$DEV"
