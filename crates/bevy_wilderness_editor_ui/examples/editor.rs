//! The editor host app (design doc §7): adds the renderer + editor + default
//! UI plugins, marks the terrain [`Editable`], and registers a third-party
//! prop-placement tool to prove the extension API — the same embedding path a
//! game uses (its glTF-placement tool is this tool with a manifest).
//!
//! ```sh
//! cargo run -p bevy_wilderness_editor_ui --example editor
//! ```
//!
//! Everything is driven from the side panel; keyboard shortcuts mirror it
//! (WASD + right-drag to fly):
//! - **Left mouse (held)** — apply the active tool under the brush ring
//! - **S / M / E / P / T** — sculpt / mask paint / erode / place-prop /
//!   stamp tool
//! - **1 / 2 / 3 / 4** — sculpt mode: Raise / Lower / Smooth / Flatten
//! - **Shift+LMB** (mask tool) — erase mask; **C** — clear the whole mask
//! - **[ / ]** — brush radius down / up
//! - **- / =** — brush strength down / up
//! - **Ctrl+Z / Ctrl+Shift+Z** — undo / redo
//! - **B** — toggle the tile-boundary ring (the terrain loops; edits and
//!   erosion wrap across it)
//!
//! The stamp tool (T, D11) floats a heightfield PNG under the cursor as a
//! live GPU preview; **click commits it**. While it's active the wheel
//! belongs to the stamp — **wheel** = strength (negative carves),
//! **Ctrl+wheel** = size, **Shift+wheel** = rotate — and the fly camera's
//! scroll-speed binding is paused. Pick stamps in the panel's gallery; the
//! example generates a few starter stamps (hill / ridge / ring) into its
//! stamps folder on first run — drop your own 8/16-bit grayscale PNGs
//! there and hit "rescan".
//!
//! A painted mask (orange tint) confines sculpting *and* erosion to it,
//! feathered at the edge. Erosion runs in the background over the mask (the
//! whole map if none); when it lands, the re-bake turns fresh cliffs rocky.
//! The place-prop tool (P) drops cubes that snap to the surface and *re-snap*
//! whenever the ground under them changes — sculpt or erode under one and
//! watch it follow.
//!
//! The editor opens on a **new terrain** by default: a flat plain at the D1
//! 4096² working resolution over an 8192 m world footprint. Sculpt it, erode
//! it, export it — the exports are the new terrain's master files. The panel's
//! **New** button starts over (at a chosen resolution) and **Load terrain…**
//! opens an existing heightmap through a file dialog, both without a restart.
//!
//! The panel's Export button saves the heightmap into the renderer crate's
//! assets dir in both formats: `heightmap_export.ktx2` (the engine master)
//! and `heightmap_export.png` (16-bit interchange, e.g. for a physics
//! pipeline's collision heightfield). To prove the round-trip, restart
//! loading either export (the renderer retags the PNG's `R16Uint` in place):
//!
//! ```sh
//! WILDERNESS_HEIGHTMAP=heightmap_export.ktx2 \
//!     cargo run -p bevy_wilderness_editor_ui --example editor
//! ```
//!
//! `WILDERNESS_NEW=<texels>` changes the default terrain's starting resolution
//! (the world footprint is unchanged — resolution is texel density, not
//! scale):
//!
//! ```sh
//! WILDERNESS_NEW=2048 cargo run -p bevy_wilderness_editor_ui --example editor
//! ```

use bevy::{
    camera::{Exposure, Hdr},
    camera_controller::free_camera::{FreeCamera, FreeCameraPlugin},
    image::ImageLoaderSettings,
    light::{
        Atmosphere, AtmosphereEnvironmentMapLight, SunDisk, atmosphere::ScatteringMedium,
        light_consts::lux,
    },
    pbr::AtmosphereSettings,
    post_process::bloom::{Bloom, BloomCompositeMode, BloomPrefilter},
    prelude::*,
};

