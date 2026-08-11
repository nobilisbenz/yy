# Stage 1 — Index and lexical search

**Goal:** ask a keyword-style question and land in the correct Markdown section in nvim.

Still no LLM. When this stage is done Brain Dock is already a useful tool — a fast,
section-level, context-aware grep over your notes with a beautiful launcher on it. That is
deliberate: if Stage 2 never happens, Stage 1 still earns its keep.

**Prerequisite:** Stage 0 verification passes.

---

## Deliverables

- `brain-core`: config loading, `Document`, `Section`, `Action`, ids.
- `brain-index`: migrations, DB access, Markdown parser, indexer, file watcher.
- `brain-engine`: query normalization, FTS5 search, result assembly.
- Dock renders real sources; `Alt+1` / `Enter` opens the note at the right line.
- `brainctl reindex`, `brainctl status`, `brainctl sources`, `brainctl pause-indexing`.

---

## 1.1 Config

`~/.config/brain/config.toml`, layout per spec §49. Ship `config/brain.example.toml` and
copy it on `brainctl init`.

- Resolve XDG paths with `etcetera`; expand `~` yourself (no crate does it correctly for
  `~otheruser` and you do not need that).
- **Validate at load**: every `[[sources]]` path must exist and be a directory; every glob
  must compile. Report all errors at once, not the first.
- `brainctl doctor` re-runs validation and checks the opener binaries exist.
- Never index `$HOME` by default (spec §29). If a configured source is `$HOME` or `/`,
  refuse and say why.

## 1.2 Schema and migrations

Take the SQL from spec §15 verbatim into `migrations/001_initial.sql`. Add:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous  = NORMAL;
PRAGMA foreign_keys = ON;          -- must be set per-connection, not once
PRAGMA busy_timeout = 5000;

CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- schema_version, index_generation, parser_version, retrieval_config_version
```

Migration runner: read `schema_version` from `meta`, apply each numbered file above it
inside one transaction each, bump the version in the same transaction. Files embedded with
`include_str!` so the binary is self-contained.

`index_generation` is the cache invalidation counter from spec §36. Bump it in the same
transaction as any index write.

**FTS5** (`migrations/002_fts.sql`), from spec §15 with one change:

```sql
CREATE VIRTUAL TABLE sections_fts USING fts5(
    heading, heading_path, body,
    content='sections', content_rowid='id',
    tokenize="unicode61 remove_diacritics 2 tokenchars '_-.'"
);
```

`tokenchars '_-.'` keeps `calculate_pivot`, `cursor-follow`, and `sprite.rs` as single
tokens. Without it a search for `calculate_pivot` becomes `calculate OR pivot` and
precision collapses on any corpus containing code. This matters more than it sounds.

Deliberately **not** using the `porter` stemmer initially: it helps prose recall
(`mirroring` → `mirror`) and hurts identifiers. Make it a config flag
(`[search] stemmer = "none" | "porter"`) and settle it with the Stage 7 benchmark rather
than by intuition.

Keep the external-content table in sync with **triggers**, not by hand:

```sql
CREATE TRIGGER sections_ai AFTER INSERT ON sections BEGIN
  INSERT INTO sections_fts(rowid, heading, heading_path, body)
  VALUES (new.id, new.heading, new.heading_path, new.body);
END;
CREATE TRIGGER sections_ad AFTER DELETE ON sections BEGIN
  INSERT INTO sections_fts(sections_fts, rowid, heading, heading_path, body)
  VALUES ('delete', old.id, old.heading, old.heading_path, old.body);
END;
CREATE TRIGGER sections_au AFTER UPDATE ON sections BEGIN
  INSERT INTO sections_fts(sections_fts, rowid, heading, heading_path, body)
  VALUES ('delete', old.id, old.heading, old.heading_path, old.body);
  INSERT INTO sections_fts(rowid, heading, heading_path, body)
  VALUES (new.id, new.heading, new.heading_path, new.body);
