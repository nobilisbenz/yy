//! Reaching the index from async code without stalling the runtime.
//!
//! `yalive::db::Database` is one `rusqlite::Connection`. That is not `Sync`, it is not
//! async, and a full reindex inside it runs for seconds. Calling it directly from a Tokio
//! worker blocks that worker — during a reindex of a large vault, the daemon stops
//! answering the socket, which the user experiences as the dock hanging on summon.
//!
//! So (Stage 1 §1.3):
//!
//! - **One writer**, on a dedicated thread that owns the mutable `Database`. SQLite allows
//!   one writer anyway; funnelling every write through one owner removes every
//!   `SQLITE_BUSY` question rather than answering it with a retry loop.
//! - **N readers**, each its own connection, checked out of an elastic pool. WAL means
//!   they never block on the writer.
//! - **Everything through `spawn_blocking`.** rusqlite is synchronous; there is no way to
//!   make a query await, so it belongs on the blocking pool.
//!
//! The graph is the other half. Building adjacency and running PageRank costs O(sections)
//! per call, which cannot happen once per keystroke on the interactive path. The writer
//! rebuilds it after each reindex and publishes it as an immutable snapshot; readers clone
//! an `Arc`. `PLAN.md` §3.2: the interactive path reads precomputed analytics, never
//! computes them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use yalive::db::Database;
use yalive::graph::Graph;

use crate::{Hit, IndexError, Result, fts};

/// Idle read connections kept alive between queries.
///
/// The pool is elastic: a checkout when it is empty opens a connection rather than waiting,
/// and a return when it is full drops one rather than growing without bound. Waiting would
/// mean a queue on the interactive path, and the bound that actually matters is already
/// imposed by Tokio's blocking pool.
const MAX_IDLE_READERS: usize = 4;

/// What the writer thread accepts. Every variant carries its own reply channel, so a caller
/// that goes away mid-reindex just drops the receiver and the writer notices on send.
enum Command {
    Reindex(oneshot::Sender<Result<IndexStats>>),
    Stats(oneshot::Sender<Result<IndexStats>>),
    Shutdown,
}

/// A snapshot of what the index currently holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub documents: usize,
    pub sections: usize,
    pub relations: usize,
    /// Bumped on every write. The cache-invalidation counter from spec §36 — anything
    /// derived from the index (a graph snapshot, a cached ranking) is stale when this moves.
    pub generation: u64,
}

struct Inner {
    vault: PathBuf,
    writer: std::sync::mpsc::Sender<Command>,
    readers: Mutex<Vec<Database>>,
    /// Published by the writer, cloned by readers. Never rebuilt on the query path.
    graph: Mutex<Arc<Graph>>,
    generation: AtomicU64,
    /// Set by `brainctl pause-indexing`, checked before a reindex starts — useful when a
    /// `cargo build` is churning a source directory.
    paused: std::sync::atomic::AtomicBool,
}

/// A handle to one vault's index. Cheap to clone; every clone talks to the same writer.
#[derive(Clone)]
pub struct Index {
    inner: Arc<Inner>,
}

impl Index {
    /// Open the vault and start the writer thread.
    ///
    /// Does not index. Call [`Index::reindex`] for that — startup should not block on
    /// walking a vault, because the dock has to be summonable immediately.
    pub fn open(vault: impl AsRef<Path>) -> Result<Self> {
        let vault = vault.as_ref().to_path_buf();

        // Open once here so a broken vault path fails at startup with a clear error rather
        // than on the first query.
        let database = Database::open(&vault)?;
        let graph = Arc::new(Graph::new(&database.graph()?));

        let (sender, receiver) = std::sync::mpsc::channel();
        let inner = Arc::new(Inner {
            vault: vault.clone(),
            writer: sender,
            readers: Mutex::new(Vec::new()),
            graph: Mutex::new(graph),
            generation: AtomicU64::new(0),
            paused: std::sync::atomic::AtomicBool::new(false),
        });

        let writer_inner = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("brain-index-writer".into())
            .spawn(move || writer_loop(database, vault, receiver, writer_inner))
            .map_err(|source| IndexError::Writer(source.to_string()))?;

        Ok(Self { inner })
    }

