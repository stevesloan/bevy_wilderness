//! Real-world terrain import: fill the map with actual Earth elevation
//! centered on a latitude/longitude — the fastest route to base shapes with
//! believable ridge hierarchy for the erosion tool to sharpen, since nature
//! already ran the weathering at full fidelity.
//!
//! Data comes from the public AWS Open Data **Terrain Tiles** set (the former
//! Mapzen tiles): 256² web-mercator PNGs in *terrarium* encoding
//! (`height = R·256 + G + B/256 − 32768` meters), fetched over HTTPS with no
//! API key. The import picks the zoom whose ground resolution best matches
//! the terrain's texel size (capped so a run never needs more than
//! [`MAX_TILES`] tiles), fetches the covering tile rect in parallel on
//! `AsyncComputeTaskPool`, and bilinearly resamples into the terrain's own
//! resolution — north up (world −Z), the map's world extent mapped 1:1 to
//! real meters on the ground.
//!
//! Heights land *relative*: the region's lowest sample sits at the encode
//! range's floor and real relief rises from there, scaled by
//! [`WorldImportSettings::vertical_scale`]; peaks that would overflow the
//! encode range clamp (with a log warning). The finished field replaces the
//! terrain through the same swap path as a file load — display heightmap,
//! overlay, re-bake, prop re-snap, and a cleared undo history (its tile
//! snapshots describe the replaced field) all follow.
//!
//! The tile fetcher is injected (`Fetch`), so tests exercise the whole
//! zoom/stitch/resample pipeline against synthetic tiles, offline.

use std::io::Read;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use bevy::{
    prelude::*,
    tasks::{AsyncComputeTaskPool, Task, TaskPool, block_on, poll_once},
};
use bevy_wilderness::Clipmap;

use crate::field::TerrainField;
use crate::gesture::TerrainGesture;
use crate::swap::swap_in;
use crate::terrain::EditableTerrain;
use crate::undo::UndoHistory;

/// Tile grid edge, pixels.
const TILE: usize = 256;

/// Cap on tiles fetched per import (~100 MB decoded); the zoom steps down
/// until the covering rect fits, trading resolution for coverage.
const MAX_TILES: u64 = 400;

/// Deepest zoom requested. The global datasets behind the tile set (SRTM et
/// al.) carry real detail to about z14 (~10 m/px); deeper just upsamples.
const MAX_ZOOM: u8 = 14;

/// Meters per degree of latitude (and of longitude at the equator).
const METERS_PER_DEGREE: f64 = 111_320.0;

/// Web-mercator ground resolution at the equator for 256-pixel tiles at
/// zoom 0, meters per pixel.
const ZOOM0_RESOLUTION: f64 = 156_543.033_92;

/// Where an import centers and how it scales (a resource a UI edits).
#[derive(Resource, Clone, Debug)]
pub struct WorldImportSettings {
    /// Degrees north; the imported region is centered here.
    pub latitude: f64,
    /// Degrees east.
    pub longitude: f64,
    /// Vertical exaggeration: 1 = true relief (clamped into the encode
    /// range), lower flattens, higher dramatizes.
    pub vertical_scale: f32,
    /// Condition the import to tile without a seam (see [`crate::periodic`]).
    /// A crop of Earth is not periodic, so a looping terrain otherwise repeats
    /// against a cliff hundreds of meters high. Ignored on finite terrains.
    pub seamless: bool,
}

impl Default for WorldImportSettings {
    fn default() -> Self {
        Self {
            // The Matterhorn — unmistakable relief for a first import.
            latitude: 45.9766,
            longitude: 7.6585,
            vertical_scale: 1.0,
            seamless: true,
        }
    }
}

/// Import real-world terrain into a terrain entity (e.g. a UI's "Import"
/// button). Ignored while that terrain already has a [`WorldImportRun`] in
/// flight; completion arrives as [`WorldImported`].
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldImportRequested {
    pub terrain: Entity,
}

/// A [`WorldImportRequested`] finished. `error` is `None` on success; a UI
/// shows this as its status line.
#[derive(Message, Debug, Clone)]
pub struct WorldImported {
    pub terrain: Entity,
    pub error: Option<String>,
}

/// Present on a terrain while its import downloads and resamples. A UI reads
/// [`progress`](Self::progress) for a bar; the component disappears when the
/// new field lands.
#[derive(Component)]
pub struct WorldImportRun {
    task: Task<Result<Vec<f32>, String>>,
    tiles_done: Arc<AtomicU32>,
    tiles_total: u32,
}

