# WaySL

Show Wayland applications from WSL on the Windows desktop.

WaySL runs a small, standard Wayland compositor on the Linux side and forwards
composited frames to a native Bevy window on Windows. No X server (VcXsrv,
XQuartz, XWayland) is involved, and no private Wayland extension is required of
the application.

```text
┌─────────────────────────── WSL / Linux ───────────────────────────┐
│  Wayland app                                                      │
│    │ wl_shm + xdg-shell (standard protocols)                      │
│    ▼                                                              │
│  WaySL compositor  ──►  internal frame bridge (TCP, RGBA frames)  │
└───────────────────────────────────────────────────────────────────┘
                                   │
                                   ▼
┌───────────────────────────── Windows ─────────────────────────────┐
│  Native Bevy renderer  ──►  one Windows window per Wayland surface │
└───────────────────────────────────────────────────────────────────┘
```

Two pieces of state travel between them, and they are deliberately separate:

- **Wayland** is the client-facing protocol. It is implemented with
  `wayland-server`, advertises only `wl_compositor` v4, `wl_shm` v1 and
  `xdg_wm_base` v6, and is what applications (or Waypipe) speak to.
- **The frame bridge** is WaySL's own transport between the Linux compositor
  and the Windows renderer. It is *not* a Wayland protocol and carries no
  protocol semantics — only already-composited RGBA surfaces.

## Modes

| Mode | What runs | Local display needed |
|---|---|---|
| Windows renderer (`waysl`) | Bevy renderer | No |
| Headless compositor (`WAYSL_FRAME_FORWARD` set, Linux only) | Wayland compositor + frame forwarder | No |
| Single-process (`waysl`, Linux only) | Compositor + Bevy renderer | Yes (Wayland or X11) |

The headless compositor plus the Windows renderer is the recommended
VcXsrv-free arrangement.

### Headless compositor in WSL

```sh
# Linux/WSL side
WAYSL_WAYLAND_SOCKET=/tmp/waysl-wayland-0 \
WAYSL_FRAME_FORWARD=172.30.0.1:48765 \
  cargo run --release --features wsl1-system-backend   # WSL1 only; omit on WSL2

# Windows side
WAYSL_LISTEN=0.0.0.0:48765 cargo run --release
```

`WAYSL_FRAME_FORWARD` names the Windows renderer's frame-bridge address.
WaySL exits with an error if the variable is unset, so this mode never starts
a hidden renderer by accident.

### Waypipe

Waypipe sits between the application and WaySL on the Linux side. It needs
`memfd_create`, which rules out WSL1 for the stock binary; use WSL2 with nested
virtualization when available.

```sh
# WaySL listens on /tmp/waysl-wayland-0.
XDG_RUNTIME_DIR=/tmp WAYLAND_DISPLAY=/tmp/waysl-wayland-0 \
  waypipe --socket /tmp/waysl-waypipe.sock client

# Separate shell: run the application through Waypipe.
XDG_RUNTIME_DIR=/tmp waypipe --socket /tmp/waysl-waypipe.sock \
  server -- <wayland-client-command>
```

Waypipe options come before the mode; `server` takes the application command
after `--`.

## Configuration

| Variable | Default | Used by |
|---|---|---|
| `WAYSL_WAYLAND_SOCKET` | `/tmp/waysl-wayland-0` | compositor |
| `WAYSL_FRAME_FORWARD` | unset | headless compositor; setting it selects that mode |
| `WAYSL_LISTEN` | `127.0.0.1:48765` | renderer frame bridge (bind address) |
| `WAYSL_WAYLAND_TCP` | `127.0.0.1:48766` | Windows TCP framing endpoint (experimental) |
| `WAYSL_WAYPIPE_COMMAND` | unset | optional Waypipe launch from the single-process mode |

## Frame bridge format

The bridge streams little-endian frames. Every frame starts with a 36-byte
header, followed by the UTF-8 title and then the RGBA8 pixels:

```text
magic "WSL1" | width:u32 | height:u32 | surface_id:u64 | generation:u64 |
title_len:u32 | pixel_len:u32 | UTF-8 title | RGBA8 pixels
```

Each surface is identified by `surface_id`; `generation` lets the renderer skip
unchanged frames. The renderer sizes its window from `width` and `height` and
uses the title for the Windows window caption.

Limits are enforced on receipt: titles up to 4 KiB and pixel payloads up to
256 MiB, with non-zero dimensions. Malformed frames close that connection
instead of being rendered.

