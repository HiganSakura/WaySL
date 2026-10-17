# WaySL

Native Wayland presentation for WSL on Windows.

This repository is intentionally independent from SakuraProj. The first ported
piece is the Bevy-side surface/window renderer, based on the structure of
`skpr-view`:

- one native Bevy window per remote surface;
- one camera and texture-backed sprite per surface;
- generation-based frame uploads;
- no SakuraProj path dependencies;
- optional Waypipe process launch through `WAYSL_WAYPIPE_COMMAND`.

WaySL now includes a standard Unix Wayland endpoint with `wl_shm` and
`xdg-shell` support. On Unix it accepts normal Wayland clients on
`WAYSL_WAYLAND_SOCKET` (default `/tmp/waysl-wayland-0`) and forwards committed
shared-memory frames into the Bevy renderer. The Windows build also exposes a
small dependency-free TCP wire endpoint at `WAYSL_WAYLAND_TCP` (default
`127.0.0.1:48766`) for registry/framing experiments.

The listener defaults to `127.0.0.1:48765` and accepts repeated little-endian
frames with this layout:

```text
WSL1 | width:u32 | height:u32 | surface_id:u64 | generation:u64 |
title_len:u32 | pixel_len:u32 | UTF-8 title | RGBA8 pixels
```

Set `WAYSL_LISTEN` to change the address. A WSL capture/forwarder can send
frames directly to this stream without depending on SakuraProj or Waypipe.
`WAYSL_WAYPIPE_COMMAND` remains available for experiments, but Waypipe does
not feed this stream automatically. To smoke-test the fallback:

```sh
cargo run
python3 tools/send_test_frame.py
```

The sender is only a protocol test; a real WSL producer still needs to capture
its Wayland surfaces and emit these RGBA frames.

For a Unix-side standard-protocol smoke test, start WaySL and run:

```sh
python3 tools/wayland_shm_smoke.py /tmp/waysl-wayland-0
```

This uses only Python’s standard library, passes a real `wl_shm` file descriptor,
creates an `xdg_toplevel`, and commits one test frame.

WSL1 compatibility: build WaySL with `--features wsl1-system-backend` and install
the system `libwayland-server` development package. The default build uses the
pure-Rust server backend. WSL1 lacks `memfd_create`; unmodified Waypipe 0.11
cannot run its shared-memory proxy there, so use WSL2 with nested virtualization
when available.

WSL1 has a `recvmsg` ancillary-data compatibility issue in the pure-Rust
Wayland backend. For WSL1 builds only, use `--features wsl1-system-backend`
with the system `libwayland-server` development package installed; normal Unix
builds use the pure-Rust backend by default.

## Wayland socket transport

Waypipe speaks standard Wayland to a compositor; it cannot consume the `WSL1`
frame stream. On Unix, point a normal Wayland client (or waypipe) at
`WAYSL_WAYLAND_SOCKET` and it can use the full `wl_shm`/`xdg-shell` endpoint.
The Windows TCP endpoint is a wire/framing MVP only.

`tools/wayland_tcp_relay.py` is a byte-for-byte WSL Unix-socket-to-TCP bridge.
It does not parse or translate Wayland messages. Raw TCP cannot carry the Unix
file descriptors required by `wl_shm.create_pool`, so this bridge currently
supports registry, surface, and xdg-shell framing rather than full pixel
transport:

```sh
python3 tools/wayland_tcp_relay.py \\
  --socket /tmp/waysl-wayland-0 --tcp-host 127.0.0.1 --tcp-port 48766
```

A compositor must still listen on TCP port `48766`; the relay only solves the
WSL Unix-socket versus Windows transport boundary.

## WSL-side waypipe

The supported standard-protocol deployment point is the Unix side. Start
WaySL on its Unix socket, then run a Waypipe client connected to that socket
and a matching server that launches the Wayland application:

```sh
# WaySL runs separately and listens on /tmp/waysl-wayland-0.
# In one WSL shell, connect Waypipe to WaySL:
XDG_RUNTIME_DIR=/tmp WAYLAND_DISPLAY=/tmp/waysl-wayland-0 \
  waypipe --socket /tmp/waysl-waypipe.sock client
# In another WSL shell, run the client through Waypipe:
XDG_RUNTIME_DIR=/tmp waypipe --socket /tmp/waysl-waypipe.sock \
  server -- <wayland-client-command>
```

Both Waypipe options precede the mode; `server` takes the application command.
This keeps `wl_shm` FD passing on Unix. The Windows TCP endpoint must not be
used for this path because it cannot carry those descriptors.
