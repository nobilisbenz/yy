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
use brain_engine::llm::{Chunk, Llm, ModelState};
use brain_engine::desktop::DesktopIndex;
use brain_engine::store::{Rating, Store};
use brain_engine::{Ranked, actions, prompt, store};
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

/// Tokens reserved for the system block and chat scaffolding.
///
/// [`brain_engine::prompt::SYSTEM`] is around 200 tokens; the rest is slack so a prompt
/// never overflows and gets truncated **from the left**, which would remove the system
/// block and look exactly like the model ignoring its instructions.
const SCAFFOLD_TOKENS: usize = 320;

pub struct Backend {
    config: Config,
    index: Index,
    /// Generation. `None` is a normal, supported state — the daemon is a working lexical
    /// search engine without it, which is the whole point of Stage 1 shipping first.
    llm: Llm,
    /// The answer cache and provenance rows. `None` when the store could not be opened —
    /// a degraded but working daemon, since both are accelerations, not requirements.
    store: Option<Store>,
    /// Actions by query, most recent last.
    recent: Mutex<VecDeque<(Uuid, Vec<Action>)>>,
    /// Provenance row id per query, so a rating keystroke can find what it refers to.
    provenance: Mutex<VecDeque<(Uuid, i64)>>,
    /// Installed applications, for resolving `@app`. Scanned once at startup: the set
    /// changes when something is installed, which is rare next to how often it is read.
    apps: DesktopIndex,
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

        let llm = Llm::new(&config.llm);

        // A store that cannot be opened costs the cache and the benchmark data, not the
        // product. Saying so and carrying on beats refusing to start.
        let store = Store::default_path()
            .and_then(|path| match Store::open(&path) {
                Ok(store) => {
                    tracing::info!(path = %path.display(), "store ready");
                    Some(store)
                }
                Err(error) => {
                    tracing::warn!(%error, "continuing without the answer cache or provenance");
                    None
                }
            });

