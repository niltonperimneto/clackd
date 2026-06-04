#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Epomaker EK68 (non-VIA, USB 05ac:024f) hardware smoke test.
#
# The EK68 is firmware-identical to the Zuoya GMK67. Lighting is a *multi-frame
# transaction*, not a single frame -- this is why earlier single-frame /
# byte-exact replays never rendered. The sequence (PACKET_HEADER = 0x04):
#
#   SetCustomization(ON)  04 18                -> Send, Read
#   StartEffectPage       04 13 [8]=01         -> Send, Read
#   <mode frame>          mode,R,G,B,bri,spd.. -> Send, Read
#   EndCommunication      04 02                -> Send, Read
#   StartEffectCommand    04 F0                -> Send
#
# Decoded from OpenRGB issue #4512 / gitlab.com/aethernali.live/OpenRGB branch
# `gmk67`. Full spec: docs/protocol/epomaker-ek68.md §3.6.
#
# Cross-platform (Windows / Linux / macOS) via hidapi:
#     pip install hidapi
#     python tools/ek68_smoke.py            # effect/static lighting demo (saved on board)
#     python tools/ek68_smoke.py --list     # show the EK68 HID interfaces
#     python tools/ek68_smoke.py --direct    # per-key Direct mode demo (volatile + keepalive)
#     python tools/ek68_smoke.py --remap     # remap experiment (writes EEPROM!)
#
# Linux note: needs access to the hidraw node -- install the udev rule from
# dist/udev/ (TAG+="uaccess") or run with sudo.

import argparse
import sys
import time

try:
    import hid
except ImportError:
    sys.exit("Missing dependency. Install it with:  pip install hidapi")

VID, PID = 0x05AC, 0x024F
FRAME_LEN = 64
REPORT_ID = 0x00

# ---- protocol constants (GMK67KeyboardController.h) -------------------------

PACKET_HEADER = 0x04
COMMUNICATION_END_COMMAND = 0x02
WRITE_LED_SPECIAL_EFFECT_AREA_COMMAND = 0x13
TURN_ON_CUSTOMIZATION_COMMAND = 0x18
TURN_OFF_CUSTOMIZATION_COMMAND = 0x19
LED_EFFECT_START_COMMAND = 0xF0
LED_SPECIAL_EFFECT_PACKETS = 0x01
EFFECT_PAGE_CHECK_CODE_L, EFFECT_PAGE_CHECK_CODE_H = 0xAA, 0x55

# Effect/mode ids (byte 0 of the mode frame).
MODES = {
    "static": 0x01, "breathing": 0x07, "spectrum": 0x08,
    "vertical_gradient": 0x0A, "ripples": 0x0F, "off": 0x80,
}
DIRECT_MODE_VALUE = 0x20
CUSTOM_MODE_VALUE = 0x23
LIGHTS_OFF_MODE_VALUE = 0x80

BRIGHTNESS_MAX = 0x0F  # app "15"; range 0x00..0x0F

# ---- frame encoders (mirror src/hal/epomaker.rs) ----------------------------


def cmd_frame(command, extra=None):
    """A framing command: byte0 = PACKET_HEADER, byte1 = command, + extras."""
    f = bytearray(FRAME_LEN)
    f[0] = PACKET_HEADER
    f[1] = command
    for i, v in (extra or {}).items():
        f[i] = v
    return f


def mode_frame(mode, r, g, b, brightness=BRIGHTNESS_MAX, speed=0x00,
               direction=0x00, random=False):
    """The middle 'mode' frame of an UpdateMode transaction."""
    f = bytearray(FRAME_LEN)
    f[0] = mode
    f[1], f[2], f[3] = r, g, b
    f[8] = 1 if random else 0
    f[9] = max(0x00, min(BRIGHTNESS_MAX, brightness))
    f[10] = speed
    f[11] = direction
    f[14], f[15] = EFFECT_PAGE_CHECK_CODE_L, EFFECT_PAGE_CHECK_CODE_H
    return f


def select_frame(scancode):
    f = bytearray(FRAME_LEN)
    f[20] = 0x02
    f[22] = scancode
    return f


