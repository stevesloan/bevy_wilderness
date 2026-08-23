//! The default egui UI for `bevy_wilderness_editor` (design doc P2): a side
//! panel driving the editor core purely through its public API — resources,
//! messages, and components, nothing crate-private. That's deliberate
//! dogfooding: anything this panel can do, a host app's own UI can do against
//! the same API, and anything it *can't* do cleanly is an editor-core API bug.
//!
//! Optional by design, and composable two ways:
//!
//! - **Standalone** (`TerrainEditorUiPlugin::default()`): the crate draws its
//!   own left side panel with every section, as the example editor does.
//! - **Embedded** (`TerrainEditorUiPlugin { embedded: true }`): no panel is
//!   drawn; the host takes the [`TerrainUi`] system param in its own egui
//!   system and calls the section methods it wants (`brush_section`,
//!   `erosion_section`, …) inside its own window or panel. Sections self-gate
//!   on the active tool, so a host can call them unconditionally.
//!
//! No egui type leaks back into the core (P2's hard rule); the only coupling
//! is this crate writing the core's resources.

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy_egui::{EguiContexts, EguiPlugin, EguiPrimaryContextPass, egui};

use bevy_wilderness_editor::{
    ActiveStamp, ActiveTool, BrushSettings, ClipmapReady, EditableTerrain, EditorSet, EditorTools,
    ErosionRequested, ErosionRun, ErosionSettings, ExportRequested, FogTier, HeightmapExported,
    LoadRequested, NewTerrainRequested, PointerBlocked, RebakeRequested, RebakeSettings,
    RedoRequest, SculptMode, SeamOverlay, StampData, StampSettings, TerrainGesture, TerrainLoaded,
    TerrainQuality, ToolId, UndoBuffer, UndoHistory, UndoRequest, WorldImportRequested,
    WorldImportRun, WorldImportSettings, WorldImported,
};

/// Named lat/lon starting points for the world-import section — famous
/// relief, one click away.
const WORLD_PRESETS: [(&str, f64, f64); 6] = [
    ("Matterhorn", 45.9766, 7.6585),
    ("Grand Canyon", 36.0980, -112.0970),
    ("Everest", 27.9881, 86.9250),
    ("Iceland highlands", 63.9800, -19.0600),
    ("Death Valley", 36.2400, -116.8200),
    ("Norwegian fjords", 62.1000, 7.0000),
];

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

/// The scanned stamp gallery ([`TerrainUi`] keeps one as a `Local`; public
/// only because it appears in that system param's state).
#[derive(Default)]
pub struct StampLibrary {
    entries: Vec<StampEntry>,
    scanned: bool,
    selected: Option<std::path::PathBuf>,
    status: Option<String>,
}

/// The default editor UI. Add after
/// [`TerrainEditorPlugin`](bevy_wilderness_editor::TerrainEditorPlugin); adds
/// `EguiPlugin` itself if the host hasn't.
#[derive(Default)]
pub struct TerrainEditorUiPlugin {
    /// `true`: register only the shared plumbing (pointer-over-UI blocking,
    /// the `Ui*` path resources) and draw nothing — the host embeds
    /// [`TerrainUi`] sections in its own UI. `false` (default): also draw the
    /// standalone side panel with every section.
    pub embedded: bool,
}

impl Plugin for TerrainEditorUiPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<EguiPlugin>() {
            app.add_plugins(EguiPlugin::default());
        }
        app.init_resource::<UiExportPath>()
            .init_resource::<UiStampFolder>()
            .add_systems(
                Update,
                // Before the shared pick, so a brush stroke can't land through a
                // panel the same frame the pointer moves onto it.
                block_pointer_over_ui.before(EditorSet::Pick),
            );
        if !self.embedded {
            app.add_systems(EguiPrimaryContextPass, editor_panel);
        }
    }
}