## Renderer behaviour

- One Windows window per Wayland surface, created on first frame.
- One `Camera2d` and one texture-backed sprite per window.
- Frames are uploaded only when their `generation` changes.
- Closing the last window does not exit; new surfaces can appear at any time.
- If the Linux side disconnects, existing windows stay on screen and the
  bridge accepts a new connection on the next frame.

## Testing

```sh
cargo fmt -- --check
cargo test
cargo clippy --all-targets -- -D warnings
python3 -m py_compile tools/*.py
```

`tools/wayland_shm_smoke.py` is a dependency-free standard Wayland client. It
binds only the three advertised globals, takes a real `wl_shm` file descriptor
with `SCM_RIGHTS`, creates an `xdg_toplevel`, acknowledges the configure event,
commits one 640×360 frame, and waits for `wl_buffer.release`.

```sh
# Against a compositor socket directly
WAYSL_WAYLAND_SOCKET=/tmp/waysl-wayland-0 cargo run &
python3 tools/wayland_shm_smoke.py /tmp/waysl-wayland-0

# Through Waypipe (Linux/WSL2)
XDG_RUNTIME_DIR=/tmp waypipe --socket /tmp/waysl-waypipe.sock server -- \
  python3 tools/wayland_shm_smoke.py
```

If the socket argument is omitted, the client uses `WAYLAND_DISPLAY` (resolved
against `XDG_RUNTIME_DIR` when relative), then `WAYSL_WAYLAND_SOCKET`, then
`/tmp/waysl-wayland-0`. `WAYSL_SMOKE_TITLE` overrides the window title.

### Frame bridge smoke test

`tools/send_test_frame.py` writes one synthetic frame straight into a running
renderer, which is useful for checking the Windows renderer without Wayland:

```sh
cargo run --release            # Windows renderer
python3 tools/send_test_frame.py 127.0.0.1 48765
```

`tools/wayland_tcp_relay.py` is a byte-for-byte Unix-socket-to-TCP relay for
framing experiments. Raw TCP cannot carry `SCM_RIGHTS`, so it cannot transport
pixels: a `wl_shm` client through the relay will fail at `create_pool`.

## Build notes

- **WSL1**: the pure-Rust Wayland backend mishandles ancillary data there, so
  build with `--features wsl1-system-backend` and install `libwayland-dev`
  (`libwayland-server` development package). WSL1 also lacks `memfd_create`,
  so Waypipe itself does not work on WSL1.
- **WSL2**: no special flags are required. Waypipe needs nested virtualization,
  which Proxmox guests exposing AMD `svm` provide when the VM CPU includes
  `nested-virt`.
- **Windows**: the renderer uses the default Bevy backends (DX12/Vulkan). The
  experimental TCP Wayland endpoint is compiled in but should not be treated
  as a pixel-capable transport.

## Status

Verified in this repository:

- **Frame bridge** — unit tests cover the header layout, a full
  forwarder→renderer round trip (640×360, 921,600 bytes), bad magic, and
  zero-sized frames.
- **End-to-end (WSL1)** — a headless WaySL compositor in WSL1 forwarded a real
  standard `wl_shm` client frame to the native Windows renderer. The renderer
  logged `received frame id=5 title=WaySL-Fresh width=640 height=360
  bytes=921600` followed by `Creating new window WaySL-Fresh`. No X server was
  running at any point in that path.
- **Waypipe (WSL1)** — a client window was observed rendering and releasing its
  buffer. Stock Waypipe cannot start on WSL1 (`memfd_create` is missing); a
  build with an unlinked-tempfile fallback gets further, but WSL1's
  ancillary-data handling still makes long sessions fragile.

Screenshot evidence: `waysl-headless-e2e.png` was captured from the Windows
desktop during the run above.

Known gaps:

- The renderer received frames and created Bevy windows, but the automation in
  this environment could not confirm the window pixels on the interactive
  desktop: the SSH shell runs in a service window station, so its enumeration
  and screen capture only see that station. Confirm visually on a real desktop.
- Pixel transport over the Windows TCP Wayland endpoint.
- Wayland protocols beyond `wl_compositor`, `wl_shm` and `xdg-shell`
  (`wl_seat`, `xdg_decoration`, `wp_viewporter`, fractional scaling, …).
- GPU sharing (`linux-dmabuf`); all surfaces arrive as `wl_shm` pixels.
