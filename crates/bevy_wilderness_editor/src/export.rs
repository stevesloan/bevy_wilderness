//! Heightmap export (D7): write the authoritative field to disk, format
//! chosen by the requested path's extension:
//!
//! - **`.ktx2` (default)** — R16 KTX2 with an explicit `VK_FORMAT_R16_UNORM`,
//!   the exact format and load path of the shipped heightmap asset (same
//!   `min`/`max` encode range on the `Clipmap`, same `Heightfield`
//!   compatibility). This is the *renderer/editor master*: bevy 0.19 decodes
//!   16-bit grayscale PNG to `R16Uint` — not `R16Unorm` — so PNG cannot fill
//!   this role. The container is written by hand ([`write_ktx2`]): a
//!   single-level uncompressed KTX2 is ~40 lines and the `ktx2` parser crate
//!   has no writer.
//! - **`.png`** — 16-bit grayscale PNG, the *interchange copy* for anything
//!   that reads standard images off the engine's loader path: a game's
//!   physics pipeline building a collision heightfield (e.g. Avian), DCC
//!   tools, external terrain software.
//!
//! A host that needs both (game: KTX2 for the renderer, PNG for collision)
//! sends one [`ExportRequested`] per file — exports run concurrently; the
//! default UI's Export button does exactly that. Writes run on
//! `AsyncComputeTaskPool` (a 4096² map is ~33 MB) and land as
//! [`HeightmapExported`] messages.

use std::path::{Path, PathBuf};

use bevy::{
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task, TaskPool, block_on, poll_once},
};

use crate::terrain::EditableTerrain;

/// Request writing a terrain's heightmap to `path` in the background. A UI or
/// host writes this; completion arrives as [`HeightmapExported`]. Ignored
/// while an export to the same path for the same terrain is in flight.
#[derive(Message, Debug, Clone)]
pub struct ExportRequested {
    pub terrain: Entity,
    /// Where to write. A `.png` extension selects 16-bit grayscale PNG (the
    /// interchange copy); anything else gets R16 KTX2 (the engine master).
    pub path: PathBuf,
}

/// An export finished. `error` is `None` on success; a UI shows this as its
/// status line.
#[derive(Message, Debug, Clone)]
pub struct HeightmapExported {
    pub terrain: Entity,
    pub path: PathBuf,
    pub error: Option<String>,
}

/// The in-flight export writes.
#[derive(Resource, Default)]
pub(crate) struct ExportTasks(Vec<ExportTask>);

struct ExportTask {
    terrain: Entity,
    path: PathBuf,
    task: Task<Option<String>>,
}

/// Snapshot each requested terrain's quantized texels and spawn the write.
pub(crate) fn start_requested_exports(
    mut requests: MessageReader<ExportRequested>,
    mut tasks: ResMut<ExportTasks>,
    terrains: Query<&EditableTerrain>,
) {
    for request in requests.read() {
        let Ok(terrain) = terrains.get(request.terrain) else {
            continue;
        };
        if tasks
            .0
            .iter()
            .any(|t| t.terrain == request.terrain && t.path == request.path)
        {
            continue;
        }
        let texels = terrain.field.to_r16();
        let dims = terrain.field.dimensions();
        let path = request.path.clone();
        let task = AsyncComputeTaskPool::get_or_init(TaskPool::default)
            .spawn(async move { write_export(&path, dims, texels) });
        tasks.0.push(ExportTask {
            terrain: request.terrain,
            path: request.path.clone(),
            task,
        });
    }
}

/// Land finished exports as [`HeightmapExported`] messages.
pub(crate) fn poll_export_tasks(
    mut tasks: ResMut<ExportTasks>,
    mut exported: MessageWriter<HeightmapExported>,
) {
    tasks.0.retain_mut(|export| {
        let Some(error) = block_on(poll_once(&mut export.task)) else {
            return true;
        };
        exported.write(HeightmapExported {
            terrain: export.terrain,
            path: export.path.clone(),
            error,
        });
        false
    });
}

