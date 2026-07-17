//! [`HeightFogExtension`] — a `StandardMaterial` extension that fogs a mesh
//! (character, prop) so it matches the terrain fog on the `Low` tier (where the
//! fullscreen pass is off and meshes would otherwise be unfogged cutouts). Inert
//! on `High` (the fullscreen pass fogs meshes by depth). Kept in sync with the
//! tier by the crate; use via `ExtendedMaterial<StandardMaterial, _>`.

use bevy::{
    asset::{AssetPath, embedded_path},
    pbr::MaterialExtension,
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::ShaderRef,
};

use crate::height_fog::HeightFogParams;

#[derive(Asset, AsBindGroup, Reflect, Debug, Clone, Default)]
pub struct HeightFogExtension {
    /// Driven by the crate from the active tier (`apply_terrain_quality`); leave
    /// as `default()` when constructing the material.
    #[uniform(100)]
    pub(crate) fog: HeightFogParams,
}

impl MaterialExtension for HeightFogExtension {
    // Forward only (the Low/VR tier is forward). On High the fullscreen pass fogs
    // by depth, so the deferred path keeps StandardMaterial's default gbuffer.
    fn fragment_shader() -> ShaderRef {
        ShaderRef::Path(
            AssetPath::from_path_buf(embedded_path!("mesh_fog.wgsl")).with_source("embedded"),
        )
    }
}