use bevy_wilderness::{
    Clipmap, ClipmapPlugin, DetailConfig, FogTier, HeightFog, HeightFogPlugin, HeightRule,
    SlopeRule, TerrainFog, TerrainLayer, TerrainQuality, load_terrain_array,
};
use bevy_wilderness_editor::{
    ActiveTool, BrushSettings, Editable, EditableTerrain, EditorSet, EditorTools, ErosionRun,
    RedoRequest, SculptMode, SeamOverlay, TerrainCursor, TerrainEditorPlugin, TerrainField,
    TerrainGesture, TerrainHeight, TerrainRegionChanged, ToolId, UndoApplied, UndoBuffer,
    UndoHistory, UndoRequest, tool_active,
};
use bevy_wilderness_editor_ui::{TerrainEditorUiPlugin, UiExportPath, UiStampFolder};

/// The demo third-party tool (design doc §6): places props that snap to the
/// terrain and re-snap when it changes — proving a host-registered tool gets
/// the shared pick, the height query, and the edit events without the editor
/// knowing anything about it. The game's glTF-placement tool is this shape.
const PLACE: ToolId = ToolId("example.place_prop");

/// Half-height of the demo prop cube (its base sits on the surface).
const PROP_HALF: f32 = 6.0;

/// A placed demo prop; re-snapped by [`resnap_props`].
#[derive(Component)]
struct DemoProp;

/// Generate a few 16-bit starter stamps (hill / ridge / ring) into `dir` if
/// it holds no PNGs yet — the gallery shouldn't be empty on first run. Real
/// stamps are authored heightfields; these are just clean shapes to try the
/// tool with.
fn ensure_starter_stamps(dir: &std::path::Path) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let has_png = std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.filter_map(|e| e.ok()).any(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("png"))
        })
    });
    if has_png {
        return;
    }
    const N: u32 = 256;
    let smooth = |t: f32| {
        let t = t.clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    let write = |name: &str, shape: &dyn Fn(Vec2) -> f32| {
        let buffer = image::ImageBuffer::from_fn(N, N, |x, y| {
            // Texel center → -1..1 over the stamp.
            let uv = (Vec2::new(x as f32, y as f32) + 0.5) / N as f32 * 2.0 - 1.0;
            image::Luma([(shape(uv).clamp(0.0, 1.0) * 65535.0) as u16])
        });
        if let Err(error) = buffer.save(dir.join(name)) {
            warn!("couldn't write starter stamp {name}: {error}");
        }
    };
    // A round dome, an elongated ridge, and a rim ring (positive strength
    // raises a ring range; negative carves a circular trench).
    write("hill.png", &|uv| smooth(1.0 - uv.length()));
    write("ridge.png", &|uv| {
        smooth(1.0 - Vec2::new(uv.x, uv.y * 3.5).length())
    });
    write("ring.png", &|uv| {
        smooth(1.0 - (uv.length() - 0.6).abs() * 4.0)
    });
}

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(AssetPlugin {
            // The example borrows the renderer crate's assets.
            file_path: "../bevy_wilderness/assets".into(),
            ..default()
        }))
        .add_plugins(FreeCameraPlugin)
        .add_plugins(ClipmapPlugin)
        .add_plugins(HeightFogPlugin)
        .add_plugins(TerrainEditorPlugin)
        .add_plugins(TerrainEditorUiPlugin::default())
        .add_systems(Startup, (setup, register_place_tool))
        .add_systems(
            Update,
            (
                update_sun_color,
                brush_controls,
                stamp_wheel_guard,
                undo_keys,
                clear_mask_key,
                erosion_progress,
                resnap_props,
            ),
        )
        .add_systems(
            Update,
            place_prop_tool
                .run_if(tool_active(PLACE))
                .in_set(EditorSet::Tools),
        )
        .run();
}

/// Register the demo tool — exactly what a game does for its own tools (e.g.
/// glTF placement). Sculpt starts active; P (or the panel) switches to it.
fn register_place_tool(mut tools: ResMut<EditorTools>, mut active: ResMut<ActiveTool>) {
    tools.register(PLACE, "Place Prop");
    active.0 = Some(ToolId::SCULPT);
}

