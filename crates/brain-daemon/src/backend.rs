//! The real query pipeline: retrieval, actions, and activation.
//!
//! What `mock.rs` stands in for. The event order is deliberate and matches the mock's, so
//! the dock cannot tell the difference apart from the content: sources and actions are
//! emitted **as soon as retrieval finishes**, before any generation, because retrieval
//! takes ~100 ms and generation takes an order of magnitude longer (`09-decisions.md` §3).
//! Frequently the buttons were all the user needed.
//!
//! Actions are remembered per query so that activation sends an *id*, never a command. The
//! daemon resolves that id against actions it built from parsed vault metadata, which is
//! what makes spec §48 structural rather than a rule someone has to remember.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context as _, Result};
use brain_core::{ActionId, Config, SectionId};
use brain_engine::actions::{Action, ActionError};
use brain_engine::{Ranked, actions};
use brain_index::{Index, IndexStats, Watcher};
use brain_proto::{CacheStatus, ServerEvent, SourceRef, TimingInfo};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How many queries' actions stay resolvable.
///
/// Activation follows the query that produced it by at most a few seconds, so this only has
/// to outlive the user reaching for `Alt+1`. It is a bound on memory, not a feature.
const REMEMBERED_QUERIES: usize = 8;

/// Characters of body text sent as a preview.
const SNIPPET: usize = 240;

pub struct Backend {
    config: Config,
    index: Index,
    /// Actions by query, most recent last.
    recent: Mutex<VecDeque<(Uuid, Vec<Action>)>>,
    /// Held so the watch lives as long as the daemon. Dropping it stops the watch.
    _watcher: Option<Watcher>,
}

impl Backend {
    /// Open the configured vault and start watching it.
    ///
    /// Does **not** index: startup must not block on walking a vault, because the dock has
    /// to be summonable immediately (spec §3.1). The initial reindex is kicked off in the
    /// background by the caller.
    pub fn open(config: Config) -> Result<Self> {
        let vaults: Vec<_> = config.vaults().collect();
        let vault = vaults
            .first()
            .context("no vault [[sources]] configured; nothing to search")?;

        if vaults.len() > 1 {
            // Merging BM25 scores across separate indices is not meaningful, and the
            // cross-source fusion that makes it meaningful is the non-vault superset work
            // in PLAN.md §2.2 that is not built yet. Saying so beats silently searching one.
            tracing::warn!(
                using = %vault.name,
                ignored = vaults.len() - 1,
                "multiple vault sources are configured; only the first is searched for now"
            );
        }

        let index = Index::open(&vault.path)?;
        let debounce = std::time::Duration::from_millis(config.indexing.debounce_ms);
        let watcher = match Watcher::spawn(index.clone(), debounce) {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                // A vault that is not watched is stale, not broken. Degrading to
                // reindex-on-demand beats refusing to start.
                tracing::warn!(%error, "continuing without a file watcher; use `brainctl reindex`");
                None
            }
        };

        Ok(Self {
            config,
            index,
            recent: Mutex::new(VecDeque::new()),
            _watcher: watcher,
        })
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub async fn reindex(&self) -> Result<IndexStats> {
        Ok(self.index.reindex().await?)
    }

    pub async fn stats(&self) -> Result<IndexStats> {
        Ok(self.index.stats().await?)
    }

