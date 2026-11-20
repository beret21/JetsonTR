#!/usr/bin/env python3
"""Thermalright Trofeo Vision LCD (0416:5408, LY protocol) handshake + test pattern.

Protocol per MacTR DISPLAY_PROTOCOL.md (source: thermalright-trcc-linux ly.py).
Usage: python3 trlcd_test.py [--handshake-only] [--seconds N]
"""
import argparse
import io
import struct
import sys
import time

import usb.core
import usb.util
from PIL import Image, ImageDraw

VID, PID = 0x0416, 0x5408
EP_OUT, EP_IN = 0x09, 0x81
CHUNK_DATA = 496
MAX_JPEG = 650 * 1024


def open_device():
    dev = usb.core.find(idVendor=VID, idProduct=PID)
    if dev is None:
        sys.exit("device 0416:5408 not found")
    try:
        if dev.is_kernel_driver_active(0):
            dev.detach_kernel_driver(0)
    except (NotImplementedError, usb.core.USBError):
        pass
    try:
        dev.set_configuration(1)
    except usb.core.USBError as e:
        if e.errno != 16:  # EBUSY is OK
            raise
    usb.util.claim_interface(dev, 0)
    return dev


def handshake(dev):
    req = bytes([0x02, 0xFF, 0, 0, 0, 0, 0, 0, 0x01, 0, 0, 0, 0, 0, 0, 0]) + bytes(2032)
    # The first handshake after power-up/idle can answer resp[8]==0x02 (not ready);
    # the next one returns the real info, so retry a few times.
    for _ in range(5):
        dev.write(EP_OUT, req, 1000)
        resp = bytes(dev.read(EP_IN, 512, 1000))
        if len(resp) >= 37 and resp[0] == 0x03 and resp[1] == 0xFF and resp[8] == 0x01:
            break
        time.sleep(0.2)
    else:
        raise RuntimeError("bad handshake response: " + resp[:40].hex())
    raw = resp[20]
    raw = 1 if raw <= 3 else raw
    pm = 64 + raw
    sub = resp[22] + 1
    fbl = {63: 114, 64: 114, 65: 192, 66: 192, 68: 192, 69: 192}.get(pm, pm)
    if fbl == 192:
        size = {68: (1280, 480), 69: (1920, 440)}.get(pm, (1920, 480))
    elif fbl == 114:
        size = (1600, 720)
    elif fbl == 128:
        size = (1280, 480)
    else:
        size = (1920, 462)
    return dict(pm=pm, sub=sub, fbl=fbl, size=size, raw=resp[:40].hex())


def encode_jpeg(img):
    img = img.rotate(180)
    q = 90
    while True:
        buf = io.BytesIO()
        img.save(buf, "JPEG", quality=q)
        data = buf.getvalue()
        if len(data) <= MAX_JPEG or q <= 30:
            return data
        q -= 5


def send_frame(dev, jpeg):
    total = len(jpeg)
    n = total // CHUNK_DATA + 1
    chunks = []
    for i in range(n):
        part = jpeg[i * CHUNK_DATA:(i + 1) * CHUNK_DATA]
        hdr = struct.pack("<BBIHBHH", 0x01, 0xFF, total, len(part), 0x01, n, i) + bytes(3)
        chunks.append((hdr + part).ljust(512, b"\0"))
    while len(chunks) % 4:
        chunks.append(bytes(512))
    payload = b"".join(chunks)
    off = 0
    while off < len(payload):
        step = 4096 if len(payload) - off >= 4096 else 2048
        dev.write(EP_OUT, payload[off:off + step], 5000)
        off += step
    dev.read(EP_IN, 512, 1000)


def test_image(size, t):
    colors = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)]
    img = Image.new("RGB", size, colors[int(t) % len(colors)])
    d = ImageDraw.Draw(img)
    d.rectangle([0, 0, size[0] - 1, size[1] - 1], outline=(0, 0, 0), width=8)
    d.text((40, 40), "Jetson -> Thermalright LCD  t=%.1fs" % t, fill=(0, 0, 0))
    return img


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--handshake-only", action="store_true")
    ap.add_argument("--seconds", type=float, default=10)
    a = ap.parse_args()
    dev = open_device()
    try:
        info = handshake(dev)
        print("[OK] handshake PM=%(pm)d SUB=%(sub)d FBL=%(fbl)d size=%(size)s" % info)
        print("     resp[0:40]=%s" % info["raw"])
        if a.handshake_only:
            return
        start = time.monotonic()
        deadline = start
        frames = 0
        while time.monotonic() - start < a.seconds:
            jpeg = encode_jpeg(test_image(info["size"], time.monotonic() - start))
            send_frame(dev, jpeg)
            frames += 1
            deadline += 0.5
            wait = deadline - time.monotonic()
            if wait > 0:
                time.sleep(wait)
            else:
                deadline = time.monotonic()
        print("[OK] sent %d frames (~%dKB/frame)" % (frames, len(jpeg) // 1024))
    finally:
        usb.util.release_interface(dev, 0)
        usb.util.dispose_resources(dev)


if __name__ == "__main__":
    main()