def write_frame(keycode, offset=4):
    f = bytearray(FRAME_LEN)
    f[offset] = 0x02
    f[offset + 2] = keycode & 0xFF
    f[offset + 3] = (keycode >> 8) & 0xFF
    return f

# ---- device plumbing --------------------------------------------------------


def open_ek68():
    infos = hid.enumerate(VID, PID)
    if not infos:
        sys.exit(f"No EK68 found ({VID:04x}:{PID:04x}). Plugged in by USB cable?")
    # Interface 0 (usage page 0x0001, usage 0x06) carries the vendor Feature report.
    target = next((i for i in infos if i.get("interface_number") == 0), infos[0])
    h = hid.device()
    h.open_path(target["path"])
    return h


def send(h, frame):
    # hidapi: first byte is the report id (0x00 here), then the 64-byte payload.
    h.send_feature_report(bytes([REPORT_ID]) + bytes(frame))


def read(h):
    # The app issues a GET after most SETs; the firmware appears to expect the
    # control-read to pace the transaction. Best-effort -- ignore the contents.
    try:
        h.get_feature_report(REPORT_ID, FRAME_LEN + 1)
    except Exception:
        pass


def update_mode(h, mode, r=0, g=0, b=0, brightness=BRIGHTNESS_MAX, speed=0x00,
                direction=0x00, random=False):
    """The full effect/static render transaction (A in §3.6)."""
    send(h, cmd_frame(TURN_ON_CUSTOMIZATION_COMMAND)); read(h)
    send(h, cmd_frame(WRITE_LED_SPECIAL_EFFECT_AREA_COMMAND,
                      {8: LED_SPECIAL_EFFECT_PACKETS})); read(h)
    send(h, mode_frame(mode, r, g, b, brightness, speed, direction, random)); read(h)
    send(h, cmd_frame(COMMUNICATION_END_COMMAND)); read(h)
    send(h, cmd_frame(LED_EFFECT_START_COMMAND))


def send_leds_buffer(h, colors):
    """8 packets of 16 LEDs each: color_buf[l*4]=l, +1..3 = R,G,B.

    `colors` is a list of (r,g,b); index = framebuffer slot. Missing slots stay 0.
    """
    color_buf = bytearray(8 * FRAME_LEN)  # 512 bytes = 128 slots * 4
    for l, (r, g, b) in enumerate(colors):
        if l >= 128:
            break
        color_buf[l * 4 + 0] = l
        color_buf[l * 4 + 1] = r
        color_buf[l * 4 + 2] = g
        color_buf[l * 4 + 3] = b
    for p in range(8):
        send(h, color_buf[p * FRAME_LEN:(p + 1) * FRAME_LEN])


def send_direct(h, colors):
    """Per-key Direct mode (B in §3.6). Volatile -- needs a <=2s keepalive."""
    # Header: [0]=PACKET_HEADER, [1]=DIRECT_MODE_VALUE, [8]=0x08.
    send(h, cmd_frame(DIRECT_MODE_VALUE, {8: 0x08}))
    send_leds_buffer(h, colors)
    read(h)
    send(h, cmd_frame(COMMUNICATION_END_COMMAND))


def list_interfaces():
    infos = hid.enumerate(VID, PID)
    if not infos:
        sys.exit(f"No EK68 found ({VID:04x}:{PID:04x}).")
    for i in infos:
        print(f"  iface={i.get('interface_number')} "
              f"usage_page=0x{i.get('usage_page', 0):04x} usage=0x{i.get('usage', 0):04x} "
              f"path={i['path']!r}")

# ---- demos ------------------------------------------------------------------


