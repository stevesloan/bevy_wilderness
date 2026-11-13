//! The built-in clone tool: lift a round piece of the terrain's own relief
//! and stamp it elsewhere, scaled and rotated.
//!
//! **Alt+click** captures the disc under the cursor ([`CloneSettings::radius`],
//! **Alt+wheel**). What's kept is the disc's height above the ground it stands
//! on, where that ground is the surface its rim alone implies (see
//! [`capture`]). A hill on a slope therefore lands on flat ground as the hill
//! without the slope, and a mountain that fills the disc keeps its whole
//! height: nothing inside the rim counts as ground.
//!
//! The capture is a snapshot, so stamping over the source doesn't change it.
//! From there it is a round stamp: the same GPU preview and CPU commit as the
//! stamp tool, added to what's under it. **Wheel** = height, **Ctrl+wheel** =
//! scale (height follows, so slopes keep their angle), **Shift+wheel** =
//! rotation.

use bevy::{input::mouse::AccumulatedMouseScroll, prelude::*};
use bevy_wilderness::{Clipmap, ClipmapStamp};

use crate::cursor::TerrainCursor;
use crate::field::TerrainField;
use crate::gesture::TerrainGesture;
use crate::stamp::{BakeParams, StampData, apply_stamp};
use crate::terrain::EditableTerrain;
use crate::undo::UndoHistory;

/// Captures are resampled to at most this many texels across; a larger disc
/// loses detail finer than its diameter over this.
const MAX_CAPTURE_TEXELS: u32 = 1024;
/// Samples taken around the rim, and the harmonics of them the ground keeps.
/// The ground follows the rim down to features an eighth of the radius or so
/// across; finer rim detail stays in the relief for the feather to fade.
const RIM_SAMPLES: usize = 256;
const RIM_HARMONICS: usize = 48;

/// What the clone tool stamps: the last capture. `None` until one is taken.
#[derive(Resource, Default)]
pub struct CloneSource(pub Option<ClonedRelief>);

/// A captured disc of relief.
pub struct ClonedRelief {
    data: StampData,
    /// Height in meters of the relief's largest excursion, which `data`
    /// stores as ±1.
    amplitude: f32,
    /// Where it was taken, world XZ meters, and how wide.
    pub center: Vec2,
    pub radius: f32,
}

impl ClonedRelief {
    /// Largest height above or below its ground, meters.
    pub fn amplitude(&self) -> f32 {
        self.amplitude
    }
}

/// The clone tool's settings — what the wheel adjusts and a UI shows as
/// sliders.
#[derive(Resource)]
pub struct CloneSettings {
    /// Radius of the disc the next capture takes, world meters.
    pub radius: f32,
    /// Size of the stamped copy relative to the source. Heights scale with
    /// it.
    pub scale: f32,
    /// Height multiplier on top of `scale`. Signed: negative turns the relief
    /// upside down.
    pub height: f32,
    /// Rotation about +Y, radians.
    pub rotation: f32,
    /// Rim feather (see [`BakeParams::feather`]).
    pub feather: f32,
}

impl Default for CloneSettings {
    fn default() -> Self {
        Self {
            radius: 200.0,
            scale: 1.0,
            height: 1.0,
            rotation: 0.0,
            feather: 0.15,
        }
    }
}

/// The relief of the disc at `center` (world XZ): heights in meters above the
/// ground its rim implies, row-major over the disc's bounding square, `n`
/// texels a side.
///
/// The ground is the harmonic surface through the rim's heights — the shape a
/// soap film takes on a wire bent to the rim. It has no bumps of its own, so
/// subtracting it removes the slope and elevation the disc sits on and leaves
/// everything that rises or falls inside it. On a disc it has a closed form:
/// the rim's Fourier series, each harmonic fading inward as `r^k`.
fn capture(field: &TerrainField, center: Vec2, radius: f32) -> (Vec<f32>, u32) {
    let n = ((2.0 * radius / field.texel_size()).ceil() as u32).clamp(2, MAX_CAPTURE_TEXELS);

    let mut coefficients = [(0.0f64, 0.0f64); RIM_HARMONICS + 1];
    for j in 0..RIM_SAMPLES {
        let angle = std::f64::consts::TAU * j as f64 / RIM_SAMPLES as f64;
        let (sin, cos) = angle.sin_cos();
        let rim = center + Vec2::new(cos as f32, sin as f32) * radius;
        let h = field.height_at(rim) as f64 / RIM_SAMPLES as f64;
        for (k, c) in coefficients.iter_mut().enumerate() {
            let (sin, cos) = (angle * k as f64).sin_cos();
            c.0 += h * cos;
            c.1 -= h * sin;
        }
    }

    let mut relief = Vec::with_capacity((n * n) as usize);
    for y in 0..n {
        for x in 0..n {
            let unit = (Vec2::new(x as f32, y as f32) + 0.5) / n as f32 * 2.0 - 1.0;
            if unit.length_squared() >= 1.0 {
                relief.push(0.0);
                continue;
            }
            // c0 + 2·Re Σ c_k z^k by Horner, z the point on the unit disc.
            let (zx, zy) = (unit.x as f64, unit.y as f64);
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for c in coefficients[1..].iter().rev() {
                (re, im) = (re + c.0, im + c.1);
                (re, im) = (re * zx - im * zy, re * zy + im * zx);
            }
            let ground = coefficients[0].0 + 2.0 * re;
            relief.push(field.height_at(center + unit * radius) - ground as f32);
        }
    }
    (relief, n)
}

