//! Landform masks: where water gathers, where ground wears and where debris
//! settles, computed from a heightmap. They feed the runtime's
//! [`TerrainMasks`](bevy_wilderness::TerrainMasks) — placement rules and tints
//! for variation tens of metres across, which tiling layers can't give.
//!
//! CPU, a few seconds for 4096²: run it off the main thread.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use image::RgbaImage;

/// What each channel of [`landform_masks`] holds, in RGBA order.
pub const LANDFORM_CHANNELS: [&str; 4] = ["flow", "wetness", "wear", "debris"];

const OFFS: [(i32, i32); 8] = [
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
    (0, -1),
    (1, -1),
];

struct Grid {
    w: usize,
    h: usize,
    texel: f32,
}

impl Grid {
    fn neighbour(&self, i: usize, k: usize) -> Option<usize> {
        let x = (i % self.w) as i32 + OFFS[k].0;
        let y = (i / self.w) as i32 + OFFS[k].1;
        (x >= 0 && y >= 0 && x < self.w as i32 && y < self.h as i32)
            .then(|| y as usize * self.w + x as usize)
    }

    fn dist(&self, k: usize) -> f32 {
        if k % 2 == 0 {
            self.texel
        } else {
            self.texel * std::f32::consts::SQRT_2
        }
    }
}

