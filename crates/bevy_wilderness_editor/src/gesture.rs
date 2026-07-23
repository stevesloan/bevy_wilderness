//! Terrain tile snapshots (D8): the built-in tools' [`UndoAction`], built up
//! over a gesture.
//!
//! One input gesture (a stroke's press→release, one erosion run) becomes one
//! [`TileAction`] on the [`UndoHistory`]: the first time the gesture touches a
//! 64²-texel tile, that tile's *pre-edit* contents are copied (~16 KB each) —
//! so an entry costs what it touched, never a full-field snapshot. Undo pastes
//! the saved tiles back and marks them dirty; the existing sync path then
//! handles re-quantize, [`TerrainRegionChanged`](crate::TerrainRegionChanged)
//! (prop re-snap), and the re-bake debounce — undo gets correct shading for
//! free. Redo captures the current tiles on first undo, so it round-trips.
//!
//! Snapshots are keyed by *(buffer, tile)*, so one gesture spanning several
//! buffers — a tool that moves ground *and* paints mask — undoes as one entry.
//! Tools call [`begin`](TerrainGesture::begin) / [`capture`](TerrainGesture::capture)
//! / [`seal`](TerrainGesture::seal) around their strokes (the built-in sculpt
//! does); the generic stack itself never sees terrain types.

use bevy::{platform::collections::HashMap, prelude::*};

use crate::terrain::EditableTerrain;
use crate::undo::{UndoAction, UndoHistory};

/// Snapshot tile edge, in texels (64² × f32 ≈ 16 KB per height tile).
pub const UNDO_TILE_SIZE: u32 = 64;

/// Which editable buffer a tile snapshot belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum UndoBuffer {
    /// The f32 height field.
    Height,
    /// The 0..1 paint mask (D4).
    Mask,
}

impl UndoBuffer {
    fn copy_rect(&self, terrain: &EditableTerrain, rect: URect) -> Vec<f32> {
        match self {
            UndoBuffer::Height => terrain.field.copy_rect(rect),
            UndoBuffer::Mask => terrain.mask_copy_rect(rect),
        }
    }

    fn paste_rect(&self, terrain: &mut EditableTerrain, rect: URect, data: &[f32]) {
        match self {
            UndoBuffer::Height => {
                terrain.field.paste_rect(rect, data);
                terrain.mark_dirty(rect);
            }
            UndoBuffer::Mask => {
                terrain.mask_paste_rect(rect, data);
                terrain.mark_mask_dirty(rect);
            }
        }
    }
}

/// One tile's saved contents.
struct TileSnapshot {
    rect: URect,
    data: Vec<f32>,
}

/// One gesture's tile snapshots on one terrain — the terrain tools' entry on
/// the generic [`UndoHistory`].
pub struct TileAction {
    terrain: Entity,
    /// Pre-edit tiles, captured on first touch during the gesture.
    old: HashMap<(UndoBuffer, UVec2), TileSnapshot>,
    /// Post-edit tiles, captured lazily on first undo (for redo).
    new: Option<HashMap<(UndoBuffer, UVec2), TileSnapshot>>,
}

fn tile_bytes(map: &HashMap<(UndoBuffer, UVec2), TileSnapshot>) -> usize {
    map.values().map(|t| t.data.len() * 4).sum()
}

impl UndoAction for TileAction {
    /// Restore the pre-edit tiles (capturing the current state for redo
    /// first) and mark them dirty, so quantize, events, and the re-bake
    /// debounce follow on the normal path. No-op if the terrain is gone.
    fn undo(&mut self, world: &mut World) {
        let Some(mut terrain) = world.get_mut::<EditableTerrain>(self.terrain) else {
            return;
        };
        if self.new.is_none() {
            self.new = Some(
                self.old
                    .iter()
                    .map(|(key, snap)| {
                        let current = TileSnapshot {
                            rect: snap.rect,
                            data: key.0.copy_rect(&terrain, snap.rect),
                        };
                        (*key, current)
                    })
                    .collect(),
            );
        }
        for (key, snap) in &self.old {
            key.0.paste_rect(&mut terrain, snap.rect, &snap.data);
        }
    }

    fn redo(&mut self, world: &mut World) {
        let Some(mut terrain) = world.get_mut::<EditableTerrain>(self.terrain) else {
            return;
        };
        let Some(new) = &self.new else {
            return;
        };
        for (key, snap) in new {
            key.0.paste_rect(&mut terrain, snap.rect, &snap.data);
        }
    }