    pub fn vault(&self) -> &Path {
        &self.inner.vault
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    pub fn set_paused(&self, paused: bool) {
        self.inner.paused.store(paused, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::Acquire)
    }

    /// The current graph snapshot. An `Arc` clone, not a rebuild.
    pub fn graph(&self) -> Arc<Graph> {
        Arc::clone(&self.inner.graph.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Walk the vault and bring the index up to date.
    ///
    /// Runs on the writer thread; the caller only awaits the reply. A reindex already in
    /// flight is not interrupted — commands queue, so two concurrent callers serialise
    /// rather than corrupting each other.
    pub async fn reindex(&self) -> Result<IndexStats> {
        self.ask(Command::Reindex).await
    }

    pub async fn stats(&self) -> Result<IndexStats> {
        self.ask(Command::Stats).await
    }

    async fn ask<F>(&self, build: F) -> Result<IndexStats>
    where
        F: FnOnce(oneshot::Sender<Result<IndexStats>>) -> Command,
    {
        let (reply, answer) = oneshot::channel();
        self.inner
            .writer
            .send(build(reply))
            .map_err(|_| IndexError::Writer("the index writer thread has stopped".into()))?;
        answer
            .await
            .map_err(|_| IndexError::Writer("the index writer dropped the reply".into()))?
    }

    /// Lexical search, off the async runtime.
    ///
    /// `weights` come from config so the Stage 7 sweep can move them without a rebuild.
    pub async fn search(
        &self,
        query: String,
        weights: fts::Bm25Weights,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let reader = inner.checkout()?;
            let hits = reader.search(&query, weights, limit, &inner.vault);
            inner.checkin(reader);
            hits
        })
        .await
        .map_err(|source| IndexError::Writer(format!("the search task panicked: {source}")))?
    }

    /// Run an arbitrary read against a pooled connection, off the async runtime.
    ///
    /// The escape hatch for reads this type has no named method for. It hands out a
    /// `&Database` rather than a connection, so callers still cannot write through it.
    pub async fn read<T, F>(&self, work: F) -> Result<T>
    where
        F: FnOnce(&Database) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let reader = inner.checkout()?;
            let outcome = work(&reader.database);
            inner.checkin(reader);
            outcome
        })
        .await
        .map_err(|source| IndexError::Writer(format!("the read task panicked: {source}")))?
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.writer.send(Command::Shutdown);
    }
}

impl Inner {
    fn checkout(&self) -> Result<Reader> {
        let pooled = self
            .readers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop();

        match pooled {
            Some(database) => Ok(Reader { database }),
            // Opening is a few milliseconds and only happens under concurrency the pool has
            // not seen before. Waiting for a free connection instead would put a queue on
            // the interactive path.
            None => Ok(Reader {
                database: Database::open(&self.vault)?,
            }),
        }
    }

    fn checkin(&self, reader: Reader) {
        let mut readers = self
            .readers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if readers.len() < MAX_IDLE_READERS {
            readers.push(reader.database);
        }
    }
}

/// A checked-out read connection.
struct Reader {
    database: Database,
}

impl Reader {
    fn search(
        &self,
        query: &str,
        weights: fts::Bm25Weights,
        limit: usize,
        vault: &Path,
    ) -> Result<Vec<Hit>> {
        // `None` means the query has no searchable token in it. That is an empty result,
        // not an error and not the whole vault.
        let Some(expression) = fts::expression_with(query, fts::Mode::Any) else {
            return Ok(Vec::new());
        };
        let rows = self.database.search_expression(&expression, weights, limit)?;
        Ok(rows
            .into_iter()
            .map(|row| Hit::from_row(row, vault))
            .collect())
    }
}

fn writer_loop(
    mut database: Database,
    vault: PathBuf,
    receiver: std::sync::mpsc::Receiver<Command>,
    inner: Arc<Inner>,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Shutdown => break,

            Command::Stats(reply) => {
                let _ = reply.send(read_stats(&database, &inner));
            }

            Command::Reindex(reply) => {
                if inner.paused.load(Ordering::Acquire) {
                    tracing::debug!("indexing is paused; reporting current counts unchanged");
                    let _ = reply.send(read_stats(&database, &inner));
                    continue;
                }

                let span = tracing::info_span!("reindex", vault = %vault.display());
                let _entered = span.enter();
                let started = std::time::Instant::now();

                let outcome = database.index_vault(&vault).map_err(IndexError::from);
                let result = match outcome {
                    Err(error) => Err(error),
                    Ok(summary) => {
                        inner.generation.fetch_add(1, Ordering::AcqRel);

                        // Republish the graph so the query path never rebuilds it. If this
                        // fails the old snapshot stays, which is stale but usable — losing
                        // graph expansion beats failing the reindex that succeeded.
                        match database.graph() {
                            Ok(data) => {
                                let rebuilt = Arc::new(Graph::new(&data));
                                *inner
                                    .graph
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = rebuilt;
                            }
                            Err(error) => {
                                tracing::warn!(%error, "kept the previous graph snapshot");
                            }
                        }

                        tracing::info!(
                            indexed = summary.indexed,
                            unchanged = summary.unchanged,
                            removed = summary.removed,
                            failed = summary.failed,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "reindexed"
                        );
                        read_stats(&database, &inner)
                    }
                };
                let _ = reply.send(result);
            }
        }
    }
    tracing::debug!("index writer thread stopped");
}

