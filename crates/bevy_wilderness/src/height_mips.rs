//! A copy of the heightmap with a mip chain the geometry rings read, one texel
//! a vertex.
//!
//! A ring's vertices sit `2^level` heightmap texels apart, so a far ring's reads
//! of the full-size heightmap land thousands of texels from each other and miss
//! the GPU's texture cache every time. Mip `n` here keeps every `2^n`th texel,
//! unfiltered: a ring reading its own level gets the heights it always did,
//! from texels that sit side by side.

use std::collections::HashSet;

use bevy::{
    asset::RenderAssetUsages, pbr::ExtendedMaterial, prelude::*,
    render::render_resource::TextureFormat,
};

use crate::clipmap::{Clipmap, ClipmapMaterials};
use crate::material::GridMaterial;

/// Whether the geometry rings read their heights from a mip chain built off the
/// heightmap. Applies live. Costs a third more than the heightmap again in GPU
/// memory, and a rebuild and upload of the whole chain whenever the heightmap
/// changes, so leave it off while editing.
///
/// Only takes effect on a looping clipmap whose `base_scale` is a power-of-two
/// multiple of its `texel_size`, over a power-of-two `R16Unorm` heightmap with
/// CPU-resident data: there every ring vertex lands exactly on a texel of its
/// level. Anything else keeps reading the heightmap itself.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerrainHeightMips(pub bool);

/// The chain built for a clipmap's heightmap.
#[derive(Component)]
pub(crate) struct HeightMips {
    source: AssetId<Image>,
    image: Handle<Image>,
}

fn rings_land_on_texels(clipmap: &Clipmap) -> bool {
    let ratio = clipmap.base_scale / clipmap.texel_size;
    let whole = ratio.round();
    clipmap.looping
        && whole >= 1.0
        && (ratio - whole).abs() < 1e-6
        && (whole as u32).is_power_of_two()
}

/// `image` with mip `n` holding every `2^n`th texel, down to the level where
/// the shorter side is one texel.
fn point_sampled_chain(image: &Image) -> Option<Image> {
    let (width, height) = (image.width() as usize, image.height() as usize);
    if image.texture_descriptor.format != TextureFormat::R16Unorm
        || image.texture_descriptor.mip_level_count != 1
        || !width.is_power_of_two()
        || !height.is_power_of_two()
    {
        return None;
    }
    let texels = image.data.as_deref()?;
    let levels = width.min(height).trailing_zeros() + 1;
    let mut data = Vec::with_capacity(texels.len() / 3 * 4 + 2);
    data.extend_from_slice(texels);
    for level in 1..levels {
        let step = 1 << level;
        for row in texels.chunks_exact(width * 2).step_by(step) {
            for texel in row.chunks_exact(2).step_by(step) {
                data.extend_from_slice(texel);
            }
        }
    }
    let mut chain = Image {
        data: Some(data),
        asset_usage: RenderAssetUsages::RENDER_WORLD,
        sampler: image.sampler.clone(),
        texture_descriptor: image.texture_descriptor.clone(),
        ..Default::default()
    };
    chain.texture_descriptor.mip_level_count = levels;
    Some(chain)
}

/// Points each clipmap's materials at its heightmap, or at the chain built from
/// it, and sets `FLAG_HEIGHT_MIPS` to match. Also what carries a swapped
/// `Clipmap::heightmap` to the materials.
pub(crate) fn sync_height_source(
    mut commands: Commands,
    enabled: Res<TerrainHeightMips>,
    mut events: MessageReader<AssetEvent<Image>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
    clipmaps: Query<(Entity, &Clipmap, &ClipmapMaterials, Option<&HeightMips>)>,
) {
    let modified: HashSet<AssetId<Image>> = events
        .read()
        .filter_map(|event| match event {
            AssetEvent::Modified { id } => Some(*id),
            _ => None,
        })
        .collect();

    for (entity, clipmap, mats, mips) in &clipmaps {
        let source = clipmap.heightmap.id();
        let current = mips.filter(|mips| mips.source == source && !modified.contains(&source));
        let chain = if !(enabled.0 && rings_land_on_texels(clipmap)) {
            None
        } else if let Some(mips) = current {
            Some(mips.image.clone())
        } else {
            let built = images.get(source).and_then(point_sampled_chain);
            built.map(|chain| {
                debug!(
                    "terrain height mips: {} levels for {entity}",
                    chain.texture_descriptor.mip_level_count
                );
                let image = images.add(chain);
                commands.entity(entity).insert(HeightMips {
                    source,
                    image: image.clone(),
                });
                image
            })
        };
        if chain.is_none() && mips.is_some() {
            commands.entity(entity).remove::<HeightMips>();
        }

        let heightmap = chain.as_ref().unwrap_or(&clipmap.heightmap);
        for handle in [&mats.solid, &mats.wireframe] {
            // Check before `get_mut`: mutable access alone marks the material
            // modified and rebuilds its bind group.
            let in_sync = materials.get(handle).is_none_or(|m| {
                m.extension.heightmap == *heightmap
                    && (m.extension.flags & GridMaterial::FLAG_HEIGHT_MIPS != 0) == chain.is_some()
            });
            if !in_sync && let Some(mut material) = materials.get_mut(handle) {
                material.extension.heightmap = heightmap.clone();
                material.extension.flags &= !GridMaterial::FLAG_HEIGHT_MIPS;
                if chain.is_some() {
                    material.extension.flags |= GridMaterial::FLAG_HEIGHT_MIPS;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::render::render_resource::{Extent3d, TextureDimension};

    fn heightmap(width: u32, height: u32) -> Image {
        let data = (0..width * height)
            .flat_map(|i| (i as u16).to_le_bytes())
            .collect();
        Image::new(
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            data,
            TextureFormat::R16Unorm,
            RenderAssetUsages::default(),
        )
    }

    #[test]
    fn each_mip_keeps_every_second_texel_of_the_one_before() {
        let chain = point_sampled_chain(&heightmap(8, 4)).unwrap();
        assert_eq!(chain.texture_descriptor.mip_level_count, 3);
        let texels: Vec<u16> = chain
            .data
            .unwrap()
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(texels.len(), 32 + 8 + 2);
        assert_eq!(texels[32..40], [0, 2, 4, 6, 16, 18, 20, 22]);
        assert_eq!(texels[40..], [0, 4]);
    }

    #[test]
    fn sizes_that_would_not_halve_cleanly_get_no_chain() {
        assert!(point_sampled_chain(&heightmap(6, 4)).is_none());
    }
}