    fn bytes(&self) -> usize {
        tile_bytes(&self.old) + self.new.as_ref().map(tile_bytes).unwrap_or(0)
    }
}

/// The open terrain gesture, if any (between [`begin`](Self::begin) and
/// [`seal`](Self::seal)). A resource of its own so the generic
/// [`UndoHistory`] stays terrain-free; only the terrain tools touch this.
#[derive(Resource, Default)]
pub struct TerrainGesture {
    pending: Option<(String, TileAction)>,
}

impl TerrainGesture {
    /// Open a gesture on `terrain` (sealing any still-open one). `label` is
    /// what a UI shows next to undo/redo ("Sculpt (Raise)").
    pub fn begin(&mut self, history: &mut UndoHistory, terrain: Entity, label: impl Into<String>) {
        self.seal(history);
        self.pending = Some((
            label.into(),
            TileAction {
                terrain,
                old: HashMap::default(),
                new: None,
            },
        ));
    }

    /// Record `buffer`'s `rect` (texel space, in-bounds — pass brush
    /// footprints through [`TerrainField::wrap_rect`](crate::TerrainField::wrap_rect)
    /// first) as about-to-change. Must be called **before** mutating: tiles
    /// already captured this gesture are skipped, so only first-touch data is
    /// copied. No-op without an open gesture. `terrain` must be the one the
    /// gesture was begun for.
    pub fn capture(&mut self, terrain: &EditableTerrain, buffer: UndoBuffer, rect: URect) {
        let Some((_, action)) = &mut self.pending else {
            return;
        };
        for (tile, tile_rect) in tiles_in(rect, terrain.field.dimensions()) {
            action
                .old
                .entry((buffer, tile))
                .or_insert_with(|| TileSnapshot {
                    rect: tile_rect,
                    data: buffer.copy_rect(terrain, tile_rect),
                });
        }
    }

    /// Seal the open gesture onto the history (dropping it if it captured
    /// nothing).
    pub fn seal(&mut self, history: &mut UndoHistory) {
        let Some((label, action)) = self.pending.take() else {
            return;
        };
        if action.old.is_empty() {
            return;
        }
        history.push(label, action);
    }

    /// Whether a gesture is currently open. Deferred appliers (the erosion
    /// result landing from its background task) check this and wait a frame,
    /// so they never seal another tool's stroke mid-gesture and split its
    /// entry.
    pub fn open(&self) -> bool {
        self.pending.is_some()
    }

    /// Drop the open gesture without recording it. Called when the whole
    /// field is replaced — its snapshots describe a field that no longer
    /// exists.
    pub fn abandon(&mut self) {
        self.pending = None;
    }
}

