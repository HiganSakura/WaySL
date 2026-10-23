//! Standard Wayland compositor endpoint with `wl_shm` and `xdg-shell`.
//!
//! Only standard protocols are used: `wl_compositor`, `wl_shm` and
//! `xdg_wm_base`. Committed shared-memory buffers are converted to tightly
//! packed RGBA and delivered as [`WaylandFrame`] values.
#![cfg(unix)]

use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "linux")]
use std::io::Write as _;
use wayland_protocols::wp::linux_dmabuf::zv1::server::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::wp::viewporter::server::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1;
use wayland_protocols::xdg::shell::server::{xdg_popup, xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_server::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat,
    wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface, wl_surface,
};

use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
};

/// Surface ids start here so id `1` stays free for the renderer placeholder.
static NEXT_SURFACE_ID: AtomicU64 = AtomicU64::new(2);

/// Logical output width advertised through `wl_output`.
const DEFAULT_OUTPUT_WIDTH: i32 = 1920;
/// Logical output height advertised through `wl_output`.
const DEFAULT_OUTPUT_HEIGHT: i32 = 1080;
/// Refresh rate advertised through `wl_output`, in mHz.
const DEFAULT_OUTPUT_REFRESH_MHZ: i32 = 60_000;

/// One committed `wl_shm` surface, converted to RGBA8.
#[derive(Debug, Clone)]
pub struct WaylandFrame {
    /// Surface id.
    pub id: u64,
    /// Title from `xdg_toplevel.set_title`, if any.
    pub title: String,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Tightly packed RGBA8 pixels.
    pub pixels: Vec<u8>,
    /// Increments on every committed buffer.
    pub generation: u64,
}

/// Per-client state shared across all dispatch implementations.
#[derive(Default)]
pub struct WaylandState {
    sender: Option<mpsc::Sender<WaylandFrame>>,
    next_serial: u32,
}

pub fn run_with_sender(
    socket_path: impl AsRef<Path>,
    sender: mpsc::Sender<WaylandFrame>,
) -> io::Result<()> {
    let mut display = Display::<WaylandState>::new()
        .map_err(|error| io::Error::other(format!("initialize Wayland backend: {error}")))?;
    let handle = display.handle();
    handle.create_global::<WaylandState, wl_compositor::WlCompositor, ()>(4, ());
    handle.create_global::<WaylandState, wl_shm::WlShm, ()>(1, ());
    handle.create_global::<WaylandState, xdg_wm_base::XdgWmBase, ()>(6, ());
    handle.create_global::<WaylandState, wl_seat::WlSeat, ()>(7, ());
    handle.create_global::<WaylandState, wl_output::WlOutput, ()>(3, ());
    handle
        .create_global::<WaylandState, zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()>(
            1,
            (),
        );
    // Bind before probing: `eglGetDisplay` may consult WAYLAND_DISPLAY and block
    // trying to reach a compositor, and this compositor is that socket.
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;

    // Only advertise dmabuf when the host can actually back it with a GPU.
    // Offering it blind would send clients down a path that fails at import.
    let gpu = crate::egl::probe();
    eprintln!("WaySL GPU path: {}", gpu.describe());
    if gpu.is_ready() {
        handle.create_global::<WaylandState, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()>(4, ());
    }
    // Clients only need this to exist: bitmaps have no subsurface content, so
    // any subsurface is driven to a no-op instead of leaving the client without
    // server-side decorations.
    handle.create_global::<WaylandState, wl_subcompositor::WlSubcompositor, ()>(1, ());
    // viewporter lets clients crop and scale without resizing their buffer,
    // which is how toolkits implement fractional scaling and how video players
    // letterbox without reallocating.
    handle.create_global::<WaylandState, wp_viewporter::WpViewporter, ()>(1, ());
    let mut state = WaylandState {
        sender: Some(sender),
        next_serial: 0,
    };

    loop {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(error) = display.handle().insert_client(stream, Arc::new(())) {
                        eprintln!("failed to accept Wayland client: {error}");
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        display.dispatch_clients(&mut state)?;
        display.flush_clients()?;
        thread::sleep(Duration::from_millis(2));
    }
}

struct ShmPool {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
}

// The mapping is process-local and only accessed by the Wayland dispatch thread.
unsafe impl Send for ShmPool {}
unsafe impl Sync for ShmPool {}

impl ShmPool {
    fn new(fd: OwnedFd, size: i32) -> io::Result<Self> {
        if size <= 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid wl_shm pool size",
            ));
        }
        let len = usize::try_from(size)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pool too large"))?;
        let ptr = unsafe { mmap_read_write(fd.as_raw_fd(), len) }?;
        Ok(Self { fd, ptr, len })
    }

    /// Remaps the pool so it covers at least `size` bytes.
    ///
    /// The backing file is resized by the client through `wl_shm_pool.resize`;
    /// the mapping must grow with it or later buffers read out of range.
    fn resize(&mut self, size: i32) -> io::Result<()> {
        let size = usize::try_from(size)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "negative pool size"))?;
        if size <= self.len {
            return Ok(());
        }
        let ptr = unsafe { mmap_read_write(self.fd.as_raw_fd(), size)? };
        unsafe { munmap(self.ptr, self.len) };
        self.ptr = ptr;
        self.len = size;
        Ok(())
    }

    fn bytes(&self, offset: i32, len: usize) -> Option<&[u8]> {
        let offset = usize::try_from(offset).ok()?;
        let end = offset.checked_add(len)?;
        (end <= self.len).then(|| unsafe { std::slice::from_raw_parts(self.ptr.add(offset), len) })
    }
}

impl Drop for ShmPool {
    fn drop(&mut self) {
        unsafe { munmap(self.ptr, self.len) };
        let _ = &self.fd;
    }
}

#[derive(Clone)]
struct BufferData {
    storage: BufferStorage,
    width: i32,
    height: i32,
    stride: i32,
    format: BufferFormat,
}

/// Where a buffer's pixels live.
#[derive(Clone)]
enum BufferStorage {
    /// Shared memory imported through `wl_shm.create_pool`.
    Shm {
        pool: Arc<RwLock<ShmPool>>,
        offset: i32,
    },
    /// A dma-buf imported through `zwp_linux_buffer_params_v1`.
    Dmabuf(Arc<DmabufImage>),
}

/// Pixel layout of a buffer, independent of which protocol created it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BufferFormat {
    Shm(wl_shm::Format),
    /// DRM fourcc with optional per-channel byte padding.
    Drm {
        fourcc: u32,
        modifier: u64,
    },
}