fn read_stats(database: &Database, inner: &Inner) -> Result<IndexStats> {
    let counts = database.counts()?;
    Ok(IndexStats {
        documents: counts.documents,
        sections: counts.sections,
        relations: counts.relations,
        generation: inner.generation.load(Ordering::Acquire),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn vault_with(notes: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        for (name, body) in notes {
            fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    fn sample() -> tempfile::TempDir {
        vault_with(&[(
            "obs.md",
            "---\nid: obs\ntitle: OBS\n---\n# OBS {#root}\n\
             ## Cursor follow {#follow}\nSmooth the crop target.\n\
             ## Old approach {#old}\ncontradicts:: [[obs#follow]]\nMove it directly.\n",
        )])
    }

    #[tokio::test]
    async fn reindexing_reports_counts_and_bumps_the_generation() {
        let dir = sample();
        let index = Index::open(dir.path()).unwrap();
        assert_eq!(index.generation(), 0);

        let stats = index.reindex().await.unwrap();
        assert_eq!(stats.documents, 1);
        assert_eq!(stats.sections, 3);
        assert_eq!(stats.generation, 1);
        assert_eq!(index.generation(), 1);
    }

    #[tokio::test]
    async fn search_runs_against_the_reindexed_content() {
        let dir = sample();
        let index = Index::open(dir.path()).unwrap();
        index.reindex().await.unwrap();

        let hits = index
            .search("crop target".into(), fts::Bm25Weights::default(), 10)
            .await
            .unwrap();
        assert!(hits.iter().any(|hit| hit.section_uid == "obs#follow"));
    }

    /// A hit's path must be openable, not merely printable.
    ///
    /// `yalive` stores `files.path` relative to the vault. Everything downstream — the
    /// `exists()` check that decides whether a button is enabled, and the argv handed to
    /// nvim — treats it as a path to open, and a relative one resolves against the daemon's
    /// working directory. The symptom was every `[Note]` button rendering disabled.
    #[tokio::test]
    async fn a_hit_carries_a_path_that_can_actually_be_opened() {
        let dir = sample();
        let index = Index::open(dir.path()).unwrap();
        index.reindex().await.unwrap();

        let hits = index
            .search("crop".into(), fts::Bm25Weights::default(), 5)
            .await
            .unwrap();
        let hit = hits.first().expect("no hits");

        assert!(hit.path.is_absolute(), "{} is relative", hit.path.display());
        assert!(hit.path.exists(), "{} does not exist", hit.path.display());
    }

    #[tokio::test]
    async fn the_graph_snapshot_is_republished_by_the_writer() {
        let dir = sample();
        let index = Index::open(dir.path()).unwrap();
        // Before indexing there is nothing in the graph, and asking for it must not be a
        // rebuild — the query path calls this per keystroke.
        assert!(index.graph().is_empty());

        index.reindex().await.unwrap();
        let graph = index.graph();
        assert_eq!(graph.contradictions().len(), 1, "the writer republished it");
    }

    #[tokio::test]
    async fn concurrent_searches_do_not_serialise_behind_one_connection() {
        let dir = sample();
        let index = Index::open(dir.path()).unwrap();
        index.reindex().await.unwrap();

        // The point of the pool. With a single shared connection this either deadlocks or
        // queues; with the pool each task gets its own.
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let index = index.clone();
            tasks.spawn(async move {
                index
                    .search("crop".into(), fts::Bm25Weights::default(), 10)
                    .await
            });
        }
        while let Some(joined) = tasks.join_next().await {
            assert!(joined.unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn a_paused_index_does_not_reindex() {
        let dir = sample();
        let index = Index::open(dir.path()).unwrap();
        index.reindex().await.unwrap();

        index.set_paused(true);
        fs::write(
            dir.path().join("new.md"),
            "---\nid: new\ntitle: New\n---\n# New {#root}\nAdded while paused.\n",
        )
        .unwrap();

        let stats = index.reindex().await.unwrap();
        assert_eq!(stats.documents, 1, "a paused index picked up a new file");

        index.set_paused(false);
        let stats = index.reindex().await.unwrap();
        assert_eq!(stats.documents, 2, "resuming did not pick the file up");
    }

    #[tokio::test]
    async fn an_edited_note_is_searchable_after_the_next_reindex() {
        let dir = sample();
        let index = Index::open(dir.path()).unwrap();
        index.reindex().await.unwrap();

        fs::write(
            dir.path().join("obs.md"),
            "---\nid: obs\ntitle: OBS\n---\n# OBS {#root}\n\
             ## Cursor follow {#follow}\nExponentialtoken smoothing.\n",
        )
        .unwrap();
        index.reindex().await.unwrap();

        let hits = index
            .search("Exponentialtoken".into(), fts::Bm25Weights::default(), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        // And the section that went away must be gone, not merely outranked.
        let stale = index
            .search("directly".into(), fts::Bm25Weights::default(), 10)
            .await
            .unwrap();
        assert!(stale.is_empty(), "a deleted section is still searchable");
    }
}
