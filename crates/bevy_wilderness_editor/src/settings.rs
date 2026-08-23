//! Brush / erosion state resources (design doc §6): the data a UI reads and
//! writes. The sculpt (Phase 2) and mask (Phase 4) tools consume
//! [`BrushSettings`]; the erosion run (Phase 5) consumes [`ErosionSettings`].

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

/// Erosion parameters (D3): the GPU shallow-water pipe model — rain falls,
/// water flows, flowing water dissolves and carries ground, still water
/// settles it out — plus a continuous thermal (talus) pass. All in world
/// units (meters, seconds); exposed as a resource so a UI can offer sliders.
#[derive(Resource, Clone, Debug)]
pub struct ErosionSettings {
    /// Simulation iterations per run. More = deeper carving and longer
    /// sediment runs, linearly slower. One iteration advances `time_step`
    /// seconds of simulated weather.
    pub iterations: u32,
    /// Seconds per iteration. Larger steps simulate more per iteration but
    /// destabilize the shallow-water solver; stay near the default.
    pub time_step: f32,
    /// Rainfall in meters of water per second, over the mask (or everywhere
    /// when unmasked). The main strength knob.
    pub rain_rate: f32,
    /// Fraction of standing water lost per second; bounds how far water (and
    /// its sediment) travels before it dries up.
    pub evaporation: f32,
    /// Kc: sediment the flow can carry, scaled by tilt × flow speed. Higher
    /// = deeper channels and bigger fans.
    pub capacity: f32,
    /// Ks: fraction of the unused capacity dissolved from the ground per
    /// second where the flow is under-loaded.
    pub dissolve_rate: f32,
    /// Kd: fraction of the surplus load settled out per second where the
    /// flow is over-loaded.
    pub deposit_rate: f32,
    /// Floor (degrees) on the tilt in the capacity term: channels that have
    /// graded themselves flat keep incising instead of stalling (D12). Flat
    /// ground is still safe — no flow, no carving.
    pub min_tilt_deg: f32,
    /// Water deeper than this (meters) stops eroding — deep pools armor
    /// their beds instead of digging pits (D12). 0 disables.
    pub max_erosion_depth: f32,
    /// Cap (m/s) on the flow speed the capacity term sees. Real overland
    /// flow rarely beats a few m/s; without the cap, near-dry films report
    /// huge speeds and carve washboard stripes.
    pub max_flow_speed: f32,
    /// Slopes steeper than this (degrees) shed talus (D3).
    pub talus_angle_deg: f32,
    /// Fraction of the excess slope the thermal pass relaxes per second.
    /// 0 disables.
    pub thermal_rate: f32,
    /// Keep per-run wear/deposit/flow analysis maps as an
    /// [`ErosionMaps`](crate::ErosionMaps) component on the terrain (D12) —
    /// host API for e.g. splat or scatter rules; costs three full-field
    /// buffers while retained.
    pub keep_maps: bool,
}

impl Default for ErosionSettings {
    fn default() -> Self {
        Self {
            iterations: 700,
            time_step: 0.03,
            rain_rate: 0.012,
            evaporation: 0.05,
            capacity: 0.1,
            dissolve_rate: 0.3,
            deposit_rate: 0.3,
            min_tilt_deg: 3.0,
            max_erosion_depth: 0.8,
            max_flow_speed: 4.0,
            talus_angle_deg: 33.0,
            thermal_rate: 4.0,
            keep_maps: false,
        }
    }
}
