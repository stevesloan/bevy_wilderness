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

use bevy::prelude::*;

mod cursor;
mod field;
mod rebake;
mod sculpt;
mod settings;
mod terrain;
mod tools;

pub use cursor::{TerrainCursor, TerrainHit};
pub use field::TerrainField;
pub use settings::{BrushSettings, ErosionSettings, SculptMode};
pub use terrain::{Editable, EditableTerrain, TerrainHeight, TerrainRegionChanged};
pub use tools::{ActiveTool, EditorTools, ToolId, ToolInfo, tool_active};

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
            .add_message::<TerrainRegionChanged>()
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
                    terrain::sync_dirty_regions.in_set(EditorSet::Apply),
                    // After the sync so a flush's re-armed timer isn't ticked
                    // in the same frame it was set.
                    rebake::tick_rebake_debounce.after(terrain::sync_dirty_regions),
                ),
            );

        // The built-in tools' registry entries. Their systems arrive with
        // their phases (sculpt 2, mask 4, erode 5).
        let mut tools = app.world_mut().resource_mut::<EditorTools>();
        tools.register(ToolId::SCULPT, "Sculpt");
        tools.register(ToolId::MASK, "Mask");
        tools.register(ToolId::ERODE, "Erode");
    }
}
