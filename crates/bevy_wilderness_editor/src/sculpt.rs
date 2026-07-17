//! The built-in sculpt tool (D2): raise / lower / smooth / flatten the f32
//! field under the brush, with radial falloff, while the left mouse button is
//! held. Edits mark dirty regions; the sync in `EditorSet::Apply` re-quantizes
//! them into the display heightmap the same frame, so geometry deforms live.
//! On looping terrain the brush footprint wraps modulo the heightmap.

use bevy::prelude::*;

use crate::cursor::TerrainCursor;
use crate::settings::{BrushSettings, SculptMode};
use crate::terrain::EditableTerrain;
use crate::undo::{UndoBuffer, UndoHistory};

/// Per-stroke state: the flatten target is the terrain height under the cursor
/// when the stroke starts, so a whole drag levels toward one plane.
#[derive(Default)]
pub(crate) struct StrokeState {
    flatten_target: Option<f32>,
}

/// Apply the brush to the terrain under the cursor while LMB is held. Runs in
/// `EditorSet::Tools`, gated on the sculpt tool being active. Each stroke is
/// one undo entry: begun on press, captured as it touches tiles, sealed on
/// release.
pub(crate) fn apply_sculpt(
    time: Res<Time>,
    buttons: Res<ButtonInput<MouseButton>>,
    cursor: Res<TerrainCursor>,
    brush: Res<BrushSettings>,
    mut history: ResMut<UndoHistory>,
    mut terrains: Query<&mut EditableTerrain>,
    mut stroke: Local<StrokeState>,
) {
    if !buttons.pressed(MouseButton::Left) {
        stroke.flatten_target = None;
        history.seal();
        return;
    }
    let Some(hit) = cursor.0 else {
        return;
    };
    let Ok(mut terrain) = terrains.get_mut(hit.terrain) else {
        return;
    };
    if stroke.flatten_target.is_none() {
        // First frame of the stroke (or first frame back over terrain).
        history.begin(hit.terrain, format!("Sculpt ({:?})", brush.mode));
    }
    let flatten_target = *stroke
        .flatten_target
        .get_or_insert_with(|| terrain.field.height_at(hit.position.xz()));
    sculpt_at(
        &mut terrain,
        hit.texel,
        &brush,
        time.delta_secs(),
        flatten_target,
        &mut history,
    );
}