END;
```

Hand-maintaining the FTS table works right up until one code path forgets, after which
search silently returns stale rows and you will not notice for weeks.

Run `INSERT INTO sections_fts(sections_fts) VALUES('optimize')` after a full reindex.

## 1.3 Connection strategy

`rusqlite` with the `bundled` feature — pins the SQLite and FTS5 version so behaviour does
not shift under a system update.

- **One writer**, owned by a dedicated thread with a command channel. SQLite allows one
  writer; funnelling all writes through one owner removes every `SQLITE_BUSY` question.
- **N readers**: `r2d2_sqlite` pool, or a `thread_local!` connection per Tokio blocking
  thread. WAL means readers never block on the writer.
- All DB calls from async code go through `tokio::task::spawn_blocking`. rusqlite is
  synchronous; calling it directly on a Tokio worker will stall the whole runtime under a
  large reindex.

## 1.4 Markdown parser

`pulldown-cmark`, `Parser::new_ext(src, opts).into_offset_iter()`.

**Line numbers**: build `Vec<usize>` of line-start byte offsets once per file, then
binary-search each event's byte range. Do not count newlines per event.

**Sectioning** (spec §10, §14): maintain a heading stack. A new `Heading(level)` closes
every open section at that level or deeper and opens a new one. Each section records
`heading`, `heading_path: Vec<String>`, `parent_id`, `order_index`, `start_line`,
`end_line`, and its raw body text.

Store `heading_path` in the DB as a `>`-joined string (`OBS > Cursor follow > Smoothing`)
and keep it in the FTS table — heading matches carry most of the retrieval signal, which
is why `bm25(sections_fts, 8.0, 4.0, 1.0)` weights it 8×.

**Preamble**: text before the first heading is a section with `heading = NULL` and
`heading_path` = the document title. Do not drop it.

**Oversized sections** (spec §14): if `token_estimate > max_section_tokens` (700), split
on paragraph boundaries into ~450-token subchunks with ~60 tokens of overlap, as children
with `parent_id` set. A retrieved subchunk always displays its parent's heading path.

**Token estimate**: `len_utf8 / 3.6` is close enough for chunking decisions. Only the final
prompt assembly (Stage 2) needs real tokenization.

**Front matter**: `title`, `tags`, `apps`, `project`, `status`, `supersedes`
(spec §12, §47). Enable pulldown-cmark's YAML metadata block option, or strip and parse
the leading `---` block yourself with `serde_yaml_ng`.

**`@action` lines** (spec §12): parse them in Stage 1 but only *store* them —
`@file`/`@video`/`@app`/`@project`/`@url`. They attach to the innermost open section.
Wiring them to buttons is Stage 3. Getting them into the schema now avoids a reindex then.

**Golden tests from the first commit** (spec §55):

```text
tests/fixtures/markdown/actions.md
tests/fixtures/markdown/actions.expected.json
```

Serialize the parsed sections+actions to JSON and diff. This is the single highest-value
test in the project — a silent parser regression corrupts the entire index and there is no
other way to notice.

## 1.5 Indexer and watcher

Initial walk: the `ignore` crate (`WalkBuilder`, parallel, respects `.gitignore`), filtered
by `globset` include/exclude from config.

Per file: `stat` → compare `(mtime_ns, size)` → if changed, read + `blake3` → compare
`content_hash` → if equal, update mtime only and stop (spec §28: never reindex on mtime
alone). Otherwise parse and replace.

Replace transactionally:

```sql
BEGIN;
  DELETE FROM sections WHERE document_id = ?;   -- cascades to tags, actions, FTS via triggers
  -- insert new sections, tags, actions
  UPDATE documents SET ... ;
  UPDATE meta SET value = value + 1 WHERE key = 'index_generation';
COMMIT;
```

Watching: `notify` + `notify-debouncer-full` with a ~400 ms debounce. The debouncer tracks
rename pairs, which is exactly what nvim's atomic save produces — plain `notify` reports
that as delete-then-create and a naive indexer drops the document for a moment.

Batch a burst into one transaction. On a full reindex, insert in batches of ~500 sections
inside a single transaction; per-row transactions are ~100× slower.

Indexing runs on a dedicated thread at low priority. `brainctl pause-indexing` sets a flag
the walker checks — useful when a `cargo build` is churning `~/projects`.

**Do not index `target/`, `node_modules/`, `.git/`.** The example config excludes them;
`brainctl doctor` should warn if a source would pull in more than ~50k files.

## 1.6 Query normalization and the FTS5 escaping problem

Spec §17 says "lightweight deterministic cleanup". The part it omits will bite you
immediately:

**A raw user query passed to `MATCH` is a syntax error waiting to happen.** FTS5 treats
`"`, `*`, `(`, `)`, `:`, `-`, `NEAR`, `AND`, `OR`, `NOT` as syntax. `how did I mirror
bones?` happens to work; `what's the -j flag for?` throws. Users type punctuation.