/// Scan the stamps folder into gallery entries: decode each PNG, downscale a
/// grayscale thumbnail, and register it with egui. Never fails hard — a bad
/// file becomes a status line, not a missing gallery.
fn scan_stamps(
    folder: &std::path::Path,
    images: &mut Assets<Image>,
    contexts: &mut EguiContexts,
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
fn block_pointer_over_ui(mut contexts: EguiContexts, mut blocked: ResMut<PointerBlocked>) {
    let over_ui = contexts
        .ctx_mut()
        .map(|ctx| ctx.egui_wants_pointer_input() || ctx.is_pointer_over_egui())
        .unwrap_or(false);
    blocked.set_if_neq(PointerBlocked(over_ui));
}

/// Everything the terrain sections read and write, as one system param a host
/// UI system can take alongside its own. Each `*_section` method draws one
/// block of widgets into a `&mut egui::Ui` the host provides. Tool-specific
/// sections (brush, stamp, mask, erosion) gate themselves on [`ActiveTool`]
/// and draw nothing for other tools, so hosts call every section
/// unconditionally and the UI reorganizes itself per tool; undo, bake, and
/// the seam toggle are tool-independent.
///
/// `active` is public: a host that replaces [`tools_section`](Self::tools_section)
/// with its own mode UI writes the active tool through it.
#[derive(SystemParam)]
pub struct TerrainUi<'w, 's> {
    pub active: ResMut<'w, ActiveTool>,
    tools: Res<'w, EditorTools>,
    brush: ResMut<'w, BrushSettings>,
    erosion: ResMut<'w, ErosionSettings>,
    seam: ResMut<'w, SeamOverlay>,
    history: ResMut<'w, UndoHistory>,
    gesture: ResMut<'w, TerrainGesture>,
    undo_requests: MessageWriter<'w, UndoRequest>,
    redo_requests: MessageWriter<'w, RedoRequest>,
    terrains: Query<'w, 's, (Entity, &'static mut EditableTerrain)>,
    runs: Query<'w, 's, &'static ErosionRun>,
    erode: MessageWriter<'w, ErosionRequested>,
    world_import: ResMut<'w, WorldImportSettings>,
    world_runs: Query<'w, 's, &'static WorldImportRun>,
    world_requests: MessageWriter<'w, WorldImportRequested>,
    world_imported: MessageReader<'w, 's, WorldImported>,
    world_status: Local<'s, Option<String>>,
    export_path: Res<'w, UiExportPath>,
    export: MessageWriter<'w, ExportRequested>,
    exported: MessageReader<'w, 's, HeightmapExported>,
    new_terrain: MessageWriter<'w, NewTerrainRequested>,
    load: MessageWriter<'w, LoadRequested>,
    loaded: MessageReader<'w, 's, TerrainLoaded>,
    rebake: ResMut<'w, RebakeSettings>,
    quality: ResMut<'w, TerrainQuality>,
    staged_quality: Local<'s, Option<TerrainQuality>>,
    commands: Commands<'w, 's>,
    ready: Query<'w, 's, Has<ClipmapReady>, With<EditableTerrain>>,
    active_stamp: ResMut<'w, ActiveStamp>,
    stamp_settings: ResMut<'w, StampSettings>,
    stamp_folder: Res<'w, UiStampFolder>,
    images: ResMut<'w, Assets<Image>>,
    library: Local<'s, StampLibrary>,
    export_status: Local<'s, Vec<String>>,
    new_size: Local<'s, u32>,
    terrain_status: Local<'s, Vec<String>>,
}

impl TerrainUi<'_, '_> {
    /// Every section in the standalone panel's order. Hosts embedding a
    /// subset call the individual methods instead.
    pub fn all_sections(&mut self, ui: &mut egui::Ui, contexts: &mut EguiContexts) {
        self.file_section(ui);
        self.world_section(ui);
        self.tools_section(ui);
        self.undo_section(ui);
        self.brush_section(ui);
        self.stamp_section(ui, contexts);
        self.mask_section(ui);
        self.erosion_section(ui);
        self.bake_section(ui);
        self.quality_section(ui);
        self.seam_section(ui);
        self.export_section(ui);
    }

    /// Height in meters at `xz` — [`TerrainHeight::sample`] for hosts whose
    /// UI system can't also take `TerrainHeight` (its read query conflicts
    /// with this param's mutable terrain access).
    ///
    /// [`TerrainHeight::sample`]: bevy_wilderness_editor::TerrainHeight::sample
    pub fn sample_height(&mut self, xz: Vec2) -> Option<f32> {
        self.terrains
            .iter()
            .find(|(_, t)| t.field.contains(xz))
            .map(|(_, t)| t.field.height_at(xz))
    }

    /// New-terrain and load-terrain controls. The editor opens on a fresh
    /// terrain by default; these get back to one, or open an existing
    /// heightmap, without a restart.
    pub fn file_section(&mut self, ui: &mut egui::Ui) {
        for done in self.loaded.read() {
            self.terrain_status.clear();
            self.terrain_status.push(match &done.error {
                None => format!("loaded {}", done.path.display()),
                Some(error) => format!("load failed: {error}"),
            });
        }
        // Local<u32> defaults to 0; seed the new-terrain resolution once.
        if *self.new_size == 0 {
            *self.new_size = 4096;
        }
        ui.separator();
        ui.label("Terrain");
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("new_terrain_size")
                .selected_text(format!("{}²", *self.new_size))
                .show_ui(ui, |ui| {
                    for size in [512u32, 1024, 2048, 4096] {
                        ui.selectable_value(&mut *self.new_size, size, format!("{size}²"));
                    }
                });
            if ui
                .button("New")
                .on_hover_text("Discard the current terrain and start a fresh flat plain")
                .clicked()
            {
                for (entity, _) in &self.terrains {
                    self.new_terrain.write(NewTerrainRequested {
                        terrain: entity,
                        size: *self.new_size,
                        height: 0.0,
                    });
                }
                self.terrain_status.clear();
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
            for (entity, _) in &self.terrains {
                self.load.write(LoadRequested {
                    terrain: entity,
                    path: path.clone(),
                });
            }
            self.terrain_status.clear();
        }
        for status in self.terrain_status.iter() {
            ui.small(status);
        }
    }

    /// The tool switcher: the registry, not a hard-coded list, so a
    /// host-registered tool shows up with no UI changes. Hosts with their own
    /// mode UI skip this and write [`Self::active`] directly.
    pub fn tools_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label("Tool");
        for tool in self.tools.iter() {
            if ui
                .selectable_label(self.active.0 == Some(tool.id), &tool.name)
                .clicked()
            {
                self.active.0 = Some(tool.id);
            }
        }
    }

    /// Undo / redo buttons over the shared history — requests, applied by the
    /// editor core with exclusive world access (terrain gestures and host
    /// actions alike).
    pub fn undo_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.horizontal(|ui| {
            let undo = egui::Button::new("⟲ Undo");
            if ui
                .add_enabled(self.history.undo_label().is_some(), undo)
                .on_hover_text(self.history.undo_label().unwrap_or_default())
                .clicked()
            {
                self.undo_requests.write(UndoRequest);
            }
            let redo = egui::Button::new("⟳ Redo");
            if ui
                .add_enabled(self.history.redo_label().is_some(), redo)
                .on_hover_text(self.history.redo_label().unwrap_or_default())
                .clicked()
            {
                self.redo_requests.write(RedoRequest);
            }
        });
    }

    /// Brush controls, for the tools that read them: sculpt gets the mode row
    /// plus radius and strength; mask paints with radius alone (strength
    /// does nothing there, so it's hidden). Draws nothing for other tools.
    pub fn brush_section(&mut self, ui: &mut egui::Ui) {
        let sculpting = match self.active.0 {
            Some(ToolId::SCULPT) => true,
            Some(ToolId::MASK) => false,
            _ => return,
        };
        ui.separator();
        ui.label("Brush");
        if sculpting {
            ui.horizontal_wrapped(|ui| {
                for (mode, name) in [
                    (SculptMode::Raise, "Raise"),
                    (SculptMode::Lower, "Lower"),
                    (SculptMode::Smooth, "Smooth"),
                    (SculptMode::Flatten, "Flatten"),
                ] {
                    if ui.selectable_label(self.brush.mode == mode, name).clicked() {
                        self.brush.mode = mode;
                    }
                }
            });
        }
        ui.add(
            egui::Slider::new(&mut self.brush.radius, 4.0..=2000.0)
                .logarithmic(true)
                .text("radius (m)"),
        );
        if sculpting {
            ui.add(
                egui::Slider::new(&mut self.brush.strength, 1.0..=500.0)
                    .logarithmic(true)
                    .text("strength (m/s)"),
            );
            ui.small("hold shift: smooth · ctrl: invert");
        }
    }

    /// Stamp size/strength/rotation and the PNG gallery. Draws nothing unless
    /// the stamp tool is active (the gallery scan also waits for that).
    pub fn stamp_section(&mut self, ui: &mut egui::Ui, contexts: &mut EguiContexts) {
        if self.active.0 != Some(ToolId::STAMP) {
            return;
        }
        if !self.library.scanned {
            scan_stamps(
                &self.stamp_folder.0,
                &mut self.images,
                contexts,
                &mut self.library,
            );
            self.library.scanned = true;
        }
        ui.separator();
        ui.label("Stamp");
        ui.add(
            egui::Slider::new(&mut self.stamp_settings.size, 10.0..=16384.0)
                .logarithmic(true)
                .text("size (m)"),
        );
        ui.add(
            egui::Slider::new(&mut self.stamp_settings.strength, -2000.0..=2000.0)
                .text("strength (m)"),
        );
        let mut degrees = self.stamp_settings.rotation.to_degrees();
        if ui
            .add(egui::Slider::new(&mut degrees, 0.0..=360.0).text("rotation °"))
            .changed()
        {
            self.stamp_settings.rotation = degrees.to_radians();
        }
        ui.add(egui::Slider::new(&mut self.stamp_settings.feather, 0.0..=0.5).text("feather"));
        ui.add(egui::Slider::new(&mut self.stamp_settings.offset, -1.0..=1.0).text("offset"));
        ui.small("wheel: strength · ctrl: size · shift: rotate · alt: feather");
        // The gallery: every PNG in the stamps folder, click to arm.
        let mut clicked = None;
        egui::ScrollArea::vertical()
            .max_height(180.0)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for entry in &self.library.entries {
                        let selected =
                            self.library.selected.as_deref() == Some(entry.path.as_path());
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
            match StampData::load_png(&path, &mut self.images) {
                Ok(data) => {
                    self.active_stamp.0 = Some(data);
                    self.library.selected = Some(path);
                    self.library.status = None;
                }
                Err(error) => self.library.status = Some(error),
            }
        }
        if ui.small_button("rescan folder").clicked() {
            self.library.scanned = false;
        }
        if let Some(status) = &self.library.status {
            ui.small(status.clone());
        }
        if self.active_stamp.0.is_none() {
            ui.small("pick a stamp to start previewing");
        }
    }

    /// Real-world terrain import: presets, lat/lon, vertical scale, and the
    /// import button/progress. Tool-independent (it replaces the whole map,
    /// like Load), so it draws whenever a terrain exists.
    pub fn world_section(&mut self, ui: &mut egui::Ui) {
        for done in self.world_imported.read() {
            *self.world_status = Some(match &done.error {
                None => "imported".into(),
                Some(error) => format!("import failed: {error}"),
            });
        }
        ui.separator();
        ui.label("World");
        ui.horizontal(|ui| {
            ui.label("preset");
            for chunk in WORLD_PRESETS.chunks(3) {
                for &(name, lat, lon) in chunk {
                    if ui.small_button(name).clicked() {
                        self.world_import.latitude = lat;
                        self.world_import.longitude = lon;
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut self.world_import.latitude)
                    .speed(0.01)
                    .range(-85.0..=85.0)
                    .prefix("lat "),
            );
            ui.add(
                egui::DragValue::new(&mut self.world_import.longitude)
                    .speed(0.01)
                    .range(-180.0..=180.0)
                    .prefix("lon "),
            );
        });
        ui.add(
            egui::Slider::new(&mut self.world_import.vertical_scale, 0.25..=4.0)
                .logarithmic(true)
                .text("vertical scale"),
        )
        .on_hover_text("1 = true relief; higher dramatizes, lower flattens");
        ui.checkbox(&mut self.world_import.seamless, "seamless loop")
            .on_hover_text(
                "Earth doesn't tile: without this the map repeats against a cliff \
                 hundreds of meters high. Removes the overall trend across the map \
                 (its tilt) and keeps every ridge. No effect on finite terrains.",
            );
        match self.world_runs.iter().next() {
            Some(run) => {
                ui.add(egui::ProgressBar::new(run.progress()).show_percentage());
            }
            None => {
                if ui.button("Import real terrain").clicked() {
                    *self.world_status = None;
                    for (entity, _) in &self.terrains {
                        self.world_requests.write(WorldImportRequested { terrain: entity });
                    }
                }
            }
        }
        if let Some(status) = self.world_status.as_deref() {
            ui.small(status.to_owned());
        }
        ui.small("replaces the map with Earth elevation at this point (AWS terrain tiles)");
    }

    /// Clear-mask button, for the mask tool and the erode tool (erosion runs
    /// over the mask). Draws nothing for other tools.
    pub fn mask_section(&mut self, ui: &mut egui::Ui) {
        if !matches!(self.active.0, Some(ToolId::MASK) | Some(ToolId::ERODE)) {
            return;
        }
        ui.separator();
        ui.label("Mask");
        let any_mask = self.terrains.iter().any(|(_, t)| t.mask_active());
        if ui
            .add_enabled(any_mask, egui::Button::new("Clear mask"))
            .clicked()
        {
            for (entity, mut terrain) in &mut self.terrains {
                if terrain.mask_active() {
                    self.gesture.begin(&mut self.history, entity, "Clear Mask");
                    self.gesture
                        .capture(&terrain, UndoBuffer::Mask, terrain.field.full_rect());
                    terrain.clear_mask();
                    self.gesture.seal(&mut self.history);
                }
            }
        }
    }

    /// Erosion parameters, the realism knobs, and the run button/progress.
    /// Draws nothing unless the erode tool is active.
    pub fn erosion_section(&mut self, ui: &mut egui::Ui) {
        if self.active.0 != Some(ToolId::ERODE) {
            return;
        }
        ui.separator();
        ui.label("Erosion");
        ui.add(
            egui::Slider::new(&mut self.erosion.iterations, 100..=4000)
                .logarithmic(true)
                .text("iterations"),
        )
        .on_hover_text("Simulated weather: more carves deeper, linearly slower");
        ui.add(
            egui::Slider::new(&mut self.erosion.rain_rate, 0.001..=0.05)
                .logarithmic(true)
                .text("rainfall"),
        )
        .on_hover_text("The main strength knob: meters of rain per simulated second");
        ui.add(
            egui::Slider::new(&mut self.erosion.capacity, 0.01..=0.5)
                .logarithmic(true)
                .text("capacity"),
        )
        .on_hover_text("Sediment the flow can carry: higher = deeper channels, bigger fans");
        ui.add(
            egui::Slider::new(&mut self.erosion.dissolve_rate, 0.05..=2.0).text("erode rate"),
        );
        ui.add(
            egui::Slider::new(&mut self.erosion.deposit_rate, 0.05..=2.0).text("deposit rate"),
        );
        ui.add(
            egui::Slider::new(&mut self.erosion.talus_angle_deg, 20.0..=45.0).text("talus angle °"),
        );
        // The D12 realism knobs — sane defaults, tucked away.
        egui::CollapsingHeader::new("Realism").show(ui, |ui| {
            ui.add(
                egui::Slider::new(&mut self.erosion.evaporation, 0.0..=0.2).text("evaporation"),
            )
            .on_hover_text("Water lost per second; bounds how far flows and fans reach");
            ui.add(
                egui::Slider::new(&mut self.erosion.min_tilt_deg, 0.0..=10.0).text("min tilt °"),
            )
            .on_hover_text("Channels that grade themselves flat keep incising at this tilt");
            ui.add(
                egui::Slider::new(&mut self.erosion.max_erosion_depth, 0.0..=3.0)
                    .text("max depth m"),
            )
            .on_hover_text("Water deeper than this armors the bed instead of digging pits");
            ui.add(
                egui::Slider::new(&mut self.erosion.max_flow_speed, 1.0..=15.0)
                    .text("max flow speed"),
            )
            .on_hover_text("Caps how fast water can carve; low values soften striping");
            ui.add(
                egui::Slider::new(&mut self.erosion.thermal_rate, 0.0..=20.0).text("thermal rate"),
            )
            .on_hover_text("How fast over-steep slopes shed talus; 0 disables");
            ui.add(
                egui::Slider::new(&mut self.erosion.time_step, 0.01..=0.1).text("time step s"),
            )
            .on_hover_text("Solver step; larger simulates more per iteration but can destabilize");
        });
        match self.runs.iter().next() {
            Some(run) => {
                ui.add(egui::ProgressBar::new(run.progress()).show_percentage());
            }
            None => {
                if ui.button("Erode").clicked() {
                    // Over the mask, or the whole map if none — same
                    // semantics as the erode tool's click.
                    for (entity, _) in &self.terrains {
                        self.erode.write(ErosionRequested { terrain: entity });
                    }
                }
            }
        }
        ui.small("erodes the masked region, or the whole map if none");
    }

    /// Terrain performance profile. Fog applies live; the bake-time knobs (RVT
    /// size, ambient gather, detail layers) stage locally until "Apply +
    /// rebake" writes them and queues the rebake that makes them real
    /// (`process_rebake_requests` resizes the RVT targets to match).
    pub fn quality_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label("Quality");
        let current = *self.quality;
        ui.horizontal(|ui| {
            ui.label("fog");
            let mut fog = current.fog;
            ui.selectable_value(&mut fog, FogTier::Low, "inline")
                .on_hover_text("terrain-only fog, no extra pass (iGPU/VR-friendly)");
            ui.selectable_value(&mut fog, FogTier::High, "fullscreen")
                .on_hover_text("fogs the sky too; needs Msaa::Off on the camera");
            if fog != current.fog {
                self.quality.fog = fog; // live, no rebake needed
            }
        });
        let mut staged = self.staged_quality.unwrap_or(current);
        ui.horizontal(|ui| {
            ui.label("RVT");
            egui::ComboBox::from_id_salt("quality_rvt_size")
                .selected_text(format!("{}²", staged.rvt_size))
                .show_ui(ui, |ui| {
                    for size in [2048u32, 4096, 8192] {
                        ui.selectable_value(&mut staged.rvt_size, size, format!("{size}²"));
                    }
                })
                .response
                .on_hover_text("Baked material resolution — the dominant VRAM/bake cost");
        });
        ui.checkbox(&mut staged.ambient_gather, "ambient gather")
            .on_hover_text("Bake + sample the macro-AO / bent-normal / cavity channel");
        ui.horizontal(|ui| {
            ui.label("detail layers");
            ui.selectable_value(&mut staged.detail_layers, 1, "1");
            ui.selectable_value(&mut staged.detail_layers, 2, "2");
        });
        let dirty = staged.rvt_size != current.rvt_size
            || staged.ambient_gather != current.ambient_gather
            || staged.detail_layers != current.detail_layers;
        *self.staged_quality = dirty.then_some(staged);
        if ui
            .add_enabled(dirty, egui::Button::new("Apply + rebake"))
            .clicked()
        {
            self.quality.rvt_size = staged.rvt_size;
            self.quality.ambient_gather = staged.ambient_gather;
            self.quality.detail_layers = staged.detail_layers;
            *self.staged_quality = None;
            for (entity, _) in &self.terrains {
                self.commands.entity(entity).insert(RebakeRequested);
            }
        }
        if dirty {
            ui.small("pending — Apply rebakes at the new settings");
        }
    }

    /// The seam-ring overlay toggle — a view option, not a tool setting, so
    /// it belongs with the always-visible sections.
    pub fn seam_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.checkbox(&mut self.seam.enabled, "seam ring");
    }

    /// Auto-rebake toggle and the manual bake button (which doubles as the
    /// bake-in-flight indicator).
    pub fn bake_section(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.label("Bake");
        ui.horizontal(|ui| {
            // D10: with auto off, edits never schedule the D5 re-bake —
            // model freely, then bake once. ClipmapReady is absent while a
            // bake is in flight, so the button doubles as its indicator.
            ui.checkbox(&mut self.rebake.auto, "auto re-bake")
                .on_hover_text("Re-bake shading ~200 ms after each edit settles");
            let baking = self.ready.iter().any(|ready| !ready);
            let label = if baking { "Baking…" } else { "Bake" };
            if ui
                .add_enabled(!baking, egui::Button::new(label))
                .on_hover_text("Re-bake shading (sun shadow, AO, material splat) now")
                .clicked()
            {
                for (entity, _) in &self.terrains {
                    self.commands.entity(entity).insert(RebakeRequested);
                }
            }
        });
    }

    /// The export button ([`UiExportPath`], both formats) and its status
    /// lines. Hosts with their own save pipeline skip this.
    pub fn export_section(&mut self, ui: &mut egui::Ui) {
        for done in self.exported.read() {
            // Replace the "exporting…" placeholder with per-file results.
            self.export_status.retain(|line| !line.ends_with('…'));
            self.export_status.push(match &done.error {
                None => format!("saved {}", done.path.display()),
                Some(error) => format!("failed {}: {error}", done.path.display()),
            });
        }
        ui.separator();
        ui.label("Export");
        if ui
            .button("Export heightmap")
            .on_hover_text(format!(
                "R16 KTX2 (engine) + 16-bit PNG (interchange) → {}",
                self.export_path.0.with_extension("{ktx2,png}").display()
            ))
            .clicked()
        {
            for (entity, _) in &self.terrains {
                // Both formats side by side: the KTX2 the renderer
                // re-loads and the PNG a physics/DCC pipeline reads.
                for extension in ["ktx2", "png"] {
                    self.export.write(ExportRequested {
                        terrain: entity,
                        path: self.export_path.0.with_extension(extension),
                    });
                }
            }
            self.export_status.clear();
            self.export_status.push("exporting…".into());
        }
        for status in self.export_status.iter() {
            ui.small(status);
        }
    }
}

/// The standalone side panel: every section, drawn by this crate.
fn editor_panel(mut contexts: EguiContexts, mut terrain_ui: TerrainUi) -> Result {
    let ctx = contexts.ctx_mut()?.clone();
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
            terrain_ui.all_sections(ui, &mut contexts);
        });
    Ok(())
}
