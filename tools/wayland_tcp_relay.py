#!/usr/bin/env python3
"""Bridge a WSL Wayland Unix socket to a TCP endpoint byte-for-byte.

This is only a transport bridge. It does not parse or translate Wayland
messages, and therefore does not introduce an application-private protocol.
Run it inside WSL after the Windows compositor has opened the TCP endpoint:

    python3 tools/wayland_tcp_relay.py \\
        --socket /tmp/waysl-wayland-0 --tcp-host 127.0.0.1 --tcp-port 48766

Point clients (or waypipe client) at the resulting Unix socket with
WAYLAND_DISPLAY=/tmp/waysl-wayland-0.
"""

from __future__ import annotations

import argparse
import os
import selectors
import socket


BUFFER_SIZE = 64 * 1024


def copy_bidirectionally(left: socket.socket, right: socket.socket) -> None:
    selector = selectors.DefaultSelector()
    sockets = (left, right)
    for source in sockets:
        selector.register(source, selectors.EVENT_READ, source)

    try:
        while True:
            events = selector.select()
            if not events:
                continue
            for key, _ in events:
                source = key.data
                target = right if source is left else left
                payload = source.recv(BUFFER_SIZE)
                if not payload:
                    return
                target.sendall(payload)
    finally:
        selector.close()
        left.close()
        right.close()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True, help="WSL Unix socket path")
    parser.add_argument("--tcp-host", default="127.0.0.1")
    parser.add_argument("--tcp-port", type=int, default=48766)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    try:
        os.unlink(args.socket)
    except FileNotFoundError:
        pass

    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(args.socket)
    listener.listen(16)
    try:
        while True:
            client, _ = listener.accept()
            upstream = socket.create_connection((args.tcp_host, args.tcp_port))
            copy_bidirectionally(client, upstream)
    finally:
        listener.close()
        os.unlink(args.socket)


if __name__ == "__main__":
    main()
