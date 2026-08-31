use bevy::{asset::embedded_asset, pbr::ExtendedMaterial, prelude::*, shader::load_shader_library};

mod clipmap;
#[cfg(feature = "dev-controls")]
mod dev_controls;
mod height_fog;
mod heightfield;
mod material;
mod mesh;
mod mesh_fog;
mod quality;
mod rvt;
mod sun_shadow;
mod texture;

pub use clipmap::{
    Clipmap, ClipmapReady, DetailConfig, HeightRule, MAX_TERRAIN_LAYERS, SlopeRule, TerrainLayer,
};
#[cfg(feature = "editing")]
pub use clipmap::{ClipmapStamp, RebakeRequested};
pub use height_fog::{HeightFog, HeightFogParams, HeightFogPlugin};
/// Public under `editing` so editor crates reuse the world↔texel + bilinear math
/// that must stay in sync with the shaders (design doc §5.3).
#[cfg(feature = "editing")]
pub use heightfield::Heightfield;
pub use mesh_fog::HeightFogExtension;
pub use quality::{FogTier, InlineFog, TerrainFog, TerrainQuality};
pub use sun_shadow::{SunShadowParams, TerrainSunShadow};
pub use texture::{build_terrain_array, load_terrain_array};

use clipmap::{init_clipmaps, init_grids, retag_png_heightmaps, update_grids};
use material::GridMaterial;
use rvt::{BakeMaterial, drive_rvt_bake, init_rvt, warn_late_quality, warn_unbaked_terrain};

pub struct ClipmapPlugin;

impl Plugin for ClipmapPlugin {
    fn build(&self, app: &mut App) {
        // Shared fog math, imported by terrain.wgsl (inline VR fog) and the
        // height_fog.wgsl post-process (flatscreen fog).
        load_shader_library!(app, "fog_functions.wgsl");
        // Sampling for the baked sun shadow-ceiling field, imported by game
        // materials that want terrain shadow on their own meshes.
        load_shader_library!(app, "sun_shadow.wgsl");
        embedded_asset!(app, "terrain.wgsl");
        embedded_asset!(app, "bake.wgsl");
        embedded_asset!(app, "mesh_fog.wgsl");

        app.add_plugins(MaterialPlugin::<
            ExtendedMaterial<StandardMaterial, GridMaterial>,
        >::default())
            .add_plugins(MaterialPlugin::<BakeMaterial>::default())
            .add_plugins(MaterialPlugin::<
                ExtendedMaterial<StandardMaterial, HeightFogExtension>,
            >::default())
            .init_resource::<TerrainFog>()
            .init_resource::<TerrainQuality>()
            .init_resource::<InlineFog>()
            .add_systems(
                PreUpdate,
                (
                    // PNG heightmaps arrive tagged R16Uint; retag before
                    // anything reads the image (see the system's docs).
                    // ⚠️ Must run *after* bevy applies freshly loaded assets
                    // (`handle_internal_asset_events`, which is
                    // `ambiguous_with_all`) — without the explicit ordering
                    // the load-frame image can slip past the retag and reach
                    // consumers still tagged Uint.
                    retag_png_heightmaps
                        .after(bevy::asset::AssetTrackingSystems)
                        .before(init_clipmaps),
                    init_clipmaps,
                    init_grids,
                ),
            )
            .add_systems(
                Update,
                (
                    update_grids,
                    init_rvt,
                    drive_rvt_bake,
                    quality::apply_terrain_quality,
                    quality::fog_new_mesh_materials,
                    warn_unbaked_terrain,
                    warn_late_quality,
                ),
            );

        // Editable-terrain API (design doc §5): consume RebakeRequested before
        // init_rvt so a re-armed bake re-spawns its cameras the same frame.
        #[cfg(feature = "editing")]
        app.init_resource::<clipmap::ClayShadowState>().add_systems(
            Update,
            (
                rvt::process_rebake_requests.before(init_rvt),
                clipmap::sync_editable_materials,
                clipmap::sync_clay_flag,
                clipmap::sync_clay_shadows,
                clipmap::sync_stamp_preview,
            ),
        );

        // Demo A/B keybinds for the AO/bent-normal experiment (B/N/V). Off by
        // default so the library ships no input systems; enable `dev-controls`.
        #[cfg(feature = "dev-controls")]
        app.add_systems(
            Update,
            (
                dev_controls::debug_cycle_view,
                dev_controls::toggle_ao,
                dev_controls::toggle_bent,
            ),
        );
    }
}