/// Drop a cube where the shared pick hits — the same cursor every brush uses,
/// so the prop lands exactly where the ring shows.
fn place_prop_tool(
    mut commands: Commands,
    buttons: Res<ButtonInput<MouseButton>>,
    cursor: Res<TerrainCursor>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    if !buttons.just_pressed(MouseButton::Left) {
        return;
    }
    let Some(hit) = cursor.0 else {
        return;
    };
    commands.spawn((
        DemoProp,
        Mesh3d(meshes.add(Cuboid::from_length(PROP_HALF * 2.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.9, 0.25, 0.2),
            perceptual_roughness: 0.6,
            ..default()
        })),
        Transform::from_translation(hit.position + Vec3::Y * PROP_HALF),
    ));
    info!("placed prop at {:?}", hit.position);
}

/// The extension-API hook (design doc §6): when terrain changes under a placed
/// prop — sculpt, erosion, undo, anything — re-snap it to the new surface via
/// the core's height query.
fn resnap_props(
    mut changed: MessageReader<TerrainRegionChanged>,
    height: TerrainHeight,
    mut props: Query<&mut Transform, With<DemoProp>>,
) {
    for event in changed.read() {
        for mut transform in &mut props {
            let xz = transform.translation.xz();
            if event.region.contains(xz)
                && let Some(h) = height.sample(xz)
            {
                transform.translation.y = h + PROP_HALF;
            }
        }
    }
}

/// A UI stand-in: keybinds writing the editor's state resources — the same
/// `BrushSettings` / `ActiveTool` writes an egui panel will make in Phase 7.
fn brush_controls(
    keys: Res<ButtonInput<KeyCode>>,
    mut brush: ResMut<BrushSettings>,
    mut active: ResMut<ActiveTool>,
    mut seam: ResMut<SeamOverlay>,
) {
    if keys.just_pressed(KeyCode::KeyB) {
        seam.enabled = !seam.enabled;
        info!(
            "tile boundary ring: {}",
            if seam.enabled { "on" } else { "off" }
        );
    }
    let mode = [
        (KeyCode::Digit1, SculptMode::Raise),
        (KeyCode::Digit2, SculptMode::Lower),
        (KeyCode::Digit3, SculptMode::Smooth),
        (KeyCode::Digit4, SculptMode::Flatten),
    ]
    .into_iter()
    .find(|(key, _)| keys.just_pressed(*key));
    if let Some((_, mode)) = mode {
        brush.mode = mode;
        info!("brush mode: {mode:?}");
    }
    if keys.just_pressed(KeyCode::BracketLeft) {
        brush.radius = (brush.radius / 1.3).max(4.0);
        info!("brush radius: {:.0} m", brush.radius);
    }
    if keys.just_pressed(KeyCode::BracketRight) {
        brush.radius = (brush.radius * 1.3).min(2000.0);
        info!("brush radius: {:.0} m", brush.radius);
    }
    if keys.just_pressed(KeyCode::Minus) {
        brush.strength = (brush.strength / 1.5).max(1.0);
        info!("brush strength: {:.0} m/s", brush.strength);
    }
    if keys.just_pressed(KeyCode::Equal) {
        brush.strength = (brush.strength * 1.5).min(500.0);
        info!("brush strength: {:.0} m/s", brush.strength);
    }
    if keys.just_pressed(KeyCode::KeyS) {
        active.0 = Some(ToolId::SCULPT);
        info!("tool: sculpt");
    }
    if keys.just_pressed(KeyCode::KeyM) {
        active.0 = Some(ToolId::MASK);
        info!("tool: mask paint (Shift+LMB erases, C clears)");
    }
    if keys.just_pressed(KeyCode::KeyE) {
        active.0 = Some(ToolId::ERODE);
        info!("tool: erode (click to run over the mask, or the whole map if none)");
    }
    if keys.just_pressed(KeyCode::KeyP) {
        active.0 = Some(PLACE);
        info!("tool: place prop (click to drop a cube that re-snaps to the terrain)");
    }
    if keys.just_pressed(KeyCode::KeyT) {
        active.0 = Some(ToolId::STAMP);
        info!(
            "tool: stamp (pick a stamp in the panel; wheel = strength, ctrl = size, shift = rotate)"
        );
    }
}