/// One brush application at fractional texel coordinates `center`. Iterates
/// the unwrapped footprint so falloff distances are computed in continuous
/// space, resolving each write through `wrap_texel` (wraps when looping,
/// drops the overhang when finite).
pub(crate) fn sculpt_at(
    terrain: &mut EditableTerrain,
    center: Vec2,
    brush: &BrushSettings,
    dt: f32,
    flatten_target: f32,
    history: &mut UndoHistory,
) {
    let radius_texels = brush.radius / terrain.field.texel_size();
    if radius_texels <= 0.0 {
        return;
    }
    let min = (center - radius_texels).floor().as_ivec2();
    let max = (center + radius_texels).ceil().as_ivec2() + IVec2::ONE;
    let (encode_min, encode_max) = terrain.field.min_max();

    // Snapshot first-touch undo tiles *before* mutating (D8).
    for rect in terrain.field.wrap_rect(min, max) {
        history.capture(terrain, UndoBuffer::Height, rect);
    }

    // A painted mask confines the op (D4): deltas weight by the feathered mask
    // value, so edits blend softly into unmasked ground. No mask = no
    // confinement.
    let masked = terrain.mask_active();

    for y in min.y..max.y {
        for x in min.x..max.x {
            // Texel centers sample at integer coordinates (shader convention).
            let d = Vec2::new(x as f32, y as f32).distance(center) / radius_texels;
            if d >= 1.0 {
                continue;
            }
            let Some((tx, ty)) = terrain.field.wrap_texel(x as i64, y as i64) else {
                continue;
            };
            let mask_weight = if masked {
                terrain.mask_weight(x as i64, y as i64)
            } else {
                1.0
            };
            if mask_weight <= 0.0 {
                continue;
            }
            // Smoothstep falloff: full strength at the center, eased to zero
            // at the rim (no hard brush edge).
            let t = 1.0 - d;
            let falloff = t * t * (3.0 - 2.0 * t) * mask_weight;
            let h = terrain.field.get(x as i64, y as i64);
            let new_h = match brush.mode {
                SculptMode::Raise => h + brush.strength * falloff * dt,
                SculptMode::Lower => h - brush.strength * falloff * dt,
                SculptMode::Smooth => {
                    // Relax toward the 3×3 neighborhood average; `strength`
                    // acts as a rate (full strength ≈ settled in ~1 s).
                    let (x, y) = (x as i64, y as i64);
                    let mut sum = 0.0;
                    for dy in -1..=1i64 {
                        for dx in -1..=1i64 {
                            sum += terrain.field.get(x + dx, y + dy);
                        }
                    }
                    let blend = (brush.strength * 0.025 * falloff * dt).min(1.0);
                    h + (sum / 9.0 - h) * blend
                }
                SculptMode::Flatten => {
                    // Pull toward the stroke-start height; `strength` as rate.
                    let blend = (brush.strength * 0.025 * falloff * dt).min(1.0);
                    h + (flatten_target - h) * blend
                }
            };
            terrain
                .field
                .set(tx, ty, new_h.clamp(encode_min, encode_max));
        }
    }

    for rect in terrain.field.wrap_rect(min, max) {
        terrain.mark_dirty(rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::TerrainField;

    fn brush(mode: SculptMode) -> BrushSettings {
        BrushSettings {
            mode,
            radius: 4.0,
            strength: 10.0,
        }
    }

    #[test]
    fn raise_peaks_at_center_and_ends_at_rim() {
        let field = TerrainField::flat(32, 32, 1.0, -100.0, 100.0, false, 0.0);
        let mut terrain = EditableTerrain::new(field);
        sculpt_at(
            &mut terrain,
            Vec2::new(16.0, 16.0),
            &brush(SculptMode::Raise),
            1.0,
            0.0,
            &mut UndoHistory::default(),
        );
        let center = terrain.field.get(16, 16);
        let mid = terrain.field.get(18, 16);
        let rim = terrain.field.get(20, 16);
        assert!((center - 10.0).abs() < 1e-4, "center {center}");
        assert!(mid > 0.0 && mid < center, "mid {mid}");
        assert_eq!(rim, 0.0, "outside the radius must be untouched");
    }

    #[test]
    fn looping_stroke_wraps_to_the_far_edge() {
        let field = TerrainField::flat(32, 32, 1.0, -100.0, 100.0, true, 0.0);
        let mut terrain = EditableTerrain::new(field);
        // Brush centered on the seam: half the footprint lands on x ∈ [30, 32).
        sculpt_at(
            &mut terrain,
            Vec2::new(0.0, 16.0),
            &brush(SculptMode::Raise),
            1.0,
            0.0,
            &mut UndoHistory::default(),
        );
        assert!(terrain.field.get(0, 16) > 9.9);
        assert!(
            terrain.field.get(30, 16) > 0.0,
            "wrapped side of the brush must land on the far edge"
        );
        // A finite map drops the overhang instead.
        let field = TerrainField::flat(32, 32, 1.0, -100.0, 100.0, false, 0.0);
        let mut terrain = EditableTerrain::new(field);
        sculpt_at(
            &mut terrain,
            Vec2::new(0.0, 16.0),
            &brush(SculptMode::Raise),
            1.0,
            0.0,
            &mut UndoHistory::default(),
        );
        assert_eq!(terrain.field.get(30, 16), 0.0);
    }

    #[test]
    fn flatten_pulls_toward_the_stroke_target() {
        let mut field = TerrainField::flat(32, 32, 1.0, -100.0, 100.0, false, 0.0);
        for y in 0..32 {
            for x in 0..32 {
                field.set(x, y, x as f32);
            }
        }
        let mut terrain = EditableTerrain::new(field);
        let before = terrain.field.get(16, 16);
        // Long enough that full-falloff texels converge on the target.
        sculpt_at(
            &mut terrain,
            Vec2::new(16.0, 16.0),
            &brush(SculptMode::Flatten),
            20.0,
            5.0,
            &mut UndoHistory::default(),
        );
        let after = terrain.field.get(16, 16);
        assert!(
            (after - 5.0).abs() < (before - 5.0).abs(),
            "must move toward the target: {before} -> {after}"
        );
        assert!((after - 5.0).abs() < 0.5, "center should converge: {after}");
    }

    #[test]
    fn heights_clamp_to_the_encode_range() {
        let field = TerrainField::flat(16, 16, 1.0, 0.0, 20.0, false, 15.0);
        let mut terrain = EditableTerrain::new(field);
        sculpt_at(
            &mut terrain,
            Vec2::new(8.0, 8.0),
            &brush(SculptMode::Raise),
            10.0,
            0.0,
            &mut UndoHistory::default(),
        );
        // 15 + 10*10 would be 115; the field clamps to the R16 max so the
        // display map can't silently diverge from the authoritative field.
        assert_eq!(terrain.field.get(8, 8), 20.0);
    }
}
