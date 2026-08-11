//! The graph panel, under the answer.
//!
//! `PLAN.md` §7 in one sentence: the panel shows **the neighbourhood of the answer you
//! are reading**, not the vault as a bag of dots. That is what makes it a feature and the
//! Phase D retrieval debugger out of one implementation — when expansion returns the
//! wrong section you can see whether the seed was wrong or the walk was.
//!
//! There is no embedding machinery here. `ygraphy` provides an `iced` `shader::Program`,
//! so the panel is an ordinary widget in an ordinary layout: no reparenting, no texture
//! handoff, no second process, no unstable features.
//!
//! The dock deliberately owns the *camera and selection* while `ygraphy` owns the layout
//! and drawing. Camera state has to outlive the panel being hidden and shown, and widget
//! state in iced does not.

use std::path::Path;

use ygraphy::layout::LayoutGraph;
use ygraphy::panel::Interaction;
use ygraphy::scene::Camera;

use crate::tokens;

/// Everything the panel needs, or nothing if it has never been opened.
///
/// Loading is lazy and stays lazy: a dock resident from login must not read a vault, run
/// a force simulation, or create GPU pipelines for a panel the user has not asked for.
pub struct GraphPanel {
    pub layout: LayoutGraph,
    pub camera: Camera,
    /// The section the panel is centred on — normally the answer's primary source.
    pub selected: Option<usize>,
    /// The panel's size, used to project labels and to fit the camera.
    ///
    /// Not observed from the widget's bounds but *asserted* from the tokens, because the
    /// panel is a fixed size in a fixed-width card. The shader is handed the real bounds
    /// by iced, so if these two ever disagree the labels drift away from the nodes they
    /// name — which is the failure to look for if the card ever becomes width-adaptive.
    pub viewport: [f32; 2],
    /// `section_uid` we were asked to seed on but have not applied yet, because the
    /// viewport is not known until the first frame.
    pending_focus: Option<String>,
}

impl GraphPanel {
    /// Read the vault and lay it out. Cost is paid once, on first open.
    pub fn load(vault: &Path) -> anyhow::Result<Self> {
        let graph = ygraphy::vault::reload_graph(vault)?;
        Ok(Self {
            layout: LayoutGraph::new(graph),
            camera: Camera::default(),
            selected: None,
            viewport: [tokens::DOCK_WIDTH, tokens::GRAPH_HEIGHT],
            pending_focus: None,
        })
    }

    /// Centre on a section, by the identity every other tool shares.
    ///
    /// Takes a `section_uid` rather than an index because that is what a `SourceRef`
    /// carries, and because it is parsed metadata — never model output — which is what
    /// makes it safe to act on (spec §12).
    pub fn focus_on(&mut self, section_uid: &str) {
        self.pending_focus = Some(section_uid.to_string());
        self.apply_pending_focus();
    }

    fn apply_pending_focus(&mut self) {
        let Some(uid) = self.pending_focus.take() else {
            return;
        };
        match self.layout.graph.index_of(&uid) {
            Some(index) => {
                self.selected = Some(index);
                self.camera.focus(self.layout.nodes[index].position);
            }
            None => {
                // A source from a stale index, or a non-vault source that has no
                // `section_uid` at all. Showing the whole graph is a better answer than
                // showing nothing.
                tracing::debug!(%uid, "no such section in the graph; fitting instead");
                self.camera.fit(&self.layout, self.viewport);
            }
        }
    }

    /// Advance the force simulation one frame.
    pub fn tick(&mut self) {
        self.layout.tick();
    }

    /// Has the layout stopped moving? Drives whether the dock asks for more frames.
    pub fn is_settled(&self) -> bool {
        self.layout.is_settled(tokens::GRAPH_SETTLE_SECONDS)
    }

    /// Fold an interaction from the widget back into panel state.
    ///
    /// Returns the `section_uid` the user activated, if any — the dock turns that into a
    /// jump, which is the same trusted-target rule every other action follows.
    pub fn on_interaction(&mut self, interaction: Interaction) -> Option<String> {
        match interaction {
            Interaction::Camera(camera) => self.camera = camera,
            Interaction::Selected(index) => self.selected = Some(index),
            Interaction::Dragged { index, world } => {
                self.layout.nodes[index].position = world;
                // Pinned, or the simulation pulls the node out of the hand holding it.
                self.layout.nodes[index].fixed = true;
            }
            Interaction::Activated(index) => {
                return Some(self.layout.nodes[index].uid.clone());
            }
        }
        None
    }
}
