//! GPU-backed buffer import through EGL and dma-buf.
//!
//! WSL2 exposes the GPU through `/dev/dxg` rather than a DRM render node, so
//! `gbm_bo_create` cannot allocate there. Mesa's D3D12 Gallium driver still
//! provides a full EGL implementation on top of `/dev/dxg`, and that path is
//! what WSLg itself uses. This module takes the same route: it initialises an
//! EGL display, checks that dma-buf export is offered, and reports whether a
//! GPU-backed surface path is usable.
//!
//! Everything is probed at runtime. When EGL, the driver, or dma-buf export is
//! missing the module reports the reason and the compositor keeps offering
//! `wl_shm`, so a build never depends on GPU availability at compile time.

use std::ffi::{CStr, c_void};
use std::os::raw::c_char;

/// EGL constants used by the probe (Khronos values, stable since EGL 1.0).
const EGL_NO_DISPLAY: *mut c_void = std::ptr::null_mut();
const EGL_DEFAULT_DISPLAY: *mut c_void = std::ptr::null_mut();
const EGL_VENDOR: i32 = 0x3053;
const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_VERSION: i32 = 0x3054;

/// Result of probing the GPU path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuAvailability {
    /// An EGL display is usable and can export dma-bufs.
    Ready {
        /// EGL vendor string.
        vendor: String,
        /// EGL version string.
        version: String,
    },
    /// EGL could not be loaded at all.
    NoEgl {
        /// Why loading failed.
        reason: String,
    },
    /// EGL loaded but no display could be initialised.
    NoDisplay {
        /// Why initialisation failed.
        reason: String,
    },
    /// A display exists but cannot export dma-bufs.
    NoDmabufExport {
        /// EGL vendor, for diagnostics.
        vendor: String,
    },
}

impl GpuAvailability {
    /// Whether GPU-backed buffers can be imported.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    /// A short human-readable summary for logs.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Ready { vendor, version } => {
                format!("dmabuf export available via {vendor} (EGL {version})")
            }
            Self::NoEgl { reason } => format!("EGL unavailable: {reason}"),
            Self::NoDisplay { reason } => format!("no EGL display: {reason}"),
            Self::NoDmabufExport { vendor } => {
                format!("{vendor} does not export dma-bufs")
            }
        }
    }
}

/// Minimal EGL entry points, loaded dynamically so a build without EGL still
/// links and runs.
struct EglLib {
    handle: *mut c_void,
}

// The handle is only used through EGL's thread-local state after initialisation.
// The library is intentionally never closed: EGL keeps process-global state that
// later calls still reference.
unsafe impl Send for EglLib {}

impl EglLib {
    fn open() -> Result<Self, String> {
        // `dlopen` keeps the dependency optional: no link-time requirement on
        // libEGL, so the compositor still builds on headless CI.
        unsafe extern "C" {
            fn dlopen(filename: *const c_char, flags: i32) -> *mut c_void;
        }
        const RTLD_NOW: i32 = 2;
        // WSL ships its own EGL under /usr/lib/wsl/lib, and that is the one with
        // the D3D12 backend that reaches /dev/dxg. The distro's
        // /usr/lib/x86_64-linux-gnu/libEGL.so.1 has no such driver, so prefer the
        // WSL copy when present and fall back to the default search order.
        const CANDIDATES: [&CStr; 2] = [c"/usr/lib/wsl/lib/libEGL.so.1", c"libEGL.so.1"];
        for name in CANDIDATES {
            let handle = unsafe { dlopen(name.as_ptr(), RTLD_NOW) };
            if !handle.is_null() {
                return Ok(Self { handle });
            }
        }
        Err("libEGL.so.1 not found".to_string())
    }

    /// Resolves an EGL entry point the way the spec requires.
    ///
    /// Core EGL 1.x functions are exported symbols, but extension functions
    /// (notably `eglQueryDevicesEXT` and `eglGetPlatformDisplayEXT`) are only
    /// reachable through `eglGetProcAddress`, which is why `dlsym` returns null
    /// for them even though the extension is advertised.
    fn proc(&self, name: &CStr) -> *mut c_void {
        let direct = self.symbol(name);
        if !direct.is_null() {
            return direct;
        }
        let get_proc = self.symbol(c"eglGetProcAddress");
        if get_proc.is_null() {
            return std::ptr::null_mut();
        }
        type GetProcAddressFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
        // SAFETY: the symbol came from libEGL and takes a plain C string.
        let get_proc: GetProcAddressFn =
            unsafe { std::mem::transmute::<*mut c_void, GetProcAddressFn>(get_proc) };
        unsafe { get_proc(name.as_ptr()) }
    }

    fn symbol(&self, name: &CStr) -> *mut c_void {
        unsafe extern "C" {
            fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        }
        unsafe { dlsym(self.handle, name.as_ptr()) }
    }

