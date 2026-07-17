//! Brush / erosion state resources (design doc §6): the data a UI reads and
//! writes. Data-only in Phase 1 — the sculpt tool consumes [`BrushSettings`]
//! in Phase 2, erosion consumes [`ErosionSettings`] in Phase 5, and the mask
//! state arrives with the mask tool in Phase 4.

use bevy::prelude::*;

/// What a sculpt stroke does to the height field (D2).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SculptMode {
    /// Push terrain up under the brush.
    #[default]
    Raise,
    /// Carve terrain down under the brush.
    Lower,
    /// Relax heights toward the local average.
    Smooth,
    /// Pull heights toward the height at the stroke's start.
    Flatten,
}

/// The shared brush: mode, size, and strength, with radial falloff. Sculpt
/// (Phase 2) and mask paint (Phase 4) both consume this; a UI reads/writes it.
#[derive(Resource, Clone, Debug)]
pub struct BrushSettings {
    pub mode: SculptMode,
    /// Brush radius in meters.
    pub radius: f32,
    /// Full-strength rate at the brush center, in meters of height per second
    /// of stroke (falls off radially to zero at `radius`).
    pub strength: f32,
}

impl Default for BrushSettings {
    fn default() -> Self {
        Self {
            mode: SculptMode::default(),
            radius: 60.0,
            strength: 40.0,
        }
    }
}

/// Droplet-erosion parameters (D3): ~1–3 M droplets simulated on the CPU in the
/// background, each carrying sediment downhill for up to `max_lifetime` steps.
/// Defaults follow the standard droplet model; exposed as a resource so a UI
/// can offer a few sliders. Consumed by the erosion tool in Phase 5.
#[derive(Resource, Clone, Debug)]
pub struct ErosionSettings {
    /// Droplets per run. More = stronger, more detailed erosion, linearly
    /// slower. ~1–3 M suits a 4K map; scale down with the masked area.
    pub droplets: u32,
    /// Max steps a droplet lives (each step moves one texel).
    pub max_lifetime: u32,
    /// 0 = flow follows the gradient exactly (twitchy); 1 = never turns.
    pub inertia: f32,
    /// Sediment a droplet can carry, scaled by speed × slope × water.
    pub sediment_capacity: f32,
    /// Capacity floor so droplets keep carving on near-flat ground.
    pub min_sediment_capacity: f32,
    /// Fraction of surplus sediment dropped per step when over capacity.
    pub deposit_rate: f32,
    /// Fraction of remaining capacity eroded per step when under capacity.
    pub erode_rate: f32,
    /// Fraction of water lost per step; ends the droplet's carving reach.
    pub evaporate_rate: f32,
    /// Downhill acceleration per step.
    pub gravity: f32,
    /// Radius (in texels) erosion is spread over, softening single-texel pits.
    pub erosion_radius: u32,
    /// Thermal pass: slopes steeper than this (degrees) shed talus (D3).
    pub talus_angle_deg: f32,
    /// Fraction of the excess slope relaxed per thermal iteration.
    pub thermal_rate: f32,
}

impl Default for ErosionSettings {
    fn default() -> Self {
        Self {
            droplets: 1_500_000,
            max_lifetime: 64,
            inertia: 0.05,
            sediment_capacity: 4.0,
            min_sediment_capacity: 0.01,
            deposit_rate: 0.3,
            erode_rate: 0.3,
            evaporate_rate: 0.01,
            gravity: 4.0,
            erosion_radius: 3,
            talus_angle_deg: 33.0,
            thermal_rate: 0.5,
        }
    }
}