impl WorldImportRun {
    /// Fraction of the download done (0..1); the resample after the last
    /// tile is quick, so expect a short beat at 1.0.
    pub fn progress(&self) -> f32 {
        self.tiles_done.load(Ordering::Relaxed) as f32 / self.tiles_total.max(1) as f32
    }
}

/// A tile fetcher: `(zoom, x, y) -> PNG bytes`. Injected so tests can serve
/// synthetic tiles; the real one HTTP-GETs the AWS terrain tile set.
type Fetch = Arc<dyn Fn(u8, u64, u64) -> Result<Vec<u8>, String> + Send + Sync>;

/// The production fetcher against the public S3 bucket.
fn aws_fetch() -> Fetch {
    let agent = ureq::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("bevy_wilderness_editor")
        .build();
    Arc::new(move |zoom, x, y| {
        let url = format!("https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{zoom}/{x}/{y}.png");
        // One retry: transient S3 hiccups are common enough to be worth it.
        let mut last_error = String::new();
        for _ in 0..2 {
            match agent.get(&url).call() {
                Ok(response) => {
                    let mut bytes = Vec::new();
                    match response
                        .into_reader()
                        .take(8 * 1024 * 1024)
                        .read_to_end(&mut bytes)
                    {
                        Ok(_) => return Ok(bytes),
                        Err(error) => last_error = format!("{url}: {error}"),
                    }
                }
                Err(error) => last_error = format!("{url}: {error}"),
            }
        }
        Err(last_error)
    })
}

/// Spawn the download/resample task for each requested terrain.
pub(crate) fn start_requested_imports(
    mut commands: Commands,
    mut requests: MessageReader<WorldImportRequested>,
    settings: Res<WorldImportSettings>,
    terrains: Query<(&EditableTerrain, Has<WorldImportRun>)>,
) {
    let mut started = Vec::new();
    for request in requests.read() {
        let Ok((terrain, running)) = terrains.get(request.terrain) else {
            continue;
        };
        if running || started.contains(&request.terrain) {
            continue;
        }
        started.push(request.terrain);
        let job = ImportJob::new(terrain, &settings, aws_fetch());
        let tiles_done = job.done.clone();
        let tiles_total = job.tile_count() as u32;
        let task = AsyncComputeTaskPool::get_or_init(TaskPool::default).spawn(job.run_async());
        commands.entity(request.terrain).insert(WorldImportRun {
            task,
            tiles_done,
            tiles_total,
        });
    }
}

/// Land finished imports: build the new field and swap it in exactly like a
/// file load. Runs in the pre-Pick swap slot so the fresh terrain is in
/// place before this frame's pick and tools.
pub(crate) fn apply_finished_imports(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut history: ResMut<UndoHistory>,
    mut gesture: ResMut<TerrainGesture>,
    mut finished: MessageWriter<WorldImported>,
    mut terrains: Query<(Entity, &mut Clipmap, &EditableTerrain, &mut WorldImportRun)>,
) {
    for (entity, mut clipmap, terrain, mut run) in &mut terrains {
        let Some(result) = block_on(poll_once(&mut run.task)) else {
            continue;
        };
        commands.entity(entity).remove::<WorldImportRun>();
        let error = match result {
            Ok(heights) => {
                let old = &terrain.field;
                let mut field = TerrainField::flat(
                    old.dimensions().x,
                    old.dimensions().y,
                    old.texel_size(),
                    clipmap.min,
                    clipmap.max,
                    clipmap.looping,
                    0.0,
                );
                field.paste_rect(field.full_rect(), &heights);
                let heightmap = images.add(field.to_image());
                swap_in(
                    &mut commands,
                    entity,
                    &mut clipmap,
                    &mut history,
                    &mut gesture,
                    field,
                    heightmap,
                );
                None
            }
            Err(error) => Some(error),
        };
        finished.write(WorldImported {
            terrain: entity,
            error,
        });
    }
}

/// Everything the background task needs, snapshotted at request time.
struct ImportJob {
    dims: UVec2,
    texel_size: f32,
    /// The R16 encode range the heights must land inside.
    min: f32,
    max: f32,
    latitude: f64,
    longitude: f64,
    vertical_scale: f32,
    seamless: bool,
    zoom: u8,
    fetch: Fetch,
    done: Arc<AtomicU32>,
}