def lighting_demo(h):
    print("\nLIGHTING DEMO -- full UpdateMode transaction. Watch the keyboard.\n")
    steps = [
        ("all RED, full bright",  MODES["static"], (0xFF, 0x00, 0x00), BRIGHTNESS_MAX),
        ("all GREEN",             MODES["static"], (0x00, 0xFF, 0x00), BRIGHTNESS_MAX),
        ("all BLUE",              MODES["static"], (0x00, 0x00, 0xFF), BRIGHTNESS_MAX),
        ("white, ~25% bright",    MODES["static"], (0xFF, 0xFF, 0xFF), 0x04),
        ("white, full bright",    MODES["static"], (0xFF, 0xFF, 0xFF), BRIGHTNESS_MAX),
        ("spectrum effect",       MODES["spectrum"], (0xFF, 0x00, 0x00), BRIGHTNESS_MAX),
        ("LED OFF",               MODES["off"], (0x00, 0x00, 0x00), BRIGHTNESS_MAX),
        ("back to white",         MODES["static"], (0xFF, 0xFF, 0xFF), BRIGHTNESS_MAX),
    ]
    for label, mode, (r, g, b), bri in steps:
        rnd = (mode == MODES["spectrum"])
        update_mode(h, mode, r, g, b, brightness=bri, speed=0x08, random=rnd)
        print(f"  -> {label}")
        time.sleep(1.5)
    print("\nIf the colors/effects matched the labels, the lighting protocol is confirmed. ✅")


def direct_demo(h, seconds=6.0):
    print("\nDIRECT (per-key) DEMO -- volatile, refreshed every 1s for "
          f"{seconds:.0f}s. Watch the keyboard.\n")
    # All 66 LEDs green (slot order = LED index; see §3.6 map).
    colors = [(0x00, 0xFF, 0x00)] * 66
    t0 = time.time()
    n = 0
    while time.time() - t0 < seconds:
        send_direct(h, colors)
        n += 1
        time.sleep(1.0)  # keepalive interval must be <= 2s
    print(f"  -> streamed {n} Direct refreshes (all keys green).")
    print("If the keys were green and stayed green, Direct mode + keepalive work. ✅")


def remap_experiment(h):
    print("\nREMAP EXPERIMENT -- this WRITES THE EEPROM (a few cycles). "
          "We remap the 'E' key, you test it, then we restore it.\n")
    KC_A, SCAN_E, KC_E = 0x04, 0x08, 0x08  # KC_A=0x04; E's scancode and KC_E are both 0x08
    candidates = [
        ("A: select(E)@20 + write(A)@4",  [select_frame(SCAN_E), write_frame(KC_A, offset=4)]),
        ("B: select(E)@20 + write(A)@32", [select_frame(SCAN_E), write_frame(KC_A, offset=32)]),
        ("C: write(A)@32 only",           [write_frame(KC_A, offset=32)]),
    ]
    hit = None
    for label, frames in candidates:
        print(f"\nTrying {label}")
        for fr in frames:
            send(h, fr)
            time.sleep(0.2)
        ans = input("  Open a text box and press the physical 'E' key. Did it type 'A'? [y/N] ").strip().lower()
        if ans == "y":
            hit = label
            break
    print("\nRestoring 'E' to default...")
    send(h, select_frame(SCAN_E)); time.sleep(0.2)
    send(h, write_frame(KC_E, offset=4))
    send(h, write_frame(KC_E, offset=32))
    if hit:
        print(f"\n✅ Remap worked with strategy [{hit}] -- tell the assistant; that fixes the driver's offset.")
    else:
        print("\nNone of the strategies remapped 'E'. Tell the assistant -- we'll try other offsets/selects.")
    print("If 'E' is still typing 'A', use the Epomaker app's Reset-to-default to fully restore.")


def main():
    ap = argparse.ArgumentParser(description="Epomaker EK68 protocol smoke test")
    ap.add_argument("--list", action="store_true", help="list EK68 HID interfaces and exit")
    ap.add_argument("--direct", action="store_true", help="run the per-key Direct mode demo")
    ap.add_argument("--remap", action="store_true", help="run the remap experiment (writes EEPROM)")
    args = ap.parse_args()

    if args.list:
        list_interfaces()
        return

    h = open_ek68()
    try:
        if args.direct:
            direct_demo(h)
        else:
            lighting_demo(h)
        if args.remap:
            remap_experiment(h)
    finally:
        h.close()


if __name__ == "__main__":
    main()
