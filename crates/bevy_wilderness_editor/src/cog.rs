//! Windowed reads of a cloud-optimised GeoTIFF over HTTP range requests.
//!
//! USGS publishes its 1 m elevation as COGs: internally tiled GeoTIFFs that
//! also carry a pyramid of reduced-resolution copies. Both matter here. The
//! tiling means a map only pays for the ground it covers instead of the 300 MB
//! a full 10 km × 10 km tile weighs, and the pyramid means a 2 m terrain reads
//! a 2 m level rather than decimating 1 m data — the same trade the terrarium
//! path makes when it picks a zoom.
//!
//! Only what the elevation import needs is modelled: single-sample float
//! rasters on a north-up UTM grid. Anything else is rejected by name rather
//! than silently misread.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;

use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;

/// Read `length` bytes at `offset` from `url`, returning them with the
/// resource's total size. Injected so tests exercise the reader offline.
pub(crate) type RangeFetch =
    Arc<dyn Fn(&str, u64, u64) -> Result<(Vec<u8>, u64), String> + Send + Sync>;

/// Bytes held from the front of the file for the lifetime of a read. COG
/// layout puts every IFD and tile-offset array up here, so caching it turns
/// the decoder's constant header seeking into one request.
const HEADER: u64 = 128 * 1024;

/// Smallest range request issued for tile data. Tile bytes are read
/// consecutively, so rounding up costs nothing and saves round trips.
const BLOCK: u64 = 256 * 1024;

/// `Read + Seek` over a range-request endpoint.
struct RangeReader {
    url: String,
    fetch: RangeFetch,
    length: u64,
    position: u64,
    header: Vec<u8>,
    block: Option<(u64, Vec<u8>)>,
}

impl RangeReader {
    fn open(url: &str, fetch: RangeFetch) -> Result<Self, String> {
        let (header, length) = fetch(url, 0, HEADER)?;
        Ok(Self {
            url: url.to_owned(),
            fetch,
            length,
            position: 0,
            header,
            block: None,
        })
    }
}

impl RangeReader {
    /// Serve what is already cached at the current position, if anything.
    fn cached(&self, want: usize) -> Option<&[u8]> {
        fn hit(position: u64, want: usize, start: u64, bytes: &[u8]) -> Option<&[u8]> {
            let offset = position.checked_sub(start)? as usize;
            let slice = bytes.get(offset..)?;
            (!slice.is_empty()).then(|| &slice[..slice.len().min(want)])
        }
        hit(self.position, want, 0, &self.header).or_else(|| {
            let (start, bytes) = self.block.as_ref()?;
            hit(self.position, want, *start, bytes)
        })
    }
}

impl Read for RangeReader {
    /// Fills `out` completely unless the resource ends first.
    ///
    /// A short read is legal, but the TIFF crate's LZW loop asserts its way
    /// out of one that lands mid-code — and cache and block boundaries put
    /// them exactly there. Filling the buffer keeps that off the table.
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < out.len() {
            if self.position >= self.length {
                break;
            }
            let want = (out.len() - filled).min((self.length - self.position) as usize);
            if let Some(slice) = self.cached(want) {
                let n = slice.len();
                out[filled..filled + n].copy_from_slice(slice);
                filled += n;
                self.position += n as u64;
                continue;
            }
            let length = (want as u64).max(BLOCK).min(self.length - self.position);
            let (bytes, _) = (self.fetch)(&self.url, self.position, length)
                .map_err(io::Error::other)?;
            if bytes.is_empty() {
                break;
            }
            self.block = Some((self.position, bytes));
        }
        Ok(filled)
    }
}

impl Seek for RangeReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(n) => self.position as i64 + n,
            SeekFrom::End(n) => self.length as i64 + n,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            ));
        }
        self.position = target as u64;
        Ok(self.position)
    }
}

/// One resolution in a COG's pyramid.
#[derive(Debug, Clone, Copy)]
struct Level {
    index: usize,
    width: u32,
    height: u32,
    /// Ground size of one pixel, metres.
    scale: f64,
}

/// A north-up UTM elevation raster, read on demand.
pub(crate) struct ElevationCog {
    decoder: Decoder<RangeReader>,
    levels: Vec<Level>,
    /// Upper-left corner of the raster, UTM metres. North decreases with `y`.
    origin: (f64, f64),
    nodata: Option<f32>,
    /// UTM longitude zone the raster is projected in.
    pub(crate) zone: u8,
}