impl ImportJob {
    fn new(terrain: &EditableTerrain, settings: &WorldImportSettings, fetch: Fetch) -> Self {
        let field = &terrain.field;
        let (min, max) = field.min_max();
        let latitude = settings.latitude.clamp(-85.0, 85.0);
        let mut job = Self {
            dims: field.dimensions(),
            texel_size: field.texel_size(),
            min,
            max,
            latitude,
            longitude: settings.longitude,
            vertical_scale: settings.vertical_scale,
            // A finite terrain has no repeat, so nothing to reconcile.
            seamless: settings.seamless && field.looping(),
            zoom: 0,
            fetch,
            done: Arc::new(AtomicU32::new(0)),
        };
        // Zoom whose ground resolution best matches the texel size, stepped
        // down until the covering tile rect fits the budget.
        let target = job.texel_size as f64;
        let ideal = (ZOOM0_RESOLUTION * job.latitude.to_radians().cos() / target).log2();
        let mut zoom = (ideal.round() as i32).clamp(0, MAX_ZOOM as i32) as u8;
        loop {
            job.zoom = zoom;
            if job.tile_count() <= MAX_TILES || zoom == 0 {
                break;
            }
            zoom -= 1;
        }
        job
    }

    /// The covering tile rect at the current zoom: `(tx0, ty0, tx1, ty1)`,
    /// inclusive, with a one-pixel bilinear apron. `tx` may exceed the tile
    /// grid (it wraps around the antimeridian at fetch time); `ty` clamps.
    fn tile_rect(&self) -> (i64, i64, i64, i64) {
        let (px0, py0) = self.global_pixel(0.5, 0.5);
        let (px1, py1) = self.global_pixel(self.dims.x as f64 - 0.5, self.dims.y as f64 - 0.5);
        let max_tile = (1i64 << self.zoom) - 1;
        let tx0 = ((px0.min(px1) - 1.0) / TILE as f64).floor() as i64;
        let tx1 = ((px0.max(px1) + 1.0) / TILE as f64).floor() as i64;
        let ty0 = (((py0.min(py1) - 1.0) / TILE as f64).floor() as i64).clamp(0, max_tile);
        let ty1 = (((py0.max(py1) + 1.0) / TILE as f64).floor() as i64).clamp(0, max_tile);
        (tx0, ty0, tx1, ty1)
    }

    fn tile_count(&self) -> u64 {
        let (tx0, ty0, tx1, ty1) = self.tile_rect();
        (tx1 - tx0 + 1) as u64 * (ty1 - ty0 + 1) as u64
    }

    /// Texel center → global web-mercator pixel at the job's zoom. Texel +x
    /// is east; texel +y is south (world +Z), so maps import north-up.
    fn global_pixel(&self, texel_x: f64, texel_y: f64) -> (f64, f64) {
        let east = (texel_x - self.dims.x as f64 * 0.5) * self.texel_size as f64;
        let south = (texel_y - self.dims.y as f64 * 0.5) * self.texel_size as f64;
        let latitude = self.latitude - south / METERS_PER_DEGREE;
        let longitude =
            self.longitude + east / (METERS_PER_DEGREE * self.latitude.to_radians().cos());
        let world = (TILE as f64) * (1u64 << self.zoom) as f64;
        let x = (longitude + 180.0) / 360.0 * world;
        let lat_rad = latitude.clamp(-85.051, 85.051).to_radians();
        let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0
            * world;
        (x, y)
    }

    async fn run_async(self) -> Result<Vec<f32>, String> {
        self.run()
    }