impl ClonedRelief {
    /// Capture the disc at `center` (world XZ meters) from `field`.
    pub fn capture(
        field: &TerrainField,
        center: Vec2,
        radius: f32,
        images: &mut Assets<Image>,
    ) -> Self {
        let (mut relief, n) = capture(field, center, radius);
        let amplitude = relief.iter().fold(0.0f32, |a, h| a.max(h.abs()));
        if amplitude > 0.0 {
            for h in &mut relief {
                *h /= amplitude;
            }
        }
        Self {
            data: StampData::from_relief(relief, UVec2::splat(n), images),
            amplitude,
            center,
            radius,
        }
    }
}

/// Capture on Alt+click; otherwise float the capture under the cursor, adjust
/// settings on the wheel, and commit on click. Runs in `EditorSet::Tools`,
/// gated on the clone tool.
// Bevy systems legitimately take one param per resource.
#[allow(clippy::too_many_arguments)]
pub(crate) fn drive_clone_tool(
    cursor: Res<TerrainCursor>,
    mut source: ResMut<CloneSource>,
    mut settings: ResMut<CloneSettings>,
    scroll: Res<AccumulatedMouseScroll>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut images: ResMut<Assets<Image>>,
    mut history: ResMut<UndoHistory>,
    mut gesture: ResMut<TerrainGesture>,
    mut terrains: Query<(Entity, &mut EditableTerrain, &mut Clipmap)>,
) {
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    let picking = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);

    // Wheel only while aiming at terrain, as for the stamp tool.
    let dy = scroll.delta.y;
    if dy != 0.0 && cursor.0.is_some() {
        if picking {
            settings.radius = (settings.radius * 1.1f32.powf(dy)).clamp(2.0, 20_000.0);
        } else if ctrl {
            settings.scale = (settings.scale * 1.1f32.powf(dy)).clamp(0.05, 20.0);
        } else if shift {
            settings.rotation =
                (settings.rotation + dy * 5f32.to_radians()).rem_euclid(std::f32::consts::TAU);
        } else {
            settings.height = (settings.height + dy * 0.05).clamp(-5.0, 5.0);
        }
    }

    let feather = BakeParams {
        feather: settings.feather,
        ..default()
    };
    if let Some(relief) = &mut source.0 {
        relief.data.rebake(feather, &mut images);
    }

    let clicked = buttons.just_pressed(MouseButton::Left);
    for (entity, mut terrain, mut clipmap) in &mut terrains {
        let hit = cursor.0.filter(|hit| hit.terrain == entity);
        if picking {
            if clipmap.stamp.is_some() {
                clipmap.stamp = None;
            }
            if let (Some(hit), true) = (hit, clicked) {
                let mut relief = ClonedRelief::capture(
                    &terrain.field,
                    hit.position.xz(),
                    settings.radius,
                    &mut images,
                );
                relief.data.rebake(feather, &mut images);
                source.0 = Some(relief);
            }
            continue;
        }
        let desired = match (&source.0, hit) {
            (Some(relief), Some(hit)) => Some(ClipmapStamp {
                image: relief.data.image.clone(),
                center: hit.position.xz(),
                half_size: Vec2::splat(relief.radius * settings.scale),
                rotation: settings.rotation,
                strength: relief.amplitude * settings.scale * settings.height,
                masked: terrain.mask_active(),
            }),
            _ => None,
        };
        if clipmap.stamp != desired {
            clipmap.stamp = desired.clone();
        }
        if let (Some(stamp), Some(relief), Some(hit), true) = (&desired, &source.0, hit, clicked) {
            gesture.begin(&mut history, entity, "Clone");
            apply_stamp(
                &mut terrain,
                &relief.data,
                hit.texel,
                stamp.half_size,
                stamp.rotation,
                stamp.strength,
                &mut gesture,
            );
            gesture.seal(&mut history);
        }
    }
}

