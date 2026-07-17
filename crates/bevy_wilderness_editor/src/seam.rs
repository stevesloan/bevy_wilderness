//! The tile-boundary overlay (D6). On looping terrain there is nothing to
//! stitch — every edit wraps, so the seam is invisible *by design* (position 0
//! and position *width* are the same terrain). Which is exactly why an author
//! needs a visibility aid: this draws a gizmo ring around the tile the camera
//! is over, hugging the terrain surface, so you can see where the map repeats
//! while sculpting across it.

use bevy::prelude::*;
use bevy_wilderness::Clipmap;

use crate::terrain::EditableTerrain;

/// Meters the boundary line floats above the surface (keeps it visible on
/// bumpy ground without z-fighting).
const LINE_LIFT: f32 = 1.5;

/// Texels between samples along an edge — the line follows terrain relief
/// without pushing thousands of gizmo segments per frame.
const SAMPLE_STEP_TEXELS: u32 = 4;

/// Toggleable tile-boundary visualization (D6) for looping terrains. A UI (or
/// a keybind in the example) flips `enabled`; finite terrains draw nothing.
#[derive(Resource, Clone, Copy)]
pub struct SeamOverlay {
    pub enabled: bool,
}

impl Default for SeamOverlay {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Draw the boundary ring of the tile instance under each terrain's target
/// camera. Neighboring instances share edges, so one ring marks every nearby
/// seam; the ring follows the camera from repeat to repeat.
pub(crate) fn draw_seam_overlay(
    overlay: Res<SeamOverlay>,
    terrains: Query<(&EditableTerrain, &Clipmap)>,
    cameras: Query<&GlobalTransform>,
    mut gizmos: Gizmos,
) {
    if !overlay.enabled {
        return;
    }
    for (terrain, clipmap) in &terrains {
        if !terrain.field.looping() {
            continue;
        }
        let Ok(camera) = cameras.get(clipmap.target) else {
            continue;
        };
        let field = &terrain.field;
        let size = field.dimensions().as_vec2() * field.texel_size();
        let half = field.half_extent();
        // Min corner of the tile instance containing the camera (the base
        // tile spans [-half, half)).
        let tile = ((camera.translation().xz() + half) / size).floor();
        let origin = tile * size - half;

        let color = Color::srgb(0.25, 0.8, 1.0);
        let step = SAMPLE_STEP_TEXELS as f32 * field.texel_size();
        let samples = (size / step).as_uvec2().max(UVec2::ONE);
        let surface = |xz: Vec2| Vec3::new(xz.x, field.height_at(xz) + LINE_LIFT, xz.y);
        // The four edges as line strips hugging the surface. Opposite edges
        // overlap the next repeat's — that's the point: both sides of a seam
        // are marked by one ring.
        for (corner, dir, count) in [
            (origin, Vec2::X, samples.x),
            (origin + Vec2::new(size.x, 0.0), Vec2::Y, samples.y),
            (origin + size, -Vec2::X, samples.x),
            (origin + Vec2::new(0.0, size.y), -Vec2::Y, samples.y),
        ] {
            gizmos.linestrip(
                (0..=count).map(|i| surface(corner + dir * (i as f32 * step))),
                color,
            );
        }
    }
}