    /// Answer one query.
    ///
    /// Stage 1 stops after sources and actions; there is no generation yet, which is
    /// deliberate — the plan's own framing is that Stage 1 has to earn its keep with no
    /// model at all.
    ///
    /// Returns what it cost, so `brainctl status` reports a measured last query rather
    /// than a placeholder. `None` means the query was cancelled or failed.
    pub async fn query(
        &self,
        id: Uuid,
        text: String,
        events: UnboundedSender<ServerEvent>,
        cancel: CancellationToken,
    ) -> Option<TimingInfo> {
        let span = tracing::info_span!("query", %id);
        let _entered = span.enter();
        let started = Instant::now();

        let _ = events.send(ServerEvent::QueryAccepted { id });
        let _ = events.send(ServerEvent::RetrievalStarted { id });

        let retrieval = tokio::select! {
            outcome = brain_engine::retrieve(&self.index, &self.config.search, &text) => outcome,
            _ = cancel.cancelled() => {
                tracing::debug!("cancelled during retrieval");
                return None;
            }
        };

        let mut retrieval = match retrieval {
            Ok(retrieval) => retrieval,
            Err(error) => {
                tracing::warn!(%error, "retrieval failed");
                let _ = events.send(ServerEvent::Error {
                    id: Some(id),
                    message: error.to_string(),
                });
                return None;
            }
        };

        brain_engine::boost_heading_matches(&self.config.search, &text, &mut retrieval.results);

        let retrieval_ms = started.elapsed().as_millis() as u32;
        let _ = events.send(ServerEvent::RetrievalComplete {
            id,
            source_count: retrieval.results.len(),
        });

        if retrieval.results.is_empty() {
            // A confident "not in your files" is a feature, not an error (spec §45).
            let timing = TimingInfo {
                retrieval_ms,
                total_ms: started.elapsed().as_millis() as u32,
                ..Default::default()
            };
            let _ = events.send(ServerEvent::NoAnswer {
                id,
                closest: Vec::new(),
            });
            let _ = events.send(ServerEvent::Complete {
                id,
                timing,
                cache: CacheStatus::default(),
            });
            return Some(timing);
        }

        let sources: Vec<SourceRef> = retrieval.results.iter().map(source_ref).collect();
        let built = actions::for_results(&retrieval.results);
        let views = built.iter().map(Action::view).collect();
        self.remember(id, built);

        let _ = events.send(ServerEvent::Sources { id, items: sources });
        let _ = events.send(ServerEvent::Actions { id, items: views });

        if self.config.logging.log_queries {
            tracing::info!(query = %text, results = retrieval.results.len(), "answered");
        }

        let total_ms = started.elapsed().as_millis() as u32;
        tracing::debug!(
            seeds = retrieval.seed_count,
            returned = retrieval.results.len(),
            seed_ms = retrieval.timing.seed_ms,
            expand_ms = retrieval.timing.expand_ms,
            rank_ms = retrieval.timing.rank_ms,
            total_ms,
            "query complete"
        );

        let timing = TimingInfo {
            retrieval_ms,
            total_ms,
            ..Default::default()
        };
        let _ = events.send(ServerEvent::Complete {
            id,
            timing,
            cache: CacheStatus::default(),
        });
        Some(timing)
    }

    fn remember(&self, id: Uuid, built: Vec<Action>) {
        let mut recent = self
            .recent
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        recent.push_back((id, built));
        while recent.len() > REMEMBERED_QUERIES {
            recent.pop_front();
        }
    }

    /// Run an action the daemon previously offered.
    ///
    /// The dock hides itself before this is called, not after: spawning first means the
    /// dock is still on screen while the editor maps, which reads as a stutter.
    pub fn activate(&self, query: Uuid, action: ActionId) -> Result<(), ActionError> {
        let target = {
            let recent = self
                .recent
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            recent
                .iter()
                .find(|(id, _)| *id == query)
                .and_then(|(_, actions)| actions.iter().find(|candidate| candidate.id == action))
                .map(|found| found.target.clone())
                .ok_or(ActionError::Unknown(action))?
        };

        tracing::debug!(?target, "activating");
        actions::activate(&self.config.openers, &target)
    }
}

fn source_ref(ranked: &Ranked) -> SourceRef {
    let hit = &ranked.hit;
    SourceRef {
        // `yy` keys on `section_uid`; the numeric id is not meaningful across databases,
        // so it is left as a placeholder rather than invented.
        section_id: SectionId(0),
        section_uid: hit.section_uid.clone(),
        path: hit.path.clone(),
        heading_path: hit.heading_path.clone(),
        start_line: hit.start_line as u32,
        end_line: hit.start_line as u32,
        score: ranked.score,
        snippet: snippet(&hit.body),
        explain: ranked.explain.describe(),
    }
}

