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

use brain_proto::{ServerEvent, StatusReport, TimingInfo};
use tokio::sync::mpsc::UnboundedSender;

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
    ui: Option<Ui>,
    next_ui_token: u64,
    indexing_paused: bool,
    last_query: Option<TimingInfo>,
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
                ui: None,
                next_ui_token: 1,
                indexing_paused: false,
                last_query: None,
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
        drop(inner);
        if visible {
            self.send_to_ui(ServerEvent::ShowDock {
                context: Default::default(),
            });
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
            // Desktop context is captured here in Stage 4; until then the dock
            // gets an empty struct rather than a missing field.
            ServerEvent::ShowDock {
                context: Default::default(),
            }
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

    pub fn set_indexing_paused(&self, paused: bool) {
        self.lock().indexing_paused = paused;
    }

    /// Wired up in Stage 2, when there is a query pipeline to time.
    #[allow(dead_code)]
    pub fn record_query(&self, timing: TimingInfo) {
        self.lock().last_query = Some(timing);
    }

    pub fn status(&self) -> StatusReport {
        let inner = self.lock();
        StatusReport {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.started.elapsed().as_secs(),
            // Populated in Stage 2.
            llm_model: None,
            llm_backend: None,
            llm_state: "not configured".to_string(),
            llm_context_tokens: None,
            // Populated in Stage 1.
            indexed_documents: 0,
            indexed_sections: 0,
            embedding_queue: 0,
            index_generation: 0,
            indexing_paused: inner.indexing_paused,
            ui_connected: inner.ui.is_some(),
            dock_visible: inner.visible,
            last_query: inner.last_query,
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
