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
    /// Mist's density at `base_height`. 0 for none.
    pub density: f32,
    /// World-Y both layers are measured from.
    pub base_height: f32,
    /// 1/m: how fast density drops with altitude (bigger = a thinner layer that
    /// pools in deep valleys; smaller = mist climbs higher up the slopes).
    pub falloff: f32,
    /// Distance at which fog saturates; also the distance used for sky pixels.
    pub max_distance: f32,
    /// `density`'s share per channel. Equal shares fade distance toward
    /// `color` alone, as water droplets do; blue highest also lets it drift
    /// blue, as air does.
    pub extinction: Vec3,
    /// Metres from the eye with no fog in them.
    pub start: f32,
    /// The sunlight reaching the fog, lux. Light scatters mostly onward, so fog
    /// brightens toward the sun; `color` and `haze_color` carry what it scatters
    /// evenly. Black for none.
    pub sun_color: Color,
    /// Unit, toward the sun.
    pub sun_direction: Vec3,
    /// Henyey-Greenstein's g: how much of that sunlight goes straight on, from
    /// 0, evenly, toward 1. Droplets and aerosols sit around 0.8.
    pub sun_forward: f32,
    /// Haze's density at `base_height`, a second layer for distance at every
    /// altitude, which a `falloff` steep enough to pool in valleys can't give.
    /// 0 for none.
    pub haze_density: f32,
    /// 1/m, as `falloff`, for the haze. 0 is even through the air.
    pub haze_falloff: f32,
    /// The light the haze scatters toward the eye, as `color`: the sky's horizon,
    /// so distant terrain fades into it.
    pub haze_color: Color,
    /// `haze_density`'s share per channel, as `extinction`.
    pub haze_extinction: Vec3,
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
            sun_forward: 0.8,
            haze_density: 0.0,
            haze_falloff: 0.0,
            haze_color: Color::linear_rgb(19_700.0, 22_800.0, 27_500.0),
            haze_extinction: Vec3::ONE,
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
    haze_color: Vec3,
    haze_density: f32,
    haze_extinction: Vec3,
    haze_falloff: f32,
    start: f32,
    sun_forward: f32,
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
            haze_color: Vec3::ZERO,
            haze_density: 0.0,
            haze_extinction: Vec3::ONE,
            haze_falloff: 0.0,
            start: 0.0,
            sun_forward: 0.0,
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
            haze_color: fog.haze_color.to_linear().to_vec3(),
            haze_density: fog.haze_density,
            haze_extinction: fog.haze_extinction,
            haze_falloff: fog.haze_falloff,
            start: fog.start,
            sun_forward: fog.sun_forward,
        }
    }
}
