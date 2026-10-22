//! Renderer set-up systems: placeholder surface and optional Waypipe.

use bevy::prelude::*;

use crate::config::{PLACEHOLDER_HEIGHT, PLACEHOLDER_SURFACE_ID, PLACEHOLDER_WIDTH};
use crate::renderer::surfaces::{Surface, SurfaceId, SurfaceInput, SurfaceRegistry};
use crate::renderer::waypipe::WaypipeProcess;

/// Logs the bridge address, starts Waypipe if configured, and registers the
/// placeholder surface so a window appears before any client connects.
pub fn startup(
    mut registry: ResMut<SurfaceRegistry>,
    input: Res<SurfaceInput>,
    mut waypipe: ResMut<WaypipeProcess>,
) {
    info!(address = %input.address, "WaySL frame bridge ready");
    registry.upsert(placeholder());
    if let Err(error) = waypipe.start() {
        error!(%error, "waypipe was not started; check WAYSL_WAYPIPE_COMMAND");
    }
}

/// The checkerboard surface shown until a Wayland client connects.
fn placeholder() -> Surface {
    Surface {
        id: SurfaceId(PLACEHOLDER_SURFACE_ID),
        title: "WaySL surface".to_string(),
        width: PLACEHOLDER_WIDTH,
        height: PLACEHOLDER_HEIGHT,
        pixels: checkerboard(PLACEHOLDER_WIDTH, PLACEHOLDER_HEIGHT),
        generation: 1,
    }
}

/// A quiet two-tone checkerboard.
pub fn checkerboard(width: u32, height: u32) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkerboard_fills_rgba() {
        let pixels = checkerboard(4, 4);
        assert_eq!(pixels.len(), 4 * 4 * 4);
        assert!(pixels.as_chunks::<4>().0.iter().all(|px| px[3] == 0xff));
    }

    #[test]
    fn checkerboard_has_two_tones() {
        let pixels = checkerboard(64, 64);
        let first = pixels[0];
        assert!(pixels.as_chunks::<4>().0.iter().any(|px| px[0] != first));
    }

    #[test]
    fn placeholder_matches_configured_size() {
        let surface = placeholder();
        assert_eq!(surface.width, PLACEHOLDER_WIDTH);
        assert_eq!(surface.height, PLACEHOLDER_HEIGHT);
        assert_eq!(
            surface.pixels.len(),
            (PLACEHOLDER_WIDTH * PLACEHOLDER_HEIGHT * 4) as usize
        );
    }
}
