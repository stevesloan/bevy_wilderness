//! Periodic-plus-smooth decomposition (Moisan 2011), used to make an imported
//! real-world heightmap tile without a seam.
//!
//! A crop of Earth is not periodic: its north edge is one place and its south
//! edge another, so a looping terrain meets a cliff at every repeat — hundreds
//! of meters for mountainous ground. Moisan splits a field into a *periodic*
//! component and a *smooth* one, where the smooth part carries exactly the
//! low-order trend responsible for the edge disagreement. Subtracting it
//! leaves a field whose opposite edges meet as closely as any two neighbouring
//! rows inside the map, with the fine detail untouched — unlike mirroring
//! (visible bilateral symmetry) or edge feathering (a flat frame around every
//! tile).
//!
//! The smooth component `s` solves a Poisson equation whose forcing lives only
//! on the four border lines, so it is obtained by dividing the boundary
//! difference field's DFT by the discrete Laplacian's eigenvalues. What comes
//! out is a gentle warp: it flattens the map's overall tilt (a range that rises
//! steadily to the north cannot both keep that rise and loop), while leaving
//! everything at ridge scale and below alone.

use std::f32::consts::TAU;

use bevy::tasks::{AsyncComputeTaskPool, TaskPool};
use rustfft::{FftDirection, FftPlanner, num_complex::Complex};

/// Replace `heights` (row-major, `width` × `height` texels) with its periodic
/// component, so the field tiles without a step.
///
/// `height` is the grid's row count, not an elevation. Cost is one complex
/// buffer of `width * height` — ~134 MB on a 4096² map — held only for the
/// duration of the call.
pub(crate) fn make_periodic(heights: &mut [f32], width: usize, height: usize) {
    if width < 2 || height < 2 || heights.len() != width * height {
        return;
    }

    // The boundary difference field: the whole Poisson forcing, nonzero only
    // on the four border lines. Corners take a contribution from each axis.
    let mut buf = vec![Complex::<f32>::ZERO; width * height];
    let last_row = (height - 1) * width;
    for x in 0..width {
        let step = heights[last_row + x] - heights[x];
        buf[x].re += step;
        buf[last_row + x].re -= step;
    }
    for y in 0..height {
        let row = y * width;
        let step = heights[row + width - 1] - heights[row];
        buf[row].re += step;
        buf[row + width - 1].re -= step;
    }

    fft2(&mut buf, width, height, FftDirection::Forward);

    // Divide by the discrete Laplacian's eigenvalues. Only DC has a zero
    // denominator (it is the decomposition's free additive constant), and the
    // relative landing in `world` re-floors the field anyway.
    let cos_x: Vec<f32> = (0..width).map(|x| (TAU * x as f32 / width as f32).cos()).collect();
    for y in 0..height {
        let cos_y = (TAU * y as f32 / height as f32).cos();
        for x in 0..width {
            let denominator = 2.0 * (cos_x[x] + cos_y - 2.0);
            let cell = &mut buf[y * width + x];
            *cell = if denominator == 0.0 {
                Complex::ZERO
            } else {
                *cell / denominator
            };
        }
    }

    fft2(&mut buf, width, height, FftDirection::Inverse);

    // rustfft leaves both passes unnormalized.
    let scale = 1.0 / (width * height) as f32;
    for (out, smooth) in heights.iter_mut().zip(buf.iter()) {
        *out -= smooth.re * scale;
    }
}

