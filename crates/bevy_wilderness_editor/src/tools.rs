//! The tool registry (design doc §6): one active tool at a time, built-ins and
//! host-registered tools alike. UI-agnostic — a UI lists [`EditorTools`] and
//! writes [`ActiveTool`]; tool behavior lives in systems gated by
//! [`tool_active`].

use bevy::prelude::*;

/// Identifies a tool. Use a namespaced string (`"mygame.place_gltf"`) so host
/// tools can't collide with built-ins or each other.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ToolId(pub &'static str);

impl ToolId {
    /// Built-in sculpt tool (raise / lower / smooth / flatten) — Phase 2.
    pub const SCULPT: ToolId = ToolId("wilderness.sculpt");
    /// Built-in mask paint tool — Phase 4.
    pub const MASK: ToolId = ToolId("wilderness.mask");
    /// Built-in erosion tool — Phase 5.
    pub const ERODE: ToolId = ToolId("wilderness.erode");
    /// Built-in stamp tool (heightfield PNG stamps) — Phase 11.
    pub const STAMP: ToolId = ToolId("wilderness.stamp");
}

/// A registered tool, as a UI would list it.
#[derive(Clone, Debug)]
pub struct ToolInfo {
    pub id: ToolId,
    /// Human-readable name for UI display.
    pub name: String,
}

/// Registry of every available tool: the built-ins plus whatever the host app
/// registers (e.g. the game's glTF-placement tool). Registration is just data —
/// the tool's behavior is the host's own systems, gated by [`tool_active`] and
/// driven by the shared [`TerrainCursor`](crate::TerrainCursor) /
/// [`TerrainHeight`](crate::TerrainHeight) /
/// [`TerrainRegionChanged`](crate::TerrainRegionChanged) API.
#[derive(Resource, Default)]
pub struct EditorTools {
    tools: Vec<ToolInfo>,
}

impl EditorTools {
    /// Register a tool. Re-registering an existing id replaces its info.
    pub fn register(&mut self, id: ToolId, name: impl Into<String>) {
        let info = ToolInfo {
            id,
            name: name.into(),
        };
        match self.tools.iter_mut().find(|t| t.id == id) {
            Some(existing) => *existing = info,
            None => self.tools.push(info),
        }
    }

    pub fn contains(&self, id: ToolId) -> bool {
        self.tools.iter().any(|t| t.id == id)
    }

    /// All registered tools, in registration order (built-ins first).
    pub fn iter(&self) -> impl Iterator<Item = &ToolInfo> {
        self.tools.iter()
    }
}

/// The single active tool (design doc §6: shared camera, selection, and input
/// focus — one tool owns the cursor at a time). `None` = no tool active. A UI
/// writes this; tool systems gate on [`tool_active`].
#[derive(Resource, Default, PartialEq, Eq, Debug)]
pub struct ActiveTool(pub Option<ToolId>);

/// Run condition: the given tool is active. Gate every tool's systems on this
/// so tools coexist without fighting over input:
///
/// ```no_run
/// # use bevy::prelude::*;
/// # use bevy_wilderness_editor::{ToolId, tool_active};
/// # let mut app = App::new();
/// # fn place_props() {}
/// const PLACE: ToolId = ToolId("mygame.place_gltf");
/// app.add_systems(Update, place_props.run_if(tool_active(PLACE)));
/// ```
pub fn tool_active(id: ToolId) -> impl FnMut(Res<ActiveTool>) -> bool + Clone {
    move |active: Res<ActiveTool>| active.0 == Some(id)
}
