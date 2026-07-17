//! Undo/history (D8): tile-based region snapshots in a byte-capped ring.
//!
//! One input gesture (a stroke's press→release, one erosion run) is one
//! [`UndoEntry`]: the first time the gesture touches a 64²-texel tile, that
//! tile's *pre-edit* heights are copied into the entry (~16 KB each) — so an
//! entry costs what it touched, never a full-field snapshot. Undo pastes the
//! saved tiles back and marks them dirty; the existing sync path then handles
//! re-quantize, [`TerrainRegionChanged`](crate::TerrainRegionChanged) (prop
//! re-snap), and the re-bake debounce — undo gets correct shading for free.
//! Redo captures the current tiles into the entry before restoring, so it
//! round-trips.
//!
//! Entries are keyed by *(buffer, tile)* so later buffers (the Phase 4 mask)
//! join the same machinery. Per P2 this is core API: a UI (or a Ctrl+Z keybind
//! in the example) merely calls [`UndoHistory::undo`] / [`redo`]
//! (UndoHistory::redo).

use bevy::{platform::collections::HashMap, prelude::*};

use crate::terrain::EditableTerrain;

/// Snapshot tile edge, in texels (64² × f32 ≈ 16 KB per height tile).
pub const UNDO_TILE_SIZE: u32 = 64;

/// History byte budget before the oldest entries are evicted.
const DEFAULT_MAX_BYTES: usize = 256 * 1024 * 1024;

/// Which editable buffer a tile snapshot belongs to (D8: one entry can span
/// several buffers — a tool that moves ground *and* paints mask undoes as one
/// gesture).
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

/// One undoable gesture on one terrain.
struct UndoEntry {
    terrain: Entity,
    label: String,
    /// Pre-edit tiles, captured on first touch during the gesture.
    old: HashMap<(UndoBuffer, UVec2), TileSnapshot>,
    /// Post-edit tiles, captured lazily on first undo (for redo).
    new: Option<HashMap<(UndoBuffer, UVec2), TileSnapshot>>,
}

impl UndoEntry {
    fn bytes(&self) -> usize {
        let tiles = |m: &HashMap<(UndoBuffer, UVec2), TileSnapshot>| {
            m.values().map(|t| t.data.len() * 4).sum::<usize>()
        };
        tiles(&self.old) + self.new.as_ref().map(tiles).unwrap_or(0)
    }
}

/// The edit history (D8). Tools call [`begin`](Self::begin) /
/// [`capture`](Self::capture) / [`seal`](Self::seal) around their gestures
/// (the built-in sculpt does); a UI calls [`undo`](Self::undo) /
/// [`redo`](Self::redo).
#[derive(Resource)]
pub struct UndoHistory {
    /// Sealed entries; `entries[..cursor]` are undoable, the rest redoable.
    entries: Vec<UndoEntry>,
    cursor: usize,
    /// The open gesture, if any (between `begin` and `seal`).
    pending: Option<UndoEntry>,
    /// Byte budget; oldest entries evict beyond it. Host-tunable.
    pub max_bytes: usize,
}

impl Default for UndoHistory {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            cursor: 0,
            pending: None,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl UndoHistory {
    /// Open an undo entry for a gesture on `terrain` (sealing any still-open
    /// one). `label` is what a UI shows next to undo/redo ("Sculpt (Raise)").
    pub fn begin(&mut self, terrain: Entity, label: impl Into<String>) {
        self.seal();
        self.pending = Some(UndoEntry {
            terrain,
            label: label.into(),
            old: HashMap::default(),
            new: None,
        });
    }

    /// Record `buffer`'s `rect` (texel space, in-bounds — pass brush footprints
    /// through [`TerrainField::wrap_rect`](crate::TerrainField::wrap_rect)
    /// first) as about-to-change. Must be called **before** mutating: tiles
    /// already captured this gesture are skipped, so only first-touch data is
    /// copied. No-op without an open entry. `terrain` must be the one this
    /// entry was begun for.
    pub fn capture(&mut self, terrain: &EditableTerrain, buffer: UndoBuffer, rect: URect) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        for (tile, tile_rect) in tiles_in(rect, terrain.field.dimensions()) {
            pending
                .old
                .entry((buffer, tile))
                .or_insert_with(|| TileSnapshot {
                    rect: tile_rect,
                    data: buffer.copy_rect(terrain, tile_rect),
                });
        }
    }