    fn run(self) -> Result<Vec<f32>, String> {
        let (tx0, ty0, tx1, ty1) = self.tile_rect();
        let tiles_x = (tx1 - tx0 + 1) as usize;
        let tiles_y = (ty1 - ty0 + 1) as usize;
        let max_tile = 1u64 << self.zoom;

        // Fetch and decode every covering tile, parallel over the pool.
        let pool = AsyncComputeTaskPool::get_or_init(TaskPool::default);
        let coords: Vec<(i64, i64)> = (0..tiles_y)
            .flat_map(|ty| (0..tiles_x).map(move |tx| (tx0 + tx as i64, ty0 + ty as i64)))
            .collect();
        let job = &self;
        let decoded: Vec<Result<Vec<f32>, String>> = pool
            .scope(|scope| {
                for &(tx, ty) in &coords {
                    scope.spawn(async move {
                        // Longitude wraps around the antimeridian.
                        let x = tx.rem_euclid(max_tile as i64) as u64;
                        let bytes = (job.fetch)(job.zoom, x, ty as u64)?;
                        let tile = decode_terrarium(&bytes)?;
                        job.done.fetch_add(1, Ordering::Relaxed);
                        Ok(tile)
                    });
                }
            })
            .into_iter()
            .collect();

        // Stitch into one buffer for painless bilinear sampling.
        let (width, height) = (tiles_x * TILE, tiles_y * TILE);
        let mut stitched = vec![0.0f32; width * height];
        for (i, tile) in decoded.into_iter().enumerate() {
            let tile = tile?;
            let (bx, by) = ((i % tiles_x) * TILE, (i / tiles_x) * TILE);
            for row in 0..TILE {
                let src = row * TILE;
                let dst = (by + row) * width + bx;
                stitched[dst..dst + TILE].copy_from_slice(&tile[src..src + TILE]);
            }
        }

        // Resample into the terrain's own grid, parallel over rows.
        let dims = self.dims;
        let origin = (tx0 as f64 * TILE as f64, ty0 as f64 * TILE as f64);
        let mut heights = vec![0.0f32; (dims.x * dims.y) as usize];
        let rows_per_batch = (dims.y as usize).div_ceil(pool.thread_num().max(1));
        pool.scope(|scope| {
            for (batch, chunk) in heights.chunks_mut(rows_per_batch * dims.x as usize).enumerate() {
                let row0 = batch * rows_per_batch;
                let stitched = &stitched;
                scope.spawn(async move {
                    for (i, out) in chunk.iter_mut().enumerate() {
                        let x = (i % dims.x as usize) as f64 + 0.5;
                        let y = (row0 + i / dims.x as usize) as f64 + 0.5;
                        let (px, py) = job.global_pixel(x, y);
                        *out = sample_bilinear(
                            stitched,
                            width,
                            height,
                            px - origin.0 - 0.5,
                            py - origin.1 - 0.5,
                        );
                    }
                });
            }
        });

        if self.seamless {
            crate::periodic::make_periodic(&mut heights, dims.x as usize, dims.y as usize);
        }

        // Relative landing: the region's floor sits at the encode minimum,
        // relief rises from there (scaled), clamped into the encode range.
        let floor = heights.iter().copied().fold(f32::MAX, f32::min);
        let mut clipped = 0usize;
        for h in &mut heights {
            let scaled = self.min + (*h - floor) * self.vertical_scale;
            if scaled > self.max {
                clipped += 1;
            }
            *h = scaled.clamp(self.min, self.max);
        }
        if clipped > 0 {
            warn!(
                "bevy_wilderness_editor: world import clipped {clipped} texels at the encode \
                 ceiling — lower vertical scale or widen the terrain's min/max range"
            );
        }
        Ok(heights)
    }
}

/// Decode one terrarium PNG into meters: `R·256 + G + B/256 − 32768`.
fn decode_terrarium(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let rgb = image::load_from_memory(bytes)
        .map_err(|e| format!("tile decode: {e}"))?
        .into_rgb8();
    if (rgb.width() as usize, rgb.height() as usize) != (TILE, TILE) {
        return Err(format!(
            "tile is {}×{}, expected {TILE}²",
            rgb.width(),
            rgb.height()
        ));
    }
    Ok(rgb
        .pixels()
        .map(|p| p.0[0] as f32 * 256.0 + p.0[1] as f32 + p.0[2] as f32 / 256.0 - 32768.0)
        .collect())
}

