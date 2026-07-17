//! The built-in mask paint tool (D4): paint a 0..1 weight mask with the shared
//! brush while LMB is held (Shift+LMB erases). The brush's smoothstep falloff
//! feathers the edge by construction — no hard rectangular seam — and masked
//! ops (sculpt now, erosion in Phase 5) weight their deltas by it so edited
//! ground blends into untouched ground. Footprints wrap on looping terrain
//! like every other edit.

use bevy::prelude::*;

use crate::cursor::TerrainCursor;
use crate::settings::BrushSettings;
use crate::terrain::EditableTerrain;
use crate::undo::{UndoBuffer, UndoHistory};

/// How fast a held brush saturates the mask, in full-range units per second at
/// the brush center.
const PAINT_RATE: f32 = 2.5;

/// Paint (or Shift-erase) the mask under the cursor while LMB is held. Runs in
/// `EditorSet::Tools`, gated on the mask tool being active.
// Bevy systems legitimately take one param per resource they touch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_mask_paint(
    time: Res<Time>,
    buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    cursor: Res<TerrainCursor>,
    brush: Res<BrushSettings>,
    mut history: ResMut<UndoHistory>,
    mut terrains: Query<&mut EditableTerrain>,
    mut painting: Local<bool>,
) {
    if !buttons.pressed(MouseButton::Left) {
        if *painting {
            history.seal();
            *painting = false;
        }
        return;
    }
    let Some(hit) = cursor.0 else {
        return;
    };
    let Ok(mut terrain) = terrains.get_mut(hit.terrain) else {
        return;
    };
    let erase = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    if !*painting {
        history.begin(hit.terrain, if erase { "Mask Erase" } else { "Mask Paint" });
        *painting = true;
    }
    paint_mask_at(
        &mut terrain,
        hit.texel,
        brush.radius,
        erase,
        time.delta_secs(),
        &mut history,
    );
}