    /// Seal the open gesture into the history (dropping it if it captured
    /// nothing), truncating any redo tail and evicting the oldest entries
    /// beyond [`max_bytes`](Self::max_bytes).
    pub fn seal(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if pending.old.is_empty() {
            return;
        }
        self.entries.truncate(self.cursor);
        self.entries.push(pending);
        self.cursor = self.entries.len();
        let mut total: usize = self.entries.iter().map(UndoEntry::bytes).sum();
        while total > self.max_bytes && self.cursor > 1 {
            total -= self.entries.remove(0).bytes();
            self.cursor -= 1;
        }
    }

    /// Undo the most recent entry: restore its pre-edit tiles (capturing the
    /// current state for redo first) and mark them dirty, so quantize, events,
    /// and the re-bake debounce follow on the normal path. Returns the entry's
    /// label, or `None` if there's nothing to undo (or its terrain is gone).
    pub fn undo(&mut self, terrains: &mut Query<&mut EditableTerrain>) -> Option<String> {
        self.seal();
        let entry = self.entries[..self.cursor].last_mut()?;
        let mut terrain = terrains.get_mut(entry.terrain).ok()?;
        // First undo of this entry: capture the post-edit state for redo.
        if entry.new.is_none() {
            entry.new = Some(
                entry
                    .old
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
        for (key, snap) in &entry.old {
            key.0.paste_rect(&mut terrain, snap.rect, &snap.data);
        }
        self.cursor -= 1;
        Some(entry.label.clone())
    }

    /// Re-apply the most recently undone entry. Returns its label, or `None`
    /// if there's nothing to redo.
    pub fn redo(&mut self, terrains: &mut Query<&mut EditableTerrain>) -> Option<String> {
        let entry = self.entries.get(self.cursor)?;
        let mut terrain = terrains.get_mut(entry.terrain).ok()?;
        for (key, snap) in entry.new.as_ref()? {
            key.0.paste_rect(&mut terrain, snap.rect, &snap.data);
        }
        self.cursor += 1;
        Some(entry.label.clone())
    }

    /// Drop all history (undo, redo, and any open gesture). Called when the
    /// whole field is replaced — a new or loaded terrain — since the tile
    /// snapshots describe a field that no longer exists.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.cursor = 0;
        self.pending = None;
    }

    /// Whether a gesture is currently open (between [`begin`](Self::begin) and
    /// [`seal`](Self::seal)). Deferred appliers (the erosion result landing
    /// from its background task) check this and wait a frame, so they never
    /// seal another tool's stroke mid-gesture and split its entry.
    pub fn gesture_open(&self) -> bool {
        self.pending.is_some()
    }

    /// The label the next [`undo`](Self::undo) would revert (for UI).
    pub fn undo_label(&self) -> Option<&str> {
        self.entries[..self.cursor].last().map(|e| e.label.as_str())
    }

    /// The label the next [`redo`](Self::redo) would re-apply (for UI).
    pub fn redo_label(&self) -> Option<&str> {
        self.entries.get(self.cursor).map(|e| e.label.as_str())
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
    use bevy::ecs::system::SystemState;

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

    fn height(world: &mut World, entity: Entity, x: u32, y: u32) -> f32 {
        world
            .get::<EditableTerrain>(entity)
            .unwrap()
            .field
            .get(x as i64, y as i64)
    }

    #[test]
    fn undo_restores_and_redo_reapplies() {
        let (mut world, entity) = world_with_terrain(128);
        let mut history = UndoHistory::default();

        history.begin(entity, "Sculpt (Raise)");
        {
            let terrain = world.get::<EditableTerrain>(entity).unwrap();
            history.capture(terrain, UndoBuffer::Height, URect::new(10, 10, 20, 20));
        }
        set_height(&mut world, entity, 12, 12, 50.0);
        history.seal();

        let mut state: SystemState<Query<&mut EditableTerrain>> = SystemState::new(&mut world);
        let label = history.undo(&mut state.get_mut(&mut world).unwrap());
        assert_eq!(label.as_deref(), Some("Sculpt (Raise)"));
        assert_eq!(height(&mut world, entity, 12, 12), 0.0, "undo restores");

        let label = history.redo(&mut state.get_mut(&mut world).unwrap());
        assert_eq!(label.as_deref(), Some("Sculpt (Raise)"));
        assert_eq!(height(&mut world, entity, 12, 12), 50.0, "redo re-applies");
        assert!(history.undo_label().is_some());
        assert!(history.redo_label().is_none());
    }

    #[test]
    fn new_gesture_truncates_the_redo_tail() {
        let (mut world, entity) = world_with_terrain(128);
        let mut history = UndoHistory::default();
        let mut state: SystemState<Query<&mut EditableTerrain>> = SystemState::new(&mut world);

        history.begin(entity, "first");
        {
            let terrain = world.get::<EditableTerrain>(entity).unwrap();
            history.capture(terrain, UndoBuffer::Height, URect::new(0, 0, 8, 8));
        }
        set_height(&mut world, entity, 1, 1, 10.0);
        history.seal();
        history.undo(&mut state.get_mut(&mut world).unwrap());

        // A new gesture after undo discards the redoable "first".
        history.begin(entity, "second");
        {
            let terrain = world.get::<EditableTerrain>(entity).unwrap();
            history.capture(terrain, UndoBuffer::Height, URect::new(0, 0, 8, 8));
        }
        set_height(&mut world, entity, 2, 2, 20.0);
        history.seal();
        assert_eq!(history.redo_label(), None);
        assert_eq!(history.undo_label(), Some("second"));
    }

    #[test]
    fn empty_gestures_leave_no_entry() {
        let (_, entity) = world_with_terrain(64);
        let mut history = UndoHistory::default();
        history.begin(entity, "nothing");
        history.seal();
        assert_eq!(history.undo_label(), None);
    }

    #[test]
    fn capture_only_copies_first_touch() {
        let (mut world, entity) = world_with_terrain(128);
        let mut history = UndoHistory::default();
        history.begin(entity, "stroke");
        {
            let terrain = world.get::<EditableTerrain>(entity).unwrap();
            history.capture(terrain, UndoBuffer::Height, URect::new(10, 10, 20, 20));
        }
        set_height(&mut world, entity, 12, 12, 50.0);
        // Second capture of the same tile mid-gesture must keep the original
        // pre-stroke data, not the half-edited state.
        {
            let terrain = world.get::<EditableTerrain>(entity).unwrap();
            history.capture(terrain, UndoBuffer::Height, URect::new(10, 10, 20, 20));
        }
        set_height(&mut world, entity, 12, 12, 80.0);
        history.seal();

        let mut state: SystemState<Query<&mut EditableTerrain>> = SystemState::new(&mut world);
        history.undo(&mut state.get_mut(&mut world).unwrap());
        assert_eq!(height(&mut world, entity, 12, 12), 0.0);
    }

    #[test]
    fn eviction_drops_the_oldest_entries() {
        let (mut world, entity) = world_with_terrain(256);
        let mut history = UndoHistory {
            // Room for roughly two 1-tile entries (16 KB each), not three.
            max_bytes: 40 * 1024,
            ..default()
        };
        let mut state: SystemState<Query<&mut EditableTerrain>> = SystemState::new(&mut world);
        for i in 0..3u32 {
            history.begin(entity, format!("stroke {i}"));
            {
                let terrain = world.get::<EditableTerrain>(entity).unwrap();
                history.capture(
                    terrain,
                    UndoBuffer::Height,
                    URect::new(i * 64, 0, i * 64 + 8, 8),
                );
            }
            set_height(&mut world, entity, i * 64, 0, i as f32 + 1.0);
            history.seal();
        }
        assert_eq!(history.undo_label(), Some("stroke 2"));
        history.undo(&mut state.get_mut(&mut world).unwrap());
        history.undo(&mut state.get_mut(&mut world).unwrap());
        // "stroke 0" was evicted; only two undos are possible.
        assert_eq!(
            history.undo(&mut state.get_mut(&mut world).unwrap()),
            None,
            "oldest entry must have been evicted"
        );
        assert_eq!(height(&mut world, entity, 0, 0), 1.0, "stroke 0 survives");
    }
}