/// Rings for the clone tool: where the capture came from, and while Alt is
/// held, the disc a click would capture.
pub(crate) fn draw_clone_rings(
    cursor: Res<TerrainCursor>,
    source: Res<CloneSource>,
    settings: Res<CloneSettings>,
    keys: Res<ButtonInput<KeyCode>>,
    terrains: Query<&EditableTerrain>,
    mut gizmos: Gizmos,
) {
    let flat = Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2);
    if let Some(relief) = &source.0
        && let Some(terrain) = terrains.iter().next()
    {
        let y = terrain.field.height_at(relief.center);
        let at = Vec3::new(relief.center.x, y + 0.5, relief.center.y);
        gizmos.circle(
            Isometry3d::new(at, flat),
            relief.radius,
            Color::srgb(0.3, 0.8, 1.0),
        );
    }
    if (keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight))
        && let Some(hit) = &cursor.0
    {
        gizmos.circle(
            Isometry3d::new(hit.position + Vec3::Y * 0.5, flat),
            settings.radius,
            Color::srgb(1.0, 0.9, 0.2),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: u32 = 256;

    /// A field of `ground(x, z)` plus a cosine bump of `height` and `radius`
    /// at world `at`.
    fn field(ground: impl Fn(Vec2) -> f32, at: Vec2, radius: f32, height: f32) -> TerrainField {
        let mut field = TerrainField::flat(SIZE, SIZE, 1.0, -1000.0, 1000.0, false, 0.0);
        for y in 0..SIZE {
            for x in 0..SIZE {
                let xz = field.texel_to_world(Vec2::new(x as f32, y as f32));
                let d = (xz.distance(at) / radius).min(1.0);
                let bump = height * 0.5 * (1.0 + (d * std::f32::consts::PI).cos());
                field.set(x, y, ground(xz) + bump);
            }
        }
        field
    }

    fn at(relief: &[f32], n: u32, unit: Vec2) -> f32 {
        let p = ((unit * 0.5 + 0.5) * n as f32)
            .as_uvec2()
            .min(UVec2::splat(n - 1));
        relief[(p.y * n + p.x) as usize]
    }

    #[test]
    fn a_hill_on_a_slope_comes_away_without_the_slope() {
        let slope = |xz: Vec2| 300.0 + 0.4 * xz.x - 0.2 * xz.y;
        let field = field(slope, Vec2::new(10.0, -5.0), 30.0, 40.0);
        let (relief, n) = capture(&field, Vec2::new(10.0, -5.0), 50.0);
        let peak = at(&relief, n, Vec2::ZERO);
        assert!(
            (peak - 40.0).abs() < 1.0,
            "hill keeps its height, got {peak}"
        );
        for unit in [
            Vec2::X * 0.8,
            Vec2::NEG_X * 0.8,
            Vec2::Y * 0.8,
            Vec2::NEG_Y * 0.8,
        ] {
            let h = at(&relief, n, unit);
            assert!(h.abs() < 0.5, "ground around the hill is level, got {h}");
        }
    }

    #[test]
    fn a_mountain_filling_the_disc_keeps_its_height() {
        let field = field(|_| 100.0, Vec2::ZERO, 60.0, 500.0);
        let (relief, n) = capture(&field, Vec2::ZERO, 60.0);
        let peak = at(&relief, n, Vec2::ZERO);
        assert!((peak - 500.0).abs() < 5.0, "got {peak}");
    }

    #[test]
    fn a_capture_stamped_on_flat_ground_rebuilds_the_hill() {
        let mut images = Assets::default();
        let slope = |xz: Vec2| 200.0 + 0.3 * xz.x;
        let source = field(slope, Vec2::new(-40.0, 20.0), 30.0, 40.0);
        let relief = ClonedRelief::capture(&source, Vec2::new(-40.0, 20.0), 50.0, &mut images);
        assert!((relief.amplitude() - 40.0).abs() < 1.0);

        let mut flat = EditableTerrain::new(TerrainField::flat(
            SIZE, SIZE, 1.0, -1000.0, 1000.0, false, 0.0,
        ));
        let mut gesture = TerrainGesture::default();
        apply_stamp(
            &mut flat,
            &relief.data,
            Vec2::splat(128.0),
            Vec2::splat(relief.radius),
            0.0,
            relief.amplitude,
            &mut gesture,
        );
        let peak = flat.field.get(128, 128);
        assert!((peak - 40.0).abs() < 1.0, "got {peak}");
        assert!(
            flat.field.get(128 + 45, 128).abs() < 0.5,
            "level near the rim"
        );
        assert_eq!(flat.field.get(128 + 60, 128), 0.0, "outside untouched");
    }
}