/// Clamped bilinear read of the stitched buffer at fractional pixel (x, y).
fn sample_bilinear(buffer: &[f32], width: usize, height: usize, x: f64, y: f64) -> f32 {
    let x0 = x.floor();
    let y0 = y.floor();
    let (fx, fy) = ((x - x0) as f32, (y - y0) as f32);
    let at = |ix: i64, iy: i64| {
        let ix = ix.clamp(0, width as i64 - 1) as usize;
        let iy = iy.clamp(0, height as i64 - 1) as usize;
        buffer[iy * width + ix]
    };
    let (x0, y0) = (x0 as i64, y0 as i64);
    let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx;
    let bottom = at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx;
    top * (1.0 - fy) + bottom * fy
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terrain(size: u32, texel: f32, min: f32, max: f32) -> EditableTerrain {
        EditableTerrain::new(TerrainField::flat(size, size, texel, min, max, false, 0.0))
    }

    fn settings(latitude: f64, longitude: f64) -> WorldImportSettings {
        WorldImportSettings {
            latitude,
            longitude,
            vertical_scale: 1.0,
            seamless: false,
        }
    }

    /// Meters of elevation per global mercator pixel in the synthetic ramp —
    /// small, so even deep-zoom global pixel coordinates stay inside
    /// terrarium's ±32 km encodable range.
    const RAMP: f64 = 0.01;

    /// A synthetic tile source: elevation rises east at [`RAMP`] meters per
    /// global pixel, encoded exactly as terrarium PNG bytes — a
    /// world-spanning ramp the resampler must reproduce.
    fn ramp_fetch() -> Fetch {
        Arc::new(|_zoom, tx, ty| {
            let mut rgb = image::RgbImage::new(TILE as u32, TILE as u32);
            for (px, _py, pixel) in rgb.enumerate_pixels_mut() {
                let h = (tx * TILE as u64 + px as u64) as f64 * RAMP;
                let raw = ((h + 32768.0) * 256.0).round() as u32;
                let _ = ty;
                pixel.0 = [(raw >> 16) as u8, (raw >> 8) as u8, raw as u8];
            }
            let mut bytes = Vec::new();
            rgb.write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .map_err(|e| e.to_string())?;
            Ok(bytes)
        })
    }

    #[test]
    fn terrarium_decoding_round_trips() {
        let mut rgb = image::RgbImage::new(TILE as u32, TILE as u32);
        // 0 m, Everest-ish, below-sea, and a fractional value.
        let cases = [(0u32, 0.0f32), (1, 8848.0), (2, -415.0), (3, 100.5)];
        for &(i, meters) in &cases {
            let raw = ((meters as f64) + 32768.0) * 256.0;
            let raw = raw.round() as u32;
            rgb.put_pixel(
                i,
                0,
                image::Rgb([(raw >> 16) as u8, (raw >> 8) as u8, raw as u8]),
            );
        }
        let mut bytes = Vec::new();
        rgb.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        let tile = decode_terrarium(&bytes).expect("must decode");
        for &(i, meters) in &cases {
            assert!(
                (tile[i as usize] - meters).abs() < 0.01,
                "pixel {i}: {} vs {meters}",
                tile[i as usize]
            );
        }
    }

    #[test]
    fn zoom_matches_texel_resolution() {
        // 30 m texels at the Matterhorn's latitude: z12 is ~26.6 m/px there.
        let terrain = terrain(256, 30.0, 0.0, 5000.0);
        let job = ImportJob::new(&terrain, &settings(45.98, 7.66), ramp_fetch());
        assert_eq!(job.zoom, 12, "expected z12 for 30 m texels at 46°N");
        // Coarse texels on a huge map pick a shallow zoom, never exceeding
        // the tile budget.
        let coarse = EditableTerrain::new(TerrainField::flat(
            4096, 4096, 100.0, 0.0, 5000.0, false, 0.0,
        ));
        let job = ImportJob::new(&coarse, &settings(45.98, 7.66), ramp_fetch());
        assert!(job.tile_count() <= MAX_TILES, "{} tiles", job.tile_count());
    }

    /// The full pipeline against the synthetic ramp: heights must rise
    /// monotonically eastward, floored at the encode minimum, and the total
    /// rise must match the ramp's slope over the map's real-world width.
    #[test]
    fn import_resamples_the_ramp() {
        let terrain = terrain(64, 100.0, -50.0, 10_000.0);
        let job = ImportJob::new(&terrain, &settings(0.0, 0.0), ramp_fetch());
        let zoom = job.zoom;
        let heights = job.run().expect("synthetic import must succeed");
        for y in 0..64usize {
            for x in 1..64usize {
                assert!(
                    heights[y * 64 + x] >= heights[y * 64 + x - 1] - 1e-3,
                    "({x},{y}) must not descend eastward"
                );
            }
        }
        let floor = heights.iter().copied().fold(f32::MAX, f32::min);
        assert!((floor - -50.0).abs() < 1e-3, "floor at encode min: {floor}");
        // The ramp rises RAMP meters per mercator pixel; the map is 6.4 km
        // of ground, i.e. that many pixels at this zoom's resolution.
        let meters_per_pixel = ZOOM0_RESOLUTION / (1u64 << zoom) as f64;
        let expected_rise = 64.0 * 100.0 / meters_per_pixel * RAMP;
        let rise = (heights[63] - heights[0]) as f64;
        assert!(
            (rise - expected_rise).abs() / expected_rise < 0.05,
            "rise {rise:.1} vs expected {expected_rise:.1}"
        );
    }

    /// A looping terrain repeats forever, so a crop of a non-periodic world
    /// meets itself at a cliff. Conditioning must bring that step down to the
    /// scale of an ordinary step inside the map — the point at which it stops
    /// reading as an edge.
    #[test]
    fn seamless_import_closes_the_wrap() {
        let looping = EditableTerrain::new(TerrainField::flat(
            64, 64, 100.0, 0.0, 10_000.0, true, 0.0,
        ));
        // The synthetic ramp rises eastward, so west and east disagree by the
        // map's full span.
        let seam_and_step = |seamless: bool| {
            let mut cfg = settings(0.0, 0.0);
            cfg.seamless = seamless;
            let heights = ImportJob::new(&looping, &cfg, ramp_fetch())
                .run()
                .expect("must run");
            let seam: f32 = (0..64)
                .map(|y| (heights[y * 64] - heights[y * 64 + 63]).powi(2))
                .sum::<f32>()
                / 64.0;
            let step: f32 = (0..64)
                .flat_map(|y| (1..64).map(move |x| (y, x)))
                .map(|(y, x)| (heights[y * 64 + x] - heights[y * 64 + x - 1]).powi(2))
                .sum::<f32>()
                / (64.0 * 63.0);
            (seam.sqrt(), step.sqrt())
        };

        let (raw, step) = seam_and_step(false);
        assert!(
            raw > step * 20.0,
            "an unconditioned import should wrap against a cliff: {raw} vs step {step}"
        );
        // The ramp is pure trend, so conditioning leaves almost nothing behind
        // and the residual step is not a meaningful yardstick; that the wrap
        // collapses at all is what this checks. `periodic` covers the
        // "falls to interior scale" bar on a field that has detail to keep.
        let (conditioned, _) = seam_and_step(true);
        assert!(
            conditioned < raw / 20.0,
            "conditioning must collapse the wrap: {conditioned} vs raw {raw}"
        );
    }

    /// Finite terrain never repeats, so there is no seam to reconcile and the
    /// trend removal would only cost relief.
    #[test]
    fn seamless_is_inert_on_finite_terrain() {
        let finite = terrain(32, 100.0, 0.0, 10_000.0);
        let mut cfg = settings(0.0, 0.0);
        cfg.seamless = true;
        assert!(!ImportJob::new(&finite, &cfg, ramp_fetch()).seamless);
    }

    #[test]
    fn vertical_scale_and_clamping_apply() {
        // Wide encode range: doubling the scale doubles the relief.
        let base = terrain(32, 100.0, 0.0, 10_000.0);
        let peak_at = |scale: f32| {
            let mut cfg = settings(0.0, 0.0);
            cfg.vertical_scale = scale;
            let heights = ImportJob::new(&base, &cfg, ramp_fetch())
                .run()
                .expect("must run");
            heights.iter().copied().fold(f32::MIN, f32::max)
        };
        let single = peak_at(1.0);
        let double = peak_at(2.0);
        assert!(single > 0.01, "ramp must produce relief: {single}");
        assert!(
            (double - single * 2.0).abs() < single * 0.05,
            "2× scale must double relief: {double} vs 2×{single}"
        );
        // Tiny encode range: over-range relief clamps at the ceiling.
        let tiny = terrain(32, 100.0, 0.0, 0.1);
        let mut cfg = settings(0.0, 0.0);
        cfg.vertical_scale = 2.0;
        let heights = ImportJob::new(&tiny, &cfg, ramp_fetch())
            .run()
            .expect("must run");
        let peak = heights.iter().copied().fold(f32::MIN, f32::max);
        assert_eq!(peak, 0.1, "over-range relief must clamp to the ceiling");
    }

    #[test]
    fn fetch_failure_reports_not_panics() {
        let terrain = terrain(32, 30.0, 0.0, 5000.0);
        let fetch: Fetch = Arc::new(|_, _, _| Err("offline".into()));
        let error = ImportJob::new(&terrain, &settings(45.98, 7.66), fetch)
            .run()
            .expect_err("must propagate the fetch error");
        assert!(error.contains("offline"));
    }
}