impl ElevationCog {
    pub(crate) fn open(url: &str, fetch: RangeFetch) -> Result<Self, String> {
        let reader = RangeReader::open(url, fetch)?;
        let mut decoder = Decoder::new(reader)
            .map_err(|e| format!("{url}: not a TIFF: {e}"))?
            .with_limits(Limits::unlimited());

        let scales = decoder
            .get_tag_f64_vec(Tag::ModelPixelScaleTag)
            .map_err(|e| format!("{url}: no pixel scale: {e}"))?;
        let tiepoint = decoder
            .get_tag_f64_vec(Tag::ModelTiepointTag)
            .map_err(|e| format!("{url}: no tiepoint: {e}"))?;
        if scales.len() < 2 || tiepoint.len() < 6 {
            return Err(format!("{url}: malformed georeferencing"));
        }
        // A tiepoint maps a raster point to a model point; USGS anchors the
        // upper-left pixel, which is the only case handled here.
        if tiepoint[0] != 0.0 || tiepoint[1] != 0.0 {
            return Err(format!("{url}: tiepoint is not the raster origin"));
        }
        let origin = (tiepoint[3], tiepoint[4]);
        let base_scale = scales[0];
        if (scales[0] - scales[1]).abs() > 1e-9 {
            return Err(format!("{url}: non-square pixels"));
        }

        let zone = utm_zone_of(&mut decoder)
            .ok_or_else(|| format!("{url}: not a NAD83/WGS84 UTM raster"))?;
        let nodata = decoder
            .find_tag(Tag::GdalNodata)
            .ok()
            .flatten()
            .and_then(|v| v.into_string().ok())
            .and_then(|s| s.trim_end_matches('\0').trim().parse::<f32>().ok());

        // Walk the pyramid. Overviews carry no scale of their own, so it comes
        // from how far each is decimated below full resolution.
        let mut levels = Vec::new();
        let mut index = 0;
        loop {
            if decoder.seek_to_image(index).is_err() {
                break;
            }
            let Ok((width, height)) = decoder.dimensions() else {
                break;
            };
            if index == 0 && !matches!(decoder.colortype(), Ok(tiff::ColorType::Gray(32))) {
                return Err(format!("{url}: expected 32-bit grayscale elevation"));
            }
            let base_width = levels.first().map_or(width, |l: &Level| l.width);
            levels.push(Level {
                index,
                width,
                height,
                scale: base_scale * base_width as f64 / width as f64,
            });
            index += 1;
        }
        if levels.is_empty() {
            return Err(format!("{url}: no images"));
        }
        Ok(Self {
            decoder,
            levels,
            origin,
            nodata,
            zone,
        })
    }

    /// Ground extent of the raster: `(min_easting, min_northing, max_easting,
    /// max_northing)`.
    pub(crate) fn bounds(&self) -> (f64, f64, f64, f64) {
        let base = self.levels[0];
        (
            self.origin.0,
            self.origin.1 - base.height as f64 * base.scale,
            self.origin.0 + base.width as f64 * base.scale,
            self.origin.1,
        )
    }

    /// The finest pyramid level that is still no finer than `texel_size` —
    /// full detail without paying for data the terrain cannot represent.
    /// Falls back to full resolution when every level is coarser.
    pub(crate) fn level_for(&self, texel_size: f64) -> usize {
        self.levels
            .iter()
            .rfind(|l| l.scale <= texel_size * 1.001)
            .map_or(0, |l| l.index)
    }

    /// Ground size of one sample at `level`, metres.
    pub(crate) fn level_scale(&self, level: usize) -> f64 {
        self.levels[level.min(self.levels.len() - 1)].scale
    }