impl BufferFormat {
    /// Whether the alpha channel must be forced opaque (XRGB-style formats).
    fn is_opaque(self) -> bool {
        match self {
            BufferFormat::Shm(format) => format == wl_shm::Format::Xrgb8888,
            BufferFormat::Drm { fourcc, .. } => fourcc == DRM_FORMAT_XRGB8888,
        }
    }
}

/// DRM fourcc for 32-bit XRGB, which is also what `wl_drm`/dmabuf use here.
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
/// DRM fourcc for 32-bit ARGB.
const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
/// DRM fourcc for packed YUV 4:2:2 (Y first).
const DRM_FORMAT_YUYV: u32 = u32::from_le_bytes(*b"YUYV");
/// DRM fourcc for packed YUV 4:2:2 (U first).
const DRM_FORMAT_UYVY: u32 = u32::from_le_bytes(*b"UYVY");
/// DRM fourcc for semi-planar YUV 4:2:0 (luma plane + interleaved chroma).
const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");

/// One imported dma-buf plane set, kept alive for the lifetime of its buffer.
struct DmabufImage {
    width: i32,
    height: i32,
    /// Row stride of plane 0, reported alongside the buffer.
    stride: i32,
    fourcc: u32,
    /// One mapping per plane, in plane order. Single-plane formats have one.
    planes: Vec<Vec<u8>>,
}

impl DmabufImage {
    /// Reads the planes back into tightly packed RGBA8.
    ///
    /// dma-buf contents are only reachable through a mapping; `mmap` is
    /// attempted when the buffer is imported and, when it fails, the import is
    /// rejected so clients can fall back to `wl_shm` instead of receiving a
    /// corrupt frame.
    fn to_rgba(&self) -> Option<Vec<u8>> {
        match self.fourcc {
            DRM_FORMAT_YUYV | DRM_FORMAT_UYVY => self.yuv_packed_to_rgba(),
            DRM_FORMAT_NV12 => self.nv12_to_rgba(),
            _ => self.rgb_to_rgba(),
        }
    }

    /// 32-bit RGB formats: one plane, four bytes per pixel.
    fn rgb_to_rgba(&self) -> Option<Vec<u8>> {
        let width = usize::try_from(self.width).ok()?;
        let height = usize::try_from(self.height).ok()?;
        let stride = usize::try_from(self.stride).ok()?;
        let plane = self.planes.first()?;
        if plane.len() < stride.checked_mul(height)? {
            return None;
        }
        let mut pixels = vec![0; width.checked_mul(height)?.checked_mul(4)?];
        let opaque = self.fourcc == DRM_FORMAT_XRGB8888;
        for y in 0..height {
            for x in 0..width {
                let source = y * stride + x * 4;
                let target = (y * width + x) * 4;
                if source + 4 > plane.len() {
                    return None;
                }
                let b = plane[source];
                let g = plane[source + 1];
                let r = plane[source + 2];
                pixels[target..target + 3].copy_from_slice(&[r, g, b]);
                pixels[target + 3] = if opaque { 0xff } else { plane[source + 3] };
            }
        }
        Some(pixels)
    }

    /// NV12: plane 0 is 8-bit luma, plane 1 is half-height interleaved chroma.
    fn nv12_to_rgba(&self) -> Option<Vec<u8>> {
        let width = usize::try_from(self.width).ok()?;
        let height = usize::try_from(self.height).ok()?;
        let luma_stride = usize::try_from(self.stride).ok()?;
        let chroma_stride = luma_stride;
        let luma = self.planes.first()?;
        let chroma = self.planes.get(1)?;
        if luma.len() < luma_stride.checked_mul(height)? {
            return None;
        }
        let chroma_rows = height.div_ceil(2);
        if chroma.len() < chroma_stride.checked_mul(chroma_rows)? {
            return None;
        }
        let mut pixels = vec![0; width.checked_mul(height)?.checked_mul(4)?];
        for y in 0..height {
            for x in 0..width {
                let luma_index = y * luma_stride + x;
                if luma_index >= luma.len() {
                    return None;
                }
                // 4:2:0: one chroma pair covers a 2x2 block.
                let chroma_index = (y / 2) * chroma_stride + (x / 2) * 2;
                if chroma_index + 1 >= chroma.len() {
                    return None;
                }
                let (r, g, b) = yuv_to_rgb(
                    luma[luma_index],
                    chroma[chroma_index],
                    chroma[chroma_index + 1],
                );
                let target = (y * width + x) * 4;
                pixels[target..target + 3].copy_from_slice(&[r, g, b]);
                pixels[target + 3] = 0xff;
            }
        }
        Some(pixels)
    }

    /// YUYV/UYVY: one plane with two pixels per four-byte group.
    fn yuv_packed_to_rgba(&self) -> Option<Vec<u8>> {
        let width = usize::try_from(self.width).ok()?;
        let height = usize::try_from(self.height).ok()?;
        let stride = usize::try_from(self.stride).ok()?;
        let plane = self.planes.first()?;
        if plane.len() < stride.checked_mul(height)? {
            return None;
        }
        let uyvy = self.fourcc == DRM_FORMAT_UYVY;
        let mut pixels = vec![0; width.checked_mul(height)?.checked_mul(4)?];
        for y in 0..height {
            for x in 0..width {
                // Both layouts repeat every chroma sample across two pixels.
                let group = y * stride + (x / 2) * 4;
                if group + 4 > plane.len() {
                    return None;
                }
                let (y0, u, y1, v) = if uyvy {
                    (
                        plane[group + 1],
                        plane[group],
                        plane[group + 3],
                        plane[group + 2],
                    )
                } else {
                    (
                        plane[group],
                        plane[group + 1],
                        plane[group + 2],
                        plane[group + 3],
                    )
                };
                let luma = if x % 2 == 0 { y0 } else { y1 };
                let (r, g, b) = yuv_to_rgb(luma, u, v);
                let target = (y * width + x) * 4;
                pixels[target..target + 3].copy_from_slice(&[r, g, b]);
                pixels[target + 3] = 0xff;
            }
        }
        Some(pixels)
    }
}

/// Converts one BT.601 full-range YUV sample triple to RGB.
///
/// This is the conversion every software YUV path uses; the colour space is
/// assumed to be BT.601 limited range, which is what dmabuf clients send unless
/// they negotiate a colour-management protocol WaySL does not implement.
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let y = f32::from(y);
    let u = f32::from(u) - 128.0;
    let v = f32::from(v) - 128.0;
    let r = 1.164 * (y - 16.0) + 1.596 * v;
    let g = 1.164 * (y - 16.0) - 0.392 * u - 0.813 * v;
    let b = 1.164 * (y - 16.0) + 2.017 * u;
    let clamp = |value: f32| value.clamp(0.0, 255.0) as u8;
    (clamp(r), clamp(g), clamp(b))
}

