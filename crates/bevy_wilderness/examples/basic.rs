use bevy::{
    camera::{Exposure, Hdr},
    camera_controller::free_camera::{FreeCamera, FreeCameraPlugin},
    image::ImageLoaderSettings,
    light::{
        atmosphere::ScatteringMedium, light_consts::lux, Atmosphere, AtmosphereEnvironmentMapLight,
        SunDisk,
    },
    pbr::AtmosphereSettings,
    post_process::bloom::{Bloom, BloomCompositeMode, BloomPrefilter},
    prelude::*,
};

use bevy::pbr::ExtendedMaterial;
use bevy_wilderness::{
    load_terrain_array, Clipmap, ClipmapPlugin, DetailConfig, FogTier, HeightFog,
    HeightFogExtension, HeightFogPlugin, HeightRule, SlopeRule, TerrainFog, TerrainLayer,
    TerrainQuality,
};

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins)
        .add_plugins(FreeCameraPlugin)
        .add_plugins(ClipmapPlugin)
        // Installs the fullscreen fog path (`High` tier). The inline path (`Low`
        // tier) is always in the crate; the tier resource picks which is realized.
        .add_plugins(HeightFogPlugin)
        .add_systems(Startup, setup)
        .add_systems(Update, (update_sun_color, toggle_tier, spawn_character));
    // Exercises the editable-terrain API (run with `--features editing`).
    #[cfg(feature = "editing")]
    app.add_systems(Update, rebake_on_r);
    app.run();
}

/// Press R to swing the sun ~30° and request a re-bake (`editing` feature) — the
/// acceptance check for the re-bake API: the *baked* terrain self-shadows/AO
/// visibly move to the new sun a moment later. (The real-time diffuse shading
/// moves instantly either way; without the re-bake the baked shadows would stay
/// stale at the old sun angle.)
#[cfg(feature = "editing")]
fn rebake_on_r(
    keys: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
    mut suns: Query<&mut Transform, With<DirectionalLight>>,
    clipmaps: Query<Entity, With<Clipmap>>,
) {
    if !keys.just_pressed(KeyCode::KeyR) {
        return;
    }
    for mut transform in &mut suns {
        let position = Quat::from_rotation_y(0.5) * transform.translation;
        *transform = Transform::from_translation(position).looking_at(Vec3::ZERO, Vec3::Y);
    }
    for clipmap in &clipmaps {
        commands
            .entity(clipmap)
            .insert(bevy_wilderness::RebakeRequested);
    }
    info!("sun moved 0.5 rad; terrain re-bake requested");
}

/// Press C to drop a "character" (capsule) ~20 m in front of the camera. It uses
/// `ExtendedMaterial<StandardMaterial, HeightFogExtension>`, so the crate keeps its
/// fog in sync with the tier — fly down into a misty valley and spawn one to see it
/// sit *in* the fog (on the Low tier it would be a crisp cutout without the
/// extension; on High the fullscreen pass fogs it anyway).
fn spawn_character(
    keys: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut fog_materials: ResMut<Assets<ExtendedMaterial<StandardMaterial, HeightFogExtension>>>,
    camera: Query<&GlobalTransform, With<FreeCamera>>,
    mut mesh: Local<Option<Handle<Mesh>>>,
) {
    if !keys.just_pressed(KeyCode::KeyC) {
        return;
    }
    let Ok(cam) = camera.single() else {
        return;
    };
    let capsule = mesh
        .get_or_insert_with(|| meshes.add(Capsule3d::new(1.0, 3.0)))
        .clone();
    let material = fog_materials.add(ExtendedMaterial {
        base: StandardMaterial {
            base_color: Color::srgb(0.9, 0.15, 0.15),
            perceptual_roughness: 0.6,
            ..default()
        },
        extension: HeightFogExtension::default(),
    });
    commands.spawn((
        Mesh3d(capsule),
        MeshMaterial3d(material),
        Transform::from_translation(cam.translation() + cam.forward().as_vec3() * 20.0),
    ));
}

