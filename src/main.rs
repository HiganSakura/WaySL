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
#[cfg(test)]
use std::io::Write;
use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

const DEFAULT_WIDTH: u32 = 960;
const DEFAULT_HEIGHT: u32 = 540;

#[cfg(unix)]
fn start_wayland_endpoint() -> mpsc::Receiver<wayland::WaylandFrame> {
    let socket = std::env::var_os("WAYSL_WAYLAND_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/waysl-wayland-0"));
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
        id: SurfaceId(1),
        title: "WaySL surface".to_string(),
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        pixels: checkerboard(DEFAULT_WIDTH, DEFAULT_HEIGHT),
        generation: 1,
    });
    info!(address = %input.address, "WaySL frame listener ready");
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

        let listener = TcpListener::bind(&address).unwrap_or_else(|error| {
            panic!("cannot bind WAYSL_LISTEN={address}: {error}");
        });
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if let Err(error) = receive_stream(stream, &sender) {
                            eprintln!("WaySL frame stream closed: {error}");
                        }
                    }
                    Err(error) => eprintln!("WaySL listener failed: {error}"),
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
        let listener = TcpListener::bind(&address).unwrap_or_else(|error| {
            panic!("cannot bind WAYSL_LISTEN={address}: {error}");
        });
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if let Err(error) = receive_stream(stream, &sender) {
                            eprintln!("WaySL frame stream closed: {error}");
                        }
                    }
                    Err(error) => eprintln!("WaySL listener failed: {error}"),
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
        registry.upsert(surface);
    }
}

fn receive_stream(mut stream: TcpStream, sender: &mpsc::Sender<Surface>) -> io::Result<()> {
    const MAGIC: &[u8; 4] = b"WSL1";
    const MAX_TITLE: u32 = 4096;
    const MAX_PIXELS: u32 = 256 * 1024 * 1024;
    loop {
        let mut magic = [0; 4];
        stream.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid WaySL frame magic",
            ));
        }
        let width = read_u32(&mut stream)?;
        let height = read_u32(&mut stream)?;
        let id = read_u64(&mut stream)?;
        let generation = read_u64(&mut stream)?;
        let title_len = read_u32(&mut stream)?;
        let pixel_len = read_u32(&mut stream)?;
        if title_len > MAX_TITLE || pixel_len > MAX_PIXELS || width == 0 || height == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid WaySL frame dimensions",
            ));
        }
        let mut title_bytes = vec![0; title_len as usize];
        stream.read_exact(&mut title_bytes)?;
        let mut pixels = vec![0; pixel_len as usize];
        stream.read_exact(&mut pixels)?;
        sender
            .send(Surface {
                id: SurfaceId(id),
                title: String::from_utf8(title_bytes)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid title"))?,
                width,
                height,
                pixels,
                generation,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "WaySL receiver stopped"))?;
    }
}

fn read_u32(stream: &mut TcpStream) -> io::Result<u32> {
    let mut bytes = [0; 4];
    stream.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(stream: &mut TcpStream) -> io::Result<u64> {
    let mut bytes = [0; 8];
    stream.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[derive(Component)]
struct SurfaceWindow;

#[derive(Component)]
struct SurfaceSprite;

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

        let image = images.add(Image::new_fill(
            Extent3d {
                width: surface.width,
                height: surface.height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            &surface.pixels,
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
        commands.spawn((
            Camera2d,
            RenderTarget::Window(WindowRef::Entity(window_entity)),
        ));
        commands.spawn((
            Sprite {
                image: image.clone(),
                custom_size: Some(Vec2::new(surface.width as f32, surface.height as f32)),
                ..default()
            },
            SurfaceSprite,
        ));
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
    fn receives_one_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let sender = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect");
            let title = b"ok";
            let pixels = [1, 2, 3, 4];
            let mut frame = Vec::new();
            frame.extend_from_slice(b"WSL1");
            frame.extend_from_slice(&2u32.to_le_bytes());
            frame.extend_from_slice(&1u32.to_le_bytes());
            frame.extend_from_slice(&7u64.to_le_bytes());
            frame.extend_from_slice(&1u64.to_le_bytes());
            frame.extend_from_slice(&(title.len() as u32).to_le_bytes());
            frame.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
            frame.extend_from_slice(title);
            frame.extend_from_slice(&pixels);
            stream.write_all(&frame).expect("frame");
        });
        let (stream, _) = listener.accept().expect("accept");
        let (frame_sender, frame_receiver) = mpsc::channel();
        let _ = receive_stream(stream, &frame_sender);
        let frame = frame_receiver.recv().expect("frame receiver");
        assert_eq!(frame.id, SurfaceId(7));
        assert_eq!(frame.title, "ok");
        assert_eq!(frame.pixels, pixels_for_test());
        sender.join().expect("sender");
    }

    fn pixels_for_test() -> Vec<u8> {
        vec![1, 2, 3, 4]
    }
}