/// Pending `zwp_linux_buffer_params_v1`, populated plane by plane.
#[derive(Default)]
struct DmabufParams {
    planes: Vec<DmabufPlane>,
    width: i32,
    height: i32,
    format: u32,
    modifier: u64,
}

struct DmabufPlane {
    fd: OwnedFd,
    offset: u32,
    stride: u32,
}

impl DmabufParams {
    /// Maps the accumulated planes into an image.
    ///
    /// Only single-plane 32-bit formats are rasterised; anything else is
    /// rejected so the client can pick a supported alternative.
    fn build(&self) -> Option<DmabufImage> {
        let first = self.planes.first()?;
        let stride = i32::try_from(first.stride).ok()?;
        let height = usize::try_from(self.height).ok()?;
        let required_planes = plane_count(self.format)?;
        if self.planes.len() < required_planes {
            return None;
        }

        // Chroma planes are subsampled, so each plane's mapping length is
        // derived from its own stride and the format's vertical subsampling.
        let mut mappings = Vec::with_capacity(required_planes);
        for (index, plane) in self.planes.iter().take(required_planes).enumerate() {
            let plane_stride = usize::try_from(plane.stride).ok()?;
            let rows = if index == 0 {
                height
            } else {
                height.div_ceil(vertical_subsampling(self.format)?)
            };
            let offset = usize::try_from(plane.offset).ok()?;
            let len = plane_stride.checked_mul(rows)?.checked_add(offset)?;
            // SAFETY: `mmap_read_write` maps the plane's fd; the slice is
            // copied immediately, so it cannot outlive the mapping.
            let mapping = unsafe {
                let ptr = mmap_read_write(plane.fd.as_raw_fd(), len).ok()?;
                let slice = std::slice::from_raw_parts(ptr, len);
                slice[offset..].to_vec()
            };
            if mapping.len() < plane_stride.checked_mul(rows)? {
                return None;
            }
            mappings.push(mapping);
        }
        Some(DmabufImage {
            width: self.width,
            height: self.height,
            stride,
            fourcc: self.format,
            planes: mappings,
        })
    }
}

/// Number of dma-buf planes a format requires.
fn plane_count(fourcc: u32) -> Option<usize> {
    match fourcc {
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 | DRM_FORMAT_YUYV | DRM_FORMAT_UYVY => Some(1),
        DRM_FORMAT_NV12 => Some(2),
        _ => None,
    }
}

/// Vertical chroma subsampling factor for a format.
fn vertical_subsampling(fourcc: u32) -> Option<usize> {
    match fourcc {
        DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888 | DRM_FORMAT_YUYV | DRM_FORMAT_UYVY => Some(1),
        DRM_FORMAT_NV12 => Some(2),
        _ => None,
    }
}

#[derive(Default)]
struct SurfaceInner {
    id: u64,
    attached: Option<wl_buffer::WlBuffer>,
    callbacks: Vec<wl_callback::WlCallback>,
    title: String,
    generation: u64,
    /// Source rectangle set through `wp_viewport.set_source`, in buffer units.
    viewport_source: Option<ViewportRect>,
    /// Destination size set through `wp_viewport.set_destination`, in surface
    /// units. `None` means "use the buffer size".
    viewport_destination: Option<(i32, i32)>,
}

/// A rectangle in `wp_viewport` terms; coordinates may be fractional.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ViewportRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

type SurfaceData = Arc<Mutex<SurfaceInner>>;

fn new_surface() -> SurfaceData {
    Arc::new(Mutex::new(SurfaceInner {
        id: NEXT_SURFACE_ID.fetch_add(1, Ordering::Relaxed),
        title: "WaySL surface".to_string(),
        ..Default::default()
    }))
}

impl GlobalDispatch<wl_compositor::WlCompositor, ()> for WaylandState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_compositor::WlCompositor>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                init.init(id, new_surface());
            }
            wl_compositor::Request::CreateRegion { id } => {
                init.init(id, ());
            }
            // An opcode outside the version we advertise is a client bug.
            // Ignoring it keeps other clients alive, but it is worth surfacing
            // because it usually means a protocol mismatch.
            other => eprintln!("[waysl] ignoring unsupported request: {other:?}"),
        }
    }
}

impl GlobalDispatch<wl_shm::WlShm, ()> for WaylandState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_shm::WlShm>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let shm = init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl Dispatch<wl_shm::WlShm, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_shm::WlShm,
        request: wl_shm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        // Any other opcode is outside the version we advertise; ignoring it
        // keeps other clients alive instead of aborting the compositor.
        if let wl_shm::Request::CreatePool { id, fd, size } = request
            && let Ok(pool) = ShmPool::new(fd, size)
        {
            init.init(id, Arc::new(RwLock::new(pool)));
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, Arc<RwLock<ShmPool>>> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        pool: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        data: &Arc<RwLock<ShmPool>>,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_shm_pool::Request::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                if let wayland_server::WEnum::Value(format) = format
                    && width > 0
                    && height > 0
                    && stride >= width.saturating_mul(4)
                {
                    init.init(
                        id,
                        BufferData {
                            storage: BufferStorage::Shm {
                                pool: data.clone(),
                                offset,
                            },
                            width,
                            height,
                            stride,
                            format: BufferFormat::Shm(format),
                        },
                    );
                }
            }
            wl_shm_pool::Request::Destroy => {
                let _ = pool;
            }
            wl_shm_pool::Request::Resize { size } => {
                // A failed remap leaves the pool at its old size; later
                // buffers outside it fail validation in `copy_buffer`.
                if let Ok(mut mapping) = data.write() {
                    let _ = mapping.resize(size);
                }
            }
            // An opcode outside the version we advertise is a client bug.
            // Ignoring it keeps other clients alive, but it is worth surfacing
            // because it usually means a protocol mismatch.
            other => eprintln!("[waysl] ignoring unsupported request: {other:?}"),
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_buffer::WlBuffer,
        request: wl_buffer::Request,
        _: &BufferData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // `wl_buffer` only defines `destroy` at version 1; nothing to release
        // here because the imported mapping is dropped with the buffer data.
        let _ = request;
    }
}