/// Press T to flip the fog tier. A real app would set the quality profile once at
/// startup from device detection (XR session present → dial the knobs down), not on
/// a keypress. (Only the fog tier is live-switchable; the bake-time knobs aren't.)
fn toggle_tier(keys: Res<ButtonInput<KeyCode>>, mut quality: ResMut<TerrainQuality>) {
    if keys.just_pressed(KeyCode::KeyT) {
        quality.fog = match quality.fog {
            FogTier::High => FogTier::Low,
            FogTier::Low => FogTier::High,
        };
        info!("fog tier: {:?}", quality.fog);
    }
}

/// Physically-derived sun color from its elevation: the Rayleigh transmittance of
/// the atmosphere along the sun's path. Blue scatters out fastest, so the sun
/// warms and reddens as it drops toward the horizon — white overhead, golden at a
/// low morning angle, deep orange at sunset. Recomputed each frame, so it tracks a
/// moving sun for free (and settles instantly for a fixed one).
fn update_sun_color(mut suns: Query<(&mut DirectionalLight, &GlobalTransform)>) {
    for (mut light, transform) in &mut suns {
        // `back()` is the direction toward the sun; its Y is sin(elevation).
        let to_sun = transform.back().as_vec3();
        let cos_zenith = to_sun.y.clamp(0.0, 1.0);
        // Kasten–Young relative air mass, clamped so the horizon doesn't blow up.
        let zenith_deg = cos_zenith.acos().to_degrees();
        let air_mass = 1.0 / (cos_zenith + 0.15 * (93.885 - zenith_deg).max(0.05).powf(-1.253));
        // Approx sea-level Rayleigh optical depth per RGB at zenith (~λ⁻⁴).
        let tau = Vec3::new(0.05, 0.10, 0.23) * air_mass;
        let t = Vec3::new((-tau.x).exp(), (-tau.y).exp(), (-tau.z).exp());
        // Normalize to the brightest channel: this tints hue only, leaving overall
        // brightness to `illuminance` (raise-to-white overhead, warm when low).
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
    // Authored fog (the "what") + the starting tier (the "how"). The crate keeps
    // both fog paths in sync with these. max_distance MUST match the camera `far`,
    // or the fog steps where the terrain skirt meets sky.
    commands.insert_resource(TerrainFog(HeightFog {
        density: 0.002e-4,
        falloff: 0.0128,
        base_height: 0.0,
        max_distance: 16384.0,
        ..default()
    }));
    // Performance profile, set once from device detection. Desktop-grade values
    // (== TerrainQuality::default()); a headset dials them down (FogTier::Low,
    // 2048² RVT, no ambient gather, single detail layer).
    commands.insert_resource(TerrainQuality {
        fog: FogTier::High,
        rvt_size: 8192,
        ambient_gather: true,
        detail_layers: 2,
        sun_shadow_size: 2048,
    });

    // The atmosphere renders its planet limb as a hard brown line at eye level
    // (ground_albedo can't brighten it — grazing transmittance extinguishes it).
    // Sinking the planet 3 km dips that line below terrain silhouettes and softens
    // it
    let atmosphere = Atmosphere::earth(scattering_mediums.add(ScatteringMedium::earth(256, 256)));
    let planet_center = -Vec3::Y * (atmosphere.inner_radius + 3_000.0);
    commands.spawn((atmosphere, Transform::from_translation(planet_center)));

    let target = commands
        .spawn((
            Camera3d::default(),
            // HDR so RAW_SUNLIGHT lux + bloom aren't clipped to LDR before tonemap.
            Hdr,
            Projection::from(PerspectiveProjection {
                fov: 90.0_f32.to_radians(),
                // Default 1 km frustum-culls distant terrain; match the aerial-view
                // LUT (16 km) so distant peaks render and pick up aerial perspective.
                far: 16384.0,
                ..Default::default()
            }),
            // Additive + threshold so only the bright sun/highlights glow (terrain
            // stays crisp); NATURAL's energy-conserving mode blooms everything flat.
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
            // Physical-sky env map is the sole ambient — zero the flat fill so
            // shadowed valleys are lit only by the sky they can see.
            AmbientLight {
                brightness: 0.0,
                ..default()
            },
            // Fixed exposure (applied before bloom, so the threshold can isolate
            // the sun). See the T toggle for the auto-exposure caveat.
            Exposure::SUNLIGHT,
            // Fog params, driven by the crate from the active tier; placeholders here.
            HeightFog::default(),
            // FogTier::High needs MSAA off (single-sampled depth). MSAA is ours to
            // set, not the crate's; FogTier::Low is MSAA-friendly inline fog.
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

    // Fixed sun for baked terrain self-shadowing: 8am North American summer —
    // east, slightly north, ~28 degrees above the horizon (+X east, -Z north).
    let sun_direction = Vec3::new(0.87, 0.47, -0.15).normalize();
    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: false,
            illuminance: lux::RAW_SUNLIGHT,
            // Driven each frame by `update_sun_color` from the sun's elevation;
            // white is just the frame-0 value before that runs.
            color: Color::WHITE,
            ..Default::default()
        },
        SunDisk {
            angular_size: SunDisk::EARTH.angular_size * 3.0,
            intensity: 30.0,
        },
        Transform::from_translation(sun_direction * 1000.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // CC0 texture sets from polyhaven.com, one file per layer in the same order
    // as `Clipmap::layers` — run `python3 assets/fetch_textures.py` once to
    // download them. srgb = true for color, false for linear normal / ORM.
    // ORM packs occlusion, roughness, metallic into R, G, B (metallic ~0).
    let layer = |suffix: &str| {
        ["grass", "dirt", "rock", "snow"].map(|name| format!("assets/terrain/{name}_{suffix}.png"))
    };
    let albedo_array = load_terrain_array(&mut images, &layer("albedo"), true);
    let normal_array = load_terrain_array(&mut images, &layer("normal"), false);
    let orm_array = load_terrain_array(&mut images, &layer("orm"), false);
    // Close-range detail reuses the same arrays at a finer tiling — the RVT
    // only holds ~2m texels, so all sub-2m structure comes from these.
    let detail_albedo_array = albedo_array.clone();
    let detail_normal_array = normal_array.clone();
    let detail_orm_array = orm_array.clone();

    commands.spawn(Clipmap {
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
        // Two demo holes ahead of the camera (it looks down -Z from 150 m up): a
        // circle and a rect turned 30° — see them cut at any LOD ring they span
        // ≥ HOLE_MIN_CELLS of, and seal when flown far enough away.
        holes: vec![
            bevy_wilderness::HoleShape::Circle {
                center: Vec2::new(0.0, -300.0),
                radius: 40.0,
            },
            bevy_wilderness::HoleShape::Rect {
                center: Vec2::new(160.0, -420.0),
                half_extents: Vec2::new(60.0, 15.0),
                basis: Vec2::new(0.866, 0.5),
            },
        ],
        // Fully procedural placement (no control map): grass/dirt/rock partition
        // by slope, snow by height. Snowline ~700 m (height range is ±1312.5).
        layers: vec![
            // grass — base layer, everywhere below the snowline. No slope band, so
            // it competes on cliffs and pokes through the rock (natural look).
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
            // dirt — mid slopes, below the snowline
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
            // rock — steep terrain at any height (cliffs stay bare above snow)
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
            // snow — above the snowline, on all but the steepest faces
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
        // The editor assigns these when embedding; unused in the plain example.
        #[cfg(feature = "editing")]
        edit_overlay: None,
        #[cfg(feature = "editing")]
        clay: false,
        #[cfg(feature = "editing")]
        stamp: None,
    });
}
