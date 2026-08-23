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
//! - [`PointerBlocked`] — a UI sets it while it owns the pointer; the pick
//!   goes empty so tools don't paint through panels.
//! - [`TerrainHeight`] — height at (x, z), for snapping props to the surface.
//! - [`TerrainRegionChanged`] — emitted when terrain changes; re-snap props.
//! - [`BrushSettings`] / [`ErosionSettings`] — the state a UI reads/writes.
//! - [`ErosionRequested`] / [`ErosionRun`] — start an erosion run / watch its
//!   progress (D3).
//! - [`ErosionMaps`] — per-run wear/deposit/flow analysis maps (D12), kept
//!   on the terrain when [`ErosionSettings::keep_maps`] is set — e.g. for
//!   host splat or scatter rules.
//! - [`SeamOverlay`] — toggle the looping tile-boundary visualization (D6).
//! - [`ActiveStamp`] / [`StampSettings`] — the stamp tool's heightfield PNG
//!   and transform (D11); the GPU preview floats it under the cursor, a
//!   click commits it.
//! - [`ExportRequested`] / [`HeightmapExported`] — write the heightmap to an
//!   R16 KTX2 (round-trips through the asset loader) and/or a 16-bit PNG
//!   interchange copy, by extension (D7).
//! - [`NewTerrainRequested`] / [`LoadRequested`] / [`TerrainLoaded`] — reset a
//!   terrain to a fresh flat plain (the default starting state) or load a
//!   heightmap file into it at runtime.
//! - [`WorldImportRequested`] / [`WorldImportRun`] / [`WorldImported`] — fill
//!   the map with real-world elevation centered on a lat/lon
//!   ([`WorldImportSettings`]). [`WorldImportSource`] picks between the
//!   worldwide terrain tiles (~10 m) and USGS 3DEP 1 m lidar, which resolves
//!   an FPS-scale terrain properly but only covers the United States.
//! - [`RebakeSettings`] — auto re-bake on/off (D10); with it off, bake
//!   manually by inserting [`RebakeRequested`] on the terrain
//!   ([`ClipmapReady`]'s absence = a bake is in flight).
//! - [`UndoHistory`] — the shared undo/redo stack (D8): terrain gestures and
//!   host actions ([`UndoAction`], e.g. prop placement) interleave in one
//!   stream. A UI binds Ctrl+Z to [`UndoRequest`] / [`RedoRequest`] and shows
//!   [`UndoApplied`] labels.

use bevy::prelude::*;

mod cog;
mod cursor;
mod erosion;
mod export;
mod field;
mod gesture;
mod mask;
mod periodic;
mod rebake;
mod sculpt;
mod seam;
mod settings;
mod stamp;
mod swap;
mod terrain;
mod tools;
mod undo;
mod usgs;
mod utm;
mod world;

