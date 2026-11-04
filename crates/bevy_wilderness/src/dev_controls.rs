//! Demo keybind (V) cycling the terrain's debug views (`dev-controls`
//! feature). Off by default so the library ships no input systems.

use bevy::{core_pipeline::tonemapping::Tonemapping, prelude::*};

use crate::{Clipmap, TerrainDebugView};

/// Press V to cycle the terrain debug view: lit → cavity → sun visibility →
/// flat → lit. All but lit render unlit, so values read literally.
pub(crate) fn debug_cycle_view(
    keys: Res<ButtonInput<KeyCode>>,
    mut view: ResMut<TerrainDebugView>,
    clipmaps: Query<&Clipmap>,
    mut commands: Commands,
) {
    if !keys.just_pressed(KeyCode::KeyV) {
        return;
    }
    *view = match *view {
        TerrainDebugView::Lit => TerrainDebugView::Cavity,
        TerrainDebugView::Cavity => TerrainDebugView::SunVisibility,
        TerrainDebugView::SunVisibility => TerrainDebugView::Flat,
        _ => TerrainDebugView::Lit,
    };
    // The debug channels output raw 0..1 values; bypass the filmic tonemapper
    // while one is active so they read faithfully (0.9 shows near-white, not
    // gray-compressed). Restore the default tonemapper for the lit view.
    let tonemapping = if *view == TerrainDebugView::Lit {
        Tonemapping::default()
    } else {
        Tonemapping::None
    };
    for clipmap in &clipmaps {
        commands.entity(clipmap.target).insert(tonemapping);
    }
    info!("terrain debug view: {:?}", *view);
}
