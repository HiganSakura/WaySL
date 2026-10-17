#!/usr/bin/env python3
"""Send one synthetic RGBA surface to a running WaySL instance."""

import socket
import struct
import sys


def main() -> None:
    host = sys.argv[1] if len(sys.argv) > 1 else "127.0.0.1"
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 48765
    width, height = 640, 360
    pixels = bytearray()
    for y in range(height):
        for x in range(width):
            tile = ((x // 32) + (y // 32)) % 2
            pixels.extend((0x30 + tile * 0x20, 0x70, 0xb0, 0xff))
    title = b"WSL frame stream"
    header = struct.pack(
        "<4sIIQQII", b"WSL1", width, height, 1, 1, len(title), len(pixels)
    )
    with socket.create_connection((host, port)) as stream:
        stream.sendall(header + title + pixels)


if __name__ == "__main__":
    main()