impl Dispatch<wl_surface::WlSurface, SurfaceData> for WaylandState {
    fn request(
        state: &mut Self,
        _: &Client,
        surface: &wl_surface::WlSurface,
        request: wl_surface::Request,
        data: &SurfaceData,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        let mut data = data.lock().expect("surface lock");
        match request {
            wl_surface::Request::Attach { buffer, .. } => data.attached = buffer,
            wl_surface::Request::Frame { callback } => data.callbacks.push(init.init(callback, ())),
            wl_surface::Request::Commit => {
                if let Some(buffer) = data.attached.take()
                    && let Some(buffer_data) = buffer.data::<BufferData>()
                    && let Some(pixels) = copy_buffer(buffer_data)
                {
                    data.generation = data.generation.wrapping_add(1);
                    // Apply the viewport last: a client may crop and scale in the
                    // same commit that attaches the buffer.
                    let (frame_width, frame_height, pixels) = apply_viewport(
                        &pixels,
                        buffer_data.width,
                        buffer_data.height,
                        data.viewport_source,
                        data.viewport_destination,
                    );
                    if let Some(sender) = &state.sender {
                        let _ = sender.send(WaylandFrame {
                            id: data.id,
                            title: data.title.clone(),
                            width: frame_width,
                            height: frame_height,
                            pixels,
                            generation: data.generation,
                        });
                    }
                    buffer.release();
                }
                // Every commit completes any pending frame callbacks, even when
                // no new buffer was attached. Otherwise a client that commits
                // after a configure (no buffer yet) would wait forever for
                // `wl_callback.done` and stop rendering.
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u32;
                for callback in data.callbacks.drain(..) {
                    callback.done(timestamp);
                }
            }
            wl_surface::Request::Destroy => {
                let _ = surface;
            }
            wl_surface::Request::Damage { .. }
            | wl_surface::Request::DamageBuffer { .. }
            | wl_surface::Request::SetOpaqueRegion { .. }
            | wl_surface::Request::SetInputRegion { .. }
            | wl_surface::Request::SetBufferTransform { .. }
            | wl_surface::Request::SetBufferScale { .. }
            | wl_surface::Request::Offset { .. } => {}
            // An opcode outside the version we advertise is a client bug.
            // Ignoring it keeps other clients alive, but it is worth surfacing
            // because it usually means a protocol mismatch.
            other => eprintln!("[waysl] ignoring unsupported request: {other:?}"),
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_callback::WlCallback,
        _: wl_callback::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wl_region::WlRegion, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_region::WlRegion,
        request: wl_region::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_region::Request::Add { .. }
            | wl_region::Request::Subtract { .. }
            | wl_region::Request::Destroy => {}
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// wl_seat: advertise pointer and keyboard so toolkits create an input-capable
// window. Neither device sends events yet; they exist so toolkits such as
// winit can bind and create their global state.
// ---------------------------------------------------------------------------

impl GlobalDispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_seat::WlSeat>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let seat = init.init(resource, ());
        seat.capabilities(wl_seat::Capability::Pointer | wl_seat::Capability::Keyboard);
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => {
                init.init(id, ());
            }
            wl_seat::Request::GetKeyboard { id } => {
                init.init(id, ());
            }
            wl_seat::Request::GetTouch { .. } | wl_seat::Request::Release => {}
            _ => {
                let _ = state.next_serial;
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_pointer::WlPointer,
        request: wl_pointer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // Clients may request a cursor surface; we ignore it until input is
        // forwarded from the Windows side.
        let _ = request;
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_keyboard::WlKeyboard,
        request: wl_keyboard::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let _ = request;
    }
}

// ---------------------------------------------------------------------------
// wl_output: one logical output sized like the renderer's default surface.
// ---------------------------------------------------------------------------

impl GlobalDispatch<wl_output::WlOutput, ()> for WaylandState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_output::WlOutput>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let output = init.init(resource, ());
        output.geometry(
            0,
            0,
            0,
            0,
            wl_output::Subpixel::Unknown,
            "WaySL".to_string(),
            "WaySL virtual output".to_string(),
            wl_output::Transform::Normal,
        );
        output.mode(
            wl_output::Mode::Current | wl_output::Mode::Preferred,
            DEFAULT_OUTPUT_WIDTH,
            DEFAULT_OUTPUT_HEIGHT,
            DEFAULT_OUTPUT_REFRESH_MHZ,
        );
        output.scale(1);
        output.name("WAYSL-1".to_string());
        output.description("WaySL virtual output".to_string());
        output.done();
    }
}

impl Dispatch<wl_output::WlOutput, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_output::WlOutput,
        request: wl_output::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let _ = request;
    }
}

// ---------------------------------------------------------------------------
// zxdg_toplevel_decoration_v1: the renderer draws no native decorations, so
// clients are asked to draw their own.
// ---------------------------------------------------------------------------