    /// Read every pixel of `level` inside the given UTM rectangle.
    pub(crate) fn read_window(
        &mut self,
        level: usize,
        min_e: f64,
        min_n: f64,
        max_e: f64,
        max_n: f64,
    ) -> Result<Window, String> {
        let level = self.levels[level.min(self.levels.len() - 1)];
        self.decoder
            .seek_to_image(level.index)
            .map_err(|e| format!("seek to level {}: {e}", level.index))?;

        // Pad by a pixel so bilinear sampling at the edge has its neighbour.
        let to_x = |e: f64| (e - self.origin.0) / level.scale;
        let to_y = |n: f64| (self.origin.1 - n) / level.scale;
        let x0 = (to_x(min_e).floor() as i64 - 1).clamp(0, level.width as i64) as u32;
        let x1 = (to_x(max_e).ceil() as i64 + 1).clamp(0, level.width as i64) as u32;
        let y0 = (to_y(max_n).floor() as i64 - 1).clamp(0, level.height as i64) as u32;
        let y1 = (to_y(min_n).ceil() as i64 + 1).clamp(0, level.height as i64) as u32;
        if x1 <= x0 || y1 <= y0 {
            return Err("window does not overlap the raster".into());
        }

        let (width, height) = ((x1 - x0) as usize, (y1 - y0) as usize);
        let mut data = vec![f32::NAN; width * height];
        let (tile_w, tile_h) = self.decoder.chunk_dimensions();
        let tiles_across = level.width.div_ceil(tile_w);

        for ty in y0 / tile_h..=(y1 - 1) / tile_h {
            for tx in x0 / tile_w..=(x1 - 1) / tile_w {
                let index = ty * tiles_across + tx;
                let (chunk_w, _) = self.decoder.chunk_data_dimensions(index);
                let DecodingResult::F32(chunk) = self
                    .decoder
                    .read_chunk(index)
                    .map_err(|e| format!("tile {index}: {e}"))?
                else {
                    return Err("elevation tile is not 32-bit float".into());
                };
                // Blit the part of this tile that falls inside the window.
                let (base_x, base_y) = (tx * tile_w, ty * tile_h);
                for (row, line) in chunk.chunks(chunk_w as usize).enumerate() {
                    let y = base_y + row as u32;
                    if y < y0 || y >= y1 {
                        continue;
                    }
                    let out = (y - y0) as usize * width;
                    for (column, &value) in line.iter().enumerate() {
                        let x = base_x + column as u32;
                        if x >= x0 && x < x1 {
                            data[out + (x - x0) as usize] = value;
                        }
                    }
                }
            }
        }

        if let Some(nodata) = self.nodata {
            for value in &mut data {
                if (*value - nodata).abs() < 1e-3 {
                    *value = f32::NAN;
                }
            }
        }
        Ok(Window {
            data,
            width,
            height,
            origin: (
                self.origin.0 + x0 as f64 * level.scale,
                self.origin.1 - y0 as f64 * level.scale,
            ),
            scale: level.scale,
        })
    }
}

/// The UTM zone of a projected GeoTIFF, from its `ProjectedCSTypeGeoKey`.
/// NAD83 UTM north is EPSG 269xx, WGS84 UTM north 326xx.
fn utm_zone_of(decoder: &mut Decoder<RangeReader>) -> Option<u8> {
    let keys = decoder.get_tag_u16_vec(Tag::GeoKeyDirectoryTag).ok()?;
    // Header is 4 shorts, then one 4-short entry per key.
    let count = *keys.get(3)? as usize;
    for entry in keys.get(4..)?.chunks_exact(4).take(count) {
        // ProjectedCSTypeGeoKey, stored inline (location 0).
        if entry[0] == 3072 && entry[1] == 0 {
            let epsg = entry[3];
            return match epsg {
                26901..=26960 => Some((epsg - 26900) as u8),
                32601..=32660 => Some((epsg - 32600) as u8),
                _ => None,
            };
        }
    }
    None
}

/// A decoded rectangle of elevation on the UTM grid.
pub(crate) struct Window {
    data: Vec<f32>,
    width: usize,
    height: usize,
    /// Upper-left corner, UTM metres.
    origin: (f64, f64),
    scale: f64,
}

impl Window {
    /// Bilinearly sample metres above the vertical datum at a UTM point.
    /// `None` outside the window or over a gap in the source data.
    pub(crate) fn sample(&self, easting: f64, northing: f64) -> Option<f32> {
        // Pixel centres sit half a pixel in from the corner.
        let x = (easting - self.origin.0) / self.scale - 0.5;
        let y = (self.origin.1 - northing) / self.scale - 0.5;
        if !(x >= 0.0 && y >= 0.0) {
            return None;
        }
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        if x0 + 1 >= self.width || y0 + 1 >= self.height {
            return None;
        }
        let (fx, fy) = ((x - x0 as f64) as f32, (y - y0 as f64) as f32);
        let at = |x: usize, y: usize| self.data[y * self.width + x];
        let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx;
        let bottom = at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx;
        let value = top * (1.0 - fy) + bottom * fy;
        value.is_finite().then_some(value)
    }
}