/// First lines of a section, cut on a character boundary.
fn snippet(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= SNIPPET {
        return trimmed.to_string();
    }
    let cut = trimmed
        .char_indices()
        .nth(SNIPPET)
        .map(|(index, _)| index)
        .unwrap_or(trimmed.len());
    format!("{}…", trimmed[..cut].trim_end())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn config_for(vault: &std::path::Path) -> Config {
        Config {
            sources: vec![brain_core::Source {
                name: "notes".into(),
                path: vault.to_path_buf(),
                include: vec!["**/*.md".into()],
                exclude: vec!["**/.notes/**".into()],
                vault: true,
            }],
            ..Config::default()
        }
    }

    async fn backend() -> (tempfile::TempDir, Backend) {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("obs.md"),
            "---\nid: obs\ntitle: OBS\n---\n# OBS {#root}\n\
             ## Cursor follow {#follow}\nSmooth the crop target each frame.\n",
        )
        .unwrap();
        let backend = Backend::open(config_for(dir.path())).unwrap();
        backend.reindex().await.unwrap();
        (dir, backend)
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServerEvent>) -> Vec<ServerEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn a_query_emits_sources_and_actions_before_it_completes() {
        let (_dir, backend) = backend().await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();

        backend
            .query(id, "crop target".into(), tx, CancellationToken::new())
            .await;

        let events = drain(&mut rx);
        let position = |name: &str| {
            events.iter().position(|event| match (event, name) {
                (ServerEvent::Sources { .. }, "sources") => true,
                (ServerEvent::Actions { .. }, "actions") => true,
                (ServerEvent::Complete { .. }, "complete") => true,
                _ => false,
            })
        };

        let sources = position("sources").expect("no sources were sent");
        let actions = position("actions").expect("no actions were sent");
        let complete = position("complete").expect("the query never completed");
        assert!(sources < complete && actions < complete, "{events:#?}");
    }

    #[tokio::test]
    async fn a_source_carries_the_uid_line_and_explanation_the_dock_needs() {
        let (_dir, backend) = backend().await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        backend
            .query(
                Uuid::new_v4(),
                "crop target".into(),
                tx,
                CancellationToken::new(),
            )
            .await;

        let items = drain(&mut rx)
            .into_iter()
            .find_map(|event| match event {
                ServerEvent::Sources { items, .. } => Some(items),
                _ => None,
            })
            .expect("no sources");

        let follow = items
            .iter()
            .find(|item| item.section_uid == "obs#follow")
            .expect("the matching section is missing");
        assert_eq!(follow.heading_path, "OBS > Cursor follow");
        assert!(follow.start_line > 0, "Alt+1 needs a real line number");
        assert!(!follow.explain.is_empty(), "no explanation for the result");
        assert!(!follow.snippet.is_empty());
    }

    #[tokio::test]
    async fn a_query_matching_nothing_reports_no_answer_rather_than_an_error() {
        let (_dir, backend) = backend().await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        backend
            .query(
                Uuid::new_v4(),
                "zzzznotinthevault".into(),
                tx,
                CancellationToken::new(),
            )
            .await;

        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ServerEvent::NoAnswer { .. })),
            "{events:#?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ServerEvent::Error { .. })),
            "an empty result set was reported as an error"
        );
    }

    #[tokio::test]
    async fn an_action_id_from_an_unknown_query_is_refused() {
        // The guarantee: activation resolves an id against actions the daemon itself
        // built. An id that was never offered has nothing to resolve to.
        let (_dir, backend) = backend().await;
        assert!(matches!(
            backend.activate(Uuid::new_v4(), ActionId(1)),
            Err(ActionError::Unknown(_))
        ));
    }

    #[tokio::test]
    async fn only_the_most_recent_queries_stay_activatable() {
        let (_dir, backend) = backend().await;
        let mut ids = Vec::new();
        for _ in 0..REMEMBERED_QUERIES + 2 {
            let id = Uuid::new_v4();
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            backend
                .query(id, "crop".into(), tx, CancellationToken::new())
                .await;
            ids.push(id);
        }

        let recent = backend.recent.lock().unwrap();
        assert_eq!(recent.len(), REMEMBERED_QUERIES);
        assert!(recent.iter().all(|(id, _)| ids[2..].contains(id)));
    }

    #[test]
    fn a_snippet_is_cut_on_a_character_boundary() {
        // Slicing bytes through a multi-byte character panics, and note bodies are full of
        // typographic quotes and accented words.
        let body = "ö".repeat(SNIPPET * 2);
        let cut = snippet(&body);
        assert!(cut.ends_with('…'));
        assert!(cut.chars().count() <= SNIPPET + 1);
    }
}
