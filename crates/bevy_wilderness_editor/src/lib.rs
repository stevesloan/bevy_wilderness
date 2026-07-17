//! Embeddable terrain editor core for `bevy_wilderness` — UI-agnostic (design
//! doc P2): all state and behavior is exposed as resources, events, and
//! components, never assuming any UI toolkit. A host app adds
//! [`TerrainEditorPlugin`], marks a `Clipmap` [`Editable`], and drives the
//! editor through this API — with its own UI, the default egui UI crate, or
//! none.
//!
//! Extension points for host tools (design doc §6):
//! - [`EditorTools`] / [`ActiveTool`] / [`tool_active`] — the tool registry.
//! - [`TerrainCursor`] — the shared cursor→terrain pick every tool uses.
//! - [`TerrainHeight`] — height at (x, z), for snapping props to the surface.
//! - [`TerrainRegionChanged`] — emitted when terrain changes; re-snap props.
//! - [`BrushSettings`] / [`ErosionSettings`] — the state a UI reads/writes.
//! - [`ErosionRequested`] / [`ErosionRun`] — start an erosion run / watch its
//!   progress (D3).
//! - [`SeamOverlay`] — toggle the looping tile-boundary visualization (D6).
//! - [`UndoHistory`] — tile-snapshot undo/redo; a UI binds Ctrl+Z to it (D8).

use bevy::prelude::*;

mod cursor;
mod erosion;
mod field;
mod mask;
mod rebake;
mod sculpt;
mod seam;
mod settings;
mod terrain;
mod tools;
mod undo;

pub use cursor::{TerrainCursor, TerrainHit};
pub use erosion::{ErosionRequested, ErosionRun};
pub use seam::SeamOverlay;
pub use field::TerrainField;
pub use settings::{BrushSettings, ErosionSettings, SculptMode};
pub use terrain::{Editable, EditableTerrain, TerrainHeight, TerrainRegionChanged};
pub use tools::{ActiveTool, EditorTools, ToolId, ToolInfo, tool_active};
pub use undo::{UNDO_TILE_SIZE, UndoBuffer, UndoHistory};

/// The editor's `Update` phases. Host tool systems go in
/// [`Tools`](EditorSet::Tools), between the shared pick and the flush:
/// [`TerrainCursor`] is current when they run, and any
/// [`EditableTerrain::mark_dirty`] they do is applied (quantized + event
/// emitted) the same frame.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditorSet {
    /// The shared cursor→terrain raycast ([`TerrainCursor`] updates).
    Pick,
    /// Tool behavior, built-in and host-registered alike.
    Tools,
    /// Dirty f32 regions quantize into the display heightmap;
    /// [`TerrainRegionChanged`] is emitted.
    Apply,
}

/// The terrain editor core. Add alongside `ClipmapPlugin`, then mark a
/// `Clipmap` entity [`Editable`].
pub struct TerrainEditorPlugin;

impl Plugin for TerrainEditorPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EditorTools>()
            .init_resource::<ActiveTool>()
            .init_resource::<TerrainCursor>()
            .init_resource::<BrushSettings>()
            .init_resource::<ErosionSettings>()
            .init_resource::<UndoHistory>()
            .init_resource::<SeamOverlay>()
            .add_message::<TerrainRegionChanged>()
            .add_message::<ErosionRequested>()
            .configure_sets(
                Update,
                (EditorSet::Pick, EditorSet::Tools, EditorSet::Apply).chain(),
            )
            .add_systems(
                Update,
                (
                    terrain::init_editable_terrains.before(EditorSet::Pick),
                    cursor::update_terrain_cursor.in_set(EditorSet::Pick),
                    sculpt::apply_sculpt
                        .run_if(tool_active(ToolId::SCULPT))
                        .in_set(EditorSet::Tools),
                    mask::apply_mask_paint
                        .run_if(tool_active(ToolId::MASK))
                        .in_set(EditorSet::Tools),
                    erosion::request_on_click
                        .run_if(tool_active(ToolId::ERODE))
                        .in_set(EditorSet::Tools),
                    // Between Tools and Apply: a click's request starts its
                    // task the same frame, and a landed result's dirty region
                    // flushes (quantize + event + re-bake debounce) the same
                    // frame it applies.
                    (erosion::start_requested_runs, erosion::apply_finished_runs)
                        .after(EditorSet::Tools)
                        .before(EditorSet::Apply),
                    (terrain::sync_dirty_regions, terrain::sync_dirty_masks)
                        .in_set(EditorSet::Apply),
                    seam::draw_seam_overlay,
                    // After the sync so a flush's re-armed timer isn't ticked
                    // in the same frame it was set.
                    rebake::tick_rebake_debounce.after(terrain::sync_dirty_regions),
                ),
            );

        // The built-in tools' registry entries.
        let mut tools = app.world_mut().resource_mut::<EditorTools>();
        tools.register(ToolId::SCULPT, "Sculpt");
        tools.register(ToolId::MASK, "Mask");
        tools.register(ToolId::ERODE, "Erode");
    }
}
