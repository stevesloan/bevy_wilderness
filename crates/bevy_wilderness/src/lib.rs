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
mod texture;

#[cfg(feature = "editing")]
pub use clipmap::RebakeRequested;
pub use clipmap::{
    Clipmap, ClipmapReady, DetailConfig, HeightRule, MAX_TERRAIN_LAYERS, SlopeRule, SunVisibility,
    TerrainLayer,
};
pub use height_fog::{HeightFog, HeightFogParams, HeightFogPlugin};
/// Public under `editing` so editor crates reuse the world↔texel + bilinear math
/// that must stay in sync with the shaders (design doc §5.3).
#[cfg(feature = "editing")]
pub use heightfield::Heightfield;
pub use mesh_fog::HeightFogExtension;
pub use quality::{FogTier, InlineFog, TerrainFog, TerrainQuality};
pub use texture::{build_terrain_array, load_terrain_array};

use clipmap::{init_clipmaps, init_grids, update_grids};
use material::GridMaterial;
use rvt::{BakeMaterial, drive_rvt_bake, init_rvt, warn_late_quality, warn_unbaked_terrain};

pub struct ClipmapPlugin;

impl Plugin for ClipmapPlugin {
    fn build(&self, app: &mut App) {
        // Shared fog math, imported by terrain.wgsl (inline VR fog) and the
        // height_fog.wgsl post-process (flatscreen fog).
        load_shader_library!(app, "fog_functions.wgsl");
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
            .add_systems(PreUpdate, (init_clipmaps, init_grids))
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
        app.add_systems(Update, rvt::process_rebake_requests.before(init_rvt));

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