Build the MATCH expression, never pass the raw string:

1. Extract `"quoted phrases"` first; keep each as a phrase term.
2. Split the rest on non-`tokenchars` boundaries.
3. Drop a small stopword list (`how`, `did`, `i`, `the`, `a`, `to`, `do`, `my`) — these
   are pure noise in a personal-notes corpus and they wreck BM25.
4. Wrap every remaining token in double quotes with internal `"` doubled.
5. Join with `OR`. Require-all is too strict for natural questions; BM25 ranking handles
   the rest.
6. Append `*` to the final token for as-you-type prefix matching.

Result: `how did I mirror bones in Blender?` → `"mirror" OR "bones" OR "blender"*`.

If every token is a stopword, return no results rather than an FTS5 error.

Unit-test this function hard (spec §55). Fuzz it with arbitrary strings and assert it never
produces an expression SQLite rejects.

## 1.7 Search

```sql
SELECT s.id, s.document_id, s.heading, s.heading_path, s.body,
       s.start_line, s.end_line, d.path,
       bm25(sections_fts, 8.0, 4.0, 1.0) AS score
FROM sections_fts
JOIN sections   s ON s.id = sections_fts.rowid
JOIN documents  d ON d.id = s.document_id
WHERE sections_fts MATCH ?1
ORDER BY score
LIMIT ?2;
```

FTS5's `bm25()` returns a **negative** value where more negative is better, so plain
`ORDER BY score ASC` is correct — do not "fix" the sign.

Post-filters, from config, applied as score multipliers (spec §19, §47):

```text
status = obsolete   × 0.25
status = archived   × 0.4
status = draft      × 0.9
heading exact match × 1.30
```

Keep the weights in `[search.*]` config. Everything here gets re-tuned against the Stage 7
benchmark, so nothing should be a literal in the code.

Wrap the whole query path in a `tracing` span with the query id and record per-stage
durations from the start (spec §50) — retrofitting instrumentation after a latency problem
appears is how you end up guessing.

## 1.8 Opening the note

Spec §30. `std::process::Command` with separate args, **never** `sh -c`.

Template expansion is yours to implement: substitute `{path}`, `{line}`, `{url}`,
`{seconds}` into each argv element independently. A path containing a space, a quote, or a
`$` must survive unchanged — add a test for exactly that.

```rust
Command::new("ghostty").args(["-e", "nvim", "+41", "/home/nabi/brain/obs.md"]).spawn()?
```

Spawn detached (`setsid` via `process::CommandExt::process_group(0)`) so the opened editor
does not die with the daemon.

Hide the dock immediately on action activation — before the process spawns, not after.

## 1.9 UI

Results list replaces the fake answer. Show `path · heading path` per spec §44, primary
source prominent, `3 sources` when there are more, `Ctrl+S` expands the list.

**Search as you type**, debounced ~80 ms, cancelling the in-flight query by id. FTS5 over a
few tens of thousands of sections returns in single-digit milliseconds; live results feel
dramatically better than press-Enter-and-wait, and they set the expectation that Stage 2's
answer is an *addition* to instant results rather than a replacement for them.

---

## Definition of done

```text
Ask a keyword-style question and immediately jump to the correct Markdown section.
```

```bash
brainctl reindex && brainctl status      # document/section counts look right
brainctl ask "obs cursor follow"         # correct section ranked first
# edit a note in nvim, :w, then re-ask within ~1s — the change is already searchable
# Alt+1 opens ghostty+nvim at the exact line
```

Latency, from the tracing spans, on your real vault:

```text
FTS search:               < 20 ms
end-to-end query → UI:    < 100 ms
full reindex of ~/brain:  report it; it just needs to not be minutes
```

Golden parser tests and the FTS-escaping fuzz test pass under `cargo nextest run`.
