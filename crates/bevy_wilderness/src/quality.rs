use bevy::{pbr::ExtendedMaterial, prelude::*};

use crate::Clipmap;
use crate::height_fog::{HeightFog, HeightFogParams};
use crate::material::GridMaterial;
use crate::mesh_fog::HeightFogExtension;

/// Authored height-fog parameters — how the fog *looks* (an art knob, separate
/// from the [`TerrainQuality`] performance profile). The crate realizes these
/// inline in the terrain (`FogTier::Low`) or via the fullscreen
/// [`HeightFogPlugin`](crate::HeightFogPlugin) post-process (`FogTier::High`);
/// both paths stay in sync. `HeightFog::default().density > 0`, so fog is on by
/// default — set `density: 0.0` to disable.
#[derive(Resource, Clone, Default)]
pub struct TerrainFog(pub HeightFog);

/// How the fog is rendered (a field of [`TerrainQuality`]). Live-switchable —
/// both paths are always compiled.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum FogTier {
    /// Fullscreen fog post-process (fogs the sky too, no seam) at the cost of one
    /// framebuffer pass. **Requires `Msaa::Off`** on the target camera — that pass
    /// samples single-sample depth. The crate doesn't set MSAA (that's the game's);
    /// if MSAA is left on, the fog pass skips itself and warns. Desktop budget.
    #[default]
    High,
    /// Inline terrain fog (virtually free, terrain-only, no extra pass). MSAA-friendly
    /// — the game picks the AA level. Tiled GPUs punish fullscreen passes, and
    /// standalone VR wants stable MSAA, so this trades sky-fog for that freedom.
    Low,
}

/// Terrain performance profile — set **once at startup** from device detection
/// (dial the knobs down for a standalone headset, up for desktop). Only
/// [`fog`](Self::fog) applies live; the bake-time fields
/// (`rvt_size`, `ambient_gather`, `detail_layers`, `sun_shadow_size`) are read
/// when a clipmap bakes —
/// changing them after has no effect (it would need a rebake). [`default`]
/// (Self::default) is desktop-grade.
#[derive(Resource, Clone, Copy, Debug)]
pub struct TerrainQuality {
    /// Fog method (live-switchable). See [`FogTier`].
    pub fog: FogTier,
    /// RVT bake resolution (square). The dominant VRAM cost — three targets of
    /// `size²·4` bytes each (8192² ≈ 768 MB total; 4096² ≈ 192 MB).
    pub rvt_size: u32,
    /// Bake + sample the macro-AO / bent-normal / cavity channel. Off drops a
    /// whole RVT target (VRAM + a slow bake gather) and a per-fragment sample; the
    /// effect is subtle on open terrain, so it's the first thing to cut for VR.
    pub ambient_gather: bool,
    /// Near-detail overlay: blend the top `1` (cheapest, ~3 fewer samples) or `2`
    /// (smoothest boundaries) materials per fragment.
    pub detail_layers: u8,
    /// Resolution (square) of the sun shadow-ceiling field that shadows *meshes*
    /// standing in terrain shadow (see `bevy_wilderness::sun_shadow`). Sized
    /// separately from [`rvt_size`](Self::rvt_size) and much smaller: it holds a
    /// height, not surface detail, and terrain shadow edges are metres wide, so
    /// resolution buys little. `size²·8` bytes.
    pub sun_shadow_size: u32,
}

impl Default for TerrainQuality {
    /// Desktop-grade: fullscreen fog, 8192² RVT, ambient gather, top-2 detail.
    /// Dial these down for standalone VR / low-end (e.g. `FogTier::Low`, 2048²
    /// RVT, no ambient gather, single-layer detail).
    fn default() -> Self {
        Self {
            fog: FogTier::High,
            rvt_size: 8192,
            ambient_gather: true,
            detail_layers: 2,
            sun_shadow_size: 2048,
        }
    }
}

/// The active tier's inline fog params — apply these to fog **your own** opaque
/// materials on fast per-material uniforms (no shared buffer / SSBO cost on tiled
/// VR GPUs). The crate's terrain + [`HeightFogExtension`] use it automatically.
///
/// - **Opaque** (buildings, characters): `#import bevy_wilderness::fog_functions`,
///   embed a `#[uniform(N)] HeightFogParams`, copy this in on `.is_changed()`.
///   It's density-0 on `High` (the fullscreen pass fogs opaques there), so it's
///   correct on both tiers.
/// - **Transparent** (particles, explosions): the fullscreen pass can't fog them,
///   so fog on *both* tiers from [`TerrainFog`] (`HeightFogParams::from(&fog.0)`).
#[derive(Resource, Clone, Default)]
pub struct InlineFog(pub HeightFogParams);

/// Inline-terrain fog params for the active tier: the authored fog with density
/// gated to 0 outside the `Low` tier (`High` uses the fullscreen pass instead).
pub(crate) fn inline_fog_params(fog: &TerrainFog, tier: FogTier) -> HeightFogParams {
    let density = if tier == FogTier::Low {
        fog.0.density
    } else {
        0.0
    };
    HeightFogParams::from(&fog.0).with_density(density)
}

/// Realizes [`TerrainFog`] across both fog paths for the active [`TerrainQuality::fog`]
/// tier whenever either changes: writes the inline params into every terrain
/// material, and (if the target camera has a [`HeightFog`], i.e. the fullscreen
/// path is installed) drives its density to match the tier. MSAA is left to the
/// game — the fullscreen fog pass requires `Msaa::Off` and self-skips otherwise.
pub(crate) fn apply_terrain_quality(
    quality: Res<TerrainQuality>,
    fog: Res<TerrainFog>,
    mut inline_fog: ResMut<InlineFog>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
    mut mesh_materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, HeightFogExtension>>>,
    clipmaps: Query<&Clipmap>,
    mut cameras: Query<&mut HeightFog>,
) {
    if !quality.is_changed() && !fog.is_changed() {
        return;
    }
    let low = quality.fog == FogTier::Low;
    let inline = inline_fog_params(&fog, quality.fog);
    // Publish for the game to fog its own materials (change-detected).
    inline_fog.0 = inline.clone();
    for (_, material) in materials.iter_mut() {
        material.extension.fog = inline.clone();
    }
    // Meshes (characters/props) using HeightFogExtension get the same inline fog,
    // so they don't render as unfogged cutouts on the Low tier.
    for (_, material) in mesh_materials.iter_mut() {
        material.extension.fog = inline.clone();
    }
    for clipmap in &clipmaps {
        if let Ok(mut camera_fog) = cameras.get_mut(clipmap.target) {
            *camera_fog = fog.0.clone();
            camera_fog.density = if low { 0.0 } else { fog.0.density };
        }
    }
}

/// Fog mesh materials the moment they're created, so a character/prop spawned at
/// runtime picks up the tier's fog immediately (else an unfogged cutout on `Low`
/// until the next tier change). Touches only new materials — free at steady state.
pub(crate) fn fog_new_mesh_materials(
    mut events: MessageReader<AssetEvent<ExtendedMaterial<StandardMaterial, HeightFogExtension>>>,
    quality: Res<TerrainQuality>,
    fog: Res<TerrainFog>,
    mut mesh_materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, HeightFogExtension>>>,
) {
    let inline = inline_fog_params(&fog, quality.fog);
    for event in events.read() {
        if let AssetEvent::Added { id } = event
            && let Some(mut material) = mesh_materials.get_mut(*id)
        {
            material.extension.fog = inline.clone();
        }
    }
}
