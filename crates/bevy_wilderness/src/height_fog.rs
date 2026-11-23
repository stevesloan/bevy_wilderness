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

/// The share of sunlight scattered evenly that goes any one way: 1 / 4π, as
/// `fog_functions.wesl`'s `EVEN_SHARE`.
const EVEN_SHARE: f32 = 0.079_577_47;

/// Height-fog parameters.
#[derive(Clone)]
pub struct HeightFog {
    /// The light the mist scatters toward the eye, linear, in cd/m² as the
    /// scene's lights are: the camera's exposure applies to it like anything
    /// else. The sun's even share of it, `sun_color / 4π`, is what `shadow`
    /// takes out where the terrain hides the sun.
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
    /// The light the haze, molecules and aerosols, scatters toward the eye, as
    /// `color`: the sky's horizon, so distant terrain fades into it.
    pub haze_color: Color,
    /// Molecules' density at `base_height`, a layer for distance at every
    /// altitude, which a `falloff` steep enough to pool in valleys can't give.
    /// They scatter close to evenly. 0 for none.
    pub molecule_density: f32,
    /// `molecule_density`'s share per channel, as `extinction`: blue highest
    /// lets distance drift blue, as air does.
    pub molecule_extinction: Vec3,
    /// 1/m, as `falloff`, for the molecules. 0 is even through the air.
    pub molecule_falloff: f32,
    /// Aerosols' density at `base_height`, every channel alike: a layer nearer
    /// the ground than the molecules', glowing strongly around the sun as the
    /// mist does (`sun_forward`). 0 for none.
    pub aerosol_density: f32,
    /// 1/m, as `falloff`, for the aerosols.
    pub aerosol_falloff: f32,
    /// The fog shadow volume's size ([`crate::FogShadowVolume`]): with one,
    /// mist in a shaded valley loses the sun it can't see, and a ridge between
    /// the eye and the sun cuts a dark shaft into the haze. `None` shades the
    /// fog as a whole on the CPU instead ([`seen_from`](Self::seen_from)): the
    /// glow toward the sun goes when the eye is in shade, and the mist takes
    /// the colour of however much of it is, but a shaft across lit air doesn't
    /// show, nor one bank of mist lit beside another shaded.
    pub shadow: Option<crate::FogShadowSize>,
}

impl HeightFog {
    /// Whether any layer has density.
    pub fn is_on(&self) -> bool {
        self.density + self.molecule_density + self.aerosol_density > 0.0
    }

    /// The fog where the viewer gets `eye` of the sun
    /// ([`EyeSunlight`](crate::EyeSunlight)) and the mist around them `mist` of
    /// it ([`MistSunlight`](crate::MistSunlight)), for when there's no shadow
    /// volume to shade it point by point. The sun's glow is scaled by `eye`,
    /// and the sun's even share of the mist's colour by `mist`, as the shader
    /// does for shaded fog. The colour doesn't follow `eye`: the sun lights the
    /// mist alike from every side, so that would shift the fog's colour in
    /// every direction as the viewer steps into shade, while the glow changes
    /// only toward the sun.
    pub fn seen_from(&self, eye: f32, mist: f32) -> Self {
        let sun = self.sun_color.to_linear().to_vec3();
        let color = (self.color.to_linear().to_vec3() - (1.0 - mist) * sun * EVEN_SHARE).max(Vec3::ZERO);
        let glow = sun * eye;
        Self {
            color: Color::linear_rgb(color.x, color.y, color.z),
            sun_color: Color::linear_rgb(glow.x, glow.y, glow.z),
            ..self.clone()
        }
    }
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
            haze_color: Color::linear_rgb(19_700.0, 22_800.0, 27_500.0),
            molecule_density: 0.0,
            molecule_extinction: Vec3::ONE,
            molecule_falloff: 0.0,
            aerosol_density: 0.0,
            aerosol_falloff: 0.0,
            shadow: None,
        }
    }
}

/// GPU form of [`HeightFog`], matching the WGSL `HeightFog`.
#[derive(ShaderType, Clone, PartialEq, Reflect, Debug, Default)]
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
    molecule_density: f32,
    molecule_extinction: Vec3,
    molecule_falloff: f32,
    start: f32,
    sun_forward: f32,
    aerosol_density: f32,
    aerosol_falloff: f32,
    shadowed: u32,
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
            molecule_density: 0.0,
            molecule_extinction: Vec3::ONE,
            molecule_falloff: 0.0,
            start: 0.0,
            sun_forward: 0.0,
            aerosol_density: 0.0,
            aerosol_falloff: 0.0,
            shadowed: 0,
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
            molecule_density: fog.molecule_density,
            molecule_extinction: fog.molecule_extinction,
            molecule_falloff: fog.molecule_falloff,
            start: fog.start,
            sun_forward: fog.sun_forward,
            aerosol_density: fog.aerosol_density,
            aerosol_falloff: fog.aerosol_falloff,
            shadowed: fog.shadow.is_some() as u32,
        }
    }
}
