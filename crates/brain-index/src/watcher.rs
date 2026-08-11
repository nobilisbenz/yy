//! Keeping the index current while you edit.
//!
//! Stage 1 §1.5. Two things make this less trivial than "watch a directory and reindex":
//!
//! **nvim's atomic save is a rename pair.** Plain `notify` reports `:w` as a delete
//! followed by a create, and an indexer that believes it drops the document for a moment —
//! long enough that a query landing in that window returns nothing for a note that exists.
//! `notify-debouncer-full` tracks rename pairs and coalesces the burst, which is the whole
//! reason it is used instead of `notify` alone.
//!
//! **The index lives inside the vault.** `yalive` writes `<vault>/.notes/index.sqlite`, so
//! an unfiltered watch sees its own writes, reindexes, sees those writes, and never stops.
//! [`is_relevant`] is what breaks that loop, and it is the first thing to check if the
//! daemon ever pegs a core while idle.

use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};

use crate::{Index, IndexError, Result};

/// `yalive`'s sidecar index directory — our own writes, never content.
const SIDECAR: &str = ".notes";

/// A running watch. Dropping it stops the watch.
pub struct Watcher {
    _debouncer: notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
}

impl Watcher {
    /// Watch a vault and reindex it when its files change.
    ///
    /// `debounce` is the quiet period after the last event; the config default of 400 ms is
    /// long enough to swallow an editor's save burst and short enough that a re-ask a
    /// second later already sees the change.
    ///
    /// Must be called from inside a Tokio runtime: the reindex it triggers is async.
    pub fn spawn(index: Index, debounce: Duration) -> Result<Self> {
        let vault = index.vault().to_path_buf();
        let runtime = tokio::runtime::Handle::current();

        // Capacity 1 with `try_send`: if a reindex is already queued, another event does
        // not need its own slot — the walk that runs will see every change on disk anyway.
        // Bounding it is what stops a `cargo build` in a watched tree from queueing
        // thousands of redundant reindexes.
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<()>(1);

        runtime.spawn(async move {
            while receiver.recv().await.is_some() {
                match index.reindex().await {
                    Ok(stats) => tracing::debug!(
                        documents = stats.documents,
                        sections = stats.sections,
                        generation = stats.generation,
                        "reindexed after a file change"
                    ),
                    Err(error) => tracing::warn!(%error, "reindex after a file change failed"),
                }
            }
        });

        let watched = vault.clone();
        let mut debouncer = new_debouncer(
            debounce,
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    let trigger = events
                        .iter()
                        .filter(|event| is_content_change(&event.kind))
                        .flat_map(|event| event.paths.iter())
                        .find(|path| is_relevant(path, &watched));
                    if let Some(path) = trigger {
                        // Logged because the failure mode here is a *loop*: a path that
                        // should have been filtered triggers a reindex, whose own writes
                        // trigger the next one. This line names the culprit immediately.
                        tracing::debug!(path = %path.display(), "reindexing because of");
                        // Full means "a reindex is already pending", which is the same
                        // outcome. Dropping it is correct, not a lost update.
                        let _ = sender.try_send(());
                    }
                }
                Err(errors) => {
                    for error in errors {
                        tracing::warn!(%error, "file watch error");
                    }
                }
            },
        )
        .map_err(|source| IndexError::Writer(format!("could not start the file watcher: {source}")))?;

        debouncer
            .watch(&vault, RecursiveMode::Recursive)
            .map_err(|source| {
                IndexError::Writer(format!("could not watch {}: {source}", vault.display()))
            })?;

        tracing::info!(vault = %vault.display(), debounce_ms = debounce.as_millis() as u64, "watching");
        Ok(Self {
            _debouncer: debouncer,
        })
    }
}

/// Did this event change content, or merely observe it?
///
/// **This is the one that matters.** inotify reports reads as `IN_ACCESS`, so indexing the
/// vault — which reads every file in it — generates an access event per note. Treating
/// those as changes means every reindex schedules the next one, and the daemon reindexes
/// forever at the debounce interval, pinning a core and never settling. Measured: with
/// access events included, a 50-note vault reindexed ~10 times a second indefinitely.
///
/// Metadata-only changes are excluded for the same reason — reading a file updates its
/// atime, which is a metadata change that says nothing about its contents.
fn is_content_change(kind: &notify::EventKind) -> bool {
    use notify::EventKind;
    use notify::event::ModifyKind;

    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false,
        EventKind::Modify(_) => true,
        // `Any`/`Other` are what a platform backend emits when it cannot classify an
        // event. Reindexing is cheap and idempotent, so erring toward it is right — but
        // `Access` is never worth it.
        EventKind::Any | EventKind::Other => true,
        EventKind::Access(_) => false,
    }
}