/// Write `texels` to `path`, format by extension; `None` on success, the
/// error message otherwise. `pub(crate)` so the load path (`swap.rs`) can
/// round-trip against the real writers in its tests.
pub(crate) fn write_export(path: &Path, dims: UVec2, texels: Vec<u16>) -> Option<String> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("png") => write_png(path, dims, texels),
        _ => std::fs::write(path, write_ktx2(dims, &texels))
            .err()
            .map(|e| e.to_string()),
    }
}

/// 16-bit grayscale PNG via the `image` crate (lossless for R16 texels).
fn write_png(path: &Path, dims: UVec2, texels: Vec<u16>) -> Option<String> {
    let Some(buffer) = image::ImageBuffer::<image::Luma<u16>, _>::from_raw(dims.x, dims.y, texels)
    else {
        return Some("texel buffer doesn't match dimensions".into());
    };
    buffer
        .save_with_format(path, image::ImageFormat::Png)
        .err()
        .map(|e| e.to_string())
}

/// `VK_FORMAT_R16_UNORM` — the explicit format bevy's KTX2 loader maps
/// straight to `TextureFormat::R16Unorm`, bypassing DFD interpretation.
const VK_FORMAT_R16_UNORM: u32 = 70;

/// Serialize a single-level, uncompressed `R16_UNORM` KTX2 (KTX 2.0 spec:
/// identifier, header, index, level index, DFD, then texel data). Offsets are
/// fixed because there's exactly one level and no key/value data or
/// supercompression.
fn write_ktx2(dims: UVec2, texels: &[u16]) -> Vec<u8> {
    let data_len = (texels.len() * 2) as u64;
    // identifier(12) + header(36) + index(32) + level index(24) = 104,
    // then the 44-byte DFD; data follows at 148 (4-byte aligned ✓).
    const DFD_OFFSET: u32 = 104;
    const DATA_OFFSET: u64 = 148;
    let mut out = Vec::with_capacity(DATA_OFFSET as usize + data_len as usize);

    // Identifier.
    out.extend_from_slice(&[
        0xAB, 0x4B, 0x54, 0x58, 0x20, 0x32, 0x30, 0xBB, 0x0D, 0x0A, 0x1A, 0x0A,
    ]);
    // Header.
    for value in [
        VK_FORMAT_R16_UNORM,
        2, // typeSize: 16-bit texels
        dims.x,
        dims.y,
        0, // pixelDepth: 2D
        0, // layerCount: not an array
        1, // faceCount: not a cubemap
        1, // levelCount: no mips
        0, // supercompressionScheme: none
    ] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    // Index: DFD present; no key/value data, no supercompression global data.
    out.extend_from_slice(&DFD_OFFSET.to_le_bytes());
    out.extend_from_slice(&44u32.to_le_bytes());
    out.extend_from_slice(&[0; 8]); // kvd offset + length
    out.extend_from_slice(&[0; 16]); // sgd offset + length
    // Level index (one level).
    for value in [DATA_OFFSET, data_len, data_len] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    // Data format descriptor: the Khronos basic block for one unsigned
    // 16-bit linear R sample — spec-required even though bevy trusts the
    // header's vkFormat instead.
    out.extend_from_slice(&44u32.to_le_bytes()); // dfdTotalSize
    out.extend_from_slice(&0u32.to_le_bytes()); // vendor 0 (Khronos), type 0 (basic)
    out.extend_from_slice(&(2u32 | (40 << 16)).to_le_bytes()); // version 2, block size 40
    out.extend_from_slice(&[1, 1, 1, 0]); // RGBSDA model, BT709, linear transfer, flags
    out.extend_from_slice(&[0; 4]); // texel block dimensions (1×1×1×1, stored -1)
    out.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 0]); // bytesPlane: 2 bytes/texel
    out.extend_from_slice(&0u16.to_le_bytes()); // sample 0: bitOffset
    out.extend_from_slice(&[15, 0]); // bitLength - 1; channel R, unsigned
    out.extend_from_slice(&[0; 4]); // samplePosition
    out.extend_from_slice(&0u32.to_le_bytes()); // sampleLower
    out.extend_from_slice(&65535u32.to_le_bytes()); // sampleUpper
    debug_assert_eq!(out.len() as u64, DATA_OFFSET);
    // Texel data.
    for texel in texels {
        out.extend_from_slice(&texel.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::TerrainField;
    use bevy::image::{CompressedImageFormats, ktx2_buffer_to_image};

    fn ramp_field() -> TerrainField {
        let mut field = TerrainField::flat(32, 32, 1.0, 0.0, 200.0, false, 0.0);
        for y in 0..32 {
            for x in 0..32 {
                field.set(x, y, (x + y) as f32 * 3.0);
            }
        }
        field
    }

    #[test]
    fn ktx2_round_trips_through_the_engine_loader() {
        // Field → KTX2 bytes → bevy's actual KTX2 decoder → field: the same
        // path a restart takes, so this is the D7 acceptance in miniature.
        let field = ramp_field();
        let bytes = write_ktx2(field.dimensions(), &field.to_r16());
        let image = ktx2_buffer_to_image(&bytes, CompressedImageFormats::NONE, false)
            .expect("bevy's KTX2 loader must accept the export");
        assert_eq!(
            image.texture_descriptor.format,
            bevy::render::render_resource::TextureFormat::R16Unorm,
            "must load as R16Unorm (the editable/renderable format)"
        );
        let back = TerrainField::from_image(&image, 1.0, 0.0, 200.0, false)
            .expect("the editor must be able to re-open the export");
        for y in 0..32 {
            for x in 0..32 {
                let (a, b) = (field.get(x, y), back.get(x, y));
                assert!(
                    (a - b).abs() < 200.0 / 65535.0,
                    "texel ({x},{y}): {a} vs {b}"
                );
            }
        }
    }

    #[test]
    fn ktx2_parses_with_expected_dimensions() {
        let bytes = write_ktx2(UVec2::new(8, 4), &[0u16; 32]);
        let reader = ktx2::Reader::new(&bytes).expect("must parse");
        let header = reader.header();
        assert_eq!(header.pixel_width, 8);
        assert_eq!(header.pixel_height, 4);
        assert_eq!(header.format, Some(ktx2::Format::R16_UNORM));
        assert_eq!(
            reader.levels().next().map(|level| level.data.len()),
            Some(64)
        );
    }

    #[test]
    fn png_export_is_lossless_16_bit() {
        // The interchange copy: a physics pipeline (Avian collision build)
        // reads this with a standard image decoder and must see the exact
        // same texels.
        let field = ramp_field();
        let texels = field.to_r16();
        let path = std::env::temp_dir().join("wilderness_export_test.png");
        assert_eq!(
            write_export(&path, field.dimensions(), texels.clone()),
            None,
            "png export must succeed"
        );
        let decoded = image::open(&path).expect("must reopen").into_luma16();
        assert_eq!(decoded.dimensions(), (32, 32));
        assert_eq!(
            decoded.into_raw(),
            texels,
            "PNG round-trip must be lossless"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn extension_picks_the_format() {
        let dims = UVec2::new(4, 4);
        let texels = vec![1234u16; 16];
        let dir = std::env::temp_dir();
        let ktx2_path = dir.join("wilderness_export_test.ktx2");
        let png_path = dir.join("wilderness_export_test2.PNG"); // case-insensitive
        assert_eq!(write_export(&ktx2_path, dims, texels.clone()), None);
        assert_eq!(write_export(&png_path, dims, texels), None);
        let ktx2_bytes = std::fs::read(&ktx2_path).unwrap();
        assert!(
            ktx2_bytes.starts_with(&[0xAB, 0x4B, 0x54, 0x58]),
            "KTX2 magic"
        );
        let png_bytes = std::fs::read(&png_path).unwrap();
        assert!(
            png_bytes.starts_with(&[0x89, b'P', b'N', b'G']),
            "PNG magic"
        );
        std::fs::remove_file(&ktx2_path).ok();
        std::fs::remove_file(&png_path).ok();
    }

    #[test]
    fn write_export_reports_errors() {
        let error = write_export(
            Path::new("/nonexistent-dir/export.ktx2"),
            UVec2::new(2, 2),
            vec![0; 4],
        );
        assert!(error.is_some(), "unwritable path must report an error");
    }
}
