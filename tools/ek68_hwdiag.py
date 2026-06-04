#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
#
# EK68 direct-HID diagnostic (Windows/Linux, via hidapi). Drives the keyboard's
# interface-0 64-byte Feature report. See docs/protocol/epomaker-ek68.md section 4.
#
#   pip install hidapi
#   python tools/ek68_hwdiag.py get                 # read the (paged) per-key framebuffer
#   python tools/ek68_hwdiag.py color 255 0 0 [sec] # render a static colour (transaction)
#   python tools/ek68_hwdiag.py off [sec]           # render lights-off (mode 0x80)
#   python tools/ek68_hwdiag.py replay caps.pcapng [--with-gets]
#       # replay a capture's SET frames (and optionally interleave GET reads).
#       # needs tshark on PATH or at C:\Program Files\Wireshark\tshark.exe
#
# Rendering is a multi-frame TRANSACTION (PACKET_HEADER 0x04, doc section 3),
# not a single frame — `color`/`off` below now send the full sequence. The
# `replay` of bare mode frames is kept only for forensic comparison.

import sys, time, subprocess, shutil, os
import hid

VID, PID, FRAME = 0x05ac, 0x024f, 64

def F(pairs):
    x = bytearray(FRAME)
    for i, v in pairs: x[i] = v
    return bytes([0x00]) + bytes(x)

def send(h, pairs):           # build + SET a framed report
    h.send_feature_report(F(pairs))

def read(h):                  # paced GET the firmware expects between SETs
    try: h.get_feature_report(0x00, 65)
    except Exception: pass

def update_mode(h, mode, r=0, g=0, b=0, bri=0x0F, spd=0):
    # Transaction (doc section 4.3): customization-on, start-page, mode frame, end, start.
    send(h, [(0,0x04),(1,0x18)]);                 read(h)   # TURN_ON_CUSTOMIZATION
    send(h, [(0,0x04),(1,0x13),(8,0x01)]);        read(h)   # WRITE_LED_SPECIAL_EFFECT_AREA
    send(h, [(0,mode),(1,r),(2,g),(3,b),(9,bri),(10,spd),(14,0xAA),(15,0x55)]); read(h)
    send(h, [(0,0x04),(1,0x02)]);                 read(h)   # COMMUNICATION_END
    send(h, [(0,0x04),(1,0xf0)])                            # LED_EFFECT_START

def open_dev():
    p = next(d['path'] for d in hid.enumerate(VID, PID) if d.get('interface_number') == 0)
    h = hid.device(); h.open_path(p); return h

def tshark():
    return shutil.which("tshark") or r"C:\Program Files\Wireshark\tshark.exe"

def main():
    if len(sys.argv) < 2:
        print(__doc__); return
    cmd = sys.argv[1]
    h = open_dev()
    try:
        if cmd == "get":
            for _ in range(int(sys.argv[2]) if len(sys.argv) > 2 else 3):
                print(bytes(h.get_feature_report(0x00, 65)).hex())
        elif cmd == "color":
            r, g, b = (int(x) for x in sys.argv[2:5])
            secs = float(sys.argv[5]) if len(sys.argv) > 5 else 3.0
            print(f"rendering static {r},{g},{b} (transaction) -- WATCH for {secs}s")
            update_mode(h, 0x01, r, g, b)   # 0x01 = static
            time.sleep(secs)
        elif cmd == "off":
            secs = float(sys.argv[2]) if len(sys.argv) > 2 else 2.5
            print(f"rendering lights-off (mode 0x80) -- WATCH for {secs}s")
            update_mode(h, 0x80)            # 0x80 = lights off
            time.sleep(secs)
        elif cmd == "replay":
            cap = sys.argv[2]; with_gets = "--with-gets" in sys.argv
            flt = "usbhid.setup.bRequest" if with_gets else "usbhid.setup.bRequest==0x09"
            out = subprocess.check_output([tshark(), "-r", cap, "-Y", flt, "-T", "fields",
                   "-e", "usbhid.setup.bRequest", "-e", "usb.data_fragment", "-E", "separator=|"],
                   text=True)
            ops = [l for l in out.splitlines() if l.strip()]
            print(f"replaying {len(ops)} ops (with_gets={with_gets}) -- WATCH")
            for line in ops:
                parts = line.split("|"); breq = parts[0]; data = parts[1] if len(parts) > 1 else ""
                if breq == "0x09" and data:
                    h.send_feature_report(bytes([0x00]) + bytes.fromhex(data))
                elif breq == "0x01" and with_gets:
                    try: h.get_feature_report(0x00, 65)
                    except Exception: pass
                time.sleep(0.005)
            time.sleep(2)
        else:
            print(__doc__)
    finally:
        h.close()

if __name__ == "__main__":
    main()