    /// Enumerates EGL devices and builds a display for the first usable one.
    ///
    /// This mirrors what `eglinfo` does: query devices, then ask for a display
    /// per device through `EGL_PLATFORM_DEVICE_EXT`. Calling `eglGetDisplay`
    /// alone fails on drivers that only expose devices (WSL's D3D12 path
    /// reports `EGL_NOT_INITIALIZED` in that case).
    fn device_display(&self) -> Option<*mut c_void> {
        let query_devices = self.proc(c"eglQueryDevicesEXT");
        let get_platform_display = self.proc(c"eglGetPlatformDisplayEXT");
        if query_devices.is_null() || get_platform_display.is_null() {
            return None;
        }
        type QueryDevicesFn = unsafe extern "C" fn(i32, *mut *mut c_void, *mut i32) -> u32;
        type GetPlatformDisplayFn =
            unsafe extern "C" fn(u32, *mut c_void, *const i32) -> *mut c_void;
        const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
        const MAX_DEVICES: usize = 8;

        // SAFETY: both symbols were resolved from libEGL and are called with
        // their documented argument shapes.
        let query_devices: QueryDevicesFn =
            unsafe { std::mem::transmute::<*mut c_void, QueryDevicesFn>(query_devices) };
        let get_platform_display: GetPlatformDisplayFn = unsafe {
            std::mem::transmute::<*mut c_void, GetPlatformDisplayFn>(get_platform_display)
        };

        let mut devices: [*mut c_void; MAX_DEVICES] = [std::ptr::null_mut(); MAX_DEVICES];
        let mut count = 0i32;
        let ok = unsafe { query_devices(MAX_DEVICES as i32, devices.as_mut_ptr(), &raw mut count) };
        if ok == 0 {
            return None;
        }
        let count = usize::try_from(count).ok()?.min(MAX_DEVICES);
        for device in devices.iter().take(count) {
            if device.is_null() {
                continue;
            }
            // SAFETY: the device handle came from `eglQueryDevicesEXT`; a null
            // attribute list is the documented form.
            let display =
                unsafe { get_platform_display(EGL_PLATFORM_DEVICE_EXT, *device, std::ptr::null()) };
            if !display.is_null() && display != EGL_NO_DISPLAY {
                return Some(display);
            }
        }
        None
    }

    /// Builds a surfaceless display when the driver supports that platform.
    fn platform_display(&self) -> Option<*mut c_void> {
        let get_platform_display = self.proc(c"eglGetPlatformDisplayEXT");
        let query_string = self.symbol(c"eglQueryString");
        if get_platform_display.is_null() || query_string.is_null() {
            return None;
        }
        // SAFETY: symbols come from libEGL; QueryString is called with the
        // documented EGL_NO_DISPLAY sentinel, which is valid for extension
        // queries.
        let query: QueryStringFn =
            unsafe { std::mem::transmute::<*mut c_void, QueryStringFn>(query_string) };
        let extensions = unsafe { read_string(query, EGL_NO_DISPLAY, EGL_EXTENSIONS) };
        if !extensions.contains("EGL_MESA_platform_surfaceless") {
            return None;
        }
        type GetPlatformDisplayFn =
            unsafe extern "C" fn(u32, *mut c_void, *const i32) -> *mut c_void;
        const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
        // SAFETY: the platform enum and null native display are the documented
        // arguments for the surfaceless platform.
        let get_platform_display: GetPlatformDisplayFn = unsafe {
            std::mem::transmute::<*mut c_void, GetPlatformDisplayFn>(get_platform_display)
        };
        let display = unsafe {
            get_platform_display(
                EGL_PLATFORM_SURFACELESS_MESA,
                EGL_DEFAULT_DISPLAY,
                std::ptr::null(),
            )
        };
        (!display.is_null() && display != EGL_NO_DISPLAY).then_some(display)
    }

    /// Loads `eglGetDisplay` and `eglInitialize`.
    fn entry_points(&self) -> Option<(GetDisplayFn, InitializeFn, QueryStringFn)> {
        let get_display = self.symbol(c"eglGetDisplay");
        let initialize = self.symbol(c"eglInitialize");
        let query_string = self.symbol(c"eglQueryString");
        if get_display.is_null() || initialize.is_null() || query_string.is_null() {
            return None;
        }
        // SAFETY: the symbols were just resolved from libEGL and are only called
        // with arguments matching their documented signatures.
        Some(unsafe {
            (
                std::mem::transmute::<*mut c_void, GetDisplayFn>(get_display),
                std::mem::transmute::<*mut c_void, InitializeFn>(initialize),
                std::mem::transmute::<*mut c_void, QueryStringFn>(query_string),
            )
        })
    }
}

type GetDisplayFn = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type InitializeFn = unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> u32;
type QueryStringFn = unsafe extern "C" fn(*mut c_void, i32) -> *const c_char;

