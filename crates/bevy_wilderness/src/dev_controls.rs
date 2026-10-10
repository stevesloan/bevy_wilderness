//! Demo A/B keybinds (`dev-controls` feature): B/N/V for the AO/bent-normal
//! experiment, H for hex-tiled detail. Off by default so the library ships no
//! input systems.

use bevy::{core_pipeline::tonemapping::Tonemapping, pbr::ExtendedMaterial, prelude::*};

use crate::material::GridMaterial;
use crate::{Clipmap, TerrainQuality};

/// Press V to cycle the terrain debug view: lit → macro AO → bent normal →
/// cavity → shape → lit. AO / bent normal / cavity render the raw baked
/// channel unlit so it reads as a literal value; shape is neutral grey lit by
/// the baked normals and shadows only, to judge terrain form (relief).
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
            next = (material.extension.dev.debug_view + 1) % 5;
            computed = true;
        }
        material.extension.dev.debug_view = next;
    }
    // The debug channels output raw 0..1 values; bypass the filmic tonemapper
    // while one is active so they read faithfully (AO ~0.9 shows near-white, not
    // gray-compressed). Restore the default tonemapper for the lit view.
    let tonemapping = if next == 0 || next == 4 {
        Tonemapping::default()
    } else {
        Tonemapping::None
    };
    for clipmap in &clipmaps {
        commands.entity(clipmap.target).insert(tonemapping);
    }
    let name = match next {
        1 => "macro AO",
        2 => "bent normal",
        3 => "cavity",
        4 => "shape (grey, baked normals + shadows)",
        _ => "off (lit terrain)",
    };
    info!("terrain debug view: {name}");
}

/// Press B to toggle the macro AO on/off in the lit render (flips `ao_strength`
/// 1 ↔ 0), so its contribution can be A/B'd on its own.
pub(crate) fn toggle_ao(
    keys: Res<ButtonInput<KeyCode>>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
) {
    if !keys.just_pressed(KeyCode::KeyB) {
        return;
    }
    let mut next = 1.0f32;
    let mut computed = false;
    for (_, material) in materials.iter_mut() {
        if !computed {
            next = if material.extension.dev.ao_strength > 0.5 {
                0.0
            } else {
                1.0
            };
            computed = true;
        }
        material.extension.dev.ao_strength = next;
    }
    info!(
        "terrain macro AO: {}",
        if next > 0.5 { "on" } else { "off" }
    );
}

/// Press N to toggle the bent-normal ambient direction on/off (flips
/// `bent_strength` 1 ↔ 0), independently of the macro AO.
pub(crate) fn toggle_bent(
    keys: Res<ButtonInput<KeyCode>>,
    mut materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, GridMaterial>>>,
) {
    if !keys.just_pressed(KeyCode::KeyN) {
        return;
    }
    let mut next = 1.0f32;
    let mut computed = false;
    for (_, material) in materials.iter_mut() {
        if !computed {
            next = if material.extension.dev.bent_strength > 0.5 {
                0.0
            } else {
                1.0
            };
            computed = true;
        }
        material.extension.dev.bent_strength = next;
    }
    info!(
        "terrain bent-normal ambient: {}",
        if next > 0.5 { "on" } else { "off" }
    );
}

/// Press H to toggle hex-tiled near detail
/// ([`TerrainQuality::detail_hex_tiling`]), to compare it with plain tiling.
pub(crate) fn toggle_detail_hex(
    keys: Res<ButtonInput<KeyCode>>,
    mut quality: ResMut<TerrainQuality>,
) {
    if !keys.just_pressed(KeyCode::KeyH) {
        return;
    }
    quality.detail_hex_tiling = !quality.detail_hex_tiling;
    info!(
        "terrain hex-tiled detail: {}",
        if quality.detail_hex_tiling {
            "on"
        } else {
            "off"
        }
    );
}