/// One brush application onto the mask at fractional texel coordinates
/// `center`. Same footprint conventions as sculpt: continuous falloff in
/// unwrapped space, writes resolved through `wrap_texel`.
pub(crate) fn paint_mask_at(
    terrain: &mut EditableTerrain,
    center: Vec2,
    radius_m: f32,
    erase: bool,
    dt: f32,
    history: &mut UndoHistory,
) {
    let radius_texels = radius_m / terrain.field.texel_size();
    if radius_texels <= 0.0 {
        return;
    }
    let min = (center - radius_texels).floor().as_ivec2();
    let max = (center + radius_texels).ceil().as_ivec2() + IVec2::ONE;

    // Snapshot first-touch undo tiles *before* mutating (D8).
    for rect in terrain.field.wrap_rect(min, max) {
        history.capture(terrain, UndoBuffer::Mask, rect);
    }

    let direction = if erase { -1.0 } else { 1.0 };
    for y in min.y..max.y {
        for x in min.x..max.x {
            let d = Vec2::new(x as f32, y as f32).distance(center) / radius_texels;
            if d >= 1.0 {
                continue;
            }
            let Some((tx, ty)) = terrain.field.wrap_texel(x as i64, y as i64) else {
                continue;
            };
            // Smoothstep falloff = the feathered edge (D4).
            let t = 1.0 - d;
            let falloff = t * t * (3.0 - 2.0 * t);
            let value = terrain.mask_weight(x as i64, y as i64);
            terrain.set_mask(tx, ty, value + direction * PAINT_RATE * falloff * dt);
        }
    }

    for rect in terrain.field.wrap_rect(min, max) {
        terrain.mark_mask_dirty(rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::TerrainField;

    #[test]
    fn paint_feathers_and_erase_removes() {
        let field = TerrainField::flat(64, 64, 1.0, -100.0, 100.0, false, 0.0);
        let mut terrain = EditableTerrain::new(field);
        let mut history = UndoHistory::default();

        // Paint long enough to saturate the center.
        paint_mask_at(
            &mut terrain,
            Vec2::new(32.0, 32.0),
            8.0,
            false,
            1.0,
            &mut history,
        );
        assert!(terrain.mask_active());
        let center = terrain.mask_weight(32, 32);
        let mid = terrain.mask_weight(37, 32);
        let outside = terrain.mask_weight(41, 32);
        assert_eq!(center, 1.0, "center saturates");
        assert!(mid > 0.0 && mid < center, "edge is feathered: {mid}");
        assert_eq!(outside, 0.0, "outside the radius untouched");

        // Erase it back down.
        paint_mask_at(
            &mut terrain,
            Vec2::new(32.0, 32.0),
            8.0,
            true,
            2.0,
            &mut history,
        );
        assert_eq!(terrain.mask_weight(32, 32), 0.0);
        assert!(!terrain.mask_active());
    }

    #[test]
    fn sculpt_is_confined_by_the_mask() {
        use crate::settings::{BrushSettings, SculptMode};

        let brush = BrushSettings {
            mode: SculptMode::Raise,
            radius: 20.0,
            strength: 10.0,
        };
        let mut history = UndoHistory::default();
        let stroke = |terrain: &mut EditableTerrain, history: &mut UndoHistory| {
            crate::sculpt::sculpt_at(terrain, Vec2::new(26.0, 32.0), &brush, 1.0, 0.0, history);
        };

        // Control: the same stroke with no mask painted.
        let field = TerrainField::flat(64, 64, 1.0, -100.0, 100.0, false, 0.0);
        let mut control = EditableTerrain::new(field);
        stroke(&mut control, &mut history);

        // Masked: a small patch around (20, 32) only. Painted briefly (0.5 s)
        // so the center saturates to 1.0 but the falloff band stays partial —
        // a long hold would drive the whole footprint to 1.0.
        let field = TerrainField::flat(64, 64, 1.0, -100.0, 100.0, false, 0.0);
        let mut terrain = EditableTerrain::new(field);
        paint_mask_at(
            &mut terrain,
            Vec2::new(20.0, 32.0),
            6.0,
            false,
            0.5,
            &mut history,
        );
        assert_eq!(terrain.mask_weight(20, 32), 1.0, "center saturates");
        stroke(&mut terrain, &mut history);

        // Fully masked ground raises exactly as if unconfined.
        assert_eq!(terrain.field.get(20, 32), control.field.get(20, 32));
        // Unmasked ground inside the brush stays untouched.
        assert!(control.field.get(32, 32) > 0.0);
        assert_eq!(terrain.field.get(32, 32), 0.0);
        // The feathered mask edge blends between the two.
        let feathered = terrain.field.get(24, 32);
        let unconfined = control.field.get(24, 32);
        assert!(
            feathered > 0.0 && feathered < unconfined,
            "feathered edge blends: {feathered} vs {unconfined}"
        );
    }

    #[test]
    fn mask_undo_round_trips() {
        use bevy::ecs::system::SystemState;

        let field = TerrainField::flat(64, 64, 1.0, -100.0, 100.0, false, 0.0);
        let mut world = World::new();
        let entity = world.spawn(EditableTerrain::new(field)).id();
        let mut history = UndoHistory::default();

        history.begin(entity, "Mask Paint");
        {
            let mut terrain = world.get_mut::<EditableTerrain>(entity).unwrap();
            paint_mask_at(
                &mut terrain,
                Vec2::new(32.0, 32.0),
                8.0,
                false,
                1.0,
                &mut history,
            );
        }
        history.seal();

        let mut state: SystemState<Query<&mut EditableTerrain>> = SystemState::new(&mut world);
        history.undo(&mut state.get_mut(&mut world).unwrap());
        let terrain = world.get::<EditableTerrain>(entity).unwrap();
        assert!(!terrain.mask_active(), "undo clears the painted mask");

        history.redo(&mut state.get_mut(&mut world).unwrap());
        let terrain = world.get::<EditableTerrain>(entity).unwrap();
        assert_eq!(terrain.mask_weight(32, 32), 1.0, "redo repaints");
    }
}
