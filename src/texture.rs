//! Loading and mip-generation for the tiling terrain texture arrays.

use std::path::Path;

use bevy::{
    asset::RenderAssetUsages,
    image::{
        CompressedImageFormats, ImageAddressMode, ImageFilterMode, ImageSampler,
        ImageSamplerDescriptor, ImageType,
    },
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};

/// Linear + Repeat sampler for a looping clipmap's RVT targets, so the baked
/// albedo/normal/AO tile toroidally as the terrain repeats. No anisotropy — the
/// RVT is sampled at a fixed `uv`, not tiled per-fragment like the layer arrays.
pub(crate) fn looping_rvt_sampler() -> ImageSampler {
    ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        ..default()
    })
}

/// Repeat + anisotropic sampler for the tiling terrain layer arrays.
pub(crate) fn terrain_tiling_sampler() -> ImageSampler {
    ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        anisotropy_clamp: 8,
        ..default()
    })
}

/// Decodes one image file per layer and stacks them into a tiling `2d_array`
/// for a [`Clipmap`](crate::Clipmap)'s `albedo_array` / `normal_array` /
/// `orm_array`, generating a full mip chain (file formats like PNG carry none,
/// and the RVT bake samples these heavily minified — without mips the result
/// aliases into noise).
///
/// Pass one path per terrain layer, in the same order as
/// [`Clipmap::layers`](crate::Clipmap::layers). All images must share the same
/// dimensions (this decodes but does not resample — export your set at a single
/// resolution).
///
/// Set `srgb` to `true` for color/albedo maps and `false` for normal and ORM
/// maps, which hold linear data. ORM maps pack occlusion, roughness, metallic
/// into R, G, B (metallic is ~0 for terrain); build them from the separate
/// AO/roughness files that texture sites ship.
///
/// This reads files synchronously and is meant for one-time setup. It panics on
/// a missing/undecodable file or a dimension mismatch — asset-authoring errors
/// worth surfacing immediately at startup.
///
/// ```no_run
/// # use bevy::prelude::*;
/// # use bevy_wilderness::load_terrain_array;
/// # fn setup(mut images: ResMut<Assets<Image>>) {
/// let albedo = load_terrain_array(
///     &mut images,
///     &["terrain/grass_albedo.png", "terrain/rock_albedo.png"],
///     true,
/// );
/// # }
/// ```
pub fn load_terrain_array(
    images: &mut Assets<Image>,
    paths: &[impl AsRef<Path>],
    srgb: bool,
) -> Handle<Image> {
    assert!(
        !paths.is_empty(),
        "load_terrain_array needs at least one layer"
    );
    let layers = paths
        .iter()
        .map(|path| {
            let path = path.as_ref();
            let bytes = std::fs::read(path)
                .unwrap_or_else(|e| panic!("load_terrain_array: reading {}: {e}", path.display()));
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_else(|| {
                    panic!(
                        "load_terrain_array: {} has no file extension",
                        path.display()
                    )
                });
            Image::from_buffer(
                &bytes,
                ImageType::Extension(ext),
                CompressedImageFormats::NONE,
                srgb,
                ImageSampler::Default,
                RenderAssetUsages::RENDER_WORLD,
            )
            .unwrap_or_else(|e| panic!("load_terrain_array: decoding {}: {e:?}", path.display()))
        })
        .collect::<Vec<_>>();
    images.add(build_terrain_array(&layers, srgb))
}

