//! The default egui UI for `bevy_wilderness_editor` (design doc P2): a side
//! panel driving the editor core purely through its public API — resources,
//! messages, and components, nothing crate-private. That's deliberate
//! dogfooding: anything this panel can do, a host app's own UI can do against
//! the same API, and anything it *can't* do cleanly is an editor-core API bug.
//!
//! Optional by design — the closed-source game embeds
//! [`TerrainEditorPlugin`](bevy_wilderness_editor::TerrainEditorPlugin) with
//! its own UI instead of this crate. No egui type leaks back into the core
//! (P2's hard rule); the only coupling is this crate writing the core's
//! resources.

use bevy::prelude::*;
use bevy_egui::{EguiPlugin, EguiPrimaryContextPass, egui};

use bevy_wilderness_editor::{
    ActiveStamp, ActiveTool, BrushSettings, ClipmapReady, EditableTerrain, EditorSet, EditorTools,
    ErosionRequested, ErosionRun, ErosionSettings, ExportRequested, HeightmapExported,
    LoadRequested, NewTerrainRequested, PointerBlocked, RebakeRequested, RebakeSettings,
    SculptMode, SeamOverlay, StampData, StampSettings, TerrainLoaded, ToolId, UndoBuffer,
    UndoHistory,
};

/// Base path for the panel's Export button. One click writes **both** export
/// formats beside each other — `<base>.ktx2` (the engine master a `Clipmap`
/// re-loads) and `<base>.png` (the 16-bit interchange copy, e.g. for a
/// physics pipeline's collision heightfield). The host app sets this — e.g.
/// into its assets directory so the export loads straight back.
#[derive(Resource, Clone, Debug)]
pub struct UiExportPath(pub std::path::PathBuf);

impl Default for UiExportPath {
    fn default() -> Self {
        Self("heightmap_export.ktx2".into())
    }
}

/// Folder the stamp gallery scans for heightfield PNGs (D11). The host app
/// points this somewhere real (the example ships starter stamps into its
/// own folder); the panel's Rescan button picks up files added while
/// running.
#[derive(Resource, Clone, Debug)]
pub struct UiStampFolder(pub std::path::PathBuf);

impl Default for UiStampFolder {
    fn default() -> Self {
        Self("stamps".into())
    }
}

/// One gallery entry: a PNG in the stamps folder with its egui thumbnail.
struct StampEntry {
    path: std::path::PathBuf,
    name: String,
    thumb: egui::TextureId,
    /// Keeps the thumbnail image asset alive.
    _handle: Handle<Image>,
}

/// The scanned gallery (a `Local` of the panel system).
#[derive(Default)]
struct StampLibrary {
    entries: Vec<StampEntry>,
    scanned: bool,
    selected: Option<std::path::PathBuf>,
    status: Option<String>,
}

/// The default editor UI. Add after
/// [`TerrainEditorPlugin`](bevy_wilderness_editor::TerrainEditorPlugin); adds
/// `EguiPlugin` itself if the host hasn't.
pub struct TerrainEditorUiPlugin;

impl Plugin for TerrainEditorUiPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<EguiPlugin>() {
            app.add_plugins(EguiPlugin::default());
        }
        app.init_resource::<UiExportPath>()
            .init_resource::<UiStampFolder>()
            .add_systems(EguiPrimaryContextPass, editor_panel)
            .add_systems(
                Update,
                // Before the shared pick, so a brush stroke can't land through a
                // panel the same frame the pointer moves onto it.
                block_pointer_over_ui.before(EditorSet::Pick),
            );
    }
}

/// Scan the stamps folder into gallery entries: decode each PNG, downscale a
/// grayscale thumbnail, and register it with egui. Never fails hard — a bad
/// file becomes a status line, not a missing gallery.
fn scan_stamps(
    folder: &std::path::Path,
    images: &mut Assets<Image>,
    contexts: &mut bevy_egui::EguiContexts,
    library: &mut StampLibrary,
) {
    library.entries.clear();
    library.status = None;
    let mut paths: Vec<_> = match std::fs::read_dir(folder) {
        Ok(dir) => dir
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| {
                path.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("png"))
            })
            .collect(),
        Err(error) => {
            library.status = Some(format!("stamp folder: {error}"));
            return;
        }
    };
    paths.sort();
    for path in paths {
        let Ok(decoded) = image::open(&path) else {
            continue; // not a decodable image; skip quietly
        };
        // Small grayscale thumbnail as RGBA for egui.
        let thumb = decoded.thumbnail(96, 96).into_luma8();
        let (w, h) = thumb.dimensions();
        let data = thumb
            .into_raw()
            .into_iter()
            .flat_map(|v| [v, v, v, 255])
            .collect();
        let handle = images.add(Image::new(
            bevy::render::render_resource::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            bevy::render::render_resource::TextureDimension::D2,
            data,
            bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
            bevy::asset::RenderAssetUsages::MAIN_WORLD
                | bevy::asset::RenderAssetUsages::RENDER_WORLD,
        ));
        let thumb = contexts.add_image(bevy_egui::EguiTextureHandle::Strong(handle.clone()));
        library.entries.push(StampEntry {
            name: path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path,
            thumb,
            _handle: handle,
        });
    }
    if library.entries.is_empty() && library.status.is_none() {
        library.status = Some(format!("no PNGs in {}", folder.display()));
    }
}

