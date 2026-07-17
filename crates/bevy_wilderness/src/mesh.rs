//! GPU-clipmap ring mesh topology (Hoppe): the square / filler / center / trim /
//! stitch parts, built once per clipmap and shared across LOD levels.

use std::collections::HashMap;

use bevy::{
    asset::RenderAssetUsages,
    camera::primitives::Aabb,
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
};

/// One clipmap ring part: its mesh handle + culling bounds.
pub(crate) struct ClipmapPart {
    pub(crate) handle: Handle<Mesh>,
    pub(crate) aabb: Aabb,
}

impl ClipmapPart {
    fn build(meshes: &mut Assets<Mesh>, builder: MeshBuilder) -> Self {
        let mut min = Vec3::from_slice(&builder.vertices[0]);
        let mut max = min;
        for v in builder.vertices.iter().map(|v| Vec3::from_slice(v)) {
            min = min.min(v);
            max = max.max(v);
        }
        Self {
            handle: meshes.add(builder.build()),
            aabb: Aabb::from_min_max(min, max),
        }
    }
}

/// The five ring parts of a clipmap.
#[derive(Component)]
pub(crate) struct ClipmapParts {
    pub(crate) square: ClipmapPart,
    pub(crate) filler: ClipmapPart,
    pub(crate) center: ClipmapPart,
    pub(crate) trim: ClipmapPart,
    pub(crate) stitch: ClipmapPart,
}

/// Build the five ring-part meshes for a clipmap of the given half-width.
pub(crate) fn build_clipmap_parts(meshes: &mut Assets<Mesh>, half_width: u32) -> ClipmapParts {
    let builder_width = half_width as i32 * 2;
    let filler_width = 2 - half_width as i32 % 2;
    let square_width = (half_width as i32 - filler_width) / 2;

    let mut square = MeshBuilder::new();
    let mut filler = MeshBuilder::new();
    let mut center = MeshBuilder::new();
    let mut trim = MeshBuilder::new();
    let mut stitch = MeshBuilder::new();

    for xy in 0..builder_width.pow(2) {
        let x = xy % builder_width;
        let y = xy / builder_width;
        if x < square_width && y < square_width {
            square.add_square(x, y);
        }
        let range = square_width * 2..square_width * 2 + filler_width;
        if (range.contains(&x) || range.contains(&y))
            && x < builder_width - filler_width
            && y < builder_width - filler_width
        {
            center.add_square(x, y);
            let range = square_width..builder_width - square_width - filler_width;
            if !range.contains(&x) || !range.contains(&y) {
                filler.add_square(x, y);
            }
        }
        if x >= builder_width - filler_width || y >= builder_width - filler_width {
            trim.add_square(x, y);
        }
    }

    for x in 0..builder_width / 2 {
        let x = x * 2;
        stitch.add_triangle(x, 0, x + 1, 0, x + 2, 0);
        stitch.add_triangle(x + 2, builder_width, x + 1, builder_width, x, builder_width);
        stitch.add_triangle(0, x + 2, 0, x + 1, 0, x);
        stitch.add_triangle(builder_width, x, builder_width, x + 1, builder_width, x + 2);
    }

    ClipmapParts {
        square: ClipmapPart::build(meshes, square),
        filler: ClipmapPart::build(meshes, filler),
        center: ClipmapPart::build(meshes, center),
        trim: ClipmapPart::build(meshes, trim),
        stitch: ClipmapPart::build(meshes, stitch),
    }
}

/// Accumulates a triangle mesh, de-duplicating vertices by integer grid position.
struct MeshBuilder {
    unique_vertices: HashMap<(i32, i32), u32>,
    vertices: Vec<[f32; 3]>,
    indices: Vec<u32>,
}

impl MeshBuilder {
    fn new() -> Self {
        Self {
            unique_vertices: HashMap::new(),
            vertices: vec![],
            indices: vec![],
        }
    }

    fn add_vertex(&mut self, x: i32, y: i32) -> u32 {
        if let Some(index) = self.unique_vertices.get(&(x, y)) {
            *index
        } else {
            let index = self.vertices.len() as u32;
            self.vertices.push([x as f32, 0.0, y as f32]);
            self.unique_vertices.insert((x, y), index);
            index
        }
    }

    fn add_triangle(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, x3: i32, y3: i32) {
        let p1 = self.add_vertex(x1, y1);
        let p2 = self.add_vertex(x2, y2);
        let p3 = self.add_vertex(x3, y3);
        self.indices.extend([p1, p2, p3]);
    }

    fn add_square(&mut self, x: i32, y: i32) {
        let p1 = self.add_vertex(x, y);
        let p2 = self.add_vertex(x, y + 1);
        let p3 = self.add_vertex(x + 1, y + 1);
        let p4 = self.add_vertex(x + 1, y);
        self.indices.extend([p1, p2, p3]);
        self.indices.extend([p1, p3, p4]);
    }

    fn build(self) -> Mesh {
        Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::all())
            .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, self.vertices)
            .with_inserted_indices(Indices::U32(self.indices))
    }
}