/// While the stamp tool is active the wheel belongs to the stamp (strength /
/// size / rotation), so pause the fly camera's scroll-speed binding and
/// restore it on tool switch.
fn stamp_wheel_guard(
    active: Res<ActiveTool>,
    mut cameras: Query<&mut FreeCamera>,
    mut saved: Local<Option<f32>>,
) {
    let stamping = active.0 == Some(ToolId::STAMP);
    for mut camera in &mut cameras {
        if stamping && saved.is_none() {
            *saved = Some(camera.scroll_factor);
            camera.scroll_factor = 0.0;
        } else if !stamping && let Some(factor) = saved.take() {
            camera.scroll_factor = factor;
        }
    }
}

/// C clears the whole mask — as an undoable gesture, like any other edit.
fn clear_mask_key(
    keys: Res<ButtonInput<KeyCode>>,
    mut history: ResMut<UndoHistory>,
    mut gesture: ResMut<TerrainGesture>,
    mut terrains: Query<(Entity, &mut EditableTerrain)>,
) {
    if !keys.just_pressed(KeyCode::KeyC) {
        return;
    }
    for (entity, mut terrain) in &mut terrains {
        if terrain.mask_active() {
            gesture.begin(&mut history, entity, "Clear Mask");
            gesture.capture(&terrain, UndoBuffer::Mask, terrain.field.full_rect());
            terrain.clear_mask();
            gesture.seal(&mut history);
            info!("mask cleared");
        }
    }
}

/// Ctrl+Z / Ctrl+Shift+Z → undo/redo requests — the same messages a UI's
/// buttons write; the editor core applies them. `UndoApplied` carries the
/// entry labels back for the log.
fn undo_keys(
    keys: Res<ButtonInput<KeyCode>>,
    mut undo: MessageWriter<UndoRequest>,
    mut redo: MessageWriter<RedoRequest>,
    mut applied: MessageReader<UndoApplied>,
) {
    for done in applied.read() {
        let verb = if done.redone { "redo" } else { "undo" };
        info!("{verb}: {}", done.label);
    }
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    if !ctrl || !keys.just_pressed(KeyCode::KeyZ) {
        return;
    }
    if shift {
        redo.write(RedoRequest);
    } else {
        undo.write(UndoRequest);
    }
}

/// Progress-bar stand-in: logs a running erosion every ~20%, and once when the
/// result lands — the same `ErosionRun::progress` a UI's bar will read.
fn erosion_progress(runs: Query<&ErosionRun>, mut last: Local<Option<u32>>) {
    match runs.iter().next() {
        Some(run) => {
            let pct = (run.progress() * 100.0) as u32 / 20 * 20;
            if *last != Some(pct) {
                *last = Some(pct);
                info!("erosion: {pct}% of droplets simulated");
            }
        }
        None => {
            if last.take().is_some() {
                info!("erosion: result applied");
            }
        }
    }
}

/// Physically-derived sun color from its elevation (see the basic example).
fn update_sun_color(mut suns: Query<(&mut DirectionalLight, &GlobalTransform)>) {
    for (mut light, transform) in &mut suns {
        let to_sun = transform.back().as_vec3();
        let cos_zenith = to_sun.y.clamp(0.0, 1.0);
        let zenith_deg = cos_zenith.acos().to_degrees();
        let air_mass = 1.0 / (cos_zenith + 0.15 * (93.885 - zenith_deg).max(0.05).powf(-1.253));
        let tau = Vec3::new(0.05, 0.10, 0.23) * air_mass;
        let t = Vec3::new((-tau.x).exp(), (-tau.y).exp(), (-tau.z).exp());
        let t = t / t.max_element();
        light.color = Color::linear_rgb(t.x, t.y, t.z);
    }
}