pub use cursor::{BrushRing, PointerBlocked, TerrainCursor, TerrainHit};
pub use erosion::{ErosionMaps, ErosionRequested, ErosionRun};
pub use export::{ExportRequested, HeightmapExported};
pub use field::TerrainField;
pub use rebake::RebakeSettings;
pub use seam::SeamOverlay;
pub use settings::{BrushSettings, ErosionSettings, SculptMode};
pub use stamp::{ActiveStamp, BakeParams, StampData, StampSettings};
pub use swap::{LoadRequested, NewTerrainRequested, TerrainLoaded};
pub use terrain::{Editable, EditableTerrain, TerrainHeight, TerrainRegionChanged};
pub use gesture::{TerrainGesture, UNDO_TILE_SIZE, UndoBuffer};
pub use tools::{ActiveTool, EditorTools, ToolId, ToolInfo, tool_active};
pub use undo::{RedoRequest, UndoAction, UndoApplied, UndoHistory, UndoRequest};
pub use world::{
    WorldImportRequested, WorldImportRun, WorldImportSettings, WorldImportSource, WorldImported,
};
// The manual-bake trigger, bake-completion marker (design doc §5/D10), and the
// quality profile a UI's quality section edits, re-exported so a UI crate can
// drive bakes without depending on the renderer.
pub use bevy_wilderness::{ClipmapReady, FogTier, RebakeRequested, TerrainQuality};

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
            .init_resource::<PointerBlocked>()
            .init_resource::<BrushSettings>()
            .init_resource::<ErosionSettings>()
            .init_resource::<WorldImportSettings>()
            .init_resource::<UndoHistory>()
            .init_resource::<TerrainGesture>()
            .init_resource::<RebakeSettings>()
            .init_resource::<SeamOverlay>()
            .init_resource::<cursor::BrushRing>()
            .init_resource::<ActiveStamp>()
            .init_resource::<StampSettings>()
            .init_resource::<export::ExportTasks>()
            .add_message::<TerrainRegionChanged>()
            .add_message::<ErosionRequested>()
            .add_message::<ExportRequested>()
            .add_message::<HeightmapExported>()
            .add_message::<NewTerrainRequested>()
            .add_message::<LoadRequested>()
            .add_message::<TerrainLoaded>()
            .add_message::<WorldImportRequested>()
            .add_message::<WorldImported>()
            .add_message::<UndoRequest>()
            .add_message::<RedoRequest>()
            .add_message::<UndoApplied>()
            .configure_sets(
                Update,
                (EditorSet::Pick, EditorSet::Tools, EditorSet::Apply).chain(),
            )
            .add_systems(
                Update,
                (
                    // New/load swaps run first so the fresh EditableTerrain and
                    // reset overlay are in place before init and the pick; the
                    // chained sync point makes their inserts visible downstream.
                    (
                        swap::apply_new_terrain,
                        swap::apply_load,
                        world::apply_finished_imports,
                        terrain::init_editable_terrains,
                        terrain::init_edit_overlays,
                    )
                        .chain()
                        .before(EditorSet::Pick),
                    // Import requests just spawn download tasks; landing
                    // happens in the swap slot above on a later frame.
                    world::start_requested_imports
                        .after(EditorSet::Tools)
                        .before(EditorSet::Apply),
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
                    // Rebake first so a changed bake setting previews (and
                    // could commit) the same frame.
                    (stamp::rebake_on_settings_change, stamp::drive_stamp_tool)
                        .chain()
                        .run_if(tool_active(ToolId::STAMP))
                        .in_set(EditorSet::Tools),
                    // Drop a floating preview the frame the tool deactivates.
                    stamp::clear_stamp_preview
                        .run_if(not(tool_active(ToolId::STAMP)))
                        .in_set(EditorSet::Tools),
                    // Between Tools and Apply: a click's request starts its
                    // GPU run the same frame, each frame advances a bounded
                    // chunk of iterations, and a landed result's dirty region
                    // flushes (quantize + event + re-bake debounce) the same
                    // frame it applies.
                    (
                        erosion::start_requested_runs,
                        erosion::drive_runs,
                        erosion::apply_finished_runs,
                    )
                        .chain()
                        .after(EditorSet::Tools)
                        .before(EditorSet::Apply),
                    // Same slot: an undone action's dirty regions flush
                    // (quantize + event + re-bake debounce) the same frame.
                    undo::apply_undo_requests
                        .after(EditorSet::Tools)
                        .before(EditorSet::Apply),
                    (terrain::sync_dirty_regions, terrain::sync_dirty_masks)
                        .in_set(EditorSet::Apply),
                    seam::draw_seam_overlay,
                    cursor::draw_brush_ring,
                    // Export is read-only on the field; the message-in /
                    // message-out pair can run any time after Tools.
                    (export::start_requested_exports, export::poll_export_tasks)
                        .after(EditorSet::Tools),
                    // After the sync so a flush's re-armed timer isn't ticked
                    // in the same frame it was set. The auto-toggle handler
                    // runs between them: it must see this frame's toggle
                    // before any armed timer gets a chance to expire.
                    rebake::debounce_on_auto_toggle.after(terrain::sync_dirty_regions),
                    rebake::tick_rebake_debounce.after(rebake::debounce_on_auto_toggle),
                    // Leave clay the frame the bake lands (Added<ClipmapReady>).
                    rebake::clear_clay_on_bake,
                ),
            );

        // The built-in tools' registry entries.
        let mut tools = app.world_mut().resource_mut::<EditorTools>();
        tools.register(ToolId::SCULPT, "Sculpt");
        tools.register(ToolId::MASK, "Mask");
        tools.register(ToolId::ERODE, "Erode");
        tools.register(ToolId::STAMP, "Stamp");
    }
}
