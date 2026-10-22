//! Surface tracking and the Bevy window/сamera/sprite systems.

use bevy::asset::RenderAssetUsages;
use bevy::camera::RenderTarget;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::window::{WindowRef, WindowResolution};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use crate::frame_bridge;

/// Stable identifier for one forwarded surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceId(pub u64);

/// The newest pixels known for a surface.
#[derive(Debug, Clone)]
pub struct Surface {
    /// Surface identifier.
    pub id: SurfaceId,
    /// Window title reported by the Wayland client.
    pub title: String,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Tightly packed RGBA8 pixels.
    pub pixels: Vec<u8>,
    /// Monotonic frame counter.
    pub generation: u64,
}

/// Every surface plus the Bevy entities rendering it.
#[derive(Resource, Default)]
pub struct SurfaceRegistry {
    surfaces: HashMap<SurfaceId, Surface>,
    windows: HashMap<SurfaceId, Entity>,
    images: HashMap<SurfaceId, Handle<Image>>,
    generations: HashMap<SurfaceId, u64>,
}

impl SurfaceRegistry {
    /// Inserts or replaces a surface, keeping its window if it already exists.
    pub fn upsert(&mut self, surface: Surface) {
        self.surfaces.insert(surface.id, surface);
    }

    /// Number of tracked surfaces.
    pub fn len(&self) -> usize {
        self.surfaces.len()
    }

    /// Whether no surfaces are tracked.
    pub fn is_empty(&self) -> bool {
        self.surfaces.is_empty()
    }

    /// Looks up a surface.
    pub fn get(&self, id: SurfaceId) -> Option<&Surface> {
        self.surfaces.get(&id)
    }
}

/// Frames arriving from the bridge, shared with the Bevy system.
#[derive(Resource)]
pub struct SurfaceInput {
    /// Address the bridge is listening on, for logging.
    pub address: String,
    receiver: Arc<Mutex<mpsc::Receiver<Surface>>>,
}

impl SurfaceInput {
    /// Starts the bridge listener and adapts its frames to surfaces.
    pub fn bind(address: &str) -> Self {
        let (sender, receiver) = mpsc::channel();
        let frames = frame_bridge::start_listener(address.to_string());
        thread::spawn(move || {
            for frame in frames {
                if sender.send(super::surface_from_frame(frame)).is_err() {
                    break;
                }
            }
        });
        Self {
            address: address.to_string(),
            receiver: Arc::new(Mutex::new(receiver)),
        }
    }
}

/// Moves every pending frame into the registry.
pub fn receive_frames(mut registry: ResMut<SurfaceRegistry>, input: Res<SurfaceInput>) {
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

/// Creates windows for new surfaces and keeps titles and sizes current.
///
/// The camera/sprite pair mirrors a verified minimal Bevy setup: a plain
/// `Camera2d` targeting the window plus one `Sprite` sized to the surface.
/// Custom projections, render layers, and non-zero transforms did not paint on
/// the software D3D12 adapter used for testing.
pub fn sync_windows(
    mut commands: Commands,
    mut registry: ResMut<SurfaceRegistry>,
    mut images: ResMut<Assets<Image>>,
    mut windows: Query<&mut Window>,
) {
    let surfaces: Vec<Surface> = registry.surfaces.values().cloned().collect();
    for surface in surfaces {
        if let Some(window_entity) = registry.windows.get(&surface.id).copied() {
            if let Ok(mut window) = windows.get_mut(window_entity) {
                window.title.clone_from(&surface.title);
                window
                    .resolution
                    .set(surface.width as f32, surface.height as f32);
            }
            continue;
        }

        let image = images.add(image_from(&surface));
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
        commands.spawn(Sprite {
            image: image.clone(),
            custom_size: Some(Vec2::new(surface.width as f32, surface.height as f32)),
            ..default()
        });
        registry.windows.insert(surface.id, window_entity);
        registry.images.insert(surface.id, image);
        registry.generations.insert(surface.id, surface.generation);
    }
}

/// Uploads pixels for surfaces whose generation changed.
pub fn upload_frames(mut registry: ResMut<SurfaceRegistry>, mut images: ResMut<Assets<Image>>) {
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

/// Marks a window created for a forwarded surface.
#[derive(Component)]
struct SurfaceWindow;

/// Builds a Bevy image holding exactly the surface pixels.
fn image_from(surface: &Surface) -> Image {
    Image::new(
        Extent3d {
            width: surface.width,
            height: surface.height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        surface.pixels.clone(),
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(id: u64, title: &str, generation: u64) -> Surface {
        Surface {
            id: SurfaceId(id),
            title: title.to_string(),
            width: 2,
            height: 2,
            pixels: vec![0; 2 * 2 * 4],
            generation,
        }
    }

    #[test]
    fn upsert_keeps_one_entry_per_id() {
        let mut registry = SurfaceRegistry::default();
        registry.upsert(surface(3, "first", 1));
        registry.upsert(surface(3, "second", 2));
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get(SurfaceId(3)).expect("surface").title, "second");
    }

    #[test]
    fn upsert_tracks_distinct_ids() {
        let mut registry = SurfaceRegistry::default();
        registry.upsert(surface(2, "client", 1));
        registry.upsert(surface(1, "placeholder", 1));
        assert_eq!(registry.len(), 2);
        assert!(!registry.is_empty());
    }

    #[test]
    fn image_holds_exact_pixels() {
        let image = image_from(&surface(1, "s", 1));
        assert_eq!(image.data.as_deref().map(<[u8]>::len), Some(16));
        assert_eq!(image.texture_descriptor.size.width, 2);
        assert_eq!(image.texture_descriptor.size.height, 2);
    }
}
