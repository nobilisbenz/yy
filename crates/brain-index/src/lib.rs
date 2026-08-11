//! The vault index — a wrapper over `yalive`, not a second implementation.
//!
//! This is `PLAN.md` §2.2 decision **A**. The original Stage 1 plan had this crate build
//! its own migrations, `sections` table, `pulldown-cmark` parser, FTS5 configuration and
//! file watcher. It does none of that. `yalive` already indexes the vault, and `yGraphy`
//! and `yReviewy` already read what it produces.
//!
//! The reason is identity, not effort. `yalive` keys everything on `section_uid`, and
//! `relations`, `cards`, and `review_state` all hang off it. A second parser with its own
//! notion of a section would mean the graph could not be shared, review state could not
//! inform ranking, and `Alt+1` would jump to a line number computed by a *different
//! parser* than the one that built the graph you are looking at in `yGraphy`.
//!
//! So: **`yalive` owns the vault schema and section identity.** Improvements that belong
//! to the index — the FTS5 escaping in [`fts`], indices, PRAGMAs — are made *there*, and
//! every consumer inherits them.
//!
//! What is left for this crate is the part `yalive` has no opinion about: `yy` indexes
//! more than a vault (`~/projects`, code, PDFs, up to 50k files), and those rows carry a
//! `source_kind` and no `section_uid`. That second store is not built yet; when it is, it
//! lives here alongside this wrapper and retrieval fuses the two. Only vault rows ever
//! participate in graph expansion and review-state ranking.

pub mod fts;
pub mod handle;
pub mod watcher;

pub use handle::{Index, IndexStats};
pub use watcher::Watcher;

use std::path::{Path, PathBuf};

use yalive::db::Database;
use yalive::graph::Graph;
use yalive::model::SectionRow;

/// Where a retrieved section came from, in the vocabulary the dock speaks.
///
/// Carries `section_uid` rather than a rowid: it is the identity `yGraphy`, `yReviewy`,
/// and the `relations` table all share, and it is what the `Show in graph` action needs
/// (`PLAN.md` §2.4 L2). It comes from parsed metadata and never from model output.
#[derive(Debug, Clone)]
pub struct Hit {
    pub section_uid: String,
    pub note_title: String,
    pub heading: String,
    /// `OBS workflows > Follow cursor > Smoothing`
    pub heading_path: String,
    pub body: String,
    pub path: PathBuf,
    pub start_line: usize,
    /// The owning note's front-matter `status:`, which ranking multiplies by
    /// (`[search.status_weight]`). `None` means unmarked, which scores as `current`.
    pub status: Option<String>,
}