/// Range requests against a real HTTP endpoint.
pub(crate) fn http_fetch() -> RangeFetch {
    let agent = ureq::builder()
        .timeout(std::time::Duration::from_secs(60))
        .user_agent("bevy_wilderness_editor")
        .build();
    Arc::new(move |url, offset, length| {
        let range = format!("bytes={offset}-{}", offset + length.max(1) - 1);
        let mut last_error = String::new();
        for _ in 0..2 {
            match agent.get(url).set("Range", &range).call() {
                Ok(response) => {
                    // A server that ignores Range answers 200 with everything.
                    let whole = response.status() == 200;
                    let total = content_range_total(&response).unwrap_or(0);
                    let mut bytes = Vec::new();
                    if let Err(error) = response
                        .into_reader()
                        .take(length.max(1) + if whole { offset } else { 0 })
                        .read_to_end(&mut bytes)
                    {
                        last_error = format!("{url}: {error}");
                        continue;
                    }
                    if whole {
                        bytes.drain(..(offset as usize).min(bytes.len()));
                    }
                    let total = if total > 0 { total } else { offset + bytes.len() as u64 };
                    return Ok((bytes, total));
                }
                Err(error) => last_error = format!("{url}: {error}"),
            }
        }
        Err(last_error)
    })
}

/// Total resource size from a 206 response's `Content-Range: bytes a-b/total`.
fn content_range_total(response: &ureq::Response) -> Option<u64> {
    response
        .header("Content-Range")?
        .rsplit_once('/')
        .and_then(|(_, total)| total.trim().parse().ok())
}

/// Builds GeoTIFFs shaped like the USGS products, so the reader, the
/// projection and the stitching can all be exercised without a network.
#[cfg(test)]
pub(crate) mod synthetic {
    use super::RangeFetch;
    use std::sync::Arc;

    /// Serve a byte slice through the range-fetch interface.
    pub(crate) fn served(bytes: Vec<u8>) -> RangeFetch {
        Arc::new(move |_url, offset, length| {
            let start = (offset as usize).min(bytes.len());
            let end = (start + length as usize).min(bytes.len());
            Ok((bytes[start..end].to_vec(), bytes.len() as u64))
        })
    }

