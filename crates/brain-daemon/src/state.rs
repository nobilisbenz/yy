//! Daemon state.
//!
//! The daemon — not the dock — owns whether the dock is visible. That is what
//! lets `brainctl toggle` be a stateless one-shot binary: it asks the daemon to
//! flip a bit, and the daemon tells the UI what to do. The dock never has to be
//! consulted, which matters because it may not be connected yet.
//!
//! A `std::sync::Mutex` is deliberate: every critical section here is a few
//! field reads with no `.await` inside, so an async mutex would buy nothing and
//! cost a scheduling hop on the summon path.

use std::sync::Mutex;
use std::time::Instant;

use brain_index::IndexStats;
use brain_proto::{ServerEvent, StatusReport, TimingInfo};
use tokio::sync::mpsc::UnboundedSender;

/// Read the focused window, or an empty context if X11 is unavailable.
///
/// Best-effort by design: a failure here costs a ranking signal, never a query. Running
/// headless, or on a machine with no X display at all, must simply mean no boosts.
fn capture_context() -> brain_proto::DesktopContext {
    let captured = (|| {
        use x11rb::connection::Connection as _;
        let (connection, screen) = x11rb::connect(None).ok()?;
        let root = connection.setup().roots.get(screen)?.root;
        let atoms = brain_x11::Atoms::intern(&connection).ok()?;
        brain_x11::context::capture(&connection, &atoms, root).ok()
    })();

    match captured {
        Some(context) => brain_proto::DesktopContext {
            wm_class: context.wm_class,
            window_title: context.window_title,
            pid: context.pid,
            process_name: context.process_name,
            cwd: context.cwd,
            workspace: context.workspace,
        },
        None => {
            tracing::debug!("no desktop context available");
            brain_proto::DesktopContext::default()
        }
    }
}

/// Outcome of a visibility request, so the caller can log what actually
/// happened rather than what it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Shown,
    Hidden,
    /// Already in the requested state; no event was sent.
    Unchanged,
}

struct Ui {
    /// Monotonic tag so a stale connection cannot unregister its successor.
    token: u64,
    events: UnboundedSender<ServerEvent>,
}

struct Inner {
    visible: bool,
    graph_visible: bool,
    ui: Option<Ui>,
    next_ui_token: u64,
    indexing_paused: bool,
    last_query: Option<TimingInfo>,
    /// Last counts read from the index. Cached because `status` is synchronous and
    /// reaching the index is not — the writer thread may be mid-reindex, and blocking the
    /// status call behind a vault walk would make `brainctl status` hang exactly when it
    /// is most likely to be asked.
    counts: IndexStats,
    model: ModelReport,
    /// `(total, good, bad)` provenance rows.
    provenance: (usize, usize, usize),
    stale_corrections: usize,
    /// What was focused at the last summon. Queries that arrive without their own context —
    /// `brainctl ask` from a terminal — use this.
    context: brain_proto::DesktopContext,
}

/// What `brainctl status` says about generation.
///
/// Held here rather than read from the backend on demand so `status` stays synchronous —
/// and so a daemon started with `--mock`, which has no model at all, still has something
/// coherent to report.
#[derive(Debug, Clone, Default)]
pub struct ModelReport {
    pub name: Option<String>,
    pub backend: Option<String>,
    pub state: String,
    pub context_tokens: Option<u32>,
}

pub struct Daemon {
    started: Instant,
    inner: Mutex<Inner>,
}

