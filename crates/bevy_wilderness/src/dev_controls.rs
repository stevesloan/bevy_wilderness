//! Demo keybind (V) cycling the terrain's debug views (`dev-controls`
//! feature). Off by default so the library ships no input systems.

use bevy::{core_pipeline::tonemapping::Tonemapping, pbr::ExtendedMaterial, prelude::*};

use crate::Clipmap;
use crate::material::GridMaterial;

/// Press V to cycle the terrain debug view: lit → cavity → sun visibility →
/// lit. Each renders its raw 0..1 value unlit, so it reads as a literal value.
pub(crate) fn debug_cycle_view(
    keys: Res<ButtonInput<KeyCode>>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
    clipmaps: Query<&Clipmap>,
    mut commands: Commands,
) {
    if !keys.just_pressed(KeyCode::KeyV) {
        return;
    }
    let mut next = 0u32;
    let mut computed = false;
    for (_, material) in materials.iter_mut() {
        if !computed {
            next = (material.extension.dev.debug_view + 1) % 3;
            computed = true;
        }
        material.extension.dev.debug_view = next;
    }
    // The debug channels output raw 0..1 values; bypass the filmic tonemapper
    // while one is active so they read faithfully (0.9 shows near-white, not
    // gray-compressed). Restore the default tonemapper for the lit view.
    let tonemapping = if next == 0 {
        Tonemapping::default()
    } else {
        Tonemapping::None
    };
    for clipmap in &clipmaps {
        commands.entity(clipmap.target).insert(tonemapping);
    }
    let name = match next {
        1 => "cavity",
        2 => "sun visibility",
        _ => "off (lit terrain)",
    };
    info!("terrain debug view: {name}");
}
