mod frame_bridge;
#[cfg(unix)]
mod wayland;
mod wayland_wire;

use bevy::asset::RenderAssetUsages;
use bevy::camera::RenderTarget;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::window::WindowRef;
use bevy::window::{ExitCondition, WindowPlugin, WindowResolution};
use std::collections::HashMap;
use std::io;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

const DEFAULT_WIDTH: u32 = 960;
const DEFAULT_HEIGHT: u32 = 540;
/// Reserved for the placeholder window shown before any Wayland surface arrives.
/// The compositor hands out surface ids starting at `PLACEHOLDER_SURFACE_ID + 1`.
const PLACEHOLDER_SURFACE_ID: u64 = 1;

#[cfg(unix)]
struct WaylandSpec {
    socket: std::path::PathBuf,
    forward: Option<String>,
}

#[cfg(unix)]
impl WaylandSpec {
    fn from_environment() -> Self {
        let socket = std::env::var_os("WAYSL_WAYLAND_SOCKET")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/waysl-wayland-0"));
        Self {
            socket,
            forward: std::env::var("WAYSL_FRAME_FORWARD").ok(),
        }
    }
}

/// Run the standard Wayland compositor without any local window system.
///
/// Committed `wl_shm` frames are forwarded to the native Windows renderer over
/// the internal frame bridge, so no X11 server or XWayland is required.
#[cfg(unix)]
fn run_headless_compositor() -> io::Result<()> {
    let spec = WaylandSpec::from_environment();
    let WaylandSpec { socket, forward } = spec;
    let forward = forward.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "WAYSL_FRAME_FORWARD must name the renderer frame bridge address",
        )
    })?;
    let (sender, receiver) = mpsc::channel();
    let forwarder = thread::spawn(move || forward_frames(receiver, forward));
    let result = wayland::run_with_sender(&socket, sender);
    let _ = forwarder.join();
    result
}

#[cfg(unix)]
fn forward_frames(receiver: mpsc::Receiver<wayland::WaylandFrame>, address: String) {
    let mut writer = FrameBridgeWriter::default();
    for frame in receiver {
        if let Err(error) = writer.send(&address, &frame) {
            eprintln!("WaySL frame forward failed on {address}: {error}");
        }
    }
}

#[cfg(unix)]
#[derive(Default)]
struct FrameBridgeWriter {
    stream: Option<std::net::TcpStream>,
}

#[cfg(unix)]
impl FrameBridgeWriter {
    fn send(&mut self, address: &str, frame: &wayland::WaylandFrame) -> io::Result<()> {
        if self.stream.is_none() {
            self.stream = Some(std::net::TcpStream::connect(address)?);
        }
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        if let Err(error) = write_frame(stream, frame) {
            self.stream = None;
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(unix)]
fn write_frame(stream: &mut std::net::TcpStream, frame: &wayland::WaylandFrame) -> io::Result<()> {
    use std::io::Write;

    let title = frame.title.as_bytes();
    let mut header = frame_bridge::frame_header(
        frame.id,
        frame.width,
        frame.height,
        frame.generation,
        title.len(),
        frame.pixels.len(),
    );
    header.extend_from_slice(title);
    stream.write_all(&header)?;
    stream.write_all(&frame.pixels)?;
    stream.flush()
}

#[cfg(unix)]
fn start_wayland_endpoint() -> mpsc::Receiver<wayland::WaylandFrame> {
    let WaylandSpec { socket, .. } = WaylandSpec::from_environment();
    if let Err(error) = std::fs::remove_file(&socket)
        && error.kind() != io::ErrorKind::NotFound
    {
        panic!(
            "cannot remove old Wayland socket {}: {error}",
            socket.display()
        );
    }
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        if let Err(error) = wayland::run_with_sender(&socket, sender) {
            eprintln!("WaySL Wayland endpoint stopped: {error}");
        }
    });
    receiver
}

