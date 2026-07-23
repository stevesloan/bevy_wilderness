//! Undo/history (D8): a generic, byte-capped action stack.
//!
//! The stack knows nothing about terrain. Each entry is a label plus a boxed
//! [`UndoAction`]; the built-in terrain tools push tile-snapshot actions
//! through [`TerrainGesture`](crate::TerrainGesture), and a host's own tools
//! (prop placement, level objects) push theirs via [`UndoHistory::push`] — so
//! terrain strokes and host edits interleave in one undo stream, in order.
//!
//! Per P2 this is core API: a UI (or a Ctrl+Z keybind) merely writes
//! [`UndoRequest`] / [`RedoRequest`]; the editor applies them with exclusive
//! world access between `EditorSet::Tools` and `EditorSet::Apply` and reports
//! each application as [`UndoApplied`] (status lines, logs).

use bevy::prelude::*;

/// History byte budget before the oldest entries are evicted.
const DEFAULT_MAX_BYTES: usize = 256 * 1024 * 1024;

/// One undoable operation. Implementations carry everything needed to revert
/// and re-apply themselves (an action referring to entities that may despawn
/// and respawn should resolve them through a stable host id, not `Entity`).
pub trait UndoAction: Send + Sync + 'static {
    /// Revert the action's effect. Runs with exclusive world access.
    fn undo(&mut self, world: &mut World);
    /// Re-apply the action after an [`undo`](Self::undo).
    fn redo(&mut self, world: &mut World);
    /// Heap bytes held, for the history's eviction budget.
    fn bytes(&self) -> usize {
        0
    }
}

/// One sealed entry: what a UI shows next to undo/redo, and how to apply it.
struct UndoEntry {
    label: String,
    action: Box<dyn UndoAction>,
}

/// Undo this. Written by a UI's button or a host keybind; applied (newest
/// entry first) by the editor's exclusive apply system.
#[derive(Message, Debug, Clone, Default)]
pub struct UndoRequest;

/// Redo the most recently undone entry.
#[derive(Message, Debug, Clone, Default)]
pub struct RedoRequest;

/// An entry was applied: `label` is what it was, `redone` distinguishes
/// redo from undo. For status lines and logs; requests with nothing to apply
/// emit nothing.
#[derive(Message, Debug, Clone)]
pub struct UndoApplied {
    pub label: String,
    pub redone: bool,
}

/// The edit history (D8): sealed entries in a byte-capped ring.
/// `entries[..cursor]` are undoable, the rest redoable.
#[derive(Resource)]
pub struct UndoHistory {
    entries: Vec<UndoEntry>,
    cursor: usize,
    /// Byte budget; oldest entries evict beyond it. Host-tunable.
    pub max_bytes: usize,
}

impl Default for UndoHistory {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            cursor: 0,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl UndoHistory {
    /// Push a completed action onto the history, truncating any redo tail and
    /// evicting the oldest entries beyond [`max_bytes`](Self::max_bytes).
    /// `label` is what a UI shows next to undo/redo ("Sculpt (Raise)",
    /// "Place building").
    pub fn push(&mut self, label: impl Into<String>, action: impl UndoAction) {
        self.entries.truncate(self.cursor);
        self.entries.push(UndoEntry {
            label: label.into(),
            action: Box::new(action),
        });
        self.cursor = self.entries.len();
        let mut total: usize = self.entries.iter().map(|e| e.action.bytes()).sum();
        while total > self.max_bytes && self.cursor > 1 {
            total -= self.entries.remove(0).action.bytes();
            self.cursor -= 1;
        }
    }

    /// Undo the most recent entry, returning its label ([`None`] if there is
    /// nothing to undo). Prefer writing [`UndoRequest`] over calling this
    /// directly — the apply system also seals any open terrain gesture first.
    pub fn undo(&mut self, world: &mut World) -> Option<String> {
        let entry = self.entries[..self.cursor].last_mut()?;
        entry.action.undo(world);
        self.cursor -= 1;
        Some(entry.label.clone())
    }

    /// Re-apply the most recently undone entry, returning its label ([`None`]
    /// if there is nothing to redo).
    pub fn redo(&mut self, world: &mut World) -> Option<String> {
        let entry = self.entries.get_mut(self.cursor)?;
        entry.action.redo(world);
        self.cursor += 1;
        Some(entry.label.clone())
    }

    /// Drop all history. Called when the edited world is replaced wholesale —
    /// a new or loaded terrain, a level switch — since entries describe state
    /// that no longer exists.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.cursor = 0;
    }