impl Daemon {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            inner: Mutex::new(Inner {
                visible: false,
                graph_visible: false,
                ui: None,
                next_ui_token: 1,
                indexing_paused: false,
                last_query: None,
                counts: IndexStats::default(),
                model: ModelReport::default(),
                provenance: (0, 0, 0),
                stale_corrections: 0,
                context: brain_proto::DesktopContext::default(),
            }),
        }
    }

    /// Register the UI connection, displacing any previous one.
    ///
    /// Displacing rather than rejecting is the right call: after a dock crash
    /// and restart, the old registration is dead but the daemon cannot know
    /// that until it tries to send. Last writer wins.
    pub fn register_ui(&self, events: UnboundedSender<ServerEvent>) -> u64 {
        let mut inner = self.lock();
        let token = inner.next_ui_token;
        inner.next_ui_token += 1;

        if inner.ui.is_some() {
            tracing::warn!("a second dock connected; replacing the previous UI connection");
        }
        inner.ui = Some(Ui { token, events });

        // A reconnecting dock does not know whether it should be on screen.
        // Tell it, so a daemon restart cannot leave the two disagreeing.
        let visible = inner.visible;
        let graph_visible = inner.graph_visible;
        drop(inner);
        if visible {
            self.send_to_ui(ServerEvent::ShowDock {
                context: Default::default(),
            });
        }
        // Replayed only when open, exactly like `ShowDock`. A dock that restarts with
        // the panel open should come back with it open; a fresh dock already defaults to
        // closed, so saying so would be a redundant event on every single connect.
        if graph_visible {
            self.send_to_ui(ServerEvent::SetGraphVisible { visible: true });
        }

        token
    }

    /// Drop the UI registration, but only if it is still ours.
    pub fn unregister_ui(&self, token: u64) {
        let mut inner = self.lock();
        if inner.ui.as_ref().is_some_and(|ui| ui.token == token) {
            inner.ui = None;
            inner.visible = false;
        }
    }

    /// Returns false when there is no dock listening.
    pub fn send_to_ui(&self, event: ServerEvent) -> bool {
        let inner = self.lock();
        match inner.ui.as_ref() {
            Some(ui) => ui.events.send(event).is_ok(),
            None => false,
        }
    }

    pub fn toggle(&self) -> Visibility {
        let target = !self.lock().visible;
        self.set_visible(target)
    }

    pub fn set_visible(&self, visible: bool) -> Visibility {
        {
            let mut inner = self.lock();
            if inner.visible == visible {
                return Visibility::Unchanged;
            }
            inner.visible = visible;
        }

        let event = if visible {
            // Captured **here**, at the moment visibility flips and before the dock maps.
            // A moment later the dock is itself the active window and the answer would
            // always be "brain-dock" (spec §18).
            let context = capture_context();
            self.lock().context = context.clone();
            ServerEvent::ShowDock { context }
        } else {
            ServerEvent::HideDock
        };

        if !self.send_to_ui(event) {
            tracing::warn!("visibility changed but no dock is connected");
            // Do not roll the flag back: the dock may connect a moment later,
            // and `register_ui` replays the current state to it.
        }

        if visible {
            Visibility::Shown
        } else {
            Visibility::Hidden
        }
    }

    /// Flip the graph panel, and tell the dock. Returns the new state.
    ///
    /// Deliberately independent of dock visibility: the panel is a mode the user is in,
    /// not part of a summon, so dismissing the dock and bringing it back should find the
    /// graph where they left it.
    pub fn toggle_graph(&self) -> bool {
        let visible = {
            let mut inner = self.lock();
            inner.graph_visible = !inner.graph_visible;
            inner.graph_visible
        };

        if !self.send_to_ui(ServerEvent::SetGraphVisible { visible }) {
            tracing::warn!("graph visibility changed but no dock is connected");
        }
        visible
    }

    pub fn set_indexing_paused(&self, paused: bool) {
        self.lock().indexing_paused = paused;
    }

    pub fn record_query(&self, timing: TimingInfo) {
        self.lock().last_query = Some(timing);
    }

    /// Publish fresh index counts, so `status` can stay synchronous.
    pub fn record_counts(&self, counts: IndexStats) {
        self.lock().counts = counts;
    }

    /// Publish what the model is doing, for the same reason.
    pub fn record_model(&self, model: ModelReport) {
        self.lock().model = model;
    }

    /// Publish how much benchmark data has accumulated.
    pub fn record_provenance(&self, counts: (usize, usize, usize)) {
        self.lock().provenance = counts;
    }

    /// Corrections whose source has been rewritten since they were confirmed.
    pub fn record_stale_corrections(&self, count: usize) {
        self.lock().stale_corrections = count;
    }

    /// The context captured at the last summon.
    pub fn context(&self) -> brain_proto::DesktopContext {
        self.lock().context.clone()
    }

    pub fn status(&self) -> StatusReport {
        let inner = self.lock();
        StatusReport {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.started.elapsed().as_secs(),
            llm_model: inner.model.name.clone(),
            llm_backend: inner.model.backend.clone(),
            llm_state: if inner.model.state.is_empty() {
                "not configured".to_string()
            } else {
                inner.model.state.clone()
            },
            llm_context_tokens: inner.model.context_tokens,
            indexed_documents: inner.counts.documents as u64,
            indexed_sections: inner.counts.sections as u64,
            // Stage 5, if the benchmark ever justifies embeddings at all.
            embedding_queue: 0,
            index_generation: inner.counts.generation,
            indexing_paused: inner.indexing_paused,
            ui_connected: inner.ui.is_some(),
            dock_visible: inner.visible,
            last_query: inner.last_query,
            answers_recorded: inner.provenance.0 as u64,
            answers_rated_good: inner.provenance.1 as u64,
            answers_rated_bad: inner.provenance.2 as u64,
            stale_corrections: inner.stale_corrections as u64,
        }
    }

    /// A poisoned lock means a panic happened inside a critical section. The
    /// state here is a handful of plain fields, none of which can be left
    /// half-written, so recovering beats taking the daemon down.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            tracing::error!("daemon state lock was poisoned; recovering");
            poisoned.into_inner()
        })
    }
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn toggle_alternates_and_emits() {
        let daemon = Daemon::new();
        let (tx, mut rx) = unbounded_channel();
        daemon.register_ui(tx);

        assert_eq!(daemon.toggle(), Visibility::Shown);
        assert!(matches!(rx.try_recv(), Ok(ServerEvent::ShowDock { .. })));

        assert_eq!(daemon.toggle(), Visibility::Hidden);
        assert!(matches!(rx.try_recv(), Ok(ServerEvent::HideDock)));
    }

    #[test]
    fn redundant_requests_send_nothing() {
        let daemon = Daemon::new();
        let (tx, mut rx) = unbounded_channel();
        daemon.register_ui(tx);

        assert_eq!(daemon.set_visible(true), Visibility::Shown);
        assert!(rx.try_recv().is_ok());

        // Pressing Show while already shown must not re-trigger the summon
        // animation or steal focus a second time.
        assert_eq!(daemon.set_visible(true), Visibility::Unchanged);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn toggle_without_a_dock_does_not_panic() {
        let daemon = Daemon::new();
        assert_eq!(daemon.toggle(), Visibility::Shown);
        assert!(daemon.status().dock_visible);
        assert!(!daemon.status().ui_connected);
    }

    #[test]
    fn a_reconnecting_dock_is_told_the_current_state() {
        let daemon = Daemon::new();
        let (tx, _rx) = unbounded_channel();
        let token = daemon.register_ui(tx);
        daemon.set_visible(true);

        // Dock crashes.
        daemon.unregister_ui(token);

        // ...and comes back while the daemon still thinks it should be shown.
        daemon.set_visible(true);
        let (tx2, mut rx2) = unbounded_channel();
        daemon.register_ui(tx2);
        assert!(matches!(rx2.try_recv(), Ok(ServerEvent::ShowDock { .. })));
    }

    #[test]
    fn the_graph_panel_toggles_independently_of_the_dock() {
        let daemon = Daemon::new();
        let (tx, mut rx) = unbounded_channel();
        daemon.register_ui(tx);

        assert!(daemon.toggle_graph(), "first toggle opens it");
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerEvent::SetGraphVisible { visible: true })
        ));

        // Dismissing the dock must not close the panel: it is a mode the user is in,
        // not part of a summon.
        daemon.set_visible(true);
        daemon.set_visible(false);
        assert!(daemon.lock().graph_visible, "hiding the dock closed the graph");

        assert!(!daemon.toggle_graph(), "second toggle closes it");
    }

    #[test]
    fn a_reconnecting_dock_is_told_about_an_open_graph_but_not_a_closed_one() {
        let daemon = Daemon::new();
        let (tx, _rx) = unbounded_channel();
        let token = daemon.register_ui(tx);

        // Closed: a fresh dock already defaults to closed, so there is nothing to say.
        let (tx2, mut rx2) = unbounded_channel();
        daemon.register_ui(tx2);
        assert!(rx2.try_recv().is_err());

        daemon.unregister_ui(token);
        daemon.toggle_graph();

        // Open: the dock has no way to know, so it must be told.
        let (tx3, mut rx3) = unbounded_channel();
        daemon.register_ui(tx3);
        assert!(matches!(
            rx3.try_recv(),
            Ok(ServerEvent::SetGraphVisible { visible: true })
        ));
    }

    #[test]
    fn a_stale_connection_cannot_unregister_its_replacement() {
        let daemon = Daemon::new();
        let (tx1, _rx1) = unbounded_channel();
        let stale = daemon.register_ui(tx1);

        let (tx2, _rx2) = unbounded_channel();
        daemon.register_ui(tx2);

        daemon.unregister_ui(stale);
        assert!(daemon.status().ui_connected, "the live dock was evicted");
    }
}
