//! The sun shadow-ceiling field: which world heights the terrain shadows.
//!
//! Baked once per clipmap (see `shadow_ceiling` in `bake.wgsl`) into a texture
//! holding, per world column, the height at which the sun ray grazing that
//! column's last occluder passes, plus the distance to that occluder. A point is
//! lit exactly when its height clears the ceiling, so *any* mesh can resolve its
//! terrain shadowing with one texture fetch, at any altitude.
//!
//! This exists because the clipmap is normally excluded from shadow casting —
//! rendering terrain into the cascades costs more than it returns, and cascades
//! are fit per view, so stereo pays it twice. Without the field, a character
//! standing in a mountain's shadow is lit as if the mountain weren't there.
//!
//! Sample it from your own material with `bevy_wilderness::sun_shadow` (WGSL):
//! bind [`TerrainSunShadow::field`] as a texture and copy
//! [`TerrainSunShadow::params`] into a uniform, the same shape as the
//! [`InlineFog`](crate::InlineFog) convention.

use bevy::{prelude::*, render::render_resource::ShaderType};

/// The baked field for a clipmap, on the clipmap entity. Read this to bind the
/// field into a material of your own; it is written by the bake.
#[derive(Component, Clone, Debug)]
pub struct TerrainSunShadow {
    /// The field itself: R = shadow ceiling (biased by
    /// [`SunShadowParams::height_bias`]), G = distance to the occluder casting
    /// it. Valid to bind before the bake finishes — it reads as full sun.
    pub field: Handle<Image>,
    /// Everything the shader needs besides the texture.
    pub params: SunShadowParams,
}

/// GPU form of the field's parameters, matching the WGSL `SunShadowParams` in
/// `sun_shadow.wgsl`.
#[derive(Component, ShaderType, Clone, Reflect, Debug)]
pub struct SunShadowParams {
    /// World XZ → UV scale; the field covers the origin-centered clipmap.
    pub inv_world_size: f32,
    /// Added back to the stored ceiling, which is biased so it is never zero
    /// (the bake's readiness sentinel reads all-zero as "not drawn yet").
    pub height_bias: f32,
    /// Penumbra half-width per metre of occluder distance — the tangent of the
    /// sun's angular radius.
    pub penumbra_per_metre: f32,
    /// 0 until the field is baked, 1 after. Unbaked reads as full sun.
    pub valid: f32,
}

/// The sun's angular radius seen from Earth is ~0.266°, so its shadow edge
/// spreads by about this much per metre of distance from the caster. Using the
/// real value is what keeps a distant ridge's shadow edge soft and a nearby
/// boulder's crisp, from one number.
const SUN_ANGULAR_RADIUS_TAN: f32 = 0.00465;

impl SunShadowParams {
    /// Before the terrain's extent is even known. Reads as full sun.
    pub fn unbaked() -> Self {
        Self {
            inv_world_size: 0.0,
            height_bias: 0.0,
            penumbra_per_metre: SUN_ANGULAR_RADIUS_TAN,
            valid: 0.0,
        }
    }

    /// Geometry resolved, bake in flight. Still reads as full sun: a render
    /// target holds undefined data until something draws to it, and reading that
    /// as heights would shadow the world with noise.
    pub(crate) fn pending(world_size: f32, min_height: f32) -> Self {
        Self {
            inv_world_size: 1.0 / world_size.max(1e-4),
            // Undoes the bake's `- (minmax.x - 1.0)`.
            height_bias: min_height - 1.0,
            penumbra_per_metre: SUN_ANGULAR_RADIUS_TAN,
            valid: 0.0,
        }
    }

    /// The bake landed; the field is real and safe to sample.
    pub(crate) fn mark_baked(&mut self) {
        self.valid = 1.0;
    }

    /// The target was blanked (a resize): stop sampling it until it re-bakes.
    pub(crate) fn invalidate(&mut self) {
        self.valid = 0.0;
    }
}
