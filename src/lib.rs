//! WaySL: show Wayland applications from WSL on the Windows desktop.
//!
//! The crate is split into independent pieces that can be used separately:
//!
//! - [`config`] — environment-variable configuration for every mode.
//! - [`frame_bridge`] — the internal RGBA frame transport between the Linux
//!   compositor and the Windows renderer. Not a Wayland protocol.
//! - [`renderer`] — the native Bevy window renderer (Windows and Unix).
//! - [`compositor`] — the headless standard Wayland compositor (Unix only).
//! - `wayland` (Unix only) — the `wl_shm`/`xdg-shell` endpoint implementation.
//! - `wayland_wire` — a dependency-free Wayland wire endpoint over TCP.

pub mod config;
pub mod frame_bridge;
pub mod renderer;

#[cfg(unix)]
pub mod compositor;

#[cfg(unix)]
pub mod wayland;

pub mod wayland_wire;

/// Runs the entry point selected by the environment and returns its exit code.
///
/// Selection order:
///
/// 1. `WAYSL_FRAME_FORWARD` set on Unix → headless compositor.
/// 2. Otherwise → the Bevy renderer.
pub fn run() -> std::io::Result<()> {
    let config = config::Config::from_environment();

    #[cfg(unix)]
    if let Some(address) = config.frame_forward.clone() {
        return compositor::run_headless(&config, &address);
    }

    renderer::run(&config)
}
