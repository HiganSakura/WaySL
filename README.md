# WaySL

Show Wayland applications from WSL on the Windows desktop.

WaySL runs a small, standard Wayland compositor on the Linux side and forwards
composited frames to a native Bevy window on Windows. No X server (VcXsrv,
XQuartz, XWayland) is involved, and no private Wayland extension is required of
the application.

```text
┌─────────────────────────── WSL / Linux ───────────────────────────┐
│  Wayland app                                                      │
│    │ wl_shm, xdg-shell, wl_seat, wl_output, linux-dmabuf          │
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

- **Wayland** is the client-facing protocol, implemented with
  `wayland-server`. Only standard protocols are advertised (see below).
- **The frame bridge** is WaySL's own transport between the Linux compositor
  and the Windows renderer. It is *not* a Wayland protocol and carries no
  protocol semantics — only already-composited RGBA surfaces.

## Advertised globals

| Global | Version | Note |
|---|---|---|
| `wl_compositor` | 4 | |
| `wl_shm` | 1 | guaranteed fallback for every client |
| `xdg_wm_base` | 6 | |
| `wl_seat` | 7 | pointer + keyboard capabilities |
| `wl_output` | 3 | geometry, mode, scale, name |
| `zxdg_toplevel_decoration_v1` | 1 | replies `client_side` |
| `zwp_linux_dmabuf_v1` | 4 | **only when the EGL probe succeeds** |
| `wl_subcompositor` | 1 | accepted; subsurface content is a no-op |
| `wp_viewporter` | 1 | source crop and destination scaling are applied |

`zwp_linux_dmabuf_v1` is gated on a runtime EGL probe so a host without a
usable GPU simply does not offer it, and clients fall back to `wl_shm` instead
of stalling on a path that cannot work.

`wl_drm` is intentionally **not** offered: it is the legacy pre-dmabuf
interface, current clients bind `linux-dmabuf` instead, and implementing its
GEM-handle sharing correctly needs a real DRM render node.

## Modes

| Mode | What runs | Local display needed |
|---|---|---|
| Windows renderer (`waysl`) | Bevy renderer | No |
| Headless compositor (`WAYSL_FRAME_FORWARD` set, Linux only) | Wayland compositor + frame forwarder | No |
| Single-process (`waysl`, Linux only) | Compositor + Bevy renderer | Yes (Wayland or X11) |

The headless compositor plus the Windows renderer is the recommended
arrangement.

### Headless compositor in WSL

```sh
# Linux/WSL side
WAYSL_WAYLAND_SOCKET=/tmp/waysl-wayland-0 \
WAYSL_FRAME_FORWARD=172.30.0.1:48765 \
  cargo run --release          # add --features wsl1-system-backend on WSL1

# Windows side
WAYSL_LISTEN=0.0.0.0:48765 cargo run --release
```

`WAYSL_FRAME_FORWARD` names the Windows renderer's frame-bridge address.
WaySL exits with an error if the variable is unset, so this mode never starts
a hidden renderer by accident.

### Reaching the Windows renderer from WSL2

WSL2 runs in NAT mode, so `127.0.0.1` inside the distro is not the Windows
host. Point `WAYSL_FRAME_FORWARD` at the WSL gateway and forward the port on
Windows:

```powershell
netsh interface portproxy add v4tov4 listenaddress=<wsl-gateway> `
  listenport=48765 connectaddress=127.0.0.1 connectport=48765
netsh advfirewall firewall add rule name="WaySL Bridge" dir=in action=allow `
  protocol=TCP localport=48765