impl GlobalDispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for WaylandState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        decoration: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        request: zxdg_toplevel_decoration_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            zxdg_toplevel_decoration_v1::Request::SetMode { .. } => {
                decoration.configure(zxdg_toplevel_decoration_v1::Mode::ClientSide);
            }
            zxdg_toplevel_decoration_v1::Request::UnsetMode
            | zxdg_toplevel_decoration_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl GlobalDispatch<xdg_wm_base::XdgWmBase, ()> for WaylandState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<xdg_wm_base::XdgWmBase>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        base: &xdg_wm_base::XdgWmBase,
        request: xdg_wm_base::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                let surface_data = surface
                    .data::<SurfaceData>()
                    .cloned()
                    .unwrap_or_else(new_surface);
                init.init(id, surface_data);
            }
            xdg_wm_base::Request::Pong { serial } => {
                let _ = (base, serial);
            }
            xdg_wm_base::Request::Destroy => {
                let _ = base;
            }
            // An opcode outside the version we advertise is a client bug.
            // Ignoring it keeps other clients alive, but it is worth surfacing
            // because it usually means a protocol mismatch.
            other => eprintln!("[waysl] ignoring unsupported request: {other:?}"),
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, SurfaceData> for WaylandState {
    fn request(
        state: &mut Self,
        _: &Client,
        surface: &xdg_surface::XdgSurface,
        request: xdg_surface::Request,
        data: &SurfaceData,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_surface::Request::GetToplevel { id } => {
                let top = init.init(id, data.clone());
                state.next_serial = state.next_serial.wrapping_add(1).max(1);
                top.configure(960, 540, Vec::new());
                surface.configure(state.next_serial);
            }
            xdg_surface::Request::GetPopup { id, .. } => {
                init.init(id, ());
            }
            xdg_surface::Request::AckConfigure { .. }
            | xdg_surface::Request::SetWindowGeometry { .. } => {}
            xdg_surface::Request::Destroy => {
                let _ = surface;
            }
            // An opcode outside the version we advertise is a client bug.
            // Ignoring it keeps other clients alive, but it is worth surfacing
            // because it usually means a protocol mismatch.
            other => eprintln!("[waysl] ignoring unsupported request: {other:?}"),
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, SurfaceData> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &xdg_toplevel::XdgToplevel,
        request: xdg_toplevel::Request,
        data: &SurfaceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_toplevel::Request::SetTitle { title } => {
                if let Ok(mut surface) = data.lock() {
                    surface.title = title;
                }
            }
            xdg_toplevel::Request::SetAppId { .. }
            | xdg_toplevel::Request::SetMaxSize { .. }
            | xdg_toplevel::Request::SetMinSize { .. }
            | xdg_toplevel::Request::SetMaximized
            | xdg_toplevel::Request::UnsetMaximized
            | xdg_toplevel::Request::SetFullscreen { .. }
            | xdg_toplevel::Request::UnsetFullscreen
            | xdg_toplevel::Request::SetMinimized
            | xdg_toplevel::Request::Move { .. }
            | xdg_toplevel::Request::Resize { .. }
            | xdg_toplevel::Request::ShowWindowMenu { .. }
            | xdg_toplevel::Request::Destroy => {}
            // Same rationale as elsewhere: never abort the compositor over one
            // client's stray opcode, but do not hide it either.
            other => eprintln!("[waysl] ignoring unsupported xdg_toplevel request: {other:?}"),
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &xdg_popup::XdgPopup,
        _: xdg_popup::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

fn copy_buffer(data: &BufferData) -> Option<Vec<u8>> {
    let width = usize::try_from(data.width).ok()?;
    let height = usize::try_from(data.height).ok()?;
    match &data.storage {
        BufferStorage::Shm { pool, offset } => {
            let stride = usize::try_from(data.stride).ok()?;
            let source_len = stride.checked_mul(height)?;
            let pool = pool.read().ok()?;
            let source = pool.bytes(*offset, source_len)?;
            let mut pixels = vec![0; width.checked_mul(height)?.checked_mul(4)?];
            let opaque = data.format.is_opaque();
            for y in 0..height {
                for x in 0..width {
                    let source_index = y * stride + x * 4;
                    let target_index = (y * width + x) * 4;
                    let b = source[source_index];
                    let g = source[source_index + 1];
                    let r = source[source_index + 2];
                    pixels[target_index..target_index + 3].copy_from_slice(&[r, g, b]);
                    pixels[target_index + 3] = if opaque {
                        0xff
                    } else {
                        source[source_index + 3]
                    };
                }
            }
            Some(pixels)
        }
        BufferStorage::Dmabuf(image) => image.to_rgba(),
    }
}

unsafe fn mmap_read_write(fd: i32, len: usize) -> io::Result<*mut u8> {
    unsafe extern "C" {
        fn mmap(
            addr: *mut u8,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: isize,
        ) -> *mut u8;
    }
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    const MAP_SHARED: i32 = 1;
    let ptr = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ | PROT_WRITE,
            MAP_SHARED,
            fd,
            0,
        )
    };
    if ptr as isize == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ptr)
    }
}

unsafe fn munmap(ptr: *mut u8, len: usize) {
    unsafe extern "C" {
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }
    unsafe {
        let _ = munmap(ptr, len);
    }
}

// ---------------------------------------------------------------------------
// zwp_linux_dmabuf_v1: GPU buffers for clients that can provide them.
//
// Clients still receive `wl_shm` as a guaranteed fallback, so offering dmabuf
// never makes a client unusable. Imports that cannot be mapped are rejected and
// the client keeps working through shared memory.
// ---------------------------------------------------------------------------