fn setup(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut images: ResMut<Assets<Image>>,
    mut scattering_mediums: ResMut<Assets<ScatteringMedium>>,
) {
    // Export into the assets dir (cwd-relative — run from the workspace root)
    // so the exported file is immediately loadable via WILDERNESS_HEIGHTMAP.
    commands.insert_resource(UiExportPath(
        "crates/bevy_wilderness/assets/heightmap_export.ktx2".into(),
    ));
    // Stamp gallery folder (D11), seeded with generated starter stamps so
    // it isn't empty on first run. Drop your own grayscale PNGs in and hit
    // "rescan" in the panel.
    let stamps = std::path::PathBuf::from("crates/bevy_wilderness_editor_ui/assets/stamps");
    ensure_starter_stamps(&stamps);
    commands.insert_resource(UiStampFolder(stamps));
    commands.insert_resource(TerrainFog(HeightFog {
        density: 0.002e-4,
        falloff: 0.0128,
        base_height: 0.0,
        max_distance: 16384.0,
        ..default()
    }));
    // Quality profile tuned for integrated GPUs (a real app sets this from
    // device detection). The desktop-grade profile (FogTier::High, 8192,
    // ambient_gather, 2 detail layers) samples three 8192² RVT targets
    // (~800 MB) per fragment plus a fullscreen fog pass — bandwidth an iGPU
    // doesn't have. 4096² quarters the RVT memory, inline fog drops the
    // fullscreen pass, and top-1 detail saves ~3 texture samples per fragment.
    commands.insert_resource(TerrainQuality {
        fog: FogTier::Low,
        rvt_size: 4096,
        ambient_gather: true,
        detail_layers: 1,
    });

    let atmosphere = Atmosphere::earth(scattering_mediums.add(ScatteringMedium::earth(256, 256)));
    let planet_center = -Vec3::Y * (atmosphere.inner_radius + 3_000.0);
    commands.spawn((atmosphere, Transform::from_translation(planet_center)));

    let target = commands
        .spawn((
            Camera3d::default(),
            Hdr,
            Projection::from(PerspectiveProjection {
                fov: 90.0_f32.to_radians(),
                far: 16384.0,
                ..Default::default()
            }),
            Bloom {
                intensity: 0.3,
                prefilter: BloomPrefilter {
                    threshold: 1.0,
                    threshold_softness: 0.5,
                },
                composite_mode: BloomCompositeMode::Additive,
                ..Bloom::NATURAL
            },
            AtmosphereSettings {
                aerial_view_lut_max_distance: 16384.0,
                ..Default::default()
            },
            AtmosphereEnvironmentMapLight::default(),
            AmbientLight {
                brightness: 0.0,
                ..default()
            },
            Exposure::SUNLIGHT,
            HeightFog::default(),
            Msaa::Off,
            Transform::from_xyz(0.0, 150.0, 0.0)
                .looking_at(Vec3::new(0.0, 150.0, -1000.0), Vec3::Y),
            FreeCamera {
                walk_speed: 500.0,
                run_speed: 1000.0,
                ..Default::default()
            },
        ))
        .id();

    // Fixed sun for baked terrain self-shadowing.
    let sun_direction = Vec3::new(0.87, 0.47, -0.15).normalize();
    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: false,
            illuminance: lux::RAW_SUNLIGHT,
            color: Color::WHITE,
            ..Default::default()
        },
        SunDisk {
            angular_size: SunDisk::EARTH.angular_size * 3.0,
            intensity: 30.0,
        },
        Transform::from_translation(sun_direction * 1000.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // Same terrain material set as the basic example (fetch the textures once
    // with `python crates/bevy_wilderness/assets/fetch_textures.py`).
    let layer = |suffix: &str| {
        ["grass", "dirt", "rock", "snow"]
            .map(|name| format!("../bevy_wilderness/assets/terrain/{name}_{suffix}.png"))
    };
    let albedo_array = load_terrain_array(&mut images, &layer("albedo"), true);
    let normal_array = load_terrain_array(&mut images, &layer("normal"), false);
    let orm_array = load_terrain_array(&mut images, &layer("orm"), false);
    let detail_albedo_array = albedo_array.clone();
    let detail_normal_array = normal_array.clone();
    let detail_orm_array = orm_array.clone();

    // Terrain heights and world footprint, shared by every source below.
    const HEIGHT_MIN: f32 = -1312.5;
    const HEIGHT_MAX: f32 = 1312.5;
    const WORLD_SIZE_M: f32 = 8192.0;

    // Terrain source: the editor opens on a **new terrain** by default — a flat
    // plain built through the editor-core path (a TerrainField, its image, an
    // EditableTerrain inserted directly). WILDERNESS_NEW=<texels> changes the
    // starting resolution; WILDERNESS_HEIGHTMAP=<file> opens an existing
    // heightmap instead (e.g. a previous export — the D7 round-trip). A fresh
    // terrain keeps the same world footprint — resolution changes texel
    // density, not scale. The "New" and "Load terrain…" buttons do the same at
    // runtime.
    let (texel_size, heightmap, from_scratch) = match std::env::var("WILDERNESS_HEIGHTMAP") {
        Ok(path) => (
            8.0,
            asset_server
                .load_builder()
                .with_settings(|settings: &mut ImageLoaderSettings| {
                    settings.is_srgb = false;
                })
                .load(path),
            None,
        ),
        Err(_) => {
            let size: u32 = std::env::var("WILDERNESS_NEW")
                .ok()
                .map(|size| {
                    size.parse()
                        .expect("WILDERNESS_NEW must be a texel count like 4096")
                })
                .unwrap_or(4096); // the D1 working resolution
            let texel_size = WORLD_SIZE_M / size as f32;
            let field = TerrainField::flat(
                size, size, texel_size, HEIGHT_MIN, HEIGHT_MAX, true, // looping
                0.0,  // a flat plain at sea level
            );
            let heightmap = images.add(field.to_image());
            (texel_size, heightmap, Some(EditableTerrain::new(field)))
        }
    };

    let mut terrain = commands.spawn(Clipmap {
        half_width: 128,
        levels: 8,
        base_scale: 1.0,
        texel_size,
        target,
        heightmap,
        albedo_array,
        normal_array,
        orm_array,
        layers: vec![
            TerrainLayer {
                tiling_scale: 150.0,
                height_blend: 0.3,
                normal_strength: 1.0,
                roughness: 0.99,
                slope: None,
                height: Some(HeightRule {
                    min: -2000.0,
                    max: 700.0,
                    blend: 250.0,
                }),
            },
            TerrainLayer {
                tiling_scale: 300.0,
                height_blend: 0.5,
                normal_strength: 1.3,
                roughness: 0.96,
                slope: Some(SlopeRule {
                    min_deg: 25.0,
                    max_deg: 55.0,
                    blend_deg: 10.0,
                }),
                height: Some(HeightRule {
                    min: -2000.0,
                    max: 700.0,
                    blend: 250.0,
                }),
            },
            TerrainLayer {
                tiling_scale: 150.0,
                height_blend: 0.95,
                normal_strength: 1.3,
                roughness: 0.94,
                slope: Some(SlopeRule {
                    min_deg: 45.0,
                    max_deg: 90.0,
                    blend_deg: 12.0,
                }),
                height: None,
            },
            TerrainLayer {
                tiling_scale: 100.0,
                height_blend: 0.4,
                normal_strength: 0.4,
                roughness: 0.7,
                slope: Some(SlopeRule {
                    min_deg: 0.0,
                    max_deg: 35.0,
                    blend_deg: 12.0,
                }),
                height: Some(HeightRule {
                    min: 500.0,
                    max: 800.0,
                    blend: 250.0,
                }),
            },
        ],
        detail: DetailConfig {
            albedo_array: detail_albedo_array,
            normal_array: detail_normal_array,
            orm_array: detail_orm_array,
            tiling: 50.0,
            normal_strength: 0.8,
            albedo_strength: 0.8,
            near: 60.0,
            far: 600.0,
        },
        min: HEIGHT_MIN,
        max: HEIGHT_MAX,
        wireframe: false,
        looping: true,
        // The editor creates and assigns the mask overlay texture, drives
        // clay mode (D10) from re-bake staleness, and floats the stamp
        // preview (D11).
        edit_overlay: None,
        clay: false,
        stamp: None,
    });
    match from_scratch {
        // From-scratch terrain: the field exists already, insert it directly.
        Some(editable) => terrain.insert(editable),
        // Loaded terrain: the marker makes the editor decode the image into
        // the authoritative field once it arrives.
        None => terrain.insert(Editable),
    };
}
