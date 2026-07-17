//! The shared terrain raycast / cursor-hit (design doc §6): cursor → terrain
//! world point, computed once per frame (`EditorSet::Pick`) and read by every
//! tool, so a placed prop lands exactly where the brush would paint.

use bevy::{prelude::*, window::PrimaryWindow};
use bevy_wilderness::Clipmap;

use crate::terrain::EditableTerrain;

/// How far the cursor pick marches before giving up, in meters.
const MAX_PICK_DISTANCE: f32 = 50_000.0;

/// Where the cursor meets the terrain this frame, if it does. Updated in
/// `EditorSet::Pick`; every tool (built-in or host) reads this same pick.
#[derive(Resource, Default, PartialEq, Debug)]
pub struct TerrainCursor(pub Option<TerrainHit>);

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
    mut cursor: ResMut<TerrainCursor>,
) {
    let hit = windows
        .single()
        .ok()
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
