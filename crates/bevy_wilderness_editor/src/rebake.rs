//! Debounced re-bake on release (D5). Geometry deforms live as the display
//! heightmap updates, but the baked shading (sun self-shadow, AO, slope-placed
//! material splat) only re-runs once edits settle: each dirty flush re-arms a
//! ~200 ms timer, and when it expires the terrain gets a `RebakeRequested` —
//! so a held stroke bakes once on release, not every frame. Matters most for
//! erosion: the splat places rock by slope, so the re-bake is what turns
//! freshly carved cliffs rocky.

use bevy::prelude::*;
use bevy_wilderness::{Clipmap, ClipmapReady, RebakeRequested};

/// Seconds of edit-silence before the re-bake fires (D5).
const REBAKE_DEBOUNCE_SECS: f32 = 0.2;

/// Whether edits re-bake automatically (D10). `auto` on (the default) is the
/// D5 behavior: every dirty flush re-arms the debounce. Off, edits never
/// schedule a bake — a modeling session batches freely and the host bakes
/// manually by inserting [`RebakeRequested`](bevy_wilderness::RebakeRequested)
/// (re-exported from this crate) on the terrain, e.g. the default UI's Bake
/// button.
#[derive(Resource)]
pub struct RebakeSettings {
    pub auto: bool,
}

impl Default for RebakeSettings {
    fn default() -> Self {
        Self { auto: true }
    }
}

/// Present on a terrain that has un-baked edits; re-inserted (timer reset) by
/// every dirty flush, so it only expires once the stroke settles.
#[derive(Component)]
pub(crate) struct RebakeDebounce(Timer);

impl Default for RebakeDebounce {
    fn default() -> Self {
        Self(Timer::from_seconds(REBAKE_DEBOUNCE_SECS, TimerMode::Once))
    }
}

/// Flipping auto off cancels any already-armed debounce — "off" means nothing
/// bakes unless the host asks, including the stroke finished just before the
/// toggle. Flipping auto *on* arms a bake for every terrain sitting in clay
/// (stale edits accumulated while off) — otherwise they'd stay clay until the
/// next edit, since only dirty flushes arm the debounce.
pub(crate) fn debounce_on_auto_toggle(
    settings: Res<RebakeSettings>,
    mut commands: Commands,
    debounces: Query<Entity, With<RebakeDebounce>>,
    stale: Query<(Entity, &Clipmap)>,
) {
    if !settings.is_changed() {
        return;
    }
    if settings.auto {
        for (entity, clipmap) in &stale {
            if clipmap.clay {
                commands.entity(entity).insert(RebakeDebounce::default());
            }
        }
    } else {
        for entity in &debounces {
            commands.entity(entity).remove::<RebakeDebounce>();
        }
    }
}

/// A landed bake means shading matches the field again: leave clay (D10). The
/// renderer re-inserts [`ClipmapReady`] exactly when a bake completes.
pub(crate) fn clear_clay_on_bake(mut baked: Query<&mut Clipmap, Added<ClipmapReady>>) {
    for mut clipmap in &mut baked {
        if clipmap.clay {
            clipmap.clay = false;
        }
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
