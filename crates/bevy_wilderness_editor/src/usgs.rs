//! Importing USGS 3DEP 1 m elevation for a map's footprint.
//!
//! The global terrarium set tops out near 10 m on the ground, which is coarser
//! than an FPS-scale terrain's texels — a 2 m map upsamples it about four
//! times and comes out smooth at exactly the scale a player sees. Over the
//! United States, 3DEP lidar carries true 1 m detail, so where it reaches it
//! is the better source by a wide margin.
//!
//! Coverage is patchy and the products are not on a predictable grid, so a
//! run starts by asking the National Map what covers the map's bounding box,
//! then reads those GeoTIFFs directly out of the public S3 bucket (see
//! [`crate::cog`]). Everything is injected, so the discovery, projection and
//! stitching are all testable offline.

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use bevy::tasks::{AsyncComputeTaskPool, TaskPool};

use crate::cog::{ElevationCog, RangeFetch, Window};
use crate::utm;
use crate::world::Framing;

/// Fetch a whole URL. Injected so tests answer product queries offline.
pub(crate) type HttpGet = Arc<dyn Fn(&str) -> Result<Vec<u8>, String> + Send + Sync>;

/// The National Map's product query, and the dataset naming 1 m elevation.
const TNM_PRODUCTS: &str = "https://tnmaccess.nationalmap.gov/api/v1/products";
const ONE_METER_DEM: &str = "Digital%20Elevation%20Model%20(DEM)%201%20meter";

/// Most products read for one import. Each is a 10 km cell, so a handful
/// covers any sane map; more than this means the query went wrong.
const MAX_PRODUCTS: usize = 16;

/// Share of texels that must land on real data for a 1 m import to stand on
/// its own, with no second source behind it.
pub(crate) const MIN_COVERAGE: f32 = 0.98;

/// Below this there is not enough lidar to be worth stitching in, and the
/// global tiles answer alone.
pub(crate) const USEFUL_COVERAGE: f32 = 0.02;

/// One product's contribution: the zone it is projected in, the pixels the
/// map covers, and that level's ground resolution.
type Overlap = Option<(u8, Window, f64)>;

/// What a 1 m import produced.
pub(crate) struct Import {
    pub heights: Vec<f32>,
    /// Share of texels that came from real data, 0..=1.
    pub coverage: f32,
    /// Ground resolution actually read, metres per sample.
    pub resolution: f64,
}

/// Ask the National Map which 1 m products cover the map's footprint.
pub(crate) fn discover(framing: &Framing, get: &HttpGet) -> Result<Vec<String>, String> {
    let (west, south, east, north) = framing.lat_lon_bounds();
    let url = format!(
        "{TNM_PRODUCTS}?bbox={west:.6},{south:.6},{east:.6},{north:.6}\
         &datasets={ONE_METER_DEM}&max={MAX_PRODUCTS}&outputFormat=JSON"
    );
    let body = get(&url)?;
    let json: serde_json::Value =
        serde_json::from_slice(&body).map_err(|e| format!("product query: {e}"))?;
    let items = json
        .get("items")
        .and_then(|i| i.as_array())
        .ok_or_else(|| "product query returned no item list".to_owned())?;
    Ok(items
        .iter()
        .filter_map(|item| item.get("downloadURL")?.as_str().map(str::to_owned))
        .take(MAX_PRODUCTS)
        .collect())
}