```

The gateway is the default route inside the distro
(`ip route | awk '/default/ {print $3}'`).

### Waypipe

Waypipe sits between the application and WaySL on the Linux side. It needs
`memfd_create`, which rules out WSL1 for the stock binary; use WSL2.

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

## Buffer import

Two paths, chosen by the client:

- **`wl_shm`** — always available. The compositor copies pixels out of the
  shared pool on commit.
- **`linux-dmabuf`** — offered when `src/egl.rs` reports a usable GPU.

`wl_shm_pool.resize` is honoured (the mapping is remapped under an `RwLock`),
because toolkits grow their pools instead of pre-sizing them.

Frame callbacks complete on **every** commit, including commits with no buffer
attached. A client that commits after `xdg_surface.configure` would otherwise
wait forever for `wl_callback.done`.

### The GPU probe

WSL2 exposes the GPU through `/dev/dxg` rather than a DRM render node, so
`gbm_bo_create` cannot allocate there (`DRM_IOCTL_MODE_CREATE_DUMB` fails) and
`/dev/dri/renderD128` is a `faux_driver` stub. Mesa's D3D12 Gallium driver still
provides a working EGL on top of `/dev/dxg`, which is the path WSLg itself uses.

Three details matter in `src/egl.rs`:

- Extension entry points (`eglQueryDevicesEXT`, `eglGetPlatformDisplayEXT`)
  are **not** exported symbols — `dlsym` returns null for them even though the
  extension is advertised. They must go through `eglGetProcAddress`.
- `eglGetDisplay` consults `WAYLAND_DISPLAY`/`DISPLAY`, and here the compositor
  *is* that socket, so the listener binds first and the probe clears those
  variables around the call.
- Device enumeration is attempted before the surfaceless platform and the
  default display, matching what `eglinfo` does.

`libEGL` is opened with `dlopen`, so the crate builds and runs on hosts without
EGL; the probe simply reports `NoEgl`.

### Making clients use the GPU

The probe only decides whether **WaySL** advertises dmabuf. A client that draws
through Mesa's GL stack picks its own driver, and on WSL2 it picks the wrong
one: `/dev/dri/card0` is `vgem` (a dummy GEM device, `DRIVER=faux_driver`), so
`dri2` reports `failed to get driver name for fd -1` and
`eglInitialize` fails with `0x3003` (`EGL_BAD_ALLOC`). The client then falls
back to llvmpipe, or aborts with
`WGPU error: Failed to create surface for any enabled backend`.

`MESA_LOADER_DRIVER_OVERRIDE` does **not** help (it still lands on llvmpipe);
only `GALLIUM_DRIVER=d3d12` selects the driver that runs on `/dev/dxg`. Set it
system-wide so every process inherits it:

```sh
cat > /etc/profile.d/waysl-gpu.sh <<'EOF'
export GALLIUM_DRIVER=d3d12
export MESA_LOADER_DRIVER_OVERRIDE=d3d12
EOF

mkdir -p /etc/systemd/system.conf.d
printf '[Manager]\nDefaultEnvironment=GALLIUM_DRIVER=d3d12\n' \
  > /etc/systemd/system.conf.d/waysl-gpu.conf

printf 'GALLIUM_DRIVER=d3d12\n' >> /etc/environment
```

The three files cover login shells, systemd units, and `wsl -- cmd`
respectively. Verify with `eglinfo -B | grep D3D12`, which should report
`D3D12 (NVIDIA Quadro P4000)` rather than `llvmpipe`.

`tools/setup-wsl2-gpu.sh` automates this instead of copying the snippet by
hand:

```sh
sudo ./tools/setup-wsl2-gpu.sh            # install
./tools/setup-wsl2-gpu.sh --check         # verify, exit 1 when absent
sudo ./tools/setup-wsl2-gpu.sh --uninstall
```

Re-running the installer is idempotent: an existing `GALLIUM_DRIVER` line in
`/etc/environment` is replaced rather than duplicated.

Applications also cache their own GPU decision: PhotoCraft writes
`~/.config/photocraft/gpu-starting.json`, and after a failed start it records
`"backend":"cpu"` and stops retrying. Delete that file after fixing the driver
selection, or it keeps falling back even when the GPU now works.

## Running PhotoCraft end to end

PhotoCraft (`winit` + `wgpu` + `egui`) is the reference application: it is a
real toolkit stack rather than a synthetic client, and every protocol gap WaySL
had was found by running it. This is the full sequence, in order.

### 1. Host prerequisites (Windows)

The renderer must run inside an **interactive desktop session**. A scheduled
task with `-LogonType Interactive` is the reliable way to get there; a task that
runs in session 0 (services) has no desktop and creates no visible window.

If the machine enforces Smart App Control, a freshly built unsigned
`waysl.exe` is blocked (`was blocked by your organization's Device Guard
policy`). Sign it with a locally trusted certificate instead of disabling SAC:

```powershell
$cert = New-SelfSignedCertificate -Type CodeSigningCert `
  -Subject "CN=WaySL Local Dev" -CertStoreLocation Cert:\CurrentUser\My `
  -KeyUsage DigitalSignature -KeyExportPolicy Exportable
Export-Certificate -Cert $cert -FilePath C:\waysl.cer | Out-Null
certutil -addstore -f Root C:\waysl.cer
certutil -addstore -f TrustedPublisher C:\waysl.cer
Set-AuthenticodeSignature -FilePath C:\path\to\waysl.exe -Certificate $cert
```

`Get-AuthenticodeSignature` should then report `Status: Valid`.

### 2. Reach the renderer from WSL2

WSL2 is NAT'd, so a renderer listening on `127.0.0.1` is not reachable from the
distro. Forward it through the gateway address:

```powershell
$gw = (Get-NetIPAddress -AddressFamily IPv4 |
  Where-Object { $_.InterfaceAlias -like "*WSL*" }).IPAddress
netsh interface portproxy add v4tov4 listenaddress=$gw listenport=48765 `
  connectaddress=127.0.0.1 connectport=48765
netsh advfirewall firewall add rule name="WaySL Bridge" dir=in `
  action=allow protocol=TCP localport=48765
