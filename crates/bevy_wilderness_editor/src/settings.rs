//! Brush state resources (design doc §6): the data a UI reads and writes.
//! The sculpt (Phase 2) and mask (Phase 4) tools consume [`BrushSettings`].

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
