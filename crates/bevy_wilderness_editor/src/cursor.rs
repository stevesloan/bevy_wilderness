//! The shared terrain raycast / cursor-hit (design doc §6): cursor → terrain
//! world point, computed once per frame (`EditorSet::Pick`) and read by every
//! tool, so a placed prop lands exactly where the brush would paint — plus
//! the gizmo indicator that visualizes it (brush ring + pick dot).

use bevy::{prelude::*, window::PrimaryWindow};
use bevy_wilderness::Clipmap;

use crate::settings::BrushSettings;
use crate::terrain::EditableTerrain;
use crate::tools::{ActiveTool, ToolId};

/// How far the cursor pick marches before giving up, in meters.
const MAX_PICK_DISTANCE: f32 = 50_000.0;

/// Where the cursor meets the terrain this frame, if it does. Updated in
/// `EditorSet::Pick`; every tool (built-in or host) reads this same pick.
#[derive(Resource, Default, PartialEq, Debug)]
pub struct TerrainCursor(pub Option<TerrainHit>);

/// Whether a UI layer owns the pointer this frame (design doc §6: shared input
/// focus). A UI writes this before `EditorSet::Pick` — the default egui UI does
/// it from `wants_pointer_input` — and while `true` the shared pick reports no
/// hit, so no tool paints or places through a panel.
#[derive(Resource, Default, PartialEq, Debug)]
pub struct PointerBlocked(pub bool);

/// One cursor-terrain intersection.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TerrainHit {
    /// The terrain (clipmap) entity that was hit.
    pub terrain: Entity,
    /// World-space surface point under the cursor.
    pub position: Vec3,
    /// The same point in fractional texel coordinates — what brush footprints
    /// work in.
    pub texel: Vec2,
}

/// Compute [`TerrainCursor`] from the primary window's cursor through each
/// editable terrain's target camera (the shared editor camera, design doc §6).
/// `set_if_neq` so an unmoved cursor doesn't dirty change detection.
pub(crate) fn update_terrain_cursor(
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform)>,
    terrains: Query<(Entity, &EditableTerrain, &Clipmap)>,
    blocked: Res<PointerBlocked>,
    mut cursor: ResMut<TerrainCursor>,
) {
    let hit = (!blocked.0)
        .then(|| windows.single().ok())
        .flatten()
        .and_then(|window| window.cursor_position())
        .and_then(|cursor_pos| {
            terrains.iter().find_map(|(entity, terrain, clipmap)| {
                let (camera, camera_transform) = cameras.get(clipmap.target).ok()?;
                let ray = camera
                    .viewport_to_world(camera_transform, cursor_pos)
                    .ok()?;
                let position =
                    terrain
                        .field
                        .raycast(ray.origin, *ray.direction, MAX_PICK_DISTANCE)?;
                Some(TerrainHit {
                    terrain: entity,
                    position,
                    texel: terrain.field.world_to_texel(position.xz()),
                })
            })
        });
    cursor.set_if_neq(TerrainCursor(hit));
}

/// The cursor indicator, drawn by the core so every host gets it: a
/// brush-radius ring for the tools that brush, and a center dot marking the
/// shared pick. `enabled: false` hides both (e.g. for a host drawing its
/// own).
#[derive(Resource, Clone, Copy)]
pub struct BrushRing {
    pub enabled: bool,
}

impl Default for BrushRing {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Ring + dot for sculpt/mask (they brush with the radius); dot alone for
/// erode (it runs over the mask, not a radius — a ring would mislead, same
/// reason the UI hides its radius slider). Stamp previews itself, and host
/// tools draw their own indicators.
pub(crate) fn draw_brush_ring(
    ring: Res<BrushRing>,
    cursor: Res<TerrainCursor>,
    brush: Res<BrushSettings>,
    active: Res<ActiveTool>,
    mut gizmos: Gizmos,
) {
    if !ring.enabled {
        return;
    }
    let with_ring = match active.0 {
        Some(ToolId::SCULPT) | Some(ToolId::MASK) => true,
        Some(ToolId::ERODE) => false,
        _ => return,
    };
    let Some(hit) = &cursor.0 else {
        return;
    };
    if with_ring {
        let up = Isometry3d::new(
            hit.position + Vec3::Y * 0.5,
            Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        );
        gizmos.circle(up, brush.radius, Color::srgb(1.0, 0.4, 0.1));
    }
    gizmos.sphere(
        Isometry3d::from_translation(hit.position),
        2.0,
        Color::srgb(1.0, 0.9, 0.2),
    );
}