```

Verify from inside WSL with `timeout 3 bash -c "echo > /dev/tcp/$GW/48765"`.

### 3. GPU driver selection (Linux)

See "Making clients use the GPU" above. Confirm with:

```sh
eglinfo -B 2>/dev/null | grep -c D3D12    # expect a non-zero count
```

### 4. Start the compositor

```sh
XDG_RUNTIME_DIR=/tmp \
WAYSL_WAYLAND_SOCKET=/tmp/waysl-wayland-0 \
WAYSL_FRAME_FORWARD=<wsl-gateway>:48765 \
  ./target/release/waysl
```

It logs the GPU decision before accepting clients:

```text
WaySL GPU path: dmabuf export available via Mesa Project (EGL 1.5)
```

When the probe fails it logs the reason instead (`no EGL display: eglInitialize
failed (EGL error 0x3003)`) and simply does not advertise `zwp_linux_dmabuf_v1`.

### 5. Launch PhotoCraft

```sh
XDG_RUNTIME_DIR=/tmp WAYLAND_DISPLAY=/tmp/waysl-wayland-0 \
  ./photocraft.AppImage
```

PhotoCraft needs `libfuse2` for the AppImage and a working X11 clipboard is not
required (it logs a harmless `arboard` fallback warning under Wayland).

### What a healthy run looks like

Compositor side:

```text
WaySL GPU path: dmabuf export available via Mesa Project (EGL 1.5)
```

PhotoCraft side — the important lines are the `GPU startup` plan and the
absence of `eglInitialize` errors:

```text
INFO photocraft: GPU startup: Plan { backend: Gl, env: None, reason: None }
INFO photocraft_ui_egui::gpu_canvas: gpu canvas: target Rgba8Unorm, max texture 16384
```

Renderer side:

```text
INFO bevy_render::renderer: AdapterInfo { name: "Quadro P4000", device_type: DiscreteGpu, backend: Vulkan }
INFO bevy_winit::system: Creating new window PhotoCraft
INFO waysl::renderer::surfaces: received frame
```

A protocol trace (`WAYLAND_DEBUG=1`) shows the full negotiation:

```text
wl_compositor#3.create_surface(new id wl_surface#10)
xdg_wm_base#8.get_xdg_surface(new id xdg_surface#13, wl_surface#12)
xdg_surface#13.get_toplevel(new id xdg_toplevel#14)
xdg_toplevel#14.set_title("PhotoCraft")
wl_surface#12.attach(wl_buffer#20, 0, 0)
wl_surface#12.commit()
```

`photocraft-gpu-final.png` is the resulting window: 1440×900, full UI, on the
Windows desktop.

### Symptoms and their causes

| Symptom | Cause | Fix |
|---|---|---|
| `eglInitialize` code `0x3003` | Mesa picked `vgem` and found no driver | Set `GALLIUM_DRIVER=d3d12` |
| `Failed to create surface for any enabled backend` | Same, surfacing through wgpu | Same |
| `the previous start didn't finish ... using cpu` | Stale `gpu-starting.json` | Delete the file |
| `Connection refused` to the bridge | WSL2 NAT | Add the portproxy |
| Renderer runs, no window appears | Task ran in session 0 | Use `-LogonType Interactive` |
| `was blocked by ... Device Guard policy` | Smart App Control | Sign the binary |
| `Subcompositor protocol not available` | Only on builds before `wl_subcompositor` was added | Update WaySL |
| `arboard ... clipboard` warnings | Expected — no data-control protocol | Harmless |

## Debugging notes

These are the specific traps encountered while building this, recorded so they
are not rediscovered.

**`dlsym` does not find EGL extension functions.** `eglQueryDevicesEXT` and
`eglGetPlatformDisplayEXT` return null from `dlsym` even though
`eglQueryString(EGL_EXTENSIONS)` advertises them. They must be resolved with
`eglGetProcAddress`. This was the difference between `EGL_NOT_INITIALIZED` and a
working display.

**Probing EGL before binding the socket deadlocks.** `eglGetDisplay` reads
`WAYLAND_DISPLAY` and tries to connect; when WaySL *is* that compositor and has
not bound yet, the probe blocks forever in `unix_stream_connect`. Bind first,
then probe with those variables cleared.

**`gbm_bo_create` is the wrong test for GPU availability.** It fails on WSL2
because `/dev/dxg` is not a DRM node, but the D3D12/EGL path works fine. A
`gbm_bo_create` failure says nothing about whether the GPU is usable.

**A dead compositor looks like a client bug.** A stale socket file with no
process behind it makes every client fail identically, and `wayland-info`
returns nothing. Always confirm `pgrep -f waysl` and the socket owner before
concluding a client is at fault.

