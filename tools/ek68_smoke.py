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
# `gmk67`. Full spec: docs/protocol/epomaker-ek68.md section 4.
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
WRITE_KEY_DEFINITION_AREA_COMMAND = 0x11      # base layer
WRITE_KEY_DEFINITION_AREA_FN_COMMAND = 0x27   # Fn layer (same layout)
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


# Key map from the vendor KeyboardLayout.xml (docs section 6): name -> (hid_code, index).
# For this board key_index == light_index, and the remap write goes to the
# key-definition area at absolute offset index*4. Basic VIA keycodes == HID code.
KEYS = {
    "esc": (0x29, 1), "1": (0x1e, 20), "2": (0x1f, 21), "3": (0x20, 22), "4": (0x21, 23),
    "5": (0x22, 24), "6": (0x23, 25), "7": (0x24, 26), "8": (0x25, 27), "9": (0x26, 28),
    "0": (0x27, 29), "-": (0x2d, 30), "=": (0x2e, 31), "backspace": (0x2a, 103),
    "tab": (0x2b, 37), "Q": (0x14, 38), "W": (0x1a, 39), "E": (0x08, 40), "R": (0x15, 41),
    "T": (0x17, 42), "Y": (0x1c, 43), "U": (0x18, 44), "I": (0x0c, 45), "O": (0x12, 46),
    "P": (0x13, 47), "[": (0x2f, 48), "]": (0x30, 49), "\\": (0x31, 67), "del": (0x4c, 119),
    "caps": (0x39, 55), "A": (0x04, 56), "S": (0x16, 57), "D": (0x07, 58), "F": (0x09, 59),
    "G": (0x0a, 60), "H": (0x0b, 61), "J": (0x0d, 62), "K": (0x0e, 63), "L": (0x0f, 64),
    ";": (0x33, 65), "'": (0x34, 66), "enter": (0x28, 85), "pageup": (0x4b, 118),
    "lshift": (0xe1, 73), "Z": (0x1d, 74), "X": (0x1b, 75), "C": (0x06, 76), "V": (0x19, 77),
    "B": (0x05, 78), "N": (0x11, 79), "M": (0x10, 80), ",": (0x36, 81), ".": (0x37, 82),
    "/": (0x38, 83), "rshift": (0xe5, 84), "up": (0x52, 101), "pagedown": (0x4e, 121),
    "lctrl": (0xe0, 91), "win": (0xe3, 92), "lalt": (0xe2, 93), "space": (0x2c, 94),
    "ralt": (0xe6, 95), "fn": (0xaf, 96), "left": (0x50, 99), "down": (0x51, 100),
    "right": (0x4f, 102),
}


def keydef_commit(h, entries, command=WRITE_KEY_DEFINITION_AREA_COMMAND):
    """Write the 9-page key-definition area (section 5). `entries`: {key_index: keycode}.

    `command` selects the layer: 0x11 base, 0x27 Fn. Each key's 4-byte entry
    [0x02, 0x00, kc_lo, kc_hi] sits at absolute offset index*4 across the 576-byte
    buffer. Zero entries keep the factory default, so pass the FULL desired keymap
    to avoid resetting other keys.
    """
    buf = bytearray(9 * FRAME_LEN)  # 576 bytes
    for idx, kc in entries.items():
        o = idx * 4
        buf[o + 0] = 0x02
        buf[o + 2] = kc & 0xFF
        buf[o + 3] = (kc >> 8) & 0xFF
    buf[8 * FRAME_LEN + 62] = 0xAA   # page-8 check code
    buf[8 * FRAME_LEN + 63] = 0x55
    send(h, cmd_frame(TURN_ON_CUSTOMIZATION_COMMAND)); read(h)
    send(h, cmd_frame(command, {8: 0x09})); read(h)
    for p in range(9):
        send(h, buf[p * FRAME_LEN:(p + 1) * FRAME_LEN]); read(h)
    send(h, cmd_frame(COMMUNICATION_END_COMMAND)); read(h)
    send(h, cmd_frame(LED_EFFECT_START_COMMAND))

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
    """The full effect/static render transaction (section 4.3)."""
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
    """Per-key Direct mode (section 4.4). Volatile -- needs a <=2s keepalive."""
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
    print("\nIf the colors/effects matched the labels, the lighting protocol is confirmed.")


