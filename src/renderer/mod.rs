//! Native Bevy renderer: one window per forwarded Wayland surface.

mod placeholder;
mod surfaces;
mod waypipe;

use bevy::prelude::*;
use bevy::window::{ExitCondition, WindowPlugin};

use crate::config::Config;
use crate::frame_bridge;

pub use surfaces::{Surface, SurfaceId, SurfaceRegistry};

/// Runs the renderer until the process exits.
pub fn run(config: &Config) -> std::io::Result<()> {
    let input = surfaces::SurfaceInput::bind(&config.frame_bridge);

    #[cfg(windows)]
    start_tcp_endpoint(config);

    let mut app = App::new();
    app.insert_resource(input);
    app.insert_resource(SurfaceRegistry::default());
    app.insert_resource(waypipe::WaypipeProcess::new(config.waypipe_command.clone()));
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: None,
        exit_condition: ExitCondition::DontExit,
        close_when_requested: false,
        ..default()
    }));
    app.add_systems(Startup, placeholder::startup);
    app.add_systems(
        Update,
        (
            surfaces::receive_frames,
            surfaces::sync_windows,
            surfaces::upload_frames,
        )
            .chain(),
    );
    app.run();
    Ok(())
}

/// Starts the experimental TCP Wayland endpoint (framing only, no pixel data).
#[cfg(windows)]
fn start_tcp_endpoint(config: &Config) {
    use crate::wayland_wire::WaylandTcpServer;

    let address = config.wayland_tcp.clone();
    std::thread::spawn(move || {
        let server = match WaylandTcpServer::bind(&address) {
            Ok(server) => server,
            Err(error) => {
                eprintln!("WaySL TCP Wayland endpoint failed on {address}: {error}");
                return;
            }
        };
        eprintln!("WaySL TCP Wayland endpoint listening on {address}");
        loop {
            if let Err(error) = server.accept_once() {
                eprintln!("WaySL TCP Wayland client stopped: {error}");
            }
        }
    });
}

/// Converts a bridge frame into a renderer surface.
pub(crate) fn surface_from_frame(frame: frame_bridge::Frame) -> Surface {
    Surface {
        id: SurfaceId(frame.id),
        title: frame.title,
        width: frame.width,
        height: frame.height,
        pixels: frame.pixels,
        generation: frame.generation,
    }
}