impl GlobalDispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for WaylandState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        let dmabuf = init.init(resource, ());
        // Advertise every layout `DmabufImage::to_rgba` can rasterise, so the
        // deprecated per-format list and the feedback format table agree.
        for fourcc in FEEDBACK_FORMATS {
            dmabuf.modifier(fourcc, 0, 0);
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        request: zwp_linux_dmabuf_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_linux_dmabuf_v1::Request::CreateParams { params_id } => {
                init.init(params_id, Arc::new(Mutex::new(DmabufParams::default())));
            }
            zwp_linux_dmabuf_v1::Request::GetDefaultFeedback { id }
            | zwp_linux_dmabuf_v1::Request::GetSurfaceFeedback { id, .. } => {
                let feedback = init.init(id, ());
                send_feedback(&feedback);
            }
            zwp_linux_dmabuf_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, Arc<Mutex<DmabufParams>>>
    for WaylandState
{
    fn request(
        _: &mut Self,
        _: &Client,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        request: zwp_linux_buffer_params_v1::Request,
        data: &Arc<Mutex<DmabufParams>>,
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_linux_buffer_params_v1::Request::Add {
                fd,
                plane_idx,
                offset,
                stride,
                modifier_hi,
                modifier_lo,
            } => {
                let modifier = (u64::from(modifier_hi) << 32) | u64::from(modifier_lo);
                if let Ok(mut params) = data.lock() {
                    params.modifier = modifier;
                    // Planes may arrive out of order, so honour `plane_idx`
                    // rather than relying on call order.
                    let index = plane_idx as usize;
                    params
                        .planes
                        .insert(plane_idx as usize, DmabufPlane { fd, offset, stride });
                    let _ = index;
                }
            }
            zwp_linux_buffer_params_v1::Request::Create {
                width,
                height,
                format,
                ..
            } => {
                // The deferred form must answer with `created(buffer)` or
                // `failed`. Without a buffer id there is nothing to report, so
                // treat it as an unsupported import.
                match record_and_build(data, width, height, format) {
                    Some(_) => {
                        // A successful import still needs a buffer to hand back;
                        // this compositor only supports the immediate form.
                        params.failed();
                    }
                    None => params.failed(),
                }
            }
            zwp_linux_buffer_params_v1::Request::CreateImmed {
                buffer_id,
                width,
                height,
                format,
                ..
            } => match record_and_build(data, width, height, format) {
                Some(image) => {
                    let buffer_data = BufferData {
                        stride: image.stride,
                        storage: BufferStorage::Dmabuf(Arc::new(image)),
                        width,
                        height,
                        format: BufferFormat::Drm {
                            fourcc: format,
                            modifier: 0,
                        },
                    };
                    init.init(buffer_id, buffer_data);
                }
                None => params.failed(),
            },
            zwp_linux_buffer_params_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

/// Crops to the viewport source rectangle and rescales to the destination size.
///
/// Returns the pixel buffer plus its resulting dimensions. When no viewport is
/// set the input is passed through untouched, so surfaces that never use
/// `wp_viewporter` pay nothing.
fn apply_viewport(
    pixels: &[u8],
    width: i32,
    height: i32,
    source: Option<ViewportRect>,
    destination: Option<(i32, i32)>,
) -> (u32, u32, Vec<u8>) {
    let (width_u, height_u) = match (u32::try_from(width), u32::try_from(height)) {
        (Ok(w), Ok(h)) => (w, h),
        _ => return (0, 0, pixels.to_vec()),
    };
    if source.is_none() && destination.is_none() {
        return (width_u, height_u, pixels.to_vec());
    }

    // Step 1: crop. Rounded outward-ish via truncation of the offsets and
    // rounding of the extents, which keeps the result within the buffer.
    let (crop_x, crop_y, crop_w, crop_h) = match source {
        Some(rect) => {
            let x = rect.x.floor().max(0.0) as u32;
            let y = rect.y.floor().max(0.0) as u32;
            let w = rect.width.round().max(1.0) as u32;
            let h = rect.height.round().max(1.0) as u32;
            (
                x.min(width_u.saturating_sub(1)),
                y.min(height_u.saturating_sub(1)),
                w,
                h,
            )
        }
        None => (0, 0, width_u, height_u),
    };
    let crop_w = crop_w.min(width_u - crop_x);
    let crop_h = crop_h.min(height_u - crop_y);

    let (out_w, out_h) = match destination {
        Some((w, h)) => (
            u32::try_from(w).unwrap_or(crop_w).max(1),
            u32::try_from(h).unwrap_or(crop_h).max(1),
        ),
        None => (crop_w, crop_h),
    };

    if (crop_x, crop_y, crop_w, crop_h) == (0, 0, width_u, height_u)
        && (out_w, out_h) == (width_u, height_u)
    {
        return (width_u, height_u, pixels.to_vec());
    }

    // Step 2: resample with nearest neighbour into the destination size.
    let mut out = vec![0u8; (out_w as usize) * (out_h as usize) * 4];
    for dy in 0..out_h {
        let sy = crop_y + (u64::from(dy) * u64::from(crop_h) / u64::from(out_h)) as u32;
        for dx in 0..out_w {
            let sx = crop_x + (u64::from(dx) * u64::from(crop_w) / u64::from(out_w)) as u32;
            let src = ((sy * width_u + sx) * 4) as usize;
            let dst = ((dy * out_w + dx) * 4) as usize;
            if src + 4 <= pixels.len() && dst + 4 <= out.len() {
                out[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
            }
        }
    }
    (out_w, out_h, out)
}

/// Stores geometry on the params and maps the accumulated planes.
fn record_and_build(
    data: &Arc<Mutex<DmabufParams>>,
    width: i32,
    height: i32,
    format: u32,
) -> Option<DmabufImage> {
    // Only the layouts we advertise can be rasterised. Anything else would be
    // reinterpreted as 32-bit RGBA and render as garbage, so reject it here and
    // let the client fall back to wl_shm.
    if !FEEDBACK_FORMATS.contains(&format) {
        return None;
    }
    let mut params = data.lock().ok()?;
    params.width = width;
    params.height = height;
    params.format = format;
    params.build()
}

/// Formats the compositor accepts, in the order advertised to clients.
const FEEDBACK_FORMATS: [u32; 5] = [
    DRM_FORMAT_XRGB8888,
    DRM_FORMAT_ARGB8888,
    DRM_FORMAT_NV12,
    DRM_FORMAT_YUYV,
    DRM_FORMAT_UYVY,
];

impl Dispatch<zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        request: zwp_linux_dmabuf_feedback_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // `destroy` is the only request; nothing to clean up beyond the object.
        let _ = request;
    }
}

/// Sends the full `zwp_linux_dmabuf_feedback_v1` sequence.
///
/// Clients allocate nothing until `done` arrives: they wait for a format table
/// and at least one tranche describing which device and formats are usable.
/// Omitting any of these leaves the client blocked on `wl_display.sync`, which
/// is what happens when the feedback object exists but stays silent.
fn send_feedback(feedback: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1) {
    // The format table is a sealed memfd holding u32 fourcc values, as the
    // protocol requires; clients map it read-only.
    let table: Vec<u8> = FEEDBACK_FORMATS
        .iter()
        .flat_map(|fourcc| fourcc.to_le_bytes())
        .collect();
    let Some(table_fd) = anonymous_memfd(&table) else {
        // Without a table the feedback is unusable; report completion so the
        // client stops waiting and falls back to wl_shm.
        feedback.done();
        return;
    };
    let size = u32::try_from(table.len()).unwrap_or(0);
    feedback.format_table(table_fd.as_fd(), size);

    // A device identifier of zeros is valid and means "the compositor's own
    // device"; clients only use it to group memory domains.
    let device = [0u8; 8];
    feedback.main_device(device.to_vec());
    feedback.tranche_target_device(device.to_vec());

    // Every format index in the table, as u16 values.
    let indices: Vec<u8> = (0..FEEDBACK_FORMATS.len() as u16)
        .flat_map(u16::to_le_bytes)
        .collect();
    feedback.tranche_formats(indices);
    feedback.tranche_flags(zwp_linux_dmabuf_feedback_v1::TrancheFlags::empty());
    feedback.tranche_done();
    feedback.done();
}

/// Creates a sealed memfd holding `contents`.
///
/// Falls back to `None` when memfd is unavailable so callers can degrade to
/// `wl_shm` instead of offering feedback they cannot back.
#[cfg(target_os = "linux")]
fn anonymous_memfd(contents: &[u8]) -> Option<OwnedFd> {
    let name = c"waysl-dmabuf-formats";
    // MFD_CLOEXEC | MFD_ALLOW_SEALING
    unsafe extern "C" {
        fn memfd_create(name: *const std::os::raw::c_char, flags: u32) -> i32;
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    let raw = unsafe { memfd_create(name.as_ptr(), 0x0001 | 0x0002) };
    if raw < 0 {
        return None;
    }
    // SAFETY: `raw` is a fresh fd owned by this function.
    // SAFETY: `raw` is a fresh fd; ownership moves into the File.
    let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
    file.write_all(contents).ok()?;
    let fd = OwnedFd::from(file);
    // Sealing prevents a client from mutating the table we advertised.
    // F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_SEAL
    let _ = unsafe { fcntl(fd.as_raw_fd(), 1031, 0x0001 | 0x0002 | 0x0004 | 0x0008) };
    Some(fd)
}

/// `memfd_create` is Linux-specific, so other Unix hosts cannot provide a
/// format table; the feedback object reports `done` without one instead.
#[cfg(not(target_os = "linux"))]
fn anonymous_memfd(_contents: &[u8]) -> Option<OwnedFd> {
    None
}

// ---------------------------------------------------------------------------
// wp_viewporter
//
// The renderer currently forwards one bitmap per surface unchanged, so a
// destination size is applied by nearest-neighbour resampling and a source
// rectangle by cropping. That is enough for the common toolkit uses (integer
// and fractional scaling, letterboxing) without pulling in a GPU scaler.
// ---------------------------------------------------------------------------

impl GlobalDispatch<wp_viewporter::WpViewporter, ()> for WaylandState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wp_viewporter::WpViewporter>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<wp_viewporter::WpViewporter, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wp_viewporter::WpViewporter,
        request: wp_viewporter::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_viewporter::Request::GetViewport { id, surface } => {
                // The viewport mutates the surface it was created for, so it
                // shares that surface's state rather than carrying its own.
                if let Some(data) = surface.data::<SurfaceData>() {
                    init.init(id, data.clone());
                }
            }
            wp_viewporter::Request::Destroy => {}
            other => eprintln!("[waysl] ignoring unsupported wp_viewporter request: {other:?}"),
        }
    }
}

