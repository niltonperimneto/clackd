#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Epomaker EK68 (non-VIA, USB 05ac:024f) hardware smoke test.
#
# Speaks the reverse-engineered 64-byte HID Feature-report protocol documented
# in docs/protocol/epomaker-ek68.md -- the same bytes src/hal/epomaker.rs builds.
# Use it to confirm the decoded protocol against a real keyboard.
#
# Cross-platform (Windows / Linux / macOS) via hidapi:
#     pip install hidapi
#     python tools/ek68_smoke.py            # safe lighting demo (volatile)
#     python tools/ek68_smoke.py --list     # show the EK68 HID interfaces
#     python tools/ek68_smoke.py --remap    # remap experiment (writes EEPROM!)
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

# ---- frame encoders (mirror src/hal/epomaker.rs) ----------------------------

MODES = {
    "off": 0x00, "static": 0x01, "breath": 0x07, "spectrum": 0x08,
    "scrolling": 0x0A, "ripples": 0x0F,
}

def lighting(mode, r, g, b, brightness=0x10, rainbow=False):
    f = bytearray(FRAME_LEN)
    f[0] = mode
    f[1], f[2], f[3] = r, g, b
    f[8] = 1 if rainbow else 0
    f[9] = max(0x01, min(0x10, brightness))
    f[10] = 0x01 if mode == 0x00 else 0x0B
    f[14], f[15] = 0xAA, 0x55
    return f

def pc_mode_handshake():
    # The app's connect sequence; without it the board ignores host lighting
    # and renders its onboard effect. INIT_B (f[5]=01, f[8]=02, footer @62) is
    # the load-bearing "enter PC-control" activation. Mirrors src/hal/epomaker.rs.
    def f(pairs):
        x = bytearray(FRAME_LEN)
        for i, v in pairs:
            x[i] = v
        return x
    return [
        f([(0, 0x04), (1, 0x18)]),
        f([(0, 0x04), (1, 0x13), (8, 0x01)]),
        f([(9, 0x01), (10, 0x01), (14, 0xAA), (15, 0x55)]),   # INIT_A
        f([(0, 0x04), (1, 0x02)]),
        f([(0, 0x04), (1, 0xF0)]),
        f([(0, 0x04), (1, 0x17), (2, 0x01), (8, 0x01)]),
        f([(5, 0x01), (8, 0x02), (62, 0xAA), (63, 0x55)]),    # INIT_B (activation)
    ]

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
    # Interface 0 carries the 64-byte vendor Feature report; prefer it.
    target = next((i for i in infos if i.get("interface_number") == 0), infos[0])
    h = hid.device()
    h.open_path(target["path"])
    return h

def send(h, frame, what):
    # hidapi: first byte is the report id (0x00 here), then the 64-byte payload.
    h.send_feature_report(bytes([0x00]) + bytes(frame))
    print(f"  -> {what}")

def list_interfaces():
    infos = hid.enumerate(VID, PID)
    if not infos:
        sys.exit(f"No EK68 found ({VID:04x}:{PID:04x}).")
    for i in infos:
        print(f"  iface={i.get('interface_number')} "
              f"usage_page=0x{i.get('usage_page',0):04x} usage=0x{i.get('usage',0):04x} "
              f"path={i['path']!r}")

# ---- demos ------------------------------------------------------------------

def lighting_demo(h):
    print("\nLIGHTING DEMO (volatile -- does not touch EEPROM). Watch the keyboard.\n")
    print("  -> enter-PC-lighting-mode handshake")
    for fr in pc_mode_handshake():
        h.send_feature_report(bytes([0x00]) + bytes(fr))
        time.sleep(0.05)
    steps = [
        ("all RED, full bright",   lighting(MODES["static"], 0xFF, 0x00, 0x00)),
        ("all GREEN",              lighting(MODES["static"], 0x00, 0xFF, 0x00)),
        ("all BLUE",               lighting(MODES["static"], 0x00, 0x00, 0xFF)),
        ("white, brightness ~25%", lighting(MODES["static"], 0xFF, 0xFF, 0xFF, brightness=0x04)),
        ("white, full bright",     lighting(MODES["static"], 0xFF, 0xFF, 0xFF, brightness=0x10)),
        ("spectrum effect",        lighting(MODES["spectrum"], 0xFF, 0x00, 0x00, rainbow=True)),
        ("LED OFF",                lighting(MODES["off"], 0, 0, 0)),
        ("back to white",          lighting(MODES["static"], 0xFF, 0xFF, 0xFF)),
    ]
    for label, frame in steps:
        send(h, frame, label)
        time.sleep(1.5)
    print("\nIf the colors/effects matched the labels, the lighting protocol is confirmed. ✅")

def remap_experiment(h):
    print("\nREMAP EXPERIMENT -- this WRITES THE EEPROM (a few cycles). "
          "We remap the 'E' key, you test it, then we restore it.\n")
    KC_A, SCAN_E, KC_E = 0x04, 0x08, 0x08  # KC_A=0x04; E's scancode and KC_E are both 0x08
    # Candidate strategies for the write offset (the one open question).
    candidates = [
        ("A: select(E)@20 + write(A)@4",  [select_frame(SCAN_E), write_frame(KC_A, offset=4)]),
        ("B: select(E)@20 + write(A)@32", [select_frame(SCAN_E), write_frame(KC_A, offset=32)]),
        ("C: write(A)@32 only",           [write_frame(KC_A, offset=32)]),
    ]
    hit = None
    for label, frames in candidates:
        print(f"\nTrying {label}")
        for fr in frames:
            send(h, fr, "frame")
            time.sleep(0.2)
        ans = input("  Open a text box and press the physical 'E' key. Did it type 'A'? [y/N] ").strip().lower()
        if ans == "y":
            hit = label
            break
    # Restore E -> E using whichever strategy (best effort).
    print("\nRestoring 'E' to default...")
    send(h, select_frame(SCAN_E), "select E")
    time.sleep(0.2)
    send(h, write_frame(KC_E, offset=4), "write KC_E @4")
    send(h, write_frame(KC_E, offset=32), "write KC_E @32")
    if hit:
        print(f"\n✅ Remap worked with strategy [{hit}] -- tell the assistant; that fixes the driver's offset.")
    else:
        print("\nNone of the strategies remapped 'E'. Tell the assistant -- we'll try other offsets/selects.")
    print("If 'E' is still typing 'A', use the Epomaker app's Reset-to-default to fully restore.")

def main():
    ap = argparse.ArgumentParser(description="Epomaker EK68 protocol smoke test")
    ap.add_argument("--list", action="store_true", help="list EK68 HID interfaces and exit")
    ap.add_argument("--remap", action="store_true", help="run the remap experiment (writes EEPROM)")
    args = ap.parse_args()

    if args.list:
        list_interfaces()
        return

    h = open_ek68()
    try:
        lighting_demo(h)
        if args.remap:
            remap_experiment(h)
    finally:
        h.close()

if __name__ == "__main__":
    main()
