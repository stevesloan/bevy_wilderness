//! Minimal editor host app (design doc §7): adds the renderer + editor plugins,
//! marks the terrain [`Editable`], and registers a third-party "probe" tool to
//! prove the extension API — the same embedding path a game uses.
//!
//! Phase 1 scope: the terrain renders from the R16 map derived from the f32
//! field, and the probe tool receives the shared cursor pick (drawn as a ring)
//! and `TerrainRegionChanged` events (logged). Sculpting arrives in Phase 2.
//!
//! ```sh
//! cargo run -p bevy_wilderness_editor --example editor
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
    ActiveTool, BrushSettings, Editable, EditorSet, EditorTools, TerrainCursor,
    TerrainEditorPlugin, TerrainRegionChanged, ToolId, tool_active,
};

/// The demo third-party tool: proves a host-registered tool receives the shared
/// pick and edit events without the editor knowing anything about it.
const PROBE: ToolId = ToolId("example.probe");

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
        .add_systems(Startup, (setup, register_probe_tool))
        .add_systems(Update, update_sun_color)
        .add_systems(
            Update,
            probe_tool
                .run_if(tool_active(PROBE))
                .in_set(EditorSet::Tools),
        )
        .run();
}

/// Register the demo tool and make it active — exactly what a game does for
/// its own tools (e.g. glTF placement).
fn register_probe_tool(mut tools: ResMut<EditorTools>, mut active: ResMut<ActiveTool>) {
    tools.register(PROBE, "Demo Probe");
    active.0 = Some(PROBE);
}

/// The no-op tool of the Phase 1 acceptance check: draws a brush-sized ring at
/// the shared cursor pick and logs `TerrainRegionChanged` events (expect one
/// full-terrain event at startup, when the display map is first derived from
/// the f32 field).
fn probe_tool(
    cursor: Res<TerrainCursor>,
    brush: Res<BrushSettings>,
    mut changed: MessageReader<TerrainRegionChanged>,
    mut gizmos: Gizmos,
) {
    if let Some(hit) = &cursor.0 {
        let up = Isometry3d::new(
            hit.position + Vec3::Y * 0.5,
            Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        );
        gizmos.circle(up, brush.radius, Color::srgb(1.0, 0.4, 0.1));
        gizmos.sphere(
            Isometry3d::from_translation(hit.position),
            2.0,
            Color::srgb(1.0, 0.9, 0.2),
        );
    }
    for event in changed.read() {
        info!(
            "probe tool: terrain {:?} changed over {:?}",
            event.terrain, event.region
        );
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

    commands.spawn((
        Clipmap {
            half_width: 128,
            levels: 8,
            base_scale: 1.0,
            texel_size: 8.0,
            target,
            heightmap: asset_server
                .load_builder()
                .with_settings(|settings: &mut ImageLoaderSettings| {
                    settings.is_srgb = false;
                })
                .load("heightmap_1024x1024.ktx2"),
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
            min: -1312.5,
            max: 1312.5,
            wireframe: false,
            looping: true,
        },
        // The one line that makes the terrain editable.
        Editable,
    ));
}
