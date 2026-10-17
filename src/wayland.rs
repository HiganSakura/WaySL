//! Minimal standard Wayland endpoint with wl_shm surface import.
#![cfg(unix)]

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use wayland_protocols::xdg::shell::server::{xdg_popup, xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_server::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
};

static NEXT_SURFACE_ID: AtomicU64 = AtomicU64::new(2);

#[derive(Debug, Clone)]
pub struct WaylandFrame {
    pub id: u64,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub generation: u64,
}

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
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
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
    pool: Arc<ShmPool>,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: wl_shm::Format,
}

#[derive(Default)]
struct SurfaceInner {
    id: u64,
    attached: Option<wl_buffer::WlBuffer>,
    callbacks: Vec<wl_callback::WlCallback>,
    title: String,
    generation: u64,
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
            _ => unreachable!("wl_compositor protocol request is covered"),
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
        match request {
            wl_shm::Request::CreatePool { id, fd, size } => {
                if let Ok(pool) = ShmPool::new(fd, size) {
                    init.init(id, Arc::new(pool));
                }
            }
            _ => unreachable!("wl_shm protocol request is covered"),
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, Arc<ShmPool>> for WaylandState {
    fn request(
        _: &mut Self,
        _: &Client,
        pool: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        data: &Arc<ShmPool>,
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
                            pool: data.clone(),
                            offset,
                            width,
                            height,
                            stride,
                            format,
                        },
                    );
                }
            }
            wl_shm_pool::Request::Destroy => {
                let _ = pool;
            }
            wl_shm_pool::Request::Resize { size } => {
                let _ = size;
            }
            _ => unreachable!("wl_shm_pool protocol request is covered"),
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
        match request {
            wl_buffer::Request::Destroy => {}
            _ => unreachable!("wl_buffer protocol request is covered"),
        }
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
                    if let Some(sender) = &state.sender {
                        let _ = sender.send(WaylandFrame {
                            id: data.id,
                            title: data.title.clone(),
                            width: buffer_data.width as u32,
                            height: buffer_data.height as u32,
                            pixels,
                            generation: data.generation,
                        });
                    }
                    buffer.release();
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u32;
                    for callback in data.callbacks.drain(..) {
                        callback.done(timestamp);
                    }
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
            _ => unreachable!("wl_surface protocol request is covered"),
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
            _ => unreachable!("xdg_wm_base protocol request is covered"),
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
            _ => unreachable!("xdg_surface protocol request is covered"),
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
            _ => {}
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
    let stride = usize::try_from(data.stride).ok()?;
    let width = usize::try_from(data.width).ok()?;
    let height = usize::try_from(data.height).ok()?;
    let source_len = stride.checked_mul(height)?;
    let source = data.pool.bytes(data.offset, source_len)?;
    let mut pixels = vec![0; width.checked_mul(height)?.checked_mul(4)?];
    for y in 0..height {
        for x in 0..width {
            let source_index = y * stride + x * 4;
            let target_index = (y * width + x) * 4;
            let b = source[source_index];
            let g = source[source_index + 1];
            let r = source[source_index + 2];
            pixels[target_index..target_index + 3].copy_from_slice(&[r, g, b]);
            pixels[target_index + 3] = if data.format == wl_shm::Format::Xrgb8888 {
                0xff
            } else {
                source[source_index + 3]
            };
        }
    }
    Some(pixels)
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