    /// A tiled, uncompressed, single-sample float32 GeoTIFF with `levels`
    /// pyramid levels, north-up on a UTM grid. `sample` is evaluated at each
    /// pixel's centre in UTM metres.
    pub(crate) fn cog(
        width: u32,
        tile: u32,
        levels: u32,
        scale: f64,
        origin: (f64, f64),
        epsg: u16,
        sample: impl Fn(f64, f64) -> f32,
    ) -> Vec<u8> {
        let mut out = vec![b'I', b'I', 42, 0, 0, 0, 0, 0];

        // Tile data first, so the directories can point at it. TIFF stores
        // every tile at full size, padded past the image edge.
        let mut per_level = Vec::new();
        for level in 0..levels {
            let level_width = width >> level;
            let level_scale = scale * (1 << level) as f64;
            let across = level_width.div_ceil(tile);
            let mut offsets = Vec::new();
            for ty in 0..across {
                for tx in 0..across {
                    offsets.push(out.len() as u32);
                    for row in 0..tile {
                        for column in 0..tile {
                            let (x, y) = (tx * tile + column, ty * tile + row);
                            let easting = origin.0 + (x as f64 + 0.5) * level_scale;
                            let northing = origin.1 - (y as f64 + 0.5) * level_scale;
                            let value = if x < level_width && y < level_width {
                                sample(easting, northing)
                            } else {
                                0.0
                            };
                            out.extend_from_slice(&value.to_le_bytes());
                        }
                    }
                }
            }
            let bytes = tile * tile * 4;
            per_level.push((level_width, level_scale, offsets, bytes));
        }

        // Directories last and in reverse, so each knows where the next sits.
        let mut next_ifd = 0u32;
        let mut first_ifd = 0u32;
        for (level_width, level_scale, offsets, bytes) in per_level.into_iter().rev() {
            let counts: Vec<u32> = offsets.iter().map(|_| bytes).collect();
            // A lone LONG fits in the entry's value field, and TIFF requires
            // it to live there rather than behind a pointer.
            let offsets_at = inline_or_append(&mut out, &offsets);
            let counts_at = inline_or_append(&mut out, &counts);
            let scales_at = append_f64(&mut out, &[level_scale, level_scale, 0.0]);
            let tie_at = append_f64(&mut out, &[0.0, 0.0, 0.0, origin.0, origin.1, 0.0]);
            let keys_at = append_u16(&mut out, &[1, 1, 0, 1, 3072, 0, 1, epsg]);
            let nodata_at = out.len() as u32;
            out.extend_from_slice(b"-999999\0");

            let ifd = out.len() as u32;
            let entries: [(u16, u16, u32, u32); 15] = [
                (256, 4, 1, level_width),
                (257, 4, 1, level_width),
                (258, 3, 1, 32),
                (259, 3, 1, 1),
                (262, 3, 1, 1),
                (277, 3, 1, 1),
                (284, 3, 1, 1),
                (322, 4, 1, tile),
                (323, 4, 1, tile),
                (324, 4, offsets.len() as u32, offsets_at),
                (325, 4, counts.len() as u32, counts_at),
                (339, 3, 1, 3),
                (33550, 12, 3, scales_at),
                (33922, 12, 6, tie_at),
                (34735, 3, 8, keys_at),
            ];
            out.extend_from_slice(&(entries.len() as u16 + 1).to_le_bytes());
            for (tag, kind, count, value) in entries {
                entry(&mut out, tag, kind, count, value);
            }
            entry(&mut out, 42113, 2, 8, nodata_at);
            out.extend_from_slice(&next_ifd.to_le_bytes());
            next_ifd = ifd;
            first_ifd = ifd;
        }
        out[4..8].copy_from_slice(&first_ifd.to_le_bytes());
        out
    }