/// The pure core of [`load_terrain_array`]: stacks already-decoded per-layer
/// images into a tiling `2d_array` [`Image`] with a full mip chain, touching
/// neither the filesystem nor [`Assets`]. That makes it runnable off the main
/// thread — decode the layers via the `AssetServer` (or any source), hand the
/// [`Image`]s here on e.g. an `AsyncComputeTaskPool`, then `images.add` the
/// returned [`Image`] back on the main thread — so the decode + mip generation
/// don't stall the main schedule.
///
/// Layers are stacked in slice order (same order as
/// [`Clipmap::layers`](crate::Clipmap::layers)). Each is converted to RGBA8 in
/// the `srgb` color space (`true` for albedo, `false` for normal/ORM); all must
/// share dimensions (this does not resample). Panics on an empty slice, a format
/// that can't convert to RGBA8, or a dimension mismatch.
pub fn build_terrain_array(layers: &[Image], srgb: bool) -> Image {
    assert!(
        !layers.is_empty(),
        "build_terrain_array needs at least one layer"
    );
    let format = if srgb {
        TextureFormat::Rgba8UnormSrgb
    } else {
        TextureFormat::Rgba8Unorm
    };

    let mut stacked = Vec::new();
    let mut dims: Option<(u32, u32)> = None;
    for (i, layer) in layers.iter().enumerate() {
        // Most sources are already 8-bit RGBA in the target color space; convert
        // anything else (e.g. 16-bit) so every layer matches `format`.
        let converted;
        let image = if layer.texture_descriptor.format == format {
            layer
        } else {
            converted = layer.convert(format).unwrap_or_else(|| {
                panic!(
                    "build_terrain_array: layer {i} is {:?}, which can't convert to RGBA8 — re-export as 8-bit",
                    layer.texture_descriptor.format,
                )
            });
            &converted
        };

        let size = (image.width(), image.height());
        if let Some(first) = dims {
            assert!(
                first == size,
                "build_terrain_array: layer {i} is {size:?} but earlier layers are {first:?}; all layers must share dimensions",
            );
        } else {
            dims = Some(size);
        }
        // Layer-major: each layer's full mip chain, then the next layer's.
        let mip0 = image
            .data
            .as_deref()
            .expect("decoded image is uncompressed and has pixel data");
        stacked.extend_from_slice(mip0);
        let mut level = mip0.to_vec();
        let (mut w, mut h) = size;
        while w > 1 || h > 1 {
            level = downsample_rgba8(&level, w, h, srgb);
            w = (w / 2).max(1);
            h = (h / 2).max(1);
            stacked.extend_from_slice(&level);
        }
    }

    let (width, height) = dims.unwrap();
    let mut array = Image::default();
    array.data = Some(stacked);
    array.texture_descriptor.size = Extent3d {
        width,
        height,
        depth_or_array_layers: layers.len() as u32,
    };
    array.texture_descriptor.dimension = TextureDimension::D2;
    array.texture_descriptor.format = format;
    array.texture_descriptor.mip_level_count = 32 - width.max(height).leading_zeros();
    array.asset_usage = RenderAssetUsages::RENDER_WORLD;
    array.sampler = terrain_tiling_sampler();
    array
}

/// Box-filters one RGBA8 mip level into the next. sRGB data is averaged in
/// roughly-linear space (averaging encoded bytes skews dark); gamma 2.0
/// (square/sqrt) stands in for the sRGB curve — indistinguishable for mip
/// averaging and much cheaper than the exact transfer function. Alpha is
/// always averaged linearly.
fn downsample_rgba8(src: &[u8], w: u32, h: u32, srgb: bool) -> Vec<u8> {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = Vec::with_capacity((nw * nh * 4) as usize);
    for y in 0..nh {
        for x in 0..nw {
            // Clamp so odd dimensions reuse the last row/column.
            let (x0, y0) = (2 * x, 2 * y);
            let (x1, y1) = ((2 * x + 1).min(w - 1), (2 * y + 1).min(h - 1));
            for c in 0..4 {
                let at = |px: u32, py: u32| src[((py * w + px) * 4 + c) as usize] as u32;
                let (a, b, cc, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
                let avg = if srgb && c < 3 {
                    (((a * a + b * b + cc * cc + d * d) as f32 / 4.0).sqrt() + 0.5) as u32
                } else {
                    (a + b + cc + d) / 4
                };
                out.push(avg as u8);
            }
        }
    }
    out
}