/// Read 1 m elevation into the map's grid, in metres above the vertical datum.
///
/// `done`/`total` drive a progress bar; `total` is only known once discovery
/// has answered, so it is published here rather than by the caller.
pub(crate) fn import(
    framing: &Framing,
    get: HttpGet,
    fetch: RangeFetch,
    done: &Arc<AtomicU32>,
    total: &Arc<AtomicU32>,
) -> Result<Import, String> {
    let products = discover(framing, &get)?;
    if products.is_empty() {
        return Err("no USGS 1 m coverage here".into());
    }
    total.store(products.len() as u32, Ordering::Relaxed);

    // Read each product's overlap with the map. One decode is inherently
    // ordered, but products are independent and the cost is bytes off the
    // wire, so they overlap.
    let texel_size = framing.texel_size as f64;
    let pool = AsyncComputeTaskPool::get_or_init(TaskPool::default);
    let read: Vec<Result<Overlap, String>> = pool.scope(|scope| {
        for url in &products {
            let fetch = fetch.clone();
            scope.spawn(async move {
                let result = read_overlap(url, framing, texel_size, fetch);
                done.fetch_add(1, Ordering::Relaxed);
                result
            });
        }
    });

    let mut coverages: Vec<(u8, Window)> = Vec::new();
    let mut resolution = f64::MAX;
    let mut failures = Vec::new();
    for outcome in read {
        match outcome {
            Ok(Some((zone, window, scale))) => {
                resolution = resolution.min(scale);
                coverages.push((zone, window));
            }
            Ok(None) => {}
            Err(error) => failures.push(error),
        }
    }
    if coverages.is_empty() {
        return Err(failures
            .first()
            .cloned()
            .unwrap_or_else(|| "no product overlapped the map".into()));
    }
    for failure in &failures {
        bevy::log::warn!("bevy_wilderness_editor: skipped a 1 m product — {failure}");
    }

    // Sample every texel, projecting once per distinct zone.
    let mut zones: Vec<u8> = coverages.iter().map(|(z, _)| *z).collect();
    zones.sort_unstable();
    zones.dedup();

    let dims = framing.dims;
    let mut heights = vec![f32::NAN; (dims.x * dims.y) as usize];
    let rows_per_batch = (dims.y as usize).div_ceil(pool.thread_num().max(1));
    pool.scope(|scope| {
        for (batch, chunk) in heights
            .chunks_mut(rows_per_batch * dims.x as usize)
            .enumerate()
        {
            let row0 = batch * rows_per_batch;
            let (zones, coverages) = (&zones, &coverages);
            scope.spawn(async move {
                for (i, out) in chunk.iter_mut().enumerate() {
                    let x = (i % dims.x as usize) as f64 + 0.5;
                    let y = (row0 + i / dims.x as usize) as f64 + 0.5;
                    let (latitude, longitude) = framing.texel_lat_lon(x, y);
                    for &zone in zones {
                        let point = utm::project(latitude, longitude, Some(zone));
                        let hit = coverages
                            .iter()
                            .filter(|(z, _)| *z == zone)
                            .find_map(|(_, w)| w.sample(point.easting, point.northing));
                        if let Some(height) = hit {
                            *out = height;
                            break;
                        }
                    }
                }
            });
        }
    });

    let filled = heights.iter().filter(|h| h.is_finite()).count();
    let coverage = filled as f32 / heights.len().max(1) as f32;
    if filled == 0 {
        return Err("no USGS 1 m coverage here".into());
    }
    // Gaps — water bodies, product edges, skipped products — stay NaN. What
    // to put there depends on whether another source is in play, which only
    // the caller knows.
    Ok(Import {
        heights,
        coverage,
        resolution,
    })
}

/// Open one product and read the part of it the map covers, if any.
fn read_overlap(
    url: &str,
    framing: &Framing,
    texel_size: f64,
    fetch: RangeFetch,
) -> Result<Overlap, String> {
    let mut cog = ElevationCog::open(url, fetch)?;
    let zone = cog.zone;
    let (min_e, min_n, max_e, max_n) = framing.utm_bounds(zone);
    let (r_min_e, r_min_n, r_max_e, r_max_n) = cog.bounds();
    if max_e <= r_min_e || min_e >= r_max_e || max_n <= r_min_n || min_n >= r_max_n {
        return Ok(None);
    }
    let level = cog.level_for(texel_size);
    let scale = cog.level_scale(level);
    let window = cog.read_window(level, min_e, min_n, max_e, max_n)?;
    Ok(Some((zone, window, scale)))
}