        Ok(Self {
            config,
            index,
            llm,
            apps: DesktopIndex::scan(),
            store,
            recent: Mutex::new(VecDeque::new()),
            provenance: Mutex::new(VecDeque::new()),
            _watcher: watcher,
        })
    }

    /// Load the model, then keep it running for the life of the daemon.
    ///
    /// Called in the background at startup, never on the query path: the model loads at
    /// daemon start and stays resident (spec §37). A failure leaves the daemon in
    /// lexical-only mode rather than taking it down.
    pub async fn start_model(&self) {
        self.llm.supervise().await;
    }

    /// What to show in `brainctl status`.
    pub fn model_report(&self) -> crate::state::ModelReport {
        crate::state::ModelReport {
            name: self.llm.model_name(),
            backend: Some(self.config.llm.backend.clone()),
            state: self.llm.state().as_str().to_string(),
            context_tokens: Some(self.llm.context_tokens() as u32),
        }
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
        retrieval_only: bool,
        context: brain_proto::DesktopContext,
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

        // Context boosts, never filters (spec §18). Applied after fusion so it reorders a
        // full result set rather than shrinking it — asking something unrelated while
        // Blender is focused must still return sensible results.
        if self.config.context.enabled && !context.is_suppressed() {
            brain_engine::apply_context(
                &self.config.search,
                &context,
                &self.config.context.aliases,
                &text,
                &mut retrieval.results,
            );
        }

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

        // `@action` rows declared on the retrieved sections. A failure here costs the extra
        // buttons, not the answer, so the implicit note action still stands.
        let uids: Vec<String> = retrieval
            .results
            .iter()
            .map(|entry| entry.hit.section_uid.clone())
            .collect();
        let declared = self
            .index
            .read(move |database| Ok(database.actions_for(&uids)?))
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "could not read declared actions");
                Vec::new()
            });

        let built = actions::for_results(&retrieval.results, &declared, &self.apps);
        let views = built.iter().map(Action::view).collect();
        self.remember(id, built);

        let _ = events.send(ServerEvent::Sources { id, items: sources });
        let _ = events.send(ServerEvent::Actions { id, items: views });

        if self.config.logging.log_queries {
            tracing::info!(query = %text, results = retrieval.results.len(), "answered");
        }

        // Search-as-you-type asks for retrieval only. Generating for a half-typed question
        // would burn the GPU on prose nobody will read and hold the slot the real query
        // needs a moment later.
        let generation = if retrieval_only {
            None
        } else {
            self.generate(id, &text, &retrieval.results, &events, &cancel)
                .await
        };

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

        let (prompt_ms, ttft_ms, output_tokens) = generation.unwrap_or_default();
        let timing = TimingInfo {
            retrieval_ms,
            prompt_ms,
            ttft_ms,
            generation_ms: total_ms.saturating_sub(ttft_ms.max(retrieval_ms)),
            output_tokens,
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

    /// Generate the prose half, streaming tokens as they arrive.
    ///
    /// Returns `(prompt_ms, ttft_ms, output_tokens)`, or `None` when nothing was generated —
    /// which is an ordinary outcome, not a failure. Every degradation path in
    /// `plan/03-stage-2-llm.md` §2.6 lands here, and all of them leave the sources and
    /// actions already on screen: the dock must never become unusable because inference
    /// broke.
    async fn generate(
        &self,
        id: Uuid,
        question: &str,
        results: &[Ranked],
        events: &UnboundedSender<ServerEvent>,
        cancel: &CancellationToken,
    ) -> Option<(u32, u32, u32)> {
        match self.llm.state() {
            ModelState::Loaded => {}
            ModelState::Loading => {
                tracing::debug!("model still loading; answering with sources only");
                return None;
            }
            ModelState::Disabled | ModelState::Failed => return None,
        }

        // The no-answer decision is made **here**, from retrieval confidence, before the
        // model is called — never by asking a 1.7B model whether its own context answers
        // the question, which it will say yes to far too often (spec §45).
        if !prompt::is_confident(results, self.config.search.min_confidence) {
            tracing::debug!("retrieval below the confidence threshold; not calling the model");
            let _ = events.send(ServerEvent::NoAnswer {
                id,
                closest: results.iter().take(3).map(source_ref).collect(),
            });
            return None;
        }

        let prompt_started = Instant::now();
        let budget = self
            .llm
            .context_tokens()
            .saturating_sub(self.config.llm.max_output_tokens)
            .saturating_sub(SCAFFOLD_TOKENS);

        let mut pack = prompt::build(
            question,
            &results[..results.len().min(self.config.search.context_sections)],
            budget,
        );

        // Count with the server's own tokenizer rather than trusting the estimate. An
        // overflowing prompt is truncated from the left, taking the system block with it.
        if let Ok(counted) = self.llm.count_tokens(&pack.user).await
            && counted > budget
        {
            tracing::debug!(counted, budget, "prompt over budget; packing fewer sources");
            let fewer = results.len().min(self.config.search.context_sections).max(2) - 1;
            pack = prompt::build(question, &results[..fewer], budget);
        }

        let prompt_ms = prompt_started.elapsed().as_millis() as u32;

        // Record what was retrieved before generating, so the row exists even if generation
        // fails — the benchmark cares about the retrieved set, not the prose.
        let packed: Vec<(String, String)> = results
            .iter()
            .take(pack.sources_used)
            .map(|entry| (entry.hit.section_uid.clone(), entry.hit.body.clone()))
            .collect();

        if let Some(store) = &self.store {
            let uids: Vec<String> = packed.iter().map(|(uid, _)| uid.clone()).collect();
            match store.record(question, &uids, self.llm.model_name().as_deref()) {
                Ok(row) => self.remember_provenance(id, row),
                Err(error) => tracing::warn!(%error, "could not record provenance"),
            }
        }

        let key = store::answer_key(
            &packed,
            self.llm.model_name().as_deref(),
            prompt::PROMPT_VERSION,
            self.config.llm.max_output_tokens,
            self.config.llm.temperature,
        );

        // A cache hit renders **immediately**, not replayed token by token. Fake-streaming
        // a cached answer is a lie that costs exactly as long as the lie is convincing.
        if let Some(store) = &self.store
            && let Ok(Some(answer)) = store.cached_answer(&key)
        {
            tracing::debug!("answer cache hit");
            let _ = events.send(ServerEvent::GenerationStarted { id });
            let _ = events.send(ServerEvent::Token { id, text: answer });
            return Some((prompt_ms, 0, 0));
        }

        let _ = events.send(ServerEvent::GenerationStarted { id });

        let generation_started = Instant::now();
        let mut ttft_ms = 0;
        let mut output_tokens = 0;
        let mut answer = String::new();

        let outcome = self
            .llm
            .generate(&pack, cancel, |chunk| match chunk {
                Chunk::Token(text) => {
                    if ttft_ms == 0 {
                        ttft_ms = generation_started.elapsed().as_millis() as u32;
                    }
                    answer.push_str(&text);
                    let _ = events.send(ServerEvent::Token { id, text });
                }
                Chunk::Done { output_tokens: n } => output_tokens = n,
            })
            .await;

        if let Err(error) = outcome {
            tracing::warn!(%error, "generation failed; the sources are still on screen");
            // Deliberately not a `ServerEvent::Error`: the query did produce a useful
            // result, and painting the card red would say otherwise.
            return None;
        }

        // Only cache a completed answer. A cancelled or truncated one would be served in
        // full next time, which is worse than regenerating it.
        if let Some(store) = &self.store
            && !answer.trim().is_empty()
            && !cancel.is_cancelled()
            && let Err(error) = store.store_answer(&key, &answer, self.llm.model_name().as_deref())
        {
            tracing::warn!(%error, "could not cache the answer");
        }

        tracing::info!(ttft_ms, output_tokens, "generated");
        Some((prompt_ms, ttft_ms, output_tokens))
    }

    fn remember_provenance(&self, query: Uuid, row: i64) {
        let mut provenance = self
            .provenance
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        provenance.push_back((query, row));
        while provenance.len() > REMEMBERED_QUERIES {
            provenance.pop_front();
        }
    }

    /// Mark the answer to a query good or bad.
    ///
    /// One keystroke in the dock, and after a fortnight of ordinary use it is a labelled
    /// retrieval benchmark built from the questions actually asked — which is strictly
    /// better data than a set invented in one sitting (`PLAN.md` §6.3).
    pub fn rate(&self, query: Uuid, rating: Rating) -> Result<()> {
        let row = {
            let provenance = self
                .provenance
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            provenance
                .iter()
                .find(|(id, _)| *id == query)
                .map(|(_, row)| *row)
        };

        let Some(row) = row else {
            // The daemon restarted, or the query aged out. Not worth an error.
            tracing::debug!(%query, "rated a query the daemon no longer remembers");
            return Ok(());
        };
        let Some(store) = &self.store else {
            return Ok(());
        };

        store.rate(row, rating)?;
        tracing::info!(?rating, "answer rated");
        Ok(())
    }

    /// `(total, good, bad)` provenance rows, for `brainctl status`.
    pub fn provenance_counts(&self) -> (usize, usize, usize) {
        self.store
            .as_ref()
            .and_then(|store| store.counts().ok())
            .unwrap_or_default()
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
            .query(
                id,
                "crop target".into(),
                true,
                Default::default(),
                tx,
                CancellationToken::new(),
            )
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
                true,
                Default::default(),
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
                true,
                Default::default(),
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
                .query(
                    id,
                    "crop".into(),
                    true,
                    Default::default(),
                    tx,
                    CancellationToken::new(),
                )
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
