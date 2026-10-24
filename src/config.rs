//! Configuration gathered from the environment.
//!
//! Every mode reads the same struct so defaults live in exactly one place.

use std::path::PathBuf;

/// Default Wayland socket advertised by the headless compositor.
pub const DEFAULT_WAYLAND_SOCKET: &str = "/tmp/waysl-wayland-0";
/// Default bind address of the renderer's frame bridge.
pub const DEFAULT_FRAME_BRIDGE: &str = "127.0.0.1:48765";
/// Default bind address of the experimental TCP Wayland endpoint.
pub const DEFAULT_WAYLAND_TCP: &str = "127.0.0.1:48766";

/// Placeholder surface shown before any Wayland surface arrives. The compositor
/// hands out real surface ids starting at `PLACEHOLDER_SURFACE_ID + 1`.
pub const PLACEHOLDER_SURFACE_ID: u64 = 1;
/// Size of the placeholder surface.
pub const PLACEHOLDER_WIDTH: u32 = 960;
/// Height of the placeholder surface.
pub const PLACEHOLDER_HEIGHT: u32 = 540;

/// Everything the process needs, resolved once at start-up.
#[derive(Debug, Clone)]
pub struct Config {
    /// Wayland socket path for the headless compositor (`WAYSL_WAYLAND_SOCKET`).
    pub wayland_socket: PathBuf,
    /// Frame bridge address for the headless compositor (`WAYSL_FRAME_FORWARD`).
    ///
    /// Setting this variable selects headless mode.
    pub frame_forward: Option<String>,
    /// Bind address of the renderer's frame bridge (`WAYSL_LISTEN`).
    pub frame_bridge: String,
    /// Bind address of the experimental TCP Wayland endpoint (`WAYSL_WAYLAND_TCP`).
    pub wayland_tcp: String,
    /// Optional Waypipe command launched by the renderer (`WAYSL_WAYPIPE_COMMAND`).
    pub waypipe_command: Option<String>,
    /// Where the renderer sends forwarded input (`WAYSL_INPUT_FORWARD`), and
    /// where the compositor listens for it (`WAYSL_INPUT_LISTEN`). Unset on
    /// either side disables input.
    pub input_forward: Option<String>,
}

impl Config {
    /// Reads the configuration from the process environment.
    pub fn from_environment() -> Self {
        Self {
            wayland_socket: std::env::var_os("WAYSL_WAYLAND_SOCKET")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_WAYLAND_SOCKET)),
            frame_forward: non_empty("WAYSL_FRAME_FORWARD"),
            frame_bridge: non_empty("WAYSL_LISTEN")
                .unwrap_or_else(|| DEFAULT_FRAME_BRIDGE.to_string()),
            wayland_tcp: non_empty("WAYSL_WAYLAND_TCP")
                .unwrap_or_else(|| DEFAULT_WAYLAND_TCP.to_string()),
            waypipe_command: non_empty("WAYSL_WAYPIPE_COMMAND"),
            input_forward: non_empty("WAYSL_INPUT_FORWARD"),
        }
    }

    /// Whether the process should run as a headless compositor.
    pub const fn is_headless(&self) -> bool {
        self.frame_forward.is_some()
    }
}

/// Reads `name`, treating an empty value as unset so scripts cannot accidentally
/// select a mode with `VAR=`.
fn non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_stable() {
        assert_eq!(DEFAULT_WAYLAND_SOCKET, "/tmp/waysl-wayland-0");
        assert_eq!(DEFAULT_FRAME_BRIDGE, "127.0.0.1:48765");
        assert_eq!(DEFAULT_WAYLAND_TCP, "127.0.0.1:48766");
    }

    #[test]
    fn headless_requires_a_forward_address() {
        let config = Config {
            wayland_socket: PathBuf::from(DEFAULT_WAYLAND_SOCKET),
            frame_forward: None,
            frame_bridge: DEFAULT_FRAME_BRIDGE.to_string(),
            wayland_tcp: DEFAULT_WAYLAND_TCP.to_string(),
            waypipe_command: None,
            input_forward: None,
        };
        assert!(!config.is_headless());
    }
}
