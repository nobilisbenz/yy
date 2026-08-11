//! `yy`'s own store: the answer cache and provenance rows.
//!
//! Deliberately **not** in the vault's `.notes/index.sqlite`. That file is `yalive`'s, it
//! describes the vault, and it is rebuilt from the Markdown whenever the schema changes.
//! Answers and provenance describe *this tool's* behaviour, survive a reindex, and would be
//! lost by exactly that rebuild. `PLAN.md` §2.2 anticipated a second store; this is it.
//!
//! Two tables, and the second is the more valuable one.
//!
//! **`answer_cache`** keys on the *content* of what was packed into the prompt, not on the
//! query. Two phrasings of the same question that retrieve the same sections share an
//! answer, and — the part that matters — editing a note invalidates every answer that was
//! generated from it, because its hash is in the key. Without the content hashes this cache
//! would confidently serve an answer describing a workflow you rewrote last week.
//!
//! **`provenance`** records what was actually retrieved for each question, and whether the
//! answer was any good. `PLAN.md` §6.3: the plan otherwise calls for hand-labelling 30–50
//! questions to build a retrieval benchmark. Logging real queries with the sections used,
//! plus one keystroke to rate them, produces a strictly better labelled set — built from the
//! questions actually asked — as a side effect of using the tool.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension as _, params};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("could not open the store at {}: {source}", .path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not create {}: {source}", .path.display())]
    Directory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

type Result<T> = std::result::Result<T, StoreError>;

/// How the user judged an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rating {
    Good,
    Bad,
}

impl Rating {
    fn as_i64(self) -> i64 {
        match self {
            Self::Good => 1,
            Self::Bad => -1,
        }
    }
}

/// One recorded answer, for the benchmark set.
#[derive(Debug, Clone)]
pub struct Provenance {
    pub query: String,
    /// The sections packed into the prompt, in rank order. This is the label.
    pub section_uids: Vec<String>,
    pub model: Option<String>,
    pub rating: Option<i64>,
}

pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    /// The default location, `$XDG_DATA_HOME/brain/brain.sqlite`.
    pub fn default_path() -> Option<PathBuf> {
        use etcetera::BaseStrategy as _;
        let strategy = etcetera::choose_base_strategy().ok()?;
        Some(strategy.data_dir().join("brain/brain.sqlite"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Directory {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let connection = Connection::open(path).map_err(|source| StoreError::Open {
            path: path.to_path_buf(),
            source,
        })?;

        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous  = NORMAL;
             PRAGMA busy_timeout = 3000;

             CREATE TABLE IF NOT EXISTS answer_cache (
                key        TEXT PRIMARY KEY,
                answer     TEXT NOT NULL,
                model      TEXT,
                created_at INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS provenance (
                id           INTEGER PRIMARY KEY,
                query        TEXT NOT NULL,
                -- JSON array of section_uid, in rank order. The uid is the identity
                -- `yalive`, `yGraphy`, and `yReviewy` all share, so a benchmark built from
                -- these rows can be checked against the graph directly.
                section_uids TEXT NOT NULL,
                model        TEXT,
                created_at   INTEGER NOT NULL,
                -- 1 good, -1 bad, NULL unrated. Most rows stay NULL; the rated ones are
                -- the benchmark.
                rating       INTEGER
             );

             CREATE INDEX IF NOT EXISTS provenance_rated ON provenance(rating)
                WHERE rating IS NOT NULL;",
        )?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// In-memory, for tests.
    pub fn in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(
            "CREATE TABLE answer_cache (
                key TEXT PRIMARY KEY, answer TEXT NOT NULL, model TEXT,
                created_at INTEGER NOT NULL);
             CREATE TABLE provenance (
                id INTEGER PRIMARY KEY, query TEXT NOT NULL, section_uids TEXT NOT NULL,
                model TEXT, created_at INTEGER NOT NULL, rating INTEGER);",
        )?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // --- answer cache -------------------------------------------------------------------

    pub fn cached_answer(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .query_row(
                "SELECT answer FROM answer_cache WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn store_answer(&self, key: &str, answer: &str, model: Option<&str>) -> Result<()> {
        self.lock().execute(
            "INSERT INTO answer_cache(key, answer, model, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET answer=excluded.answer, created_at=excluded.created_at",
            params![key, answer, model, now()],
        )?;
        Ok(())
    }

    /// Drop the whole cache. Used when the prompt or model changes under it.
    pub fn clear_answers(&self) -> Result<usize> {
        Ok(self.lock().execute("DELETE FROM answer_cache", [])?)
    }

    // --- provenance ---------------------------------------------------------------------

    /// Record what a query retrieved. Returns the row id, which is what a rating refers to.
    pub fn record(&self, query: &str, section_uids: &[String], model: Option<&str>) -> Result<i64> {
        let uids = serde_json::to_string(section_uids).unwrap_or_else(|_| "[]".into());
        let connection = self.lock();
        connection.execute(
            "INSERT INTO provenance(query, section_uids, model, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![query, uids, model, now()],
        )?;
        Ok(connection.last_insert_rowid())
    }

    pub fn rate(&self, id: i64, rating: Rating) -> Result<()> {
        self.lock().execute(
            "UPDATE provenance SET rating = ?2 WHERE id = ?1",
            params![id, rating.as_i64()],
        )?;
        Ok(())
    }

    /// Every rated query, newest first — the benchmark set.
    pub fn rated(&self) -> Result<Vec<Provenance>> {
        let connection = self.lock();
        let mut statement = connection.prepare(
            "SELECT query, section_uids, model, rating FROM provenance
             WHERE rating IS NOT NULL ORDER BY id DESC",
        )?;
        let rows = statement
            .query_map([], |row| {
                let uids: String = row.get(1)?;
                Ok(Provenance {
                    query: row.get(0)?,
                    section_uids: serde_json::from_str(&uids).unwrap_or_default(),
                    model: row.get(2)?,
                    rating: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(total, rated_good, rated_bad)`, for `brainctl status`.
    pub fn counts(&self) -> Result<(usize, usize, usize)> {
        let connection = self.lock();
        let total = connection.query_row("SELECT count(*) FROM provenance", [], |r| r.get(0))?;
        let good = connection.query_row(
            "SELECT count(*) FROM provenance WHERE rating > 0",
            [],
            |r| r.get(0),
        )?;
        let bad = connection.query_row(
            "SELECT count(*) FROM provenance WHERE rating < 0",
            [],
            |r| r.get(0),
        )?;
        Ok((total, good, bad))
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// The answer cache key.
///
/// Hashes **what was sent to the model**, not what the user typed. Two phrasings that
/// retrieve the same sections share an answer, and editing any packed section changes its
/// body and therefore the key — which is the only thing that stops a cached answer
/// outliving the note it was generated from.
pub fn answer_key(
    sections: &[(String, String)],
    model: Option<&str>,
    prompt_version: u32,
    max_tokens: usize,
    temperature: f32,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for (uid, body) in sections {
        hasher.update(uid.as_bytes());
        hasher.update(b"\0");
        hasher.update(body.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(model.unwrap_or("none").as_bytes());
    hasher.update(&prompt_version.to_le_bytes());
    hasher.update(&(max_tokens as u64).to_le_bytes());
    // Generation params are part of the key: the same context at a different temperature is
    // a different answer, and serving the cached one would silently ignore the setting.
    hasher.update(&temperature.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sections(bodies: &[(&str, &str)]) -> Vec<(String, String)> {
        bodies
            .iter()
            .map(|(uid, body)| ((*uid).to_string(), (*body).to_string()))
            .collect()
    }

    #[test]
    fn an_edited_section_invalidates_the_answer_generated_from_it() {
        // The whole reason the key hashes content rather than query text. Without this, a
        // rewritten note keeps answering with the workflow it replaced.
        let before = answer_key(&sections(&[("a#1", "use rsync")]), Some("m"), 1, 200, 0.15);
        let after = answer_key(&sections(&[("a#1", "use restic")]), Some("m"), 1, 200, 0.15);
        assert_ne!(before, after);
    }

    #[test]
    fn the_same_sections_hit_the_same_key_however_the_question_was_phrased() {
        let packed = sections(&[("a#1", "body"), ("b#2", "other")]);
        assert_eq!(
            answer_key(&packed, Some("m"), 1, 200, 0.15),
            answer_key(&packed, Some("m"), 1, 200, 0.15)
        );
    }

    #[test]
    fn changing_the_prompt_or_the_model_invalidates_everything() {
        let packed = sections(&[("a#1", "body")]);
        let base = answer_key(&packed, Some("qwen3-1.7b"), 1, 200, 0.15);

        assert_ne!(base, answer_key(&packed, Some("qwen3-4b"), 1, 200, 0.15));
        assert_ne!(base, answer_key(&packed, Some("qwen3-1.7b"), 2, 200, 0.15));
        assert_ne!(base, answer_key(&packed, Some("qwen3-1.7b"), 1, 350, 0.15));
        assert_ne!(base, answer_key(&packed, Some("qwen3-1.7b"), 1, 200, 0.7));
    }

    #[test]
    fn section_order_is_part_of_the_key() {
        // Rank order changes what the model sees first and therefore what it says.
        let forward = answer_key(&sections(&[("a#1", "x"), ("b#2", "y")]), None, 1, 200, 0.15);
        let reverse = answer_key(&sections(&[("b#2", "y"), ("a#1", "x")]), None, 1, 200, 0.15);
        assert_ne!(forward, reverse);
    }

    #[test]
    fn a_stored_answer_comes_back() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.cached_answer("k").unwrap(), None);

        store.store_answer("k", "the answer", Some("m")).unwrap();
        assert_eq!(store.cached_answer("k").unwrap().as_deref(), Some("the answer"));

        // Re-storing the same key updates rather than failing on the primary key.
        store.store_answer("k", "revised", Some("m")).unwrap();
        assert_eq!(store.cached_answer("k").unwrap().as_deref(), Some("revised"));
    }

    #[test]
    fn provenance_records_what_was_retrieved_and_can_be_rated() {
        let store = Store::in_memory().unwrap();
        let uids = vec!["obs#follow".to_string(), "obs#old".to_string()];
        let id = store.record("how do I follow the cursor?", &uids, Some("m")).unwrap();

        // Unrated rows are the majority and are not the benchmark.
        assert!(store.rated().unwrap().is_empty());
        assert_eq!(store.counts().unwrap(), (1, 0, 0));

        store.rate(id, Rating::Good).unwrap();
        let rated = store.rated().unwrap();
        assert_eq!(rated.len(), 1);
        assert_eq!(rated[0].query, "how do I follow the cursor?");
        assert_eq!(rated[0].section_uids, uids, "the label is the retrieved set");
        assert_eq!(store.counts().unwrap(), (1, 1, 0));

        // A rating can be changed; the last word wins.
        store.rate(id, Rating::Bad).unwrap();
        assert_eq!(store.counts().unwrap(), (1, 0, 1));
    }

    #[test]
    fn rating_an_unknown_row_is_a_no_op_rather_than_an_error() {
        // The dock can send a rating for a query the daemon has forgotten, e.g. after a
        // restart. That is not worth an error dialog.
        let store = Store::in_memory().unwrap();
        assert!(store.rate(999, Rating::Good).is_ok());
    }

    #[test]
    fn the_store_survives_being_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/brain.sqlite");

        let store = Store::open(&path).unwrap();
        store.store_answer("k", "persisted", None).unwrap();
        store.record("q", &["a#1".to_string()], None).unwrap();
        drop(store);

        // Surviving a daemon restart is the entire reason this is on disk rather than in
        // the retrieval LRU.
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.cached_answer("k").unwrap().as_deref(), Some("persisted"));
        assert_eq!(reopened.counts().unwrap().0, 1);
    }
}