    /// The label the next [`UndoRequest`] would revert (for UI).
    pub fn undo_label(&self) -> Option<&str> {
        self.entries[..self.cursor].last().map(|e| e.label.as_str())
    }

    /// The label the next [`RedoRequest`] would re-apply (for UI).
    pub fn redo_label(&self) -> Option<&str> {
        self.entries.get(self.cursor).map(|e| e.label.as_str())
    }
}

/// Apply this frame's [`UndoRequest`]s / [`RedoRequest`]s. Exclusive, between
/// `Tools` and `Apply`: any dirty regions an action marks flush (quantize,
/// [`TerrainRegionChanged`](crate::TerrainRegionChanged), re-bake debounce)
/// the same frame. An open terrain gesture (Ctrl+Z mid-stroke) seals first,
/// so it is what gets undone.
pub(crate) fn apply_undo_requests(world: &mut World) {
    let undos = world
        .resource_mut::<Messages<UndoRequest>>()
        .drain()
        .count();
    let redos = world
        .resource_mut::<Messages<RedoRequest>>()
        .drain()
        .count();
    if undos == 0 && redos == 0 {
        return;
    }
    world.resource_scope(|world, mut history: Mut<UndoHistory>| {
        world.resource_scope(|world, mut gesture: Mut<crate::gesture::TerrainGesture>| {
            gesture.seal(&mut history);
            let mut applied = Vec::new();
            for _ in 0..undos {
                match history.undo(world) {
                    Some(label) => applied.push(UndoApplied {
                        label,
                        redone: false,
                    }),
                    None => info!("nothing to undo"),
                }
            }
            for _ in 0..redos {
                match history.redo(world) {
                    Some(label) => applied.push(UndoApplied {
                        label,
                        redone: true,
                    }),
                    None => info!("nothing to redo"),
                }
            }
            world
                .resource_mut::<Messages<UndoApplied>>()
                .write_batch(applied);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Resource, Default, PartialEq, Debug)]
    struct Value(i32);

    /// A dummy action: undo/redo write a resource, `bytes` is whatever the
    /// test claims (to exercise eviction).
    struct SetValue {
        before: i32,
        after: i32,
        bytes: usize,
    }

    impl UndoAction for SetValue {
        fn undo(&mut self, world: &mut World) {
            world.resource_mut::<Value>().0 = self.before;
        }
        fn redo(&mut self, world: &mut World) {
            world.resource_mut::<Value>().0 = self.after;
        }
        fn bytes(&self) -> usize {
            self.bytes
        }
    }

    fn set(history: &mut UndoHistory, world: &mut World, label: &str, to: i32) {
        let before = world.resource::<Value>().0;
        world.resource_mut::<Value>().0 = to;
        history.push(
            label,
            SetValue {
                before,
                after: to,
                bytes: 0,
            },
        );
    }

    #[test]
    fn undo_restores_and_redo_reapplies() {
        let mut world = World::new();
        world.init_resource::<Value>();
        let mut history = UndoHistory::default();

        set(&mut history, &mut world, "one", 1);
        set(&mut history, &mut world, "two", 2);

        assert_eq!(history.undo(&mut world).as_deref(), Some("two"));
        assert_eq!(world.resource::<Value>().0, 1);
        assert_eq!(history.undo_label(), Some("one"));
        assert_eq!(history.redo_label(), Some("two"));

        assert_eq!(history.redo(&mut world).as_deref(), Some("two"));
        assert_eq!(world.resource::<Value>().0, 2);
        assert_eq!(history.redo(&mut world), None);
    }

    #[test]
    fn new_push_truncates_the_redo_tail() {
        let mut world = World::new();
        world.init_resource::<Value>();
        let mut history = UndoHistory::default();

        set(&mut history, &mut world, "first", 1);
        history.undo(&mut world);
        set(&mut history, &mut world, "second", 2);
        assert_eq!(history.redo_label(), None);
        assert_eq!(history.undo_label(), Some("second"));
    }

    #[test]
    fn eviction_drops_the_oldest_entries_but_keeps_one() {
        let mut world = World::new();
        world.init_resource::<Value>();
        let mut history = UndoHistory {
            max_bytes: 10,
            ..default()
        };
        for i in 0..3 {
            world.resource_mut::<Value>().0 = i;
            history.push(
                format!("entry {i}"),
                SetValue {
                    before: i - 1,
                    after: i,
                    bytes: 8,
                },
            );
        }
        // 24 bytes against a 10-byte budget: only the newest entry survives
        // (eviction never removes the last undoable entry).
        assert_eq!(history.undo_label(), Some("entry 2"));
        history.undo(&mut world);
        assert_eq!(history.undo(&mut world), None, "older entries evicted");
    }
}