def key_demo(h, idx, seconds=6.0):
    print(f"\nKEY-MAP TEST -- lighting ONLY light_index {idx} green for "
          f"{seconds:.0f}s. WHICH physical key lights up?\n")
    colors = [(0x00, 0x00, 0x00)] * 128
    if 0 <= idx < 128:
        colors[idx] = (0x00, 0xFF, 0x00)
    else:
        sys.exit(f"light_index {idx} out of range (0..127)")
    t0 = time.time()
    n = 0
    while time.time() - t0 < seconds:
        send_direct(h, colors)
        n += 1
        time.sleep(1.0)  # keepalive <= 2s
    print(f"  -> {n} refreshes. The key that lit has light_index = {idx} "
          "(cross-check docs/protocol/epomaker-ek68.md section 6).")


def direct_demo(h, seconds=6.0):
    print("\nDIRECT (per-key) DEMO -- volatile, refreshed every 1s for "
          f"{seconds:.0f}s. Watch the keyboard.\n")
    # All 66 LEDs green (slot order = LED index; see section 6 map).
    colors = [(0x00, 0xFF, 0x00)] * 66
    t0 = time.time()
    n = 0
    while time.time() - t0 < seconds:
        send_direct(h, colors)
        n += 1
        time.sleep(1.0)  # keepalive interval must be <= 2s
    print(f"  -> streamed {n} Direct refreshes (all keys green).")
    print("If the keys were green and stayed green, Direct mode + keepalive work.")


def remap_experiment(h, key_name="J", fn_layer=False, target="A"):
    layer = "Fn layer" if fn_layer else "base layer"
    command = WRITE_KEY_DEFINITION_AREA_FN_COMMAND if fn_layer else WRITE_KEY_DEFINITION_AREA_COMMAND
    press = f"Fn+'{key_name}'" if fn_layer else f"'{key_name}'"
    if key_name not in KEYS:
        sys.exit(f"unknown key '{key_name}'. Known: {', '.join(KEYS)}")
    if target not in KEYS:
        sys.exit(f"unknown target '{target}'. Known: {', '.join(KEYS)}")
    code, index = KEYS[key_name]
    target_code = KEYS[target][0]  # basic VIA keycode == HID code
    if key_name == target:
        print("  NOTE: source and target are the same key -- you won't see a "
              "change. Pass a different --to KEY.")

    print(f"\nREMAP EXPERIMENT ({layer}) -- this WRITES THE EEPROM (a few cycles). "
          f"We remap {press} -> '{target}' via the key-definition area (section 5), "
          "you test it, then we restore it.\n")
    print(f"  Writing {press} (key_index {index}, offset {index*4}) -> '{target}' "
          f"(0x{target_code:02x}) via command 0x{command:02x} ...")
    keydef_commit(h, {index: target_code}, command)
    input(f"  Open a text box and press {press}. Did it type '{target}'?  "
          "[press Enter to restore] ")

    if fn_layer:
        # The Fn-layer factory default is not the HID code (often transparent),
        # so restore by clearing the whole Fn layer to default.
        print("  Restoring the Fn layer to default ...")
        keydef_commit(h, {}, command)
    else:
        # Basic VIA keycode == HID code, so writing the key's own code restores
        # it without disturbing other keys.
        print(f"  Restoring '{key_name}' -> default (0x{code:02x}) ...")
        keydef_commit(h, {index: code}, command)
    print("  Done. If the key is still wrong, use the Epomaker app's "
          "Reset-to-default. Report the result to the assistant.")


def main():
    ap = argparse.ArgumentParser(description="Epomaker EK68 protocol smoke test")
    ap.add_argument("--list", action="store_true", help="list EK68 HID interfaces and exit")
    ap.add_argument("--direct", action="store_true", help="run the per-key Direct mode demo")
    ap.add_argument("--key", type=int, metavar="N",
                    help="light ONLY light_index N green (verify the section 6 key map)")
    ap.add_argument("--remap", nargs="?", const="J", metavar="KEY",
                    help="remap KEY (default J) -> A via the key-definition area, then restore (writes EEPROM)")
    ap.add_argument("--fn", action="store_true",
                    help="with --remap, target the Fn layer (command 0x27) instead of the base layer")
    ap.add_argument("--to", metavar="KEY", default="A",
                    help="with --remap, the key to remap TO (default A); pick a different key to see the change")
    args = ap.parse_args()

    if args.list:
        list_interfaces()
        return

    h = open_ek68()
    try:
        if args.key is not None:
            key_demo(h, args.key)
        elif args.direct:
            direct_demo(h)
        elif not args.remap:
            lighting_demo(h)
        if args.remap:
            remap_experiment(h, args.remap, fn_layer=args.fn, target=args.to)
    finally:
        h.close()


if __name__ == "__main__":
    main()
