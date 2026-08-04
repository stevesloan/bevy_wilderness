//! The built-in sculpt tool (D2): raise / lower / smooth / flatten the f32
//! field under the brush, with radial falloff, while the left mouse button is
//! held. Edits mark dirty regions; the sync in `EditorSet::Apply` re-quantizes
//! them into the display heightmap the same frame, so geometry deforms live.
//! On looping terrain the brush footprint wraps modulo the heightmap.

use bevy::prelude::*;

use crate::cursor::TerrainCursor;
use crate::field::TerrainField;
use crate::gesture::{TerrainGesture, UndoBuffer};
use crate::settings::{BrushSettings, SculptMode};
use crate::terrain::EditableTerrain;
use crate::undo::UndoHistory;

/// Smooth's blur kernel, as a fraction of the brush radius.
///
/// It scales with the brush rather than being fixed, because relaxing toward a
/// fixed-size neighborhood is diffusion: the time to soften a feature of width
/// W grows as W². A one-texel kernel therefore never converges on anything
/// broad — a stamp seam takes tens of minutes of held stroke. Tying the kernel
/// to the brush makes the cost independent of the feature scale the brush was
/// sized for.
const SMOOTH_KERNEL_FRACTION: f32 = 0.25;

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
// Bevy systems legitimately take one param per resource they touch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_sculpt(
    time: Res<Time>,
    buttons: Res<ButtonInput<MouseButton>>,
    cursor: Res<TerrainCursor>,
    brush: Res<BrushSettings>,
    mut history: ResMut<UndoHistory>,
    mut gesture: ResMut<TerrainGesture>,
    mut terrains: Query<&mut EditableTerrain>,
    mut stroke: Local<StrokeState>,
) {
    if !buttons.pressed(MouseButton::Left) {
        stroke.flatten_target = None;
        gesture.seal(&mut history);
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
        gesture.begin(
            &mut history,
            hit.terrain,
            format!("Sculpt ({:?})", brush.mode),
        );
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
        &mut gesture,
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
    gesture: &mut TerrainGesture,
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
        gesture.capture(terrain, UndoBuffer::Height, rect);
    }

    // A painted mask confines the op (D4): deltas weight by the feathered mask
    // value, so edits blend softly into unmasked ground. No mask = no
    // confinement.
    let masked = terrain.mask_active();

    // Smooth relaxes toward a blurred copy of the footprint, which has to be
    // taken before the write loop mutates the field — sampling live would let
    // already-written texels feed back into their neighbors' targets, biasing
    // the result along iteration order.
    let smooth = (brush.mode == SculptMode::Smooth).then(|| {
        let k = (radius_texels * SMOOTH_KERNEL_FRACTION).round().max(1.0) as i32;
        (
            blur_footprint(&terrain.field, min, max, k),
            (max.x - min.x) as usize,
        )
    });

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
                // Relax toward the blurred copy; `strength` acts as a rate
                // (full strength ≈ settled in ~1 s), matching Flatten.
                SculptMode::Smooth => match &smooth {
                    Some((blur, stride)) => {
                        let i = (y - min.y) as usize * stride + (x - min.x) as usize;
                        let blend = (brush.strength * 0.025 * falloff * dt).min(1.0);
                        h + (blur[i] - h) * blend
                    }
                    // Unreachable: built above for exactly this mode.
                    None => h,
                },
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

/// Box-blurred copy of the write footprint `[min, max)`, kernel radius `k`
/// texels, row-major and `max.x - min.x` wide.
///
/// Two separable sliding-window passes, so cost is O(area) regardless of `k` —
/// the kernel scales with the brush, and a naive per-texel gather would make a
/// large brush quadratically slower. The horizontal pass covers `k` extra rows
/// on each side, which the vertical pass then consumes.
///
/// Reads go through [`TerrainField::get`], which wraps on looping terrain and
/// clamps otherwise, so the footprint needs no seam special-casing. Sums are
/// `f64` because a sliding window add/subtracts across the whole span and `f32`
/// would accumulate drift along the row.
fn blur_footprint(field: &TerrainField, min: IVec2, max: IVec2, k: i32) -> Vec<f32> {
    let w = (max.x - min.x) as usize;
    let h = (max.y - min.y) as usize;
    let span = (2 * k + 1) as usize;
    let inv = 1.0 / span as f64;
    let (kx, my) = (i64::from(k), i64::from(min.y));

    let rows = h + 2 * k as usize;
    let mut tmp = vec![0.0f32; w * rows];
    for row in 0..rows {
        let y = my - kx + row as i64;
        let mut sum: f64 = (-kx..=kx)
            .map(|dx| f64::from(field.get(i64::from(min.x) + dx, y)))
            .sum();
        tmp[row * w] = (sum * inv) as f32;
        for col in 1..w {
            let x = i64::from(min.x) + col as i64;
            sum += f64::from(field.get(x + kx, y));
            sum -= f64::from(field.get(x - kx - 1, y));
            tmp[row * w + col] = (sum * inv) as f32;
        }
    }

    let mut out = vec![0.0f32; w * h];
    for col in 0..w {
        let mut sum: f64 = (0..span).map(|r| f64::from(tmp[r * w + col])).sum();
        out[col] = (sum * inv) as f32;
        for row in 1..h {
            sum += f64::from(tmp[(row + span - 1) * w + col]);
            sum -= f64::from(tmp[(row - 1) * w + col]);
            out[row * w + col] = (sum * inv) as f32;
        }
    }
    out
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
            &mut TerrainGesture::default(),
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
            &mut TerrainGesture::default(),
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
            &mut TerrainGesture::default(),
        );
        assert_eq!(terrain.field.get(30, 16), 0.0);
    }

    #[test]
    fn stroke_on_a_neighboring_repeat_edits_the_base_tile() {
        // A looping terrain renders repeats everywhere; the cursor's texel
        // coordinates there are a whole tile offset from the base. Sculpting
        // on a repeat must land on exactly the texels the base-tile stroke
        // would (D6: edits are continuous across the seam wherever you stand).
        let stroke = |center: Vec2| {
            let field = TerrainField::flat(32, 32, 1.0, -100.0, 100.0, true, 0.0);
            let mut terrain = EditableTerrain::new(field);
            sculpt_at(
                &mut terrain,
                center,
                &brush(SculptMode::Raise),
                1.0,
                0.0,
                &mut TerrainGesture::default(),
            );
            terrain
        };
        let base = stroke(Vec2::new(16.0, 16.0));
        // One tile east, two tiles north of the base instance.
        let repeat = stroke(Vec2::new(16.0 + 32.0, 16.0 - 64.0));
        for y in 0..32 {
            for x in 0..32 {
                assert_eq!(
                    base.field.get(x, y),
                    repeat.field.get(x, y),
                    "texel ({x},{y}) differs between base and repeat strokes"
                );
            }
        }
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
            &mut TerrainGesture::default(),
        );
        let after = terrain.field.get(16, 16);
        assert!(
            (after - 5.0).abs() < (before - 5.0).abs(),
            "must move toward the target: {before} -> {after}"
        );
        assert!((after - 5.0).abs() < 0.5, "center should converge: {after}");
    }

    /// A stamp seam is a step, and softening one is the tool's main job. The
    /// kernel scales with the brush, so the ramp must widen well past the
    /// immediate neighbors a fixed 3×3 could reach.
    #[test]
    fn smooth_widens_a_step_across_the_brush() {
        let mut field = TerrainField::flat(128, 128, 1.0, -500.0, 500.0, false, 0.0);
        for y in 0..128 {
            for x in 64..128 {
                field.set(x, y, 200.0);
            }
        }
        let mut terrain = EditableTerrain::new(field);
        let wide = BrushSettings {
            mode: SculptMode::Smooth,
            radius: 30.0,
            strength: 40.0,
        };
        for _ in 0..60 {
            sculpt_at(
                &mut terrain,
                Vec2::new(64.0, 64.0),
                &wide,
                1.0 / 60.0,
                0.0,
                &mut TerrainGesture::default(),
            );
        }
        // Texels well outside the old 3×3 reach must have moved off the step.
        let low = terrain.field.get(58, 64);
        let high = terrain.field.get(70, 64);
        assert!(low > 1.0, "step foot should lift: {low}");
        assert!(high < 199.0, "step shoulder should drop: {high}");
        // The step's total height is redistributed, not added to.
        assert!(
            terrain.field.get(64, 64) > 50.0 && terrain.field.get(64, 64) < 150.0,
            "midpoint should sit inside the step: {}",
            terrain.field.get(64, 64)
        );
        // Beyond the brush nothing is touched.
        assert_eq!(terrain.field.get(20, 64), 0.0);
        assert_eq!(terrain.field.get(110, 64), 200.0);
    }

    /// The blur is sampled before any write, so a stroke can't feed
    /// already-updated texels back into its neighbors' targets. A symmetric
    /// step must therefore stay symmetric about its midpoint.
    #[test]
    fn smooth_is_order_independent() {
        let mut field = TerrainField::flat(64, 64, 1.0, -500.0, 500.0, false, 0.0);
        for y in 0..64 {
            for x in 32..64 {
                field.set(x, y, 100.0);
            }
        }
        let mut terrain = EditableTerrain::new(field);
        let b = BrushSettings {
            mode: SculptMode::Smooth,
            radius: 16.0,
            strength: 200.0,
        };
        // Centered on the step itself (between texels 31 and 32), so the
        // radial falloff is symmetric about it and can't mask an ordering bias.
        for _ in 0..30 {
            sculpt_at(
                &mut terrain,
                Vec2::new(31.5, 32.0),
                &b,
                1.0 / 60.0,
                0.0,
                &mut TerrainGesture::default(),
            );
        }
        for d in 1..12i64 {
            let below = terrain.field.get(32 - d, 32) - 0.0;
            let above = 100.0 - terrain.field.get(32 + d - 1, 32);
            assert!(
                (below - above).abs() < 0.5,
                "asymmetric at d={d}: {below} vs {above}"
            );
        }
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
            &mut TerrainGesture::default(),
        );
        // 15 + 10*10 would be 115; the field clamps to the R16 max so the
        // display map can't silently diverge from the authoritative field.
        assert_eq!(terrain.field.get(8, 8), 20.0);
    }
}