/// Is this path worth reindexing for?
///
/// Excludes our own sidecar index, which is what stops the watch from feeding itself, and
/// the usual directories that produce enormous event volume and no prose.
fn is_relevant(path: &Path, vault: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(vault) else {
        // Outside the vault entirely. Should not happen on a watch rooted there, and
        // reindexing for it would be acting on something we know nothing about.
        return false;
    };

    // The vault root itself. Its mtime changes whenever anything inside it does, and the
    // event for the file that actually changed is the one worth acting on.
    if relative.as_os_str().is_empty() {
        return false;
    }

    !relative.components().any(|component| {
        let name = component.as_os_str();
        name == SIDECAR || name == ".git" || name == "target" || name == "node_modules"
    })
}

/// Where a watcher would write, exposed so callers can log it.
pub fn sidecar_dir(vault: &Path) -> PathBuf {
    vault.join(SIDECAR)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn our_own_index_writes_are_ignored() {
        let vault = Path::new("/home/nabi/brain");
        // The loop this prevents: reindex writes the sqlite file, the watch fires, it
        // reindexes again, forever.
        assert!(!is_relevant(
            &vault.join(".notes/index.sqlite"),
            vault
        ));
        assert!(!is_relevant(&vault.join(".notes/index.sqlite-wal"), vault));
        assert!(!is_relevant(&vault.join(".git/objects/ab/cd"), vault));
        assert!(!is_relevant(&vault.join("projects/x/target/debug/y"), vault));

        assert!(is_relevant(&vault.join("obs.md"), vault));
        assert!(is_relevant(&vault.join("deep/nested/note.md"), vault));

        // The vault root itself. Its mtime moves whenever anything inside it does, so
        // acting on it means reindexing twice for every change — and it was one of the two
        // paths that made the daemon reindex in a loop.
        assert!(!is_relevant(vault, vault));
        assert!(!is_relevant(Path::new("/somewhere/else/note.md"), vault));
    }

    /// Reading the vault must not look like changing it.
    ///
    /// The loop this prevents: `index_vault` reads all 50 notes, inotify reports 50
    /// `IN_ACCESS` events, the debouncer fires, the daemon reindexes, which reads all 50
    /// notes again. It never settles. The end-to-end watcher test below does not catch
    /// this — it passes happily while the daemon spins — so it is asserted directly.
    #[test]
    fn reading_a_file_is_not_a_change_to_it() {
        use notify::EventKind;
        use notify::event::{AccessKind, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind};

        assert!(!is_content_change(&EventKind::Access(AccessKind::Read)));
        assert!(!is_content_change(&EventKind::Access(AccessKind::Any)));
        // atime is metadata, and reading a file changes it.
        assert!(!is_content_change(&EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::AccessTime
        ))));

        assert!(is_content_change(&EventKind::Create(CreateKind::File)));
        assert!(is_content_change(&EventKind::Remove(RemoveKind::File)));
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Data(
            DataChange::Content
        ))));
        // nvim's atomic save arrives as a rename pair.
        assert!(is_content_change(&EventKind::Modify(ModifyKind::Name(
            notify::event::RenameMode::Both
        ))));
    }

    /// Saving a file makes it searchable without anyone asking for a reindex.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_saved_file_becomes_searchable_on_its_own() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("first.md"),
            "---\nid: first\ntitle: First\n---\n# First {#root}\nOriginal body.\n",
        )
        .unwrap();

        let index = Index::open(dir.path()).unwrap();
        index.reindex().await.unwrap();

        let debounce = Duration::from_millis(120);
        let _watcher = Watcher::spawn(index.clone(), debounce).unwrap();

        fs::write(
            dir.path().join("second.md"),
            "---\nid: second\ntitle: Second\n---\n# Second {#root}\nWatchedtoken body.\n",
        )
        .unwrap();

        // Poll rather than sleeping a fixed time: the debouncer's tick rate and the
        // filesystem's event latency both vary, and a fixed sleep is how this becomes a
        // flaky test on a loaded machine.
        let mut searchable = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let hits = index
                .search("Watchedtoken".into(), crate::fts::Bm25Weights::default(), 5)
                .await
                .unwrap();
            if !hits.is_empty() {
                searchable = true;
                break;
            }
        }
        assert!(searchable, "a saved file never became searchable");
    }
}
