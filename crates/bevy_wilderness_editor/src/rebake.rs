//! Debounced re-bake on release (D5). Geometry deforms live as the display
//! heightmap updates, but the baked shading (sun self-shadow, AO, slope-placed
//! material splat) only re-runs once edits settle: each dirty flush re-arms a
//! ~200 ms timer, and when it expires the terrain gets a `RebakeRequested` —
//! so a held stroke bakes once on release, not every frame. Matters most for
//! erosion: the splat places rock by slope, so the re-bake is what turns
//! freshly carved cliffs rocky.

use bevy::prelude::*;
use bevy_wilderness::RebakeRequested;

/// Seconds of edit-silence before the re-bake fires (D5).
const REBAKE_DEBOUNCE_SECS: f32 = 0.2;

/// Present on a terrain that has un-baked edits; re-inserted (timer reset) by
/// every dirty flush, so it only expires once the stroke settles.
#[derive(Component)]
pub(crate) struct RebakeDebounce(Timer);

impl Default for RebakeDebounce {
    fn default() -> Self {
        Self(Timer::from_seconds(REBAKE_DEBOUNCE_SECS, TimerMode::Once))
    }
}

pub(crate) fn tick_rebake_debounce(
    time: Res<Time>,
    mut commands: Commands,
    mut debounces: Query<(Entity, &mut RebakeDebounce)>,
) {
    for (entity, mut debounce) in &mut debounces {
        if debounce.0.tick(time.delta()).just_finished() {
            commands
                .entity(entity)
                .remove::<RebakeDebounce>()
                .insert(RebakeRequested);
        }
    }
}