impl Dispatch<wp_viewport::WpViewport, SurfaceData> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wp_viewport::WpViewport,
        request: wp_viewport::Request,
        data: &SurfaceData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_viewport::Request::SetSource {
                x,
                y,
                width,
                height,
            } => {
                if let Ok(mut surface) = data.lock() {
                    // -1 in every component means "reset to the whole buffer".
                    surface.viewport_source = if x < 0.0 && y < 0.0 && width < 0.0 && height < 0.0 {
                        None
                    } else if width > 0.0 && height > 0.0 {
                        Some(ViewportRect {
                            x,
                            y,
                            width,
                            height,
                        })
                    } else {
                        // A partially negative rectangle is a protocol error;
                        // ignore it and keep the previous value.
                        surface.viewport_source
                    };
                }
            }
            wp_viewport::Request::SetDestination { width, height } => {
                if let Ok(mut surface) = data.lock() {
                    surface.viewport_destination = if width <= 0 || height <= 0 {
                        None
                    } else {
                        Some((width, height))
                    };
                }
            }
            wp_viewport::Request::Destroy => {}
            other => eprintln!("[waysl] ignoring unsupported wp_viewport request: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// wl_subcompositor
//
// WaySL renders one RGBA bitmap per surface, so a subsurface has no separate
// content to composite. Offering the global still matters: libwayland clients
// (and winit's client-side-decoration path) probe for it and behave differently
// when it is absent. Subsurfaces are therefore accepted and driven to a no-op,
// which is preferable to leaving a client in its "no CSD" fallback.
// ---------------------------------------------------------------------------

impl GlobalDispatch<wl_subcompositor::WlSubcompositor, ()> for WaylandState {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<wl_subcompositor::WlSubcompositor>,
        _: &(),
        init: &mut DataInit<'_, Self>,
    ) {
        init.init(resource, ());
    }
}

impl Dispatch<wl_subcompositor::WlSubcompositor, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_subcompositor::WlSubcompositor,
        request: wl_subcompositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_subcompositor::Request::GetSubsurface { id, .. } => {
                init.init(id, ());
            }
            wl_subcompositor::Request::Destroy => {}
            other => eprintln!("[waysl] ignoring unsupported wl_subcompositor request: {other:?}"),
        }
    }
}

