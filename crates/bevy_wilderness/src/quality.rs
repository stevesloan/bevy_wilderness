use bevy::{pbr::ExtendedMaterial, prelude::*};

use crate::height_fog::{HeightFog, HeightFogParams};
use crate::material::GridMaterial;

/// Authored height-fog parameters: how the fog *looks*, an art knob separate
/// from the [`TerrainQuality`] performance profile. `HeightFog::default().density
/// > 0`, so fog is on by default; set `density: 0.0` to disable.
#[derive(Resource, Clone, Default)]
pub struct TerrainFog(pub HeightFog);

/// Terrain performance profile — set **once at startup** from device detection
/// (dial the knobs down for a standalone headset, up for desktop). Only
/// [`fog`](Self::fog) applies live; the bake-time fields
/// (`detail_layers`, `sun_shadow_size`) are read when a clipmap bakes —
/// changing them after has no effect (it would need a rebake). The texture
/// clipmap's fields (`vt_*`) rebuild it when changed. [`default`]
/// (Self::default) is desktop-grade.
#[derive(Resource, Clone, Copy, Debug)]
pub struct TerrainQuality {
    /// Texels per side of each texture-clipmap level. Two arrays of
    /// `size²·4` bytes per level (1024² with five levels ≈ 40 MB).
    pub vt_size: u32,
    /// Texel size of the finest level, metres. Each level outward doubles it,
    /// until one level covers the whole world.
    pub vt_finest_texel: f32,
    /// Texels composited per frame as the target moves. Moving into new
    /// ground costs this at most; a still target costs nothing.
    pub vt_texels_per_frame: u32,
    /// Near-detail overlay: blend the top `1` (cheapest, ~3 fewer samples) or `2`
    /// (smoothest boundaries) materials per fragment.
    pub detail_layers: u8,
    /// Resolution (square) of the sun shadow-ceiling field that shadows *meshes*
    /// standing in terrain shadow (see `bevy_wilderness::sun_shadow`). Much
    /// coarser than the surface: it holds a height, not surface detail, and terrain shadow edges are metres wide, so
    /// resolution buys little. `size²·8` bytes. It also shadows the terrain
    /// itself, so raise it if near ridges cast too soft a shadow on the ground.
    pub sun_shadow_size: u32,
}

impl Default for TerrainQuality {
    /// Desktop-grade: 0.5 m finest texel, top-2 detail. Dial these down for
    /// standalone VR and low-end GPUs (e.g. 2 m, single-layer detail).
    fn default() -> Self {
        Self {
            vt_size: 1024,
            vt_finest_texel: 0.5,
            vt_texels_per_frame: 1 << 19,
            detail_layers: 2,
            sun_shadow_size: 2048,
        }
    }
}

/// What the terrain's fragment shader outputs. Everything but
/// [`Lit`](Self::Lit) is unlit, for inspecting a channel or measuring cost.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerrainDebugView {
    #[default]
    Lit,
    /// The cavity channel as grayscale.
    Cavity,
    /// Sun visibility from the shadow field, as grayscale.
    SunVisibility,
    /// One flat colour, with no texture reads or lighting: the frame with the
    /// terrain's geometry but none of its shading, to measure what shading costs.
    Flat,
    /// The following each remove one part of the shading, to measure its cost.
    /// Lit, without the near detail overlay.
    NoDetail,
    /// Lit, reading only the clipmap's whole-world level.
    OneLevel,
    /// Lit, ignoring [`TerrainBakedLight`](crate::TerrainBakedLight).
    NoBakedLight,
    /// The clipmap's albedo with the detail overlay, unlit.
    Unlit,
}

impl TerrainDebugView {
    fn index(self) -> u32 {
        match self {
            Self::Lit => 0,
            Self::Cavity => 1,
            Self::SunVisibility => 2,
            Self::Flat => 3,
            Self::NoDetail => 4,
            Self::OneLevel => 5,
            Self::NoBakedLight => 6,
            Self::Unlit => 7,
        }
    }
}

/// Publish [`TerrainDebugView`] to the terrain's materials. Every frame, so a
/// clipmap spawned after the view was set picks it up; writes only on a
/// difference.
pub(crate) fn apply_terrain_debug_view(
    view: Res<TerrainDebugView>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
) {
    let index = view.index();
    let stale: Vec<_> = materials
        .iter()
        .filter(|(_, material)| material.extension.dev.debug_view != index)
        .map(|(id, _)| id)
        .collect();
    for id in stale {
        if let Some(mut material) = materials.get_mut(id) {
            material.extension.dev.debug_view = index;
        }
    }
}

/// The fog as a uniform, to fog **your own** materials: `#import
/// bevy_wilderness::fog_functions`, embed a `#[uniform(N)] HeightFogParams`, copy
/// this in when it changes, and call `apply_height_fog` before tonemapping. The
/// crate's terrain uses it automatically.
#[derive(Resource, Clone, Default)]
pub struct InlineFog(pub HeightFogParams);

/// Publish [`TerrainFog`] to the terrain's materials and [`InlineFog`].
pub(crate) fn apply_terrain_fog(
    fog: Res<TerrainFog>,
    mut inline_fog: ResMut<InlineFog>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
) {
    if !fog.is_changed() {
        return;
    }
    let params = HeightFogParams::from(&fog.0);
    inline_fog.0 = params.clone();
    for (_, material) in materials.iter_mut() {
        material.extension.fog = params.clone();
    }
}
