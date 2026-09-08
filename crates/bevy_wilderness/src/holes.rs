//! Terrain holes: primitive shapes in world XZ inside which the terrain isn't
//! drawn. The vertex shader tests each vertex against the packed shape list and
//! collapses the triangles of any vertex inside one (no fragment `discard`, so
//! early-Z survives on tiled GPUs). A host's physics cuts the same shapes out of
//! its own collider; this crate only removes the visuals.
//!
//! The cut is gated per LOD ring: a shape only cuts where it spans at least
//! `HOLE_MIN_CELLS` (terrain.wgsl) of that ring's cells, so a small hole seals
//! itself at distance instead of removing a coarse ring's huge triangles. Near
//! the camera the finest ring is texel-dense, so holes are texel-exact there.

use bevy::{prelude::*, render::render_resource::ShaderType};

/// Most shapes one clipmap can carry. Bounded by the uniform array below; the
/// test is per-vertex, so the ceiling is generous rather than tight.
pub const MAX_HOLES: usize = 24;

/// A hole in the terrain, in world XZ meters.
#[derive(Clone, Debug, PartialEq)]
pub enum HoleShape {
    /// An oriented rectangle. `basis` is the unit direction of its local +X in
    /// world XZ (local +Y is `basis` rotated +90°); `(1, 0)` is axis-aligned.
    /// Stored as a basis rather than an angle so consumers that need bit-exact
    /// agreement (a networked physics collider) never touch `sin`/`cos`.
    Rect {
        center: Vec2,
        half_extents: Vec2,
        basis: Vec2,
    },
    Circle {
        center: Vec2,
        radius: f32,
    },
}

impl HoleShape {
    /// Whether world XZ `p` lies inside the shape (boundary inclusive).
    /// Arithmetic only — `+ - *` and `abs` — so two machines agree bit-for-bit.
    pub fn contains(&self, p: Vec2) -> bool {
        match *self {
            HoleShape::Rect {
                center,
                half_extents,
                basis,
            } => {
                let d = p - center;
                let u = d.x * basis.x + d.y * basis.y;
                let v = -d.x * basis.y + d.y * basis.x;
                u.abs() <= half_extents.x && v.abs() <= half_extents.y
            }
            HoleShape::Circle { center, radius } => {
                let d = p - center;
                d.x * d.x + d.y * d.y <= radius * radius
            }
        }
    }

    /// Smallest width across the shape — what the LOD gate compares against a
    /// ring's cell size.
    fn min_extent(&self) -> f32 {
        match *self {
            HoleShape::Rect { half_extents, .. } => 2.0 * half_extents.min_element(),
            HoleShape::Circle { radius, .. } => 2.0 * radius,
        }
    }
}

/// The packed shape list handed to the shader: two `vec4` per shape, kept in a
/// named struct so it can be `Default` (fixed arrays past 32 aren't) and sit in
/// `DevParams` behind `#[reflect(ignore)]`. Layout is `[center.xy, basis.xy]`,
/// `[half.xy | radius, kind, min_extent]` — keep `unpack` in terrain.wgsl in
/// step with [`HoleBuffer::pack`].
#[derive(Clone, Copy, Debug, PartialEq, ShaderType)]
pub(crate) struct HoleBuffer {
    pub(crate) shapes: [Vec4; MAX_HOLES * 2],
}

impl Default for HoleBuffer {
    fn default() -> Self {
        Self {
            shapes: [Vec4::ZERO; MAX_HOLES * 2],
        }
    }
}

impl HoleBuffer {
    /// Pack `holes` (the first [`MAX_HOLES`]; the rest are dropped with a
    /// warning) and return the buffer with the count written.
    pub(crate) fn pack(holes: &[HoleShape]) -> (Self, u32) {
        if holes.len() > MAX_HOLES {
            warn!(
                "clipmap has {} holes; only the first {MAX_HOLES} are drawn",
                holes.len()
            );
        }
        let mut buffer = Self::default();
        let mut count = 0u32;
        for shape in holes.iter().take(MAX_HOLES) {
            let (a, b) = match *shape {
                HoleShape::Rect {
                    center,
                    half_extents,
                    basis,
                } => (
                    Vec4::new(center.x, center.y, basis.x, basis.y),
                    Vec4::new(half_extents.x, half_extents.y, 0.0, shape.min_extent()),
                ),
                HoleShape::Circle { center, radius } => (
                    Vec4::new(center.x, center.y, 1.0, 0.0),
                    Vec4::new(radius, radius, 1.0, shape.min_extent()),
                ),
            };
            let i = count as usize * 2;
            buffer.shapes[i] = a;
            buffer.shapes[i + 1] = b;
            count += 1;
        }
        (buffer, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_respects_its_basis() {
        // 10 m × 2 m rect rotated 90°: long axis now runs along world Z.
        let rect = HoleShape::Rect {
            center: Vec2::new(100.0, 50.0),
            half_extents: Vec2::new(5.0, 1.0),
            basis: Vec2::new(0.0, 1.0),
        };
        assert!(rect.contains(Vec2::new(100.0, 54.9)));
        assert!(!rect.contains(Vec2::new(104.9, 50.0)));
        assert!(rect.contains(Vec2::new(100.9, 50.0)));
        // Boundary is inclusive.
        assert!(rect.contains(Vec2::new(101.0, 55.0)));
        assert!(!rect.contains(Vec2::new(101.01, 55.0)));
    }

    #[test]
    fn circle_contains_by_radius() {
        let circle = HoleShape::Circle {
            center: Vec2::new(-20.0, 0.0),
            radius: 3.0,
        };
        assert!(circle.contains(Vec2::new(-20.0, 3.0)));
        assert!(circle.contains(Vec2::new(-18.0, 2.0)));
        assert!(!circle.contains(Vec2::new(-17.0, 2.0)));
    }

    #[test]
    fn pack_caps_at_max_and_writes_count() {
        let holes: Vec<_> = (0..MAX_HOLES + 3)
            .map(|i| HoleShape::Circle {
                center: Vec2::splat(i as f32),
                radius: 1.0,
            })
            .collect();
        let (buffer, count) = HoleBuffer::pack(&holes);
        assert_eq!(count as usize, MAX_HOLES);
        assert_eq!(buffer.shapes[0], Vec4::new(0.0, 0.0, 1.0, 0.0));
        assert_eq!(buffer.shapes[1], Vec4::new(1.0, 1.0, 1.0, 2.0));
        let last = (MAX_HOLES - 1) * 2;
        assert_eq!(buffer.shapes[last].x, (MAX_HOLES - 1) as f32);
    }
}