impl Dispatch<wl_subsurface::WlSubsurface, ()> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_subsurface::WlSubsurface,
        request: wl_subsurface::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        // `set_position`, `place_above`, `place_below` and the sync/desync pair
        // describe compositing the bitmap renderer does not perform; accepting
        // and ignoring them keeps clients on their normal code path.
        match request {
            wl_subsurface::Request::Destroy => {}
            other => eprintln!("[waysl] ignoring unsupported wl_subsurface request: {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an ARGB8888 image whose pixel at (x, y) has known channel values.
    fn dmabuf(width: i32, height: i32, stride: i32, fourcc: u32, alpha: u8) -> DmabufImage {
        let mut mapping = vec![0u8; (stride * height) as usize];
        for y in 0..height {
            for x in 0..width {
                let at = (y * stride + x * 4) as usize;
                mapping[at] = 10; // blue
                mapping[at + 1] = 20; // green
                mapping[at + 2] = 30; // red
                mapping[at + 3] = alpha;
            }
        }
        DmabufImage {
            width,
            height,
            stride,
            fourcc,
            planes: vec![mapping],
        }
    }

    #[test]
    fn dmabuf_argb_preserves_alpha() {
        let image = dmabuf(2, 1, 8, DRM_FORMAT_ARGB8888, 0x80);
        let pixels = image.to_rgba().expect("convert");
        assert_eq!(pixels.len(), 8);
        assert_eq!(&pixels[0..4], &[30, 20, 10, 0x80]);
    }

    #[test]
    fn dmabuf_xrgb_forces_opaque() {
        // XRGB has no meaningful alpha; the compositor must not show it through.
        let image = dmabuf(2, 1, 8, DRM_FORMAT_XRGB8888, 0x00);
        let pixels = image.to_rgba().expect("convert");
        assert_eq!(pixels[3], 0xff);
        assert_eq!(pixels[7], 0xff);
    }

    #[test]
    fn dmabuf_honours_stride_padding() {
        // A stride wider than the pixels must be skipped, not treated as data.
        let image = dmabuf(1, 2, 16, DRM_FORMAT_XRGB8888, 0xff);
        let pixels = image.to_rgba().expect("convert");
        assert_eq!(pixels.len(), 8);
        assert_eq!(&pixels[0..3], &[30, 20, 10]);
        assert_eq!(&pixels[4..7], &[30, 20, 10]);
    }

    #[test]
    fn dmabuf_rejects_short_mapping() {
        let image = DmabufImage {
            width: 4,
            height: 4,
            stride: 16,
            fourcc: DRM_FORMAT_XRGB8888,
            planes: vec![vec![0; 8]],
        };
        assert!(image.to_rgba().is_none());
    }

    #[test]
    fn buffer_format_opacity_matches_layout() {
        assert!(BufferFormat::Shm(wl_shm::Format::Xrgb8888).is_opaque());
        assert!(!BufferFormat::Shm(wl_shm::Format::Argb8888).is_opaque());
        assert!(
            BufferFormat::Drm {
                fourcc: DRM_FORMAT_XRGB8888,
                modifier: 0
            }
            .is_opaque()
        );
        assert!(
            !BufferFormat::Drm {
                fourcc: DRM_FORMAT_ARGB8888,
                modifier: 0
            }
            .is_opaque()
        );
    }

    /// Builds a width x height RGBA buffer where each pixel encodes its column.
    fn columns(width: u32, height: u32) -> Vec<u8> {
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let at = ((y * width + x) * 4) as usize;
                pixels[at] = x as u8;
                pixels[at + 3] = 0xff;
            }
        }
        pixels
    }

    #[test]
    fn viewport_passthrough_does_not_copy_or_resize() {
        let pixels = columns(4, 2);
        let (w, h, out) = apply_viewport(&pixels, 4, 2, None, None);
        assert_eq!((w, h), (4, 2));
        assert_eq!(out, pixels);
    }

    #[test]
    fn viewport_source_crops() {
        let pixels = columns(4, 2);
        let source = ViewportRect {
            x: 1.0,
            y: 0.0,
            width: 2.0,
            height: 2.0,
        };
        let (w, h, out) = apply_viewport(&pixels, 4, 2, Some(source), None);
        assert_eq!((w, h), (2, 2));
        // Cropped columns 1 and 2.
        assert_eq!(out[0], 1);
        assert_eq!(out[4], 2);
    }

    #[test]
    fn viewport_destination_scales() {
        let pixels = columns(2, 2);
        let (w, h, out) = apply_viewport(&pixels, 2, 2, None, Some((4, 4)));
        assert_eq!((w, h), (4, 4));
        assert_eq!(out.len(), 4 * 4 * 4);
        // Nearest neighbour doubles each column.
        assert_eq!(out[0], 0);
        assert_eq!(out[4], 0);
        assert_eq!(out[8], 1);
    }

    #[test]
    fn viewport_crop_and_scale_compose() {
        let pixels = columns(4, 4);
        let source = ViewportRect {
            x: 2.0,
            y: 2.0,
            width: 2.0,
            height: 2.0,
        };
        let (w, h, out) = apply_viewport(&pixels, 4, 4, Some(source), Some((1, 1)));
        assert_eq!((w, h), (1, 1));
        assert_eq!(out[0], 2);
    }

    #[test]
    fn viewport_source_is_clamped_to_buffer() {
        let pixels = columns(4, 2);
        let source = ViewportRect {
            x: 3.0,
            y: 1.0,
            width: 99.0,
            height: 99.0,
        };
        let (w, h, out) = apply_viewport(&pixels, 4, 2, Some(source), None);
        assert_eq!((w, h), (1, 1));
        assert_eq!(out[0], 3);
    }

    #[test]
    fn yuv_to_rgb_handles_reference_values() {
        // Limited-range black and white are the two values a wrong matrix gets
        // wrong most visibly.
        assert_eq!(yuv_to_rgb(16, 128, 128), (0, 0, 0));
        // 1.164 * (235 - 16) = 254.9, which truncates to 254; allow one step.
        let white = yuv_to_rgb(235, 128, 128);
        assert!(
            white.0 >= 254 && white.1 >= 254 && white.2 >= 254,
            "got {white:?}"
        );
        // Pure red in BT.601 limited range.
        let (r, g, b) = yuv_to_rgb(81, 90, 240);
        assert!(r > 200 && g < 70 && b < 70, "got {r},{g},{b}");
    }

    #[test]
    fn nv12_luma_only_plane_gives_grey() {
        // 2x2 all mid-grey luma with neutral chroma.
        let image = DmabufImage {
            width: 2,
            height: 2,
            stride: 2,
            fourcc: DRM_FORMAT_NV12,
            planes: vec![vec![128, 128, 128, 128], vec![128, 128]],
        };
        let pixels = image.to_rgba().expect("convert");
        assert_eq!(pixels.len(), 2 * 2 * 4);
        // Neutral chroma produces equal channels.
        assert_eq!(pixels[0], pixels[1]);
        assert_eq!(pixels[1], pixels[2]);
        assert_eq!(pixels[3], 0xff);
    }

    #[test]
    fn nv12_rejects_missing_chroma_plane() {
        let image = DmabufImage {
            width: 2,
            height: 2,
            stride: 2,
            fourcc: DRM_FORMAT_NV12,
            planes: vec![vec![128, 128, 128, 128]],
        };
        assert!(image.to_rgba().is_none());
    }

    #[test]
    fn nv12_rejects_short_chroma_plane() {
        let image = DmabufImage {
            width: 2,
            height: 2,
            stride: 2,
            fourcc: DRM_FORMAT_NV12,
            planes: vec![vec![128, 128, 128, 128], vec![128]],
        };
        assert!(image.to_rgba().is_none());
    }

    #[test]
    fn nv12_rejects_short_luma_plane() {
        let image = DmabufImage {
            width: 32,
            height: 32,
            stride: 32,
            fourcc: DRM_FORMAT_NV12,
            planes: vec![vec![128; 32], vec![128; 32 * 16]],
        };
        assert!(image.to_rgba().is_none());
    }

    #[test]
    fn yuyv_groups_two_pixels_per_chroma() {
        // One 2x1 row: Y0 U Y1 V.
        let image = DmabufImage {
            width: 2,
            height: 1,
            stride: 4,
            fourcc: DRM_FORMAT_YUYV,
            planes: vec![vec![128, 128, 128, 128]],
        };
        let pixels = image.to_rgba().expect("convert");
        assert_eq!(pixels.len(), 2 * 4);
        // Both pixels share chroma, so they match.
        assert_eq!(&pixels[0..3], &pixels[4..7]);
        // And the result is grey-ish for neutral chroma.
        assert_eq!(pixels[0], pixels[1]);
        assert_eq!(pixels[1], pixels[2]);
    }

    #[test]
    fn uyvy_orders_chroma_first() {
        let image = DmabufImage {
            width: 2,
            height: 1,
            stride: 4,
            fourcc: DRM_FORMAT_UYVY,
            planes: vec![vec![128, 128, 128, 128]],
        };
        let pixels = image.to_rgba().expect("convert");
        // Neutral chroma in either layout must give the same grey.
        assert_eq!(pixels[0], pixels[1]);
        assert_eq!(pixels[1], pixels[2]);
    }

    #[test]
    fn plane_count_matches_formats() {
        assert_eq!(plane_count(DRM_FORMAT_ARGB8888), Some(1));
        assert_eq!(plane_count(DRM_FORMAT_YUYV), Some(1));
        assert_eq!(plane_count(DRM_FORMAT_NV12), Some(2));
        assert_eq!(plane_count(u32::from_le_bytes(*b"YU12")), None);
        assert_eq!(vertical_subsampling(DRM_FORMAT_NV12), Some(2));
        assert_eq!(vertical_subsampling(DRM_FORMAT_ARGB8888), Some(1));
    }

    #[test]
    fn unsupported_dmabuf_format_is_rejected() {
        // A client may import any fourcc, but only the advertised 32-bit
        // layouts can be rasterised; anything else must be refused rather than
        // silently reinterpreted as RGBA.
        let params = Arc::new(Mutex::new(DmabufParams::default()));
        assert!(record_and_build(&params, 4, 4, u32::from_le_bytes(*b"YU12")).is_none());
        assert!(record_and_build(&params, 4, 4, DRM_FORMAT_ARGB8888).is_none());
    }

    #[test]
    fn dmabuf_params_without_planes_cannot_build() {
        let params = DmabufParams {
            width: 4,
            height: 4,
            format: DRM_FORMAT_ARGB8888,
            ..DmabufParams::default()
        };
        assert!(params.build().is_none());
    }
}