**Chrome-style captures can lie.** A disconnected RDP session captures as a
single flat colour. Confirm a capture by checking that it has many distinct
colours and plausible structure, not just that the file was written.

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
binds only the advertised globals, takes a real `wl_shm` file descriptor with
`SCM_RIGHTS`, creates an `xdg_toplevel`, acknowledges the configure event,
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

- **WSL2** is the supported configuration and needs no special flags.
- **WSL1**: the pure-Rust Wayland backend mishandles ancillary data there, so
  build with `--features wsl1-system-backend` and install `libwayland-dev`.
  WSL1 also lacks `memfd_create`, so Waypipe does not work there.
- **Windows**: the renderer uses the default Bevy backends (DX12/Vulkan).
- **Smart App Control / WDAC**: on a machine with Smart App Control enforced,
  a freshly built unsigned renderer is blocked (`was blocked by your
  organization's Device Guard policy`). Signing the binary with a locally
  trusted code-signing certificate resolves it without disabling SAC.

## Status

Verified in this repository:

- **Frame bridge** — unit tests cover the header layout, a full
  forwarder→renderer round trip (640×360, 921,600 bytes), bad magic, and
  zero-sized frames.
- **Unit tests** — 35 tests, including dmabuf stride/alpha handling, format
  opacity, short-mapping rejection, and buffer-format mapping.
- **Protocol trace (WSL2, PhotoCraft)** — the compositor serves the six base
  globals; PhotoCraft binds dmabuf, receives the full
  `zwp_linux_dmabuf_feedback_v1` sequence (`format_table`, `main_device`,
  `tranche_*`, `done`) and destroys the object cleanly.
- **GPU probe (WSL2 on a Quadro P4000 host)** — the compositor logs
  `dmabuf export available via Mesa Project (EGL 1.5)` and advertises seven
  globals including `zwp_linux_dmabuf_v1`. The Windows renderer picks
  `AdapterInfo { name: "Quadro P4000", device_type: DiscreteGpu, backend:
  Vulkan }`.
- **Window on the Windows desktop** — `photocraft-wsl.png` shows PhotoCraft's
  full UI with an open document, with `WaySL surface` visible in the taskbar.

Screenshot evidence:

| Image | What it shows |
|---|---|
| `photocraft-wsl.png` | PhotoCraft maximized with an open 1920×1080 document; `WaySL surface` in the taskbar |
| `photocraft-gpu.png` | PhotoCraft's start screen through WaySL |
| `photocraft-gpu-final.png` | PhotoCraft running GPU-accelerated (D3D12 via `/dev/dxg`) after the `GALLIUM_DRIVER` fix |
| `waysl-headless-e2e.png` | Headless compositor → frame bridge → `WaySL shm smoke` window |
| `waysl-screen.png` | `WaySL surface` and `WaySL-Via-Waypipe` windows side by side |
| `waysl-windows-client.png` | The `WaySL-Windows` window showing the smoke client's gradient |

Known gaps:

- PhotoCraft runs GPU-accelerated once `GALLIUM_DRIVER=d3d12` is set (see
  "Making clients use the GPU"): with it, `eglInitialize` succeeds, the GPU
  canvas initialises, 16 frames cross the bridge and `photocraft-gpu-final.png`
  shows the full UI. Without it the client still fails, because the `vgem` stub
  at `/dev/dri/card0` defeats Mesa's automatic driver detection. Making that
  selection automatic would need a change in Mesa, not in WaySL.
  Two EGL devices are enumerated on WSL2: one DRM-capable, one tagged
  `EGL_MESA_device_software`. Both report `D3D12 (NVIDIA Quadro P4000)`, and the
  software-labelled one only issues `wl_shm` capability queries, so the label
  does not mean CPU rendering.
- No client on this host imports a dma-buf: Mesa binds
  `zwp_linux_dmabuf_v1`, consumes the feedback and then never calls
  `create_params` or `create_immed`, allocating through `wl_shm` instead. The
  device wgpu binds is the `wl_shm`-only, software-labelled one. Reaching the
  dma-buf path would need wgpu to select the DRM-capable device, which is
  outside the compositor.
- The renderer paints with a plain `Camera2d` and sprite. Custom projections,
  render layers, and non-zero transforms did not paint on the software D3D12
  adapter used in early testing, so that configuration is kept deliberately
  minimal.
- Desktop capture requires an active interactive session; a disconnected RDP
  session captures as a single flat colour and must not be read as evidence.
- dma-buf import covers XRGB8888/ARGB8888, NV12 and YUYV/UYVY. YUV is converted
  with BT.601 limited range and nearest-neighbour scaling; a colour-management
  protocol would be needed for other matrices.
- Pixel transport over the Windows TCP Wayland endpoint.
- Protocols beyond the table above (`wp_viewporter`, fractional scaling,
  explicit sync, …).