/// In-place 2-D FFT: every row, then every column.
fn fft2(buf: &mut [Complex<f32>], width: usize, height: usize, direction: FftDirection) {
    let pool = AsyncComputeTaskPool::get_or_init(TaskPool::default);
    let mut planner = FftPlanner::new();
    let row_fft = planner.plan_fft(width, direction);
    let column_fft = planner.plan_fft(height, direction);

    let rows_per_batch = height.div_ceil(pool.thread_num().max(1));
    pool.scope(|scope| {
        for chunk in buf.chunks_mut(rows_per_batch * width) {
            let row_fft = &row_fft;
            scope.spawn(async move {
                for row in chunk.chunks_mut(width) {
                    row_fft.process(row);
                }
            });
        }
    });

    // Columns are strided, so they go through a contiguous scratch buffer.
    // Single-threaded: splitting a strided view across threads needs either a
    // full transpose (double the peak memory) or unsafe aliasing.
    let mut column = vec![Complex::<f32>::ZERO; height];
    for x in 0..width {
        for (y, cell) in column.iter_mut().enumerate() {
            *cell = buf[y * width + x];
        }
        column_fft.process(&mut column);
        for (y, cell) in column.iter().enumerate() {
            buf[y * width + x] = *cell;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Worst case for a seam: a field that rises steadily across both axes, so
    /// opposite edges disagree by the full span.
    fn ramp_with_detail(size: usize) -> Vec<f32> {
        (0..size * size)
            .map(|i| {
                let (x, y) = ((i % size) as f32, (i / size) as f32);
                let detail = (x * 0.7).sin() * 3.0 + (y * 0.9).cos() * 3.0;
                x * 2.0 + y * 1.5 + detail
            })
            .collect()
    }

    /// The boundary difference field the decomposition is defined against.
    fn forcing(field: &[f32], size: usize) -> Vec<f32> {
        let mut v = vec![0.0; size * size];
        let last_row = (size - 1) * size;
        for x in 0..size {
            let step = field[last_row + x] - field[x];
            v[x] += step;
            v[last_row + x] -= step;
        }
        for y in 0..size {
            let row = y * size;
            let step = field[row + size - 1] - field[row];
            v[row] += step;
            v[row + size - 1] -= step;
        }
        v
    }

    /// RMS of the step where opposite edges meet, and of a typical step
    /// between neighbouring samples inside the map.
    fn seam_and_interior(field: &[f32], size: usize) -> (f32, f32) {
        let at = |x: usize, y: usize| field[y * size + x];
        let mut seam = 0.0;
        for i in 0..size {
            seam += (at(i, 0) - at(i, size - 1)).powi(2);
            seam += (at(0, i) - at(size - 1, i)).powi(2);
        }
        let mut interior = 0.0;
        for y in 1..size {
            for x in 1..size {
                interior += (at(x, y) - at(x, y - 1)).powi(2);
                interior += (at(x, y) - at(x - 1, y)).powi(2);
            }
        }
        (
            (seam / (2 * size) as f32).sqrt(),
            (interior / (2 * (size - 1) * (size - 1)) as f32).sqrt(),
        )
    }

    #[test]
    fn periodic_component_closes_the_seam() {
        let size = 64;
        let mut field = ramp_with_detail(size);
        let (before, _) = seam_and_interior(&field, size);
        make_periodic(&mut field, size, size);
        let (after, interior) = seam_and_interior(&field, size);

        assert!(before > 50.0, "ramp should start with a big seam, got {before}");
        // The bar that makes a seam invisible is "no worse than an ordinary
        // step inside the map", not zero.
        assert!(
            after < interior * 2.0,
            "seam {after} should fall to interior scale {interior} (was {before})"
        );
    }

    /// The decomposition's defining property: what gets removed is exactly the
    /// solution of the Poisson equation whose forcing is the boundary
    /// difference field, under periodic wrapping.
    ///
    /// This is also why fine detail survives. The forcing is zero everywhere
    /// off the border, so the removed component is harmonic there — it shifts
    /// and tilts absolute heights but adds no curvature of its own, leaving
    /// every ridge exactly as the source data had it.
    #[test]
    fn removed_component_solves_the_boundary_poisson_problem() {
        let size = 64;
        let original = ramp_with_detail(size);
        let mut field = original.clone();
        make_periodic(&mut field, size, size);

        let smooth: Vec<f32> = original.iter().zip(&field).map(|(h, p)| h - p).collect();
        let v = forcing(&original, size);
        let wrap = |i: usize, d: isize| (i as isize + d).rem_euclid(size as isize) as usize;
        let tolerance = v.iter().fold(0.0f32, |a, b| a.max(b.abs())) * 1e-2;

        for y in 0..size {
            for x in 0..size {
                let laplacian = smooth[wrap(y, -1) * size + x]
                    + smooth[wrap(y, 1) * size + x]
                    + smooth[y * size + wrap(x, -1)]
                    + smooth[y * size + wrap(x, 1)]
                    - 4.0 * smooth[y * size + x];
                let want = v[y * size + x];
                assert!(
                    (laplacian - want).abs() < tolerance,
                    "at ({x},{y}): Laplacian {laplacian} != forcing {want}"
                );
            }
        }
    }

    #[test]
    fn degenerate_sizes_are_ignored() {
        let mut single = vec![5.0];
        make_periodic(&mut single, 1, 1);
        assert_eq!(single, vec![5.0]);
        let mut mismatched = vec![1.0, 2.0, 3.0];
        make_periodic(&mut mismatched, 8, 8);
        assert_eq!(mismatched, vec![1.0, 2.0, 3.0]);
    }
}
