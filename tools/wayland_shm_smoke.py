#!/usr/bin/env python3
"""Small dependency-free wl_shm/xdg-shell client for a Unix WaySL socket."""

import array
import mmap
import os
import socket
import struct
import sys
import tempfile


def message(object_id, opcode, *args):
    payload = b"".join(args)
    size = 8 + len(payload)
    return struct.pack("<II", object_id, (size << 16) | opcode) + payload


def u32(value):
    return struct.pack("<I", value)


def i32(value):
    return struct.pack("<i", value)


def string(value):
    raw = value.encode() + b"\0"
    return struct.pack("<I", len(raw)) + raw + b"\0" * ((-len(raw)) % 4)


def receive(sock):
    header = sock.recv(8)
    if len(header) != 8:
        raise RuntimeError("Wayland socket closed")
    object_id, word = struct.unpack("<II", header)
    length, opcode = word >> 16, word & 0xFFFF
    payload = b""
    while len(payload) < length - 8:
        payload += sock.recv(length - 8 - len(payload))
    return object_id, opcode, payload


def send_fd(sock, payload, fd):
    fds = array.array("i", [fd])
    sock.sendmsg([payload], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, fds)])


def main():
    path = (
        sys.argv[1]
        if len(sys.argv) > 1
        else os.environ.get(
            "WAYLAND_DISPLAY",
            os.environ.get("WAYSL_WAYLAND_SOCKET", "/tmp/waysl-wayland-0"),
        )
    )
    if not os.path.isabs(path):
        runtime_dir = os.environ.get("XDG_RUNTIME_DIR", "/run/user/1000")
        path = os.path.join(runtime_dir, path)
    width, height, stride = 640, 360, 640 * 4
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(path)

    sock.sendall(message(1, 1, u32(2)))
    globals_by_name = {}
    while len(globals_by_name) < 3:
        object_id, opcode, payload = receive(sock)
        if object_id != 2 or opcode != 0:
            continue
        name = struct.unpack_from("<I", payload)[0]
        length = struct.unpack_from("<I", payload, 4)[0]
        interface = payload[8 : 8 + length - 1].decode()
        globals_by_name[interface] = name

    sock.sendall(message(2, 0, u32(globals_by_name["wl_compositor"]), string("wl_compositor"), u32(1), u32(3)))
    sock.sendall(message(2, 0, u32(globals_by_name["wl_shm"]), string("wl_shm"), u32(1), u32(4)))
    sock.sendall(message(2, 0, u32(globals_by_name["xdg_wm_base"]), string("xdg_wm_base"), u32(1), u32(5)))
    sock.sendall(message(3, 0, u32(6)))
    sock.sendall(message(5, 2, u32(7), u32(6)))
    sock.sendall(message(7, 1, u32(8)))
    title = os.environ.get("WAYSL_SMOKE_TITLE", "WaySL shm smoke")
    sock.sendall(message(8, 2, string(title)))

    serial = None
    while serial is None:
        object_id, opcode, payload = receive(sock)
        if object_id == 7 and opcode == 0:
            serial = struct.unpack("<I", payload)[0]
    sock.sendall(message(7, 4, u32(serial)))

    with tempfile.TemporaryFile() as backing:
        backing.truncate(stride * height)
        pixels = mmap.mmap(backing.fileno(), stride * height)
        for y in range(height):
            for x in range(width):
                pixels[y * stride + x * 4 : y * stride + x * 4 + 4] = bytes(
                    (
                        0x40,
                        x * 255 // max(1, width - 1),
                        y * 255 // max(1, height - 1),
                        0xFF,
                    )
                )
        pixels.flush()
        send_fd(sock, message(4, 0, u32(9), i32(stride * height)), backing.fileno())
        sock.sendall(message(9, 0, u32(10), i32(0), i32(width), i32(height), i32(stride), u32(1)))
        sock.sendall(message(6, 1, u32(10), i32(0), i32(0)))
        sock.sendall(message(6, 2, i32(0), i32(0), i32(width), i32(height)))
        sock.sendall(message(6, 6))
        while True:
            object_id, opcode, _ = receive(sock)
            if object_id == 10 and opcode == 0:
                print(f"committed {width}x{height} wl_shm frame")
                return


if __name__ == "__main__":
    main()
