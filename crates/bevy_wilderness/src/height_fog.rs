//! Height fog: analytic, applied per pixel in the terrain's shader and in any
//! material that imports `bevy_wilderness::fog_functions`, with no pass of its
//! own. It sits in the lit colour before tonemapping, so MSAA, tiled GPUs and
//! transparent materials all take it as they are. `HeightFog` is how it looks,
//! and [`InlineFog`](crate::InlineFog) publishes it for your own materials.

use bevy::{
    color::{Color, ColorToComponents},
    math::Vec3,
    reflect::Reflect,
    render::render_resource::ShaderType,
};

/// Height-fog parameters.
#[derive(Clone)]
pub struct HeightFog {
    /// The light the mist scatters toward the eye, linear, in cd/m² as the
    /// scene's lights are: the camera's exposure applies to it like anything
    /// else.
    pub color: Color,
    /// Base density at `base_height`. 0 disables the effect.
    pub density: f32,
    /// World-Y the mist layer sits at.
    pub base_height: f32,
    /// 1/m: how fast density drops with altitude (bigger = a thinner layer that
    /// pools in deep valleys; smaller = mist climbs higher up the slopes).
    pub falloff: f32,
    /// Distance at which fog saturates; also the distance used for sky pixels.
    pub max_distance: f32,
    /// `density`'s share per channel. Equal shares fade distance toward
    /// `color` alone; blue highest also lets it drift blue, as air does.
    pub extinction: Vec3,
    /// Metres from the eye with no fog in them.
    pub start: f32,
    /// Added to `color` looking straight at the sun, cd/m²: light scatters
    /// mostly onward, so mist brightens toward the sun. Black for none.
    pub sun_color: Color,
    /// Unit, toward the sun.
    pub sun_direction: Vec3,
    /// How tightly that glow gathers around the sun.
    pub sun_exponent: f32,
}

impl Default for HeightFog {
    fn default() -> Self {
        Self {
            // Mist at `Exposure::SUNLIGHT`.
            color: Color::linear_rgb(19_700.0, 22_800.0, 27_500.0),
            density: 0.0008,
            base_height: 0.0,
            falloff: 0.0128,
            max_distance: 30000.0,
            extinction: Vec3::ONE,
            start: 0.0,
            sun_color: Color::BLACK,
            sun_direction: Vec3::Y,
            sun_exponent: 8.0,
        }
    }
}

/// GPU form of [`HeightFog`], matching the WGSL `HeightFog`.
#[derive(ShaderType, Clone, Reflect, Debug, Default)]
pub struct HeightFogParams {
    color: Vec3,
    density: f32,
    extinction: Vec3,
    base_height: f32,
    sun_color: Vec3,
    falloff: f32,
    sun_direction: Vec3,
    max_distance: f32,
    start: f32,
    sun_exponent: f32,
}

impl HeightFogParams {
    /// Disabled fog (density 0) — the terrain shader skips the effect entirely.
    pub fn disabled() -> Self {
        Self {
            color: Vec3::ZERO,
            density: 0.0,
            extinction: Vec3::ONE,
            base_height: 0.0,
            sun_color: Vec3::ZERO,
            falloff: 0.0,
            sun_direction: Vec3::Y,
            max_distance: 1.0,
            start: 0.0,
            sun_exponent: 1.0,
        }
    }
}

impl From<&HeightFog> for HeightFogParams {
    fn from(fog: &HeightFog) -> Self {
        Self {
            color: fog.color.to_linear().to_vec3(),
            density: fog.density,
            extinction: fog.extinction,
            base_height: fog.base_height,
            sun_color: fog.sun_color.to_linear().to_vec3(),
            falloff: fog.falloff,
            sun_direction: fog.sun_direction,
            max_distance: fog.max_distance,
            start: fog.start,
            sun_exponent: fog.sun_exponent,
        }
    }
}