#[cfg(windows)]
fn start_windows_wayland_endpoint() {
    let address = std::env::var("WAYSL_WAYLAND_TCP").unwrap_or_else(|_| "127.0.0.1:48766".into());
    thread::spawn(move || {
        let server = match wayland_wire::WaylandTcpServer::bind(&address) {
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

fn main() {
    #[cfg(unix)]
    if std::env::var_os("WAYSL_FRAME_FORWARD").is_some() {
        if let Err(error) = run_headless_compositor() {
            eprintln!("WaySL headless compositor stopped: {error}");
            std::process::exit(1);
        }
        return;
    }

    #[cfg(windows)]
    start_windows_wayland_endpoint();

    #[cfg(unix)]
    let wayland_receiver = start_wayland_endpoint();

    let mut app = App::new();
    app.insert_resource(SurfaceRegistry::default());
    #[cfg(unix)]
    app.insert_resource(SurfaceInput::bind_from_environment(wayland_receiver));
    #[cfg(not(unix))]
    app.insert_resource(SurfaceInput::bind_from_environment());
    app.insert_resource(WaypipeProcess::from_environment());
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: None,
        exit_condition: ExitCondition::DontExit,
        close_when_requested: false,
        ..default()
    }));
    app.add_systems(Startup, setup);
    app.add_systems(
        Update,
        (
            receive_surface_frames,
            sync_surface_windows,
            upload_surface_frames,
        )
            .chain(),
    );
    app.run();
}

fn setup(
    mut registry: ResMut<SurfaceRegistry>,
    input: Res<SurfaceInput>,
    mut waypipe: ResMut<WaypipeProcess>,
) {
    registry.upsert(Surface {
        id: SurfaceId(PLACEHOLDER_SURFACE_ID),
        title: "WaySL surface".to_string(),
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        pixels: checkerboard(DEFAULT_WIDTH, DEFAULT_HEIGHT),
        generation: 1,
    });
    info!(address = %input.address, "WaySL frame bridge ready");
    if let Err(error) = waypipe.start() {
        error!(%error, "waypipe was not started; use WAYSL_WAYPIPE_COMMAND to configure it");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SurfaceId(u64);

#[derive(Debug, Clone)]
struct Surface {
    id: SurfaceId,
    title: String,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    generation: u64,
}

#[derive(Resource, Default)]
struct SurfaceRegistry {
    surfaces: HashMap<SurfaceId, Surface>,
    windows: HashMap<SurfaceId, Entity>,
    images: HashMap<SurfaceId, Handle<Image>>,
    generations: HashMap<SurfaceId, u64>,
}

impl SurfaceRegistry {
    fn upsert(&mut self, surface: Surface) {
        self.surfaces.insert(surface.id, surface);
    }
}

#[derive(Resource)]
struct SurfaceInput {
    address: String,
    receiver: Arc<Mutex<mpsc::Receiver<Surface>>>,
}

impl SurfaceInput {
    #[cfg(unix)]
    fn bind_from_environment(wayland_receiver: mpsc::Receiver<wayland::WaylandFrame>) -> Self {
        let address = std::env::var("WAYSL_LISTEN").unwrap_or_else(|_| "127.0.0.1:48765".into());
        let (sender, receiver) = mpsc::channel();
        let wayland_sender = sender.clone();
        thread::spawn(move || {
            for frame in wayland_receiver {
                if wayland_sender
                    .send(Surface {
                        id: SurfaceId(frame.id),
                        title: frame.title,
                        width: frame.width,
                        height: frame.height,
                        pixels: frame.pixels,
                        generation: frame.generation,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        let frame_receiver = frame_bridge::start_listener(address.clone());
        thread::spawn(move || {
            for frame in frame_receiver {
                let surface = Surface {
                    id: SurfaceId(frame.id),
                    title: frame.title,
                    width: frame.width,
                    height: frame.height,
                    pixels: frame.pixels,
                    generation: frame.generation,
                };
                if sender.send(surface).is_err() {
                    break;
                }
            }
        });
        Self {
            address,
            receiver: Arc::new(Mutex::new(receiver)),
        }
    }

    #[cfg(not(unix))]
    fn bind_from_environment() -> Self {
        let address = std::env::var("WAYSL_LISTEN").unwrap_or_else(|_| "127.0.0.1:48765".into());
        let (sender, receiver) = mpsc::channel();
        let frame_receiver = frame_bridge::start_listener(address.clone());
        thread::spawn(move || {
            for frame in frame_receiver {
                let surface = Surface {
                    id: SurfaceId(frame.id),
                    title: frame.title,
                    width: frame.width,
                    height: frame.height,
                    pixels: frame.pixels,
                    generation: frame.generation,
                };
                if sender.send(surface).is_err() {
                    break;
                }
            }
        });
        Self {
            address,
            receiver: Arc::new(Mutex::new(receiver)),
        }
    }
}

fn receive_surface_frames(mut registry: ResMut<SurfaceRegistry>, input: Res<SurfaceInput>) {
    let receiver = input
        .receiver
        .lock()
        .expect("WaySL frame receiver poisoned");
    while let Ok(surface) = receiver.try_recv() {
        info!(
            id = surface.id.0,
            title = %surface.title,
            width = surface.width,
            height = surface.height,
            bytes = surface.pixels.len(),
            "received frame"
        );
        registry.upsert(surface);
    }
}

#[derive(Component)]
struct SurfaceWindow;

fn sync_surface_windows(
    mut commands: Commands,
    mut registry: ResMut<SurfaceRegistry>,
    mut images: ResMut<Assets<Image>>,
    mut windows: Query<&mut Window>,
) {
    let surfaces: Vec<Surface> = registry.surfaces.values().cloned().collect();
    for surface in surfaces {
        if let Some(window_entity) = registry.windows.get(&surface.id).copied() {
            if let Ok(mut window) = windows.get_mut(window_entity) {
                window.title = surface.title.clone();
                window
                    .resolution
                    .set(surface.width as f32, surface.height as f32);
            }
            continue;
        }

        let image = images.add(Image::new(
            Extent3d {
                width: surface.width,
                height: surface.height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            surface.pixels.clone(),
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        ));
        let window_entity = commands
            .spawn((
                Window {
                    title: surface.title.clone(),
                    resolution: WindowResolution::new(surface.width, surface.height),
                    ..default()
                },
                SurfaceWindow,
            ))
            .id();
        // Keep this identical to the verified sprite-probe configuration:
        // a plain `Camera2d` targeting the window, and one sprite sized to the
        // surface. Custom projections, render layers, and non-zero transforms
        // each failed to paint on the software D3D12 adapter.
        let width = surface.width as f32;
        let height = surface.height as f32;
        commands.spawn((
            Camera2d,
            RenderTarget::Window(WindowRef::Entity(window_entity)),
        ));
        commands.spawn(Sprite {
            image: image.clone(),
            custom_size: Some(Vec2::new(width, height)),
            ..default()
        });
        registry.windows.insert(surface.id, window_entity);
        registry.images.insert(surface.id, image);
        registry.generations.insert(surface.id, surface.generation);
    }
}

fn upload_surface_frames(mut registry: ResMut<SurfaceRegistry>, mut images: ResMut<Assets<Image>>) {
    let surfaces: Vec<Surface> = registry.surfaces.values().cloned().collect();
    for surface in surfaces {
        let Some(last_generation) = registry.generations.get(&surface.id).copied() else {
            continue;
        };
        if last_generation == surface.generation {
            continue;
        }
        let Some(image_handle) = registry.images.get(&surface.id).cloned() else {
            continue;
        };
        let Some(image) = images.get_mut(&image_handle) else {
            continue;
        };
        image.data = Some(surface.pixels.clone());
        registry.generations.insert(surface.id, surface.generation);
    }
}

fn checkerboard(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let tile = ((x / 32) + (y / 32)) % 2 == 0;
            let value = if tile { 0x38 } else { 0x58 };
            pixels.extend_from_slice(&[value, value, value + 0x20, 0xff]);
        }
    }
    pixels
}

#[derive(Resource)]
struct WaypipeProcess {
    command_line: Option<String>,
    child: Option<Child>,
}

impl WaypipeProcess {
    fn from_environment() -> Self {
        Self {
            command_line: std::env::var("WAYSL_WAYPIPE_COMMAND").ok(),
            child: None,
        }
    }

    fn start(&mut self) -> std::io::Result<()> {
        let Some(command_line) = self.command_line.as_deref() else {
            return Ok(());
        };
        let mut parts = command_line.split_whitespace();
        let Some(program) = parts.next() else {
            return Ok(());
        };
        self.child = Some(Command::new(program).args(parts).spawn()?);
        Ok(())
    }
}

impl Drop for WaypipeProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_upsert_by_id() {
        let mut registry = SurfaceRegistry::default();
        registry.upsert(Surface {
            id: SurfaceId(3),
            title: "first".to_string(),
            width: 8,
            height: 4,
            pixels: vec![0; 8 * 4 * 4],
            generation: 1,
        });
        registry.upsert(Surface {
            id: SurfaceId(3),
            title: "second".to_string(),
            width: 8,
            height: 4,
            pixels: vec![0; 8 * 4 * 4],
            generation: 2,
        });
        assert_eq!(registry.surfaces.len(), 1);
        assert_eq!(registry.surfaces[&SurfaceId(3)].title, "second");
    }
}
