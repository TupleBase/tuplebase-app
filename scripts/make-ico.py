#!/usr/bin/env python3
"""Combine square PNGs into a Windows .ico. Usage: make-ico.py out.ico a.png b.png ..."""
import struct
import sys

out, *paths = sys.argv[1:]
images = [open(p, "rb").read() for p in paths]

header = struct.pack("<HHH", 0, 1, len(images))
offset = len(header) + 16 * len(images)
entries = b""
for png in images:
    # Width is the first field of the IHDR chunk; 256 is stored as 0.
    size = struct.unpack(">I", png[16:20])[0]
    dim = 0 if size == 256 else size
    entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(png), offset)
    offset += len(png)

with open(out, "wb") as f:
    f.write(header + entries + b"".join(images))