    fn entry(out: &mut Vec<u8>, tag: u16, kind: u16, count: u32, value: u32) {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        // A SHORT that fits inline sits in the low half of the value field.
        if kind == 3 && count == 1 {
            out.extend_from_slice(&(value as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
        } else {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }

    fn inline_or_append(out: &mut Vec<u8>, values: &[u32]) -> u32 {
        match values {
            [only] => *only,
            _ => append(out, values),
        }
    }

    fn append(out: &mut Vec<u8>, values: &[u32]) -> u32 {
        let at = out.len() as u32;
        values.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        at
    }

    fn append_f64(out: &mut Vec<u8>, values: &[f64]) -> u32 {
        let at = out.len() as u32;
        values.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        at
    }

    fn append_u16(out: &mut Vec<u8>, values: &[u16]) -> u32 {
        let at = out.len() as u32;
        values.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
        at
    }
}

#[cfg(test)]
mod tests {
    use super::synthetic::served;
    use super::*;

    /// A 1 m raster whose height is its distance east of the origin, so any
    /// sample has a known answer.
    fn ramp_cog(levels: u32) -> Vec<u8> {
        synthetic::cog(512, 128, levels, 1.0, (500_000.0, 4_000_000.0), 26911, |e, _| {
            (e - 500_000.0) as f32
        })
    }

    #[test]
    fn reads_georeferencing_and_pyramid() {
        let cog = ElevationCog::open("t.tif", served(ramp_cog(3))).expect("must open");
        assert_eq!(cog.zone, 11, "EPSG 26911 is NAD83 / UTM 11N");
        let (min_e, min_n, max_e, max_n) = cog.bounds();
        assert_eq!((min_e, max_n), (500_000.0, 4_000_000.0));
        assert_eq!((max_e, min_n), (500_512.0, 3_999_488.0));

        // A terrain reads the finest level that is not finer than its texels.
        assert_eq!(cog.level_for(1.0), 0);
        assert_eq!(cog.level_for(2.0), 1);
        assert_eq!(cog.level_for(3.0), 1, "3 m must not read the 4 m level");
        assert_eq!(cog.level_for(4.0), 2);
        assert_eq!(cog.level_for(64.0), 2, "coarser than the pyramid goes");
        assert_eq!(cog.level_for(0.25), 0, "finer than the source upsamples");
    }

    #[test]
    fn samples_match_the_source_field() {
        let mut cog = ElevationCog::open("t.tif", served(ramp_cog(1))).expect("must open");
        let window = cog
            .read_window(0, 500_100.0, 3_999_700.0, 500_300.0, 3_999_900.0)
            .expect("must read");
        for east in [500_120.0, 500_200.5, 500_280.0] {
            let got = window.sample(east, 3_999_800.0).expect("inside the window");
            assert!(
                (got as f64 - (east - 500_000.0)).abs() < 0.01,
                "at easting {east}: {got}"
            );
        }
        assert!(window.sample(400_000.0, 3_999_800.0).is_none(), "outside");
    }

    /// Tiles are stored padded past the image edge; a window running to the
    /// corner must return real data, not the padding.
    #[test]
    fn window_at_the_raster_corner_is_clipped() {
        let mut cog = ElevationCog::open("t.tif", served(ramp_cog(1))).expect("must open");
        let window = cog
            .read_window(0, 500_400.0, 3_999_400.0, 500_900.0, 3_999_900.0)
            .expect("must read");
        let got = window.sample(500_500.0, 3_999_500.0).expect("inside");
        assert!((got - 500.0).abs() < 0.01, "corner sample {got}");
        assert!(window.sample(500_600.0, 3_999_500.0).is_none(), "past the edge");
    }

    #[test]
    fn gaps_in_the_source_do_not_sample() {
        let bytes = synthetic::cog(256, 128, 1, 1.0, (500_000.0, 4_000_000.0), 26911, |e, _| {
            if e < 500_100.0 { -999_999.0 } else { 42.0 }
        });
        let mut cog = ElevationCog::open("t.tif", served(bytes)).expect("must open");
        let window = cog
            .read_window(0, 500_000.0, 3_999_800.0, 500_250.0, 3_999_950.0)
            .expect("must read");
        assert!(window.sample(500_050.0, 3_999_900.0).is_none(), "nodata");
        assert_eq!(window.sample(500_200.0, 3_999_900.0), Some(42.0));
    }

    #[test]
    fn non_utm_rasters_are_refused() {
        // EPSG 3857 is web mercator, not a UTM zone.
        let bytes = synthetic::cog(128, 128, 1, 1.0, (0.0, 0.0), 3857, |_, _| 1.0);
        let error = ElevationCog::open("t.tif", served(bytes))
            .map(|_| ())
            .expect_err("must refuse");
        assert!(error.contains("UTM"), "{error}");
    }

    /// Reads the real USGS product covering Badwater Basin and checks it
    /// against the independently known depth of the lowest point in North
    /// America. Network-bound, so not part of the default run.
    #[test]
    #[ignore = "hits the USGS S3 bucket"]
    fn reads_real_usgs_elevation() {
        const URL: &str = "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1m/Projects/\
CA_FEMAR9Southeast_D24/TIFF/USGS_1M_11_x51y402_CA_FEMAR9Southeast_D24.tif";
        let mut cog = ElevationCog::open(URL, http_fetch()).expect("must open");
        assert_eq!(cog.zone, 11);
        let (min_e, min_n, max_e, max_n) = cog.bounds();
        println!("bounds E {min_e:.0}..{max_e:.0} N {min_n:.0}..{max_n:.0}, zone {}", cog.zone);

        // The pyramid must offer a 2 m level for a 2 m terrain, and 1 m below.
        assert_eq!(cog.level_for(1.0), 0, "1 m terrain reads full resolution");
        assert_eq!(cog.level_for(2.0), 1, "2 m terrain reads the 2 m overview");

        // The Death Valley preset, on the salt pan inside this product. The
        // global terrarium set reads -82.1 m at the same coordinate, so two
        // independent sources agreeing pins the projection and the decode.
        let point = crate::utm::project(36.2400, -116.8200, Some(11));
        let window = cog
            .read_window(0, point.easting - 300.0, point.northing - 300.0,
                         point.easting + 300.0, point.northing + 300.0)
            .expect("must read");
        let here = window.sample(point.easting, point.northing).expect("must sample");
        println!("1 m lidar reads {here:.2} m (terrarium reads -82.1 m)");
        assert!(
            (here - -82.1).abs() < 10.0,
            "1 m lidar {here} should agree with terrarium's -82.1 m"
        );
    }
}