#[derive(PartialEq)]
struct Cell(f32, u32);
impl Eq for Cell {}
impl Ord for Cell {
    fn cmp(&self, o: &Self) -> Ordering {
        // Min-heap on height; the index breaks ties so runs are deterministic.
        o.0.total_cmp(&self.0).then(o.1.cmp(&self.1))
    }
}
impl PartialOrd for Cell {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// The four masks for a `width`×`height` heightfield (`heights` in metres, row
/// major, `texel_size` metres apart), box-filtered down by `downsample`.
///
/// Every edge is an outlet, so on looping terrain the masks are wrong within a
/// drainage basin of the seam.
pub fn landform_masks(
    heights: &[f32],
    width: usize,
    height: usize,
    texel_size: f32,
    downsample: usize,
) -> RgbaImage {
    assert_eq!(heights.len(), width * height);
    let g = Grid {
        w: width,
        h: height,
        texel: texel_size,
    };
    let n = heights.len();
    let z = heights;

    let tanb: Vec<f32> = (0..n)
        .map(|i| {
            let (x, y) = (i % g.w, i / g.w);
            let at = |x: usize, y: usize| z[y * g.w + x];
            let dx = (at((x + 1).min(g.w - 1), y) - at(x.saturating_sub(1), y)) / (2.0 * g.texel);
            let dy = (at(x, (y + 1).min(g.h - 1)) - at(x, y.saturating_sub(1))) / (2.0 * g.texel);
            (dx * dx + dy * dy).sqrt()
        })
        .collect();

    let (filled, order) = fill(&g, z);
    let (flow, weights) = route(&g, &filled, &order);
    let debris = debris(&g, &tanb, &flow, &weights, &order);

    // Topographic wetness, ln(a / tanβ) with a the specific catchment area. The
    // slope is floored at ~5° so flat ground isn't wet merely for being flat.
    let tan_floor = 5f32.to_radians().tan();
    let twi: Vec<f32> = (0..n)
        .map(|i| (flow.area[i] * g.texel / tanb[i].max(tan_floor)).ln())
        .collect();
    let (lo, hi) = (percentile(&twi, 0.02), percentile(&twi, 0.98));
    // A filled basin's area only traces the fill's artificial gradient, and
    // most basins are dips in the source data rather than standing water, so
    // the deeper the fill the less it counts.
    let wetness: Vec<f32> = (0..n)
        .map(|i| {
            let index = ((twi[i] - lo) / (hi - lo)).clamp(0.0, 1.0);
            index * (1.0 - smoothstep(0.05, 1.0, filled[i] - z[i]))
        })
        .collect();

    // Convexity: height above the local mean at ~10 m and ~50 m, as a gradient
    // so it reads the same on any relief.
    let near = box_blur(&g, z, 2);
    let mean_s = box_blur(&g, z, 5);
    let mean_l = box_blur(&g, z, 25);
    let convex: Vec<f32> = (0..n)
        .map(|i| {
            0.5 * (near[i] - mean_s[i]) / (5.0 * g.texel)
                + 0.5 * (near[i] - mean_l[i]) / (25.0 * g.texel)
        })
        .collect();
    let c_ref = percentile(&convex.iter().map(|c| c.abs()).collect::<Vec<_>>(), 0.98).max(1e-6);
    // Wear: convex ground, more so where steep, less where water gathers.
    let wear: Vec<f32> = (0..n)
        .map(|i| {
            let cv = smoothstep(0.5, 0.85, (0.5 + 0.5 * convex[i] / c_ref).clamp(0.0, 1.0));
            let st = smoothstep(0.3, 1.2, tanb[i]);
            (cv * (0.4 + 0.6 * st) * (1.0 - 0.7 * flow.norm[i])).clamp(0.0, 1.0)
        })
        .collect();

    pack(
        &g,
        [&flow.norm, &wetness, &wear, &debris],
        downsample.max(1),
    )
}

/// Priority-flood with epsilon (Barnes 2014): pits filled so every cell drains
/// to an edge. Returns the filled heights and the pop order (low to high),
/// whose reverse runs upstream to downstream on the filled surface.
fn fill(g: &Grid, z: &[f32]) -> (Vec<f32>, Vec<u32>) {
    let n = z.len();
    let mut f = z.to_vec();
    let mut seen = vec![false; n];
    let mut heap = BinaryHeap::new();
    for i in 0..n {
        let (x, y) = (i % g.w, i / g.w);
        if x == 0 || y == 0 || x == g.w - 1 || y == g.h - 1 {
            seen[i] = true;
            heap.push(Cell(f[i], i as u32));
        }
    }
    let mut order = Vec::with_capacity(n);
    while let Some(Cell(fh, i)) = heap.pop() {
        let i = i as usize;
        order.push(i as u32);
        for k in 0..8 {
            if let Some(j) = g.neighbour(i, k)
                && !seen[j]
            {
                seen[j] = true;
                // f32 near a kilometre holds ~1e-4 m, so a millimetre step survives.
                f[j] = f[j].max(fh + 1e-3);
                heap.push(Cell(f[j], j as u32));
            }
        }
    }
    (f, order)
}

struct Flow {
    /// Upslope area, in cells.
    area: Vec<f32>,
    /// Log area, the top 0.1% of channels saturating.
    norm: Vec<f32>,
}

/// Multiple-flow-direction routing with Qin's adaptive exponent: dispersive on
/// gentle ground, near single-direction on steep ground, so hillslopes spread
/// and channels stay narrow without the grid-aligned lines D8 leaves.
fn route(g: &Grid, f: &[f32], order: &[u32]) -> (Flow, Vec<[f32; 8]>) {
    let n = f.len();
    let mut area = vec![1.0f32; n];
    let mut weights = vec![[0.0f32; 8]; n];
    for &i in order.iter().rev() {
        let i = i as usize;
        let mut emax = 0.0f32;
        for k in 0..8 {
            if let Some(j) = g.neighbour(i, k) {
                emax = emax.max((f[i] - f[j]) / g.dist(k));
            }
        }
        let p = 1.1 + 8.9 * emax.min(1.0);
        let mut w = [0.0f32; 8];
        let mut sum = 0.0;
        for k in 0..8 {
            if let Some(j) = g.neighbour(i, k) {
                let e = (f[i] - f[j]) / g.dist(k);
                if e > 0.0 {
                    // Contour length per direction (Quinn 1991).
                    w[k] = e.powf(p) * if k % 2 == 0 { 0.5 } else { 0.354 };
                    sum += w[k];
                }
            }
        }
        if sum > 0.0 {
            for k in 0..8 {
                w[k] /= sum;
                if w[k] > 0.0 {
                    area[g.neighbour(i, k).unwrap()] += area[i] * w[k];
                }
            }
        }
        weights[i] = w;
    }
    let log: Vec<f32> = area.iter().map(|a| a.ln()).collect();
    let top = percentile(&log, 0.999).max(1e-6);
    let norm = log.iter().map(|v| (v / top).clamp(0.0, 1.0)).collect();
    (Flow { area, norm }, weights)
}

/// Debris shed by faces steeper than ~40°, carried down the flow routing and
/// settling where the ground eases below ~35° (most by ~12°). Big channels keep
/// carrying it, so it lands as talus and fans rather than in stream beds.
fn debris(g: &Grid, tanb: &[f32], flow: &Flow, weights: &[[f32; 8]], order: &[u32]) -> Vec<f32> {
    let n = tanb.len();
    let mut load = vec![0.0f32; n];
    let mut settled = vec![0.0f32; n];
    let tan_shed = 40f32.to_radians().tan();
    let (settle_from, settle_full) = (35f32.to_radians().tan(), 12f32.to_radians().tan());
    for &i in order.iter().rev() {
        let i = i as usize;
        let mut l = load[i] + (tanb[i] - tan_shed).max(0.0);
        let settle = smoothstep(settle_from, settle_full, tanb[i])
            * (1.0 - smoothstep(0.6, 0.85, flow.norm[i]));
        let d = l * 0.25 * settle;
        settled[i] = d;
        l -= d;
        for k in 0..8 {
            if weights[i][k] > 0.0 {
                load[g.neighbour(i, k).unwrap()] += l * weights[i][k];
            }
        }
    }
    let smooth = box_blur(g, &settled, 2);
    let top = percentile(&smooth, 0.99).max(1e-6);
    smooth
        .iter()
        .map(|d| (d.max(0.0) / top).sqrt().clamp(0.0, 1.0))
        .collect()
}

/// Separable box blur of radius `r`, the window clipped at the edges.
fn box_blur(g: &Grid, src: &[f32], r: usize) -> Vec<f32> {
    let pass = |src: &[f32], horizontal: bool| {
        let (lines, len) = if horizontal { (g.h, g.w) } else { (g.w, g.h) };
        let at = |line: usize, t: usize| {
            if horizontal {
                line * g.w + t
            } else {
                t * g.w + line
            }
        };
        let mut out = vec![0.0f32; src.len()];
        for line in 0..lines {
            let mut sum = 0.0f64;
            let mut count = 0usize;
            for t in 0..=r.min(len - 1) {
                sum += src[at(line, t)] as f64;
                count += 1;
            }
            for t in 0..len {
                out[at(line, t)] = (sum / count as f64) as f32;
                if t + r + 1 < len {
                    sum += src[at(line, t + r + 1)] as f64;
                    count += 1;
                }
                if t >= r {
                    sum -= src[at(line, t - r)] as f64;
                    count -= 1;
                }
            }
        }
        out
    };
    pass(&pass(src, true), false)
}

fn pack(g: &Grid, channels: [&Vec<f32>; 4], s: usize) -> RgbaImage {
    let (w, h) = ((g.w / s).max(1), (g.h / s).max(1));
    let mut out = RgbaImage::new(w as u32, h as u32);
    for (px, py, pixel) in out.enumerate_pixels_mut() {
        let (px, py) = (px as usize, py as usize);
        for (c, values) in channels.iter().enumerate() {
            let mut sum = 0.0;
            for y in 0..s {
                for x in 0..s {
                    sum += values[(py * s + y).min(g.h - 1) * g.w + (px * s + x).min(g.w - 1)];
                }
            }
            pixel.0[c] = ((sum / (s * s) as f32).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
    }
    out
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Approximate: every seventh value is enough for normalising.
fn percentile(v: &[f32], p: f32) -> f32 {
    let mut s: Vec<f32> = v
        .iter()
        .step_by(7)
        .copied()
        .filter(|x| x.is_finite())
        .collect();
    if s.is_empty() {
        return 0.0;
    }
    s.sort_by(f32::total_cmp);
    s[((s.len() - 1) as f32 * p) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cone: water runs off the flanks, so the summit is the driest, most
    /// worn ground and the foot the wettest.
    #[test]
    fn a_cone_wears_at_the_top_and_gathers_water_at_the_foot() {
        let size = 65;
        let c = (size / 2) as f32;
        let heights: Vec<f32> = (0..size * size)
            .map(|i| {
                let (x, y) = ((i % size) as f32 - c, (i / size) as f32 - c);
                (40.0 - (x * x + y * y).sqrt()).max(0.0) * 2.0
            })
            .collect();
        let masks = landform_masks(&heights, size, size, 2.0, 1);
        let at = |x: u32, y: u32| masks.get_pixel(x, y).0;
        let (summit, flank) = (at(32, 32), at(32, 12));
        assert!(
            summit[2] > flank[2],
            "summit wear {} vs flank {}",
            summit[2],
            flank[2]
        );
        assert!(
            flank[1] > summit[1],
            "flank wetness {} vs summit {}",
            flank[1],
            summit[1]
        );
    }
}