/// The snapshot tiles overlapping `rect`, as `(tile coord, in-bounds rect)`.
fn tiles_in(rect: URect, dims: UVec2) -> impl Iterator<Item = (UVec2, URect)> {
    let t = UNDO_TILE_SIZE;
    let tx0 = rect.min.x / t;
    let ty0 = rect.min.y / t;
    // Max-exclusive rect: the last touched texel is max - 1.
    let tx1 = rect.max.x.saturating_sub(1) / t;
    let ty1 = rect.max.y.saturating_sub(1) / t;
    (ty0..=ty1).flat_map(move |ty| {
        (tx0..=tx1).map(move |tx| {
            let tile_rect = URect::new(
                tx * t,
                ty * t,
                ((tx + 1) * t).min(dims.x),
                ((ty + 1) * t).min(dims.y),
            );
            (UVec2::new(tx, ty), tile_rect)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::TerrainField;

    fn world_with_terrain(width: u32) -> (World, Entity) {
        let field = TerrainField::flat(width, width, 1.0, -100.0, 100.0, false, 0.0);
        let mut world = World::new();
        let entity = world.spawn(EditableTerrain::new(field)).id();
        (world, entity)
    }

    fn set_height(world: &mut World, entity: Entity, x: u32, y: u32, h: f32) {
        world
            .get_mut::<EditableTerrain>(entity)
            .unwrap()
            .field
            .set(x, y, h);
    }

    fn height(world: &World, entity: Entity, x: u32, y: u32) -> f32 {
        world
            .get::<EditableTerrain>(entity)
            .unwrap()
            .field
            .get(x as i64, y as i64)
    }

    fn capture(world: &World, gesture: &mut TerrainGesture, entity: Entity, rect: URect) {
        let terrain = world.get::<EditableTerrain>(entity).unwrap();
        gesture.capture(terrain, UndoBuffer::Height, rect);
    }

    #[test]
    fn undo_restores_and_redo_reapplies() {
        let (mut world, entity) = world_with_terrain(128);
        let mut history = UndoHistory::default();
        let mut gesture = TerrainGesture::default();

        gesture.begin(&mut history, entity, "Sculpt (Raise)");
        capture(&world, &mut gesture, entity, URect::new(10, 10, 20, 20));
        set_height(&mut world, entity, 12, 12, 50.0);
        gesture.seal(&mut history);

        let label = history.undo(&mut world);
        assert_eq!(label.as_deref(), Some("Sculpt (Raise)"));
        assert_eq!(height(&world, entity, 12, 12), 0.0, "undo restores");

        let label = history.redo(&mut world);
        assert_eq!(label.as_deref(), Some("Sculpt (Raise)"));
        assert_eq!(height(&world, entity, 12, 12), 50.0, "redo re-applies");
        assert!(history.undo_label().is_some());
        assert!(history.redo_label().is_none());
    }

    #[test]
    fn empty_gestures_leave_no_entry() {
        let (_, entity) = world_with_terrain(64);
        let mut history = UndoHistory::default();
        let mut gesture = TerrainGesture::default();
        gesture.begin(&mut history, entity, "nothing");
        gesture.seal(&mut history);
        assert_eq!(history.undo_label(), None);
        assert!(!gesture.open());
    }

    #[test]
    fn begin_seals_the_previous_gesture() {
        let (mut world, entity) = world_with_terrain(128);
        let mut history = UndoHistory::default();
        let mut gesture = TerrainGesture::default();

        gesture.begin(&mut history, entity, "first");
        capture(&world, &mut gesture, entity, URect::new(0, 0, 8, 8));
        set_height(&mut world, entity, 1, 1, 10.0);
        // A new begin without an explicit seal still records "first".
        gesture.begin(&mut history, entity, "second");
        assert_eq!(history.undo_label(), Some("first"));
    }

    #[test]
    fn capture_only_copies_first_touch() {
        let (mut world, entity) = world_with_terrain(128);
        let mut history = UndoHistory::default();
        let mut gesture = TerrainGesture::default();
        gesture.begin(&mut history, entity, "stroke");
        capture(&world, &mut gesture, entity, URect::new(10, 10, 20, 20));
        set_height(&mut world, entity, 12, 12, 50.0);
        // Second capture of the same tile mid-gesture must keep the original
        // pre-stroke data, not the half-edited state.
        capture(&world, &mut gesture, entity, URect::new(10, 10, 20, 20));
        set_height(&mut world, entity, 12, 12, 80.0);
        gesture.seal(&mut history);

        history.undo(&mut world);
        assert_eq!(height(&world, entity, 12, 12), 0.0);
    }

    #[test]
    fn eviction_drops_the_oldest_entries() {
        let (mut world, entity) = world_with_terrain(256);
        let mut history = UndoHistory::default();
        // Room for roughly two 1-tile entries (16 KB each), not three.
        history.max_bytes = 40 * 1024;
        let mut gesture = TerrainGesture::default();
        for i in 0..3u32 {
            gesture.begin(&mut history, entity, format!("stroke {i}"));
            capture(
                &world,
                &mut gesture,
                entity,
                URect::new(i * 64, 0, i * 64 + 8, 8),
            );
            set_height(&mut world, entity, i * 64, 0, i as f32 + 1.0);
            gesture.seal(&mut history);
        }
        assert_eq!(history.undo_label(), Some("stroke 2"));
        history.undo(&mut world);
        history.undo(&mut world);
        // "stroke 0" was evicted; only two undos are possible.
        assert_eq!(
            history.undo(&mut world),
            None,
            "oldest entry must have been evicted"
        );
        assert_eq!(height(&world, entity, 0, 0), 1.0, "stroke 0 survives");
    }

    #[test]
    fn undo_survives_a_despawned_terrain() {
        let (mut world, entity) = world_with_terrain(128);
        let mut history = UndoHistory::default();
        let mut gesture = TerrainGesture::default();
        gesture.begin(&mut history, entity, "stroke");
        capture(&world, &mut gesture, entity, URect::new(0, 0, 8, 8));
        set_height(&mut world, entity, 1, 1, 10.0);
        gesture.seal(&mut history);

        world.despawn(entity);
        // The action no-ops but the cursor still moves — no panic, no stall.
        assert_eq!(history.undo(&mut world).as_deref(), Some("stroke"));
        assert_eq!(history.undo(&mut world), None);
    }
}