/// Product queries against the real National Map endpoint.
pub(crate) fn http_get() -> HttpGet {
    let agent = ureq::builder()
        .timeout(std::time::Duration::from_secs(60))
        .user_agent("bevy_wilderness_editor")
        .build();
    Arc::new(move |url: &str| {
        // The service returns intermittent gateway errors under load.
        let mut last_error = String::new();
        for _ in 0..3 {
            match agent.get(url).call() {
                Ok(response) => {
                    use std::io::Read;
                    let mut bytes = Vec::new();
                    let mut reader = response.into_reader().take(8 * 1024 * 1024);
                    match reader.read_to_end(&mut bytes) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cog::synthetic;
    use bevy::math::UVec2;

    fn framing(latitude: f64, longitude: f64, size: u32, texel: f32) -> Framing {
        Framing {
            dims: UVec2::splat(size),
            texel_size: texel,
            latitude,
            longitude,
        }
    }

    fn answered(body: &str) -> HttpGet {
        let body = body.to_owned();
        Arc::new(move |_url| Ok(body.clone().into_bytes()))
    }

    /// A raster centred on the map, reading its own distance east of the
    /// raster origin so any sample has a known answer.
    fn covering_cog(framing: &Framing, span: u32) -> (f64, f64, Vec<u8>) {
        let point = utm::project(framing.latitude, framing.longitude, Some(11));
        let origin = (
            point.easting - span as f64 / 2.0,
            point.northing + span as f64 / 2.0,
        );
        let bytes = synthetic::cog(span, 128, 1, 1.0, origin, 26911, |e, _| (e - origin.0) as f32);
        (origin.0, origin.1, bytes)
    }

    #[test]
    fn discovery_pulls_download_urls() {
        let get = answered(
            r#"{"total":2,"items":[
                {"title":"a","downloadURL":"https://example/a.tif"},
                {"title":"b","downloadURL":"https://example/b.tif"}]}"#,
        );
        let found = discover(&framing(36.0, -117.0, 64, 4.0), &get).expect("must parse");
        assert_eq!(found, ["https://example/a.tif", "https://example/b.tif"]);
    }

    #[test]
    fn discovery_query_covers_the_map_footprint() {
        // A 64 × 4 m map spans 256 m, about 0.0023° of latitude.
        let seen: Arc<std::sync::Mutex<String>> = Arc::default();
        let captured = seen.clone();
        let get: HttpGet = Arc::new(move |url: &str| {
            *captured.lock().unwrap() = url.to_owned();
            Ok(br#"{"items":[]}"#.to_vec())
        });
        discover(&framing(36.0, -117.0, 64, 4.0), &get).expect("must run");
        let url = seen.lock().unwrap().clone();
        assert!(url.contains("bbox=-117.001"), "{url}");
        assert!(url.contains("1%20meter"), "must ask for the 1 m dataset: {url}");
    }

    #[test]
    fn imports_elevation_from_a_covering_product() {
        let map = framing(36.0, -117.0, 64, 4.0);
        let (origin_e, _, bytes) = covering_cog(&map, 1024);
        let import = import(
            &map,
            answered(r#"{"items":[{"downloadURL":"https://example/a.tif"}]}"#),
            synthetic::served(bytes),
            &Arc::default(),
            &Arc::default(),
        )
        .expect("must import");

        assert!(import.coverage > 0.999, "coverage {}", import.coverage);
        assert_eq!(import.resolution, 1.0);
        // The centre texel sits at the raster's centre, 512 m east of its origin.
        let centre = import.heights[(32 * 64 + 32) as usize];
        assert!((centre - 512.0).abs() < 4.0, "centre read {centre} m");
        // Height rises eastward across the map, by its full 256 m width.
        let (west, east) = (import.heights[32 * 64], import.heights[32 * 64 + 63]);
        assert!(
            (east - west - 252.0).abs() < 4.0,
            "expected a 252 m rise across the map, got {}",
            east - west
        );
        let _ = origin_e;
    }

    #[test]
    fn partial_coverage_is_reported_and_left_as_gaps() {
        // A raster far smaller than the map leaves most texels uncovered.
        let map = framing(36.0, -117.0, 64, 4.0);
        let (_, _, bytes) = covering_cog(&map, 128);
        let import = import(
            &map,
            answered(r#"{"items":[{"downloadURL":"https://example/a.tif"}]}"#),
            synthetic::served(bytes),
            &Arc::default(),
            &Arc::default(),
        )
        .expect("must import what it can");

        assert!(
            import.coverage > 0.1 && import.coverage < MIN_COVERAGE,
            "expected partial coverage, got {}",
            import.coverage
        );
        // Gaps stay open for the caller to fill from another source.
        assert!(
            import.heights.iter().any(|h| !h.is_finite()),
            "uncovered texels must be left as gaps"
        );
        assert!(
            import.heights.iter().any(|h| h.is_finite()),
            "covered texels must carry real data"
        );
    }

    /// The whole path against the live services: the National Map locates the
    /// product, the COG is read out of S3, and the result must agree with the
    /// Death Valley floor. Network-bound, so not part of the default run.
    #[test]
    #[ignore = "hits the National Map and the USGS S3 bucket"]
    fn imports_real_death_valley_lidar() {
        let map = framing(36.24, -116.82, 256, 1.0);
        let import = import(
            &map,
            http_get(),
            crate::cog::http_fetch(),
            &Arc::default(),
            &Arc::default(),
        )
        .expect("must import");

        let low = import.heights.iter().copied().fold(f32::MAX, f32::min);
        let high = import.heights.iter().copied().fold(f32::MIN, f32::max);
        println!(
            "coverage {:.1}%, {:.0} m/sample, relief {low:.1}..{high:.1} m",
            import.coverage * 100.0,
            import.resolution
        );
        assert!(import.coverage > 0.99, "coverage {}", import.coverage);
        assert_eq!(import.resolution, 1.0, "a 1 m map must read full resolution");
        assert!(
            (-95.0..-70.0).contains(&low) && (-95.0..-70.0).contains(&high),
            "Death Valley floor should sit near -85 m, got {low}..{high}"
        );
    }

    #[test]
    fn no_products_is_an_error() {
        let error = import(
            &framing(48.86, 2.35, 32, 4.0), // Paris — outside 3DEP entirely
            answered(r#"{"items":[]}"#),
            synthetic::served(Vec::new()),
            &Arc::default(),
            &Arc::default(),
        )
        .map(|_| ())
        .expect_err("must report no coverage");
        assert!(error.contains("no USGS 1 m coverage"), "{error}");
    }
}