impl Hit {
    /// Build a hit, resolving the row's path against the vault it came from.
    ///
    /// **`yalive` stores `files.path` relative to the vault root.** A `Hit` is handed to
    /// code that opens files and checks whether they exist, and a relative path there
    /// resolves against the daemon's working directory — so every note action reported
    /// itself as broken and `Alt+1` would have opened nothing. Joining here, at the one
    /// place that knows the vault, is what keeps that from being rediscovered.
    pub fn from_row(row: SectionRow, vault: &Path) -> Self {
        let path = if row.path.is_absolute() {
            row.path
        } else {
            vault.join(row.path)
        };

        Self {
            section_uid: row.uid,
            note_title: row.note_title,
            heading: row.heading,
            heading_path: row.heading_path,
            body: row.body,
            path,
            start_line: row.start_line,
            status: row.status,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("the vault index is unavailable")]
    Vault(#[from] anyhow::Error),
    #[error("{0}")]
    Writer(String),
}

type Result<T> = std::result::Result<T, IndexError>;

/// An open vault index.
///
/// Holds a `yalive::db::Database`, which is a single SQLite connection and therefore
/// **not** safe to share across threads or to call from async code directly. Stage 1's
/// writer-thread-plus-reader-pool goes around this type, not inside it — and every call
/// from the daemon goes through `spawn_blocking`, or a reindex stalls the runtime.
pub struct VaultIndex {
    vault: PathBuf,
    database: Database,
}

impl VaultIndex {
    /// Open (creating `.notes/index.sqlite` if absent) without indexing.
    pub fn open(vault: impl AsRef<Path>) -> Result<Self> {
        let vault = vault.as_ref().to_path_buf();
        let database = Database::open(&vault)?;
        Ok(Self { vault, database })
    }

    pub fn vault(&self) -> &Path {
        &self.vault
    }

    /// Walk the vault and bring the index up to date.
    pub fn reindex(&mut self) -> Result<()> {
        self.database.index_vault(&self.vault)?;
        Ok(())
    }

    /// Lexical search — the **seed** step of retrieval.
    ///
    /// Returns `Ok(vec![])` rather than an error for a query with nothing searchable in
    /// it, which is what [`fts::expression`] returning `None` means. Callers must not
    /// treat that as "no index".
    ///
    /// The results are BM25-ordered and are the input to graph expansion, not the answer.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.search_weighted(query, fts::Bm25Weights::default(), limit)
    }

    /// Lexical search with explicit BM25 column weights.
    ///
    /// The weights are swept from config against the retrieval benchmark, so the caller
    /// supplies them rather than the index choosing.
    pub fn search_weighted(
        &self,
        query: &str,
        weights: fts::Bm25Weights,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        // `fts` is the only thing that decides whether a query is searchable, and it says
        // so before we touch SQLite. `None` means no searchable token, which is an empty
        // result — not an error, and not every section in the vault.
        //
        // `Any` rather than `All`: this is retrieval, not a filter list. A natural question
        // has no section containing every one of its words, so requiring all of them
        // returns nothing exactly when the user asked something real. BM25 does the
        // separating, and these hits are a *seed* for graph expansion, so recall wins.
        let Some(expression) = fts::expression_with(query, fts::Mode::Any) else {
            return Ok(Vec::new());
        };

        let hits = self.database.search_expression(&expression, weights, limit)?;
        Ok(hits
            .into_iter()
            .map(|row| Hit::from_row(row, &self.vault))
            .collect())
    }

    /// The typed graph, for expansion and ranking.
    ///
    /// Built from a snapshot, so it is a *read* of the index at one moment. Rebuild it
    /// after a reindex; do not hold one across an index generation.
    pub fn graph(&self) -> Result<Graph> {
        Ok(Graph::new(&self.database.graph()?))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    /// A vault with two linked sections and one that contradicts them.
    fn vault() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("obs.md"),
            "---\nid: obs\ntitle: OBS\n---\n\
             # OBS {#root}\n\
             ## Cursor follow {#follow}\n\
             Smooth the crop target instead of moving it to every cursor position.\n\
             [[obs#root]]\n\
             ## Old approach {#old}\n\
             contradicts:: [[obs#follow]]\n\
             Move the crop directly. This jitters.\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn searching_returns_sections_with_their_heading_path() {
        let dir = vault();
        let mut index = VaultIndex::open(dir.path()).unwrap();
        index.reindex().unwrap();

        let hits = index.search("crop", 10).unwrap();
        assert!(!hits.is_empty(), "the word `crop` is in two sections");

        let follow = hits
            .iter()
            .find(|hit| hit.section_uid == "obs#follow")
            .expect("the cursor-follow section matches");
        assert_eq!(follow.heading_path, "OBS > Cursor follow");
        assert_eq!(follow.note_title, "OBS");
    }

    #[test]
    fn an_unsearchable_query_is_empty_not_an_error() {
        let dir = vault();
        let mut index = VaultIndex::open(dir.path()).unwrap();
        index.reindex().unwrap();

        // Every one of these reaches SQLite as an FTS5 expression if unguarded.
        for query in ["", "  ", "-", "\"", "OR", "*"] {
            let hits = index.search(query, 10);
            assert!(hits.is_ok(), "{query:?} errored: {:?}", hits.err());
        }
    }

    // The authoritative escaping test — 1331 hostile queries through a real FTS5 parser —
    // now lives with the implementation it tests, as
    // `yalive`'s `db::tests::no_query_can_make_the_search_index_raise`.

    #[test]
    fn the_graph_sees_the_relations_the_vault_declared() {
        let dir = vault();
        let mut index = VaultIndex::open(dir.path()).unwrap();
        index.reindex().unwrap();

        let graph = index.graph().unwrap();
        let contradictions = graph.contradictions();

        assert_eq!(contradictions.len(), 1, "one `contradicts::` edge");
        let (left, right) = contradictions[0];
        let uids = [
            graph.node(left).uid.as_str(),
            graph.node(right).uid.as_str(),
        ];
        assert!(uids.contains(&"obs#follow") && uids.contains(&"obs#old"));
    }

    #[test]
    fn expansion_from_a_seed_reaches_the_section_that_contradicts_it() {
        // The whole point of graph retrieval: asking about the old approach must surface
        // the note that corrects it, which lexical search alone would not connect.
        let dir = vault();
        let mut index = VaultIndex::open(dir.path()).unwrap();
        index.reindex().unwrap();

        let graph = index.graph().unwrap();
        let seed = graph.index_of("obs#old").unwrap();
        let reached = graph.expand(&[(seed, 1.0)], yalive::graph::Expansion::default());

        assert!(
            reached
                .iter()
                .any(|hit| graph.node(hit.index).uid == "obs#follow"),
            "the contradicting section must be reachable from the seed"
        );
    }
}