/// Probes whether GPU-backed buffers can be imported on this machine.
///
/// This is the entry point the compositor calls once at start-up; the answer
/// decides whether `zwp_linux_dmabuf_v1` is advertised.
#[must_use]
pub fn probe() -> GpuAvailability {
    // `eglGetDisplay(EGL_DEFAULT_DISPLAY)` resolves through DISPLAY/WAYLAND_DISPLAY.
    // WaySL *is* the compositor on that socket, so pointing EGL at it would make
    // the probe block on a connection that cannot serve a GPU display. Probe the
    // driver's own device instead by clearing those hints for the call.
    let saved: Vec<(&str, Option<std::ffi::OsString>)> = ["WAYLAND_DISPLAY", "DISPLAY"]
        .into_iter()
        .map(|key| (key, std::env::var_os(key)))
        .collect();
    for (key, _) in &saved {
        // SAFETY: the compositor calls this once at start-up, before any other
        // thread observes the environment.
        unsafe { std::env::remove_var(key) };
    }
    let availability = probe_display();
    for (key, value) in saved {
        if let Some(value) = value {
            // SAFETY: as above; still single-threaded at start-up.
            unsafe { std::env::set_var(key, value) };
        }
    }
    availability
}

/// Performs the actual EGL probe with display hints cleared.
fn probe_display() -> GpuAvailability {
    let lib = match EglLib::open() {
        Ok(lib) => lib,
        Err(reason) => return GpuAvailability::NoEgl { reason },
    };
    let Some((get_display, initialize, query_string)) = lib.entry_points() else {
        return GpuAvailability::NoEgl {
            reason: "libEGL is missing core entry points".to_string(),
        };
    };

    // SAFETY: EGL_DEFAULT_DISPLAY is the documented sentinel for the default
    // display; both calls tolerate a null/absent display and report failure.
    // Prefer the surfaceless platform: it needs no window system, which is
    // exactly the situation here (WaySL is the compositor, not a client), and
    // it is the platform that reaches the D3D12 driver over /dev/dxg on WSL.
    // Try each strategy in turn: the surfaceless platform needs no window
    // system (WaySL *is* the compositor), and the default display is the
    // fallback for drivers that only implement that.
    let mut last_error = 0i32;
    let mut chosen: Option<(*mut c_void, i32, i32)> = None;
    let candidates = [
        // Device enumeration is the recipe that works for WSL's D3D12 driver.
        lib.device_display(),
        lib.platform_display(),
        Some(unsafe { get_display(EGL_DEFAULT_DISPLAY) }),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_null() || candidate == EGL_NO_DISPLAY {
            continue;
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: `candidate` came from EGL and initialisation is idempotent for
        // an already-initialised display.
        let ok = unsafe { initialize(candidate, &raw mut major, &raw mut minor) };
        if ok != 0 {
            chosen = Some((candidate, major, minor));
            break;
        }
        // SAFETY: reading the error marker for a failed initialise.
        last_error = unsafe { read_egl_error(&lib) };
    }
    let Some((display, major, minor)) = chosen else {
        return GpuAvailability::NoDisplay {
            reason: format!("eglInitialize failed (EGL error 0x{last_error:04x})"),
        };
    };
    let _ = (major, minor);

    let vendor = unsafe { read_string(query_string, display, EGL_VENDOR) };
    let version = unsafe { read_string(query_string, display, EGL_VERSION) };
    let extensions = unsafe { read_string(query_string, display, EGL_EXTENSIONS) };

    if !extensions.contains("EGL_MESA_image_dma_buf_export") {
        return GpuAvailability::NoDmabufExport { vendor };
    }
    GpuAvailability::Ready { vendor, version }
}

/// Reads the current EGL error code, or 0 when unavailable.
///
/// # Safety
///
/// `lib` must be a loaded libEGL handle.
unsafe fn read_egl_error(lib: &EglLib) -> i32 {
    let ptr = lib.symbol(c"eglGetError");
    if ptr.is_null() {
        return 0;
    }
    let get_error: unsafe extern "C" fn() -> i32 =
        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> i32>(ptr) };
    unsafe { get_error() }
}

/// Reads one EGL string, tolerating a null pointer.
///
/// # Safety
///
/// `query_string` must be a valid EGL entry point and `display` must be a
/// display returned by `eglGetDisplay`.
unsafe fn read_string(query_string: QueryStringFn, display: *mut c_void, name: i32) -> String {
    let ptr = unsafe { query_string(display, name) };
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_predicate_matches_variant() {
        let ready = GpuAvailability::Ready {
            vendor: "Mesa".to_string(),
            version: "1.5".to_string(),
        };
        assert!(ready.is_ready());
        assert!(
            !GpuAvailability::NoEgl {
                reason: "x".to_string()
            }
            .is_ready()
        );
        assert!(
            !GpuAvailability::NoDmabufExport {
                vendor: "nvidia".to_string()
            }
            .is_ready()
        );
    }

    #[test]
    fn describe_mentions_cause() {
        assert!(
            GpuAvailability::NoDmabufExport {
                vendor: "nvidia".to_string()
            }
            .describe()
            .contains("nvidia")
        );
        assert!(
            GpuAvailability::NoEgl {
                reason: "libEGL.so.1 not found".to_string()
            }
            .describe()
            .contains("libEGL")
        );
    }

    #[test]
    fn probe_never_panics() {
        // Whatever the host offers, probing must return an answer rather than
        // aborting the compositor.
        let _ = probe();
    }
}