/// Mirror egui's pointer claim into the core's [`PointerBlocked`] — the only
/// input-focus handshake the core needs from a UI (design doc §6).
fn block_pointer_over_ui(
    mut contexts: bevy_egui::EguiContexts,
    mut blocked: ResMut<PointerBlocked>,
) {
    let over_ui = contexts
        .ctx_mut()
        .map(|ctx| ctx.egui_wants_pointer_input() || ctx.is_pointer_over_egui())
        .unwrap_or(false);
    blocked.set_if_neq(PointerBlocked(over_ui));
}

/// The side panel: tool switcher (from the registry, so host-registered tools
/// appear automatically), undo/redo, brush, mask, seam, and erosion controls.
// Bevy systems legitimately take one param per resource; the ParamSet is two
// views of the same terrain query (`UndoHistory`'s API wants the bare one).
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn editor_panel(
    mut contexts: bevy_egui::EguiContexts,
    tools: Res<EditorTools>,
    mut active: ResMut<ActiveTool>,
    mut brush: ResMut<BrushSettings>,
    mut erosion: ResMut<ErosionSettings>,
    mut seam: ResMut<SeamOverlay>,
    mut history: ResMut<UndoHistory>,
    mut terrains: ParamSet<(
        Query<&mut EditableTerrain>,
        Query<(Entity, &mut EditableTerrain)>,
    )>,
    runs: Query<&ErosionRun>,
    mut erode: MessageWriter<ErosionRequested>,
    // Grouped into tuples: bevy systems cap at 16 top-level params.
    export: (
        Res<UiExportPath>,
        MessageWriter<ExportRequested>,
        MessageReader<HeightmapExported>,
    ),
    swap: (
        MessageWriter<NewTerrainRequested>,
        MessageWriter<LoadRequested>,
        MessageReader<TerrainLoaded>,
    ),
    bake: (
        ResMut<RebakeSettings>,
        Commands,
        Query<Has<ClipmapReady>, With<EditableTerrain>>,
    ),
    stamp: (
        ResMut<ActiveStamp>,
        ResMut<StampSettings>,
        Res<UiStampFolder>,
        ResMut<Assets<Image>>,
        Local<StampLibrary>,
    ),
    locals: (Local<Vec<String>>, Local<u32>, Local<Vec<String>>),
) -> Result {
    let (export_path, mut export, mut exported) = export;
    let (mut new_terrain, mut load, mut loaded) = swap;
    let (mut rebake, mut commands, ready) = bake;
    let (mut active_stamp, mut stamp_settings, stamp_folder, mut images, mut library) = stamp;
    let (mut export_status, mut new_size, mut terrain_status) = locals;
    // Scan the gallery before the panel borrows the egui context (thumbnail
    // registration needs `contexts`).
    if active.0 == Some(ToolId::STAMP) && !library.scanned {
        scan_stamps(&stamp_folder.0, &mut images, &mut contexts, &mut library);
        library.scanned = true;
    }
    for done in exported.read() {
        // Replace the "exporting…" placeholder with per-file results.
        export_status.retain(|line| !line.ends_with('…'));
        export_status.push(match &done.error {
            None => format!("saved {}", done.path.display()),
            Some(error) => format!("failed {}: {error}", done.path.display()),
        });
    }
    for done in loaded.read() {
        terrain_status.clear();
        terrain_status.push(match &done.error {
            None => format!("loaded {}", done.path.display()),
            Some(error) => format!("load failed: {error}"),
        });
    }
    // Local<u32> defaults to 0; seed the new-terrain resolution once.
    if *new_size == 0 {
        *new_size = 4096;
    }
    let ctx = contexts.ctx_mut()?;
    // egui 0.35: panels attach to a root `Ui` spanning the viewport.
    let mut root = egui::Ui::new(
        ctx.clone(),
        "wilderness_editor_root".into(),
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(ctx.viewport_rect()),
    );
    egui::Panel::left("wilderness_editor")
        .default_size(230.0)
        .show(&mut root, |ui| {
            ui.heading("Terrain Editor");

            ui.separator();
            ui.label("Terrain");
            // The editor opens on a fresh terrain by default; these get back to
            // one, or open an existing heightmap, without a restart.
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("new_terrain_size")
                    .selected_text(format!("{}²", *new_size))
                    .show_ui(ui, |ui| {
                        for size in [512u32, 1024, 2048, 4096] {
                            ui.selectable_value(&mut *new_size, size, format!("{size}²"));
                        }
                    });
                if ui
                    .button("New")
                    .on_hover_text("Discard the current terrain and start a fresh flat plain")
                    .clicked()
                {
                    for (entity, _) in &terrains.p1() {
                        new_terrain.write(NewTerrainRequested {
                            terrain: entity,
                            size: *new_size,
                            height: 0.0,
                        });
                    }
                    terrain_status.clear();
                }
            });
            if ui
                .button("Load terrain…")
                .on_hover_text("Replace the terrain from an R16 KTX2 or 16-bit PNG heightmap")
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Heightmap", &["ktx2", "png"])
                    .pick_file()
            {
                for (entity, _) in &terrains.p1() {
                    load.write(LoadRequested {
                        terrain: entity,
                        path: path.clone(),
                    });
                }
                terrain_status.clear();
            }
            for status in terrain_status.iter() {
                ui.small(status);
            }

            ui.separator();
            ui.label("Tool");
            // The registry, not a hard-coded list: a host-registered tool
            // (the game's glTF placement) shows up here with no UI changes.
            for tool in tools.iter() {
                if ui
                    .selectable_label(active.0 == Some(tool.id), &tool.name)
                    .clicked()
                {
                    active.0 = Some(tool.id);
                }
            }

            ui.separator();
            ui.horizontal(|ui| {
                let undo = egui::Button::new("⟲ Undo");
                if ui
                    .add_enabled(history.undo_label().is_some(), undo)
                    .on_hover_text(history.undo_label().unwrap_or_default())
                    .clicked()
                {
                    history.undo(&mut terrains.p0());
                }
                let redo = egui::Button::new("⟳ Redo");
                if ui
                    .add_enabled(history.redo_label().is_some(), redo)
                    .on_hover_text(history.redo_label().unwrap_or_default())
                    .clicked()
                {
                    history.redo(&mut terrains.p0());
                }
            });

            ui.separator();
            ui.label("Brush");
            if active.0 == Some(ToolId::SCULPT) {
                ui.horizontal_wrapped(|ui| {
                    for (mode, name) in [
                        (SculptMode::Raise, "Raise"),
                        (SculptMode::Lower, "Lower"),
                        (SculptMode::Smooth, "Smooth"),
                        (SculptMode::Flatten, "Flatten"),
                    ] {
                        if ui.selectable_label(brush.mode == mode, name).clicked() {
                            brush.mode = mode;
                        }
                    }
                });
            }
            ui.add(
                egui::Slider::new(&mut brush.radius, 4.0..=2000.0)
                    .logarithmic(true)
                    .text("radius (m)"),
            );
            ui.add(
                egui::Slider::new(&mut brush.strength, 1.0..=500.0)
                    .logarithmic(true)
                    .text("strength (m/s)"),
            );

            if active.0 == Some(ToolId::STAMP) {
                ui.separator();
                ui.label("Stamp");
                ui.add(
                    egui::Slider::new(&mut stamp_settings.size, 10.0..=16384.0)
                        .logarithmic(true)
                        .text("size (m)"),
                );
                ui.add(
                    egui::Slider::new(&mut stamp_settings.strength, -2000.0..=2000.0)
                        .text("strength (m)"),
                );
                let mut degrees = stamp_settings.rotation.to_degrees();
                if ui
                    .add(egui::Slider::new(&mut degrees, 0.0..=360.0).text("rotation °"))
                    .changed()
                {
                    stamp_settings.rotation = degrees.to_radians();
                }
                ui.small("wheel: strength · ctrl: size · shift: rotate");
                // The gallery: every PNG in the stamps folder, click to arm.
                let mut clicked = None;
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            for entry in &library.entries {
                                let selected =
                                    library.selected.as_deref() == Some(entry.path.as_path());
                                let button = egui::Button::image(egui::load::SizedTexture::new(
                                    entry.thumb,
                                    [56.0, 56.0],
                                ))
                                .selected(selected);
                                if ui.add(button).on_hover_text(&entry.name).clicked() {
                                    clicked = Some(entry.path.clone());
                                }
                            }
                        });
                    });
                if let Some(path) = clicked {
                    match StampData::load_png(&path, &mut images) {
                        Ok(data) => {
                            active_stamp.0 = Some(data);
                            library.selected = Some(path);
                            library.status = None;
                        }
                        Err(error) => library.status = Some(error),
                    }
                }
                if ui.small_button("rescan folder").clicked() {
                    library.scanned = false;
                }
                if let Some(status) = &library.status {
                    ui.small(status.clone());
                }
                if active_stamp.0.is_none() {
                    ui.small("pick a stamp to start previewing");
                }
            }

            ui.separator();
            ui.label("Mask");
            ui.horizontal(|ui| {
                let any_mask = terrains.p0().iter().any(|t| t.mask_active());
                if ui
                    .add_enabled(any_mask, egui::Button::new("Clear mask"))
                    .clicked()
                {
                    for (entity, mut terrain) in &mut terrains.p1() {
                        if terrain.mask_active() {
                            history.begin(entity, "Clear Mask");
                            history.capture(&terrain, UndoBuffer::Mask, terrain.field.full_rect());
                            terrain.clear_mask();
                            history.seal();
                        }
                    }
                }
                ui.checkbox(&mut seam.enabled, "seam ring");
            });

            ui.separator();
            ui.label("Erosion");
            ui.add(
                egui::Slider::new(&mut erosion.droplet_density, 0.01..=1.0)
                    .logarithmic(true)
                    .text("droplet density"),
            );
            ui.add(
                egui::Slider::new(&mut erosion.sediment_capacity, 0.5..=16.0)
                    .logarithmic(true)
                    .text("capacity"),
            );
            ui.add(egui::Slider::new(&mut erosion.erode_rate, 0.05..=1.0).text("erode rate"));
            ui.add(egui::Slider::new(&mut erosion.deposit_rate, 0.05..=1.0).text("deposit rate"));
            ui.add(
                egui::Slider::new(&mut erosion.talus_angle_deg, 20.0..=45.0).text("talus angle °"),
            );
            match runs.iter().next() {
                Some(run) => {
                    ui.add(egui::ProgressBar::new(run.progress()).show_percentage());
                }
                None => {
                    if ui.button("Erode").clicked() {
                        // Over the mask, or the whole map if none — same
                        // semantics as the erode tool's click.
                        for (entity, _) in &terrains.p1() {
                            erode.write(ErosionRequested { terrain: entity });
                        }
                    }
                }
            }

            ui.separator();
            ui.label("Bake");
            ui.horizontal(|ui| {
                // D10: with auto off, edits never schedule the D5 re-bake —
                // model freely, then bake once. ClipmapReady is absent while a
                // bake is in flight, so the button doubles as its indicator.
                ui.checkbox(&mut rebake.auto, "auto re-bake")
                    .on_hover_text("Re-bake shading ~200 ms after each edit settles");
                let baking = ready.iter().any(|ready| !ready);
                let label = if baking { "Baking…" } else { "Bake" };
                if ui
                    .add_enabled(!baking, egui::Button::new(label))
                    .on_hover_text("Re-bake shading (sun shadow, AO, material splat) now")
                    .clicked()
                {
                    for (entity, _) in &terrains.p1() {
                        commands.entity(entity).insert(RebakeRequested);
                    }
                }
            });

            ui.separator();
            ui.label("Export");
            if ui
                .button("Export heightmap")
                .on_hover_text(format!(
                    "R16 KTX2 (engine) + 16-bit PNG (interchange) → {}",
                    export_path.0.with_extension("{ktx2,png}").display()
                ))
                .clicked()
            {
                for (entity, _) in &terrains.p1() {
                    // Both formats side by side: the KTX2 the renderer
                    // re-loads and the PNG a physics/DCC pipeline reads.
                    for extension in ["ktx2", "png"] {
                        export.write(ExportRequested {
                            terrain: entity,
                            path: export_path.0.with_extension(extension),
                        });
                    }
                }
                export_status.clear();
                export_status.push("exporting…".into());
            }
            for status in export_status.iter() {
                ui.small(status);
            }
        });
    Ok(())
}
