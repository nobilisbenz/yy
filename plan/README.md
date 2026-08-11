# Brain Dock — Build Plan

Verdict, stack decisions, and staged build files derived from `../brain-dock-spec.md`.

## Verdict: doable

Nothing in the spec requires unproven technology. Every subsystem is a known-solved
problem with a mature Rust crate or an off-the-shelf binary. The machine this was
checked on already satisfies the environment assumptions:

| Spec assumption | Reality on this machine | Status |
|---|---|---|
| Ubuntu 26 | Ubuntu 26.04 LTS (Resolute Raccoon) | ok |
| X11 + i3 | `XDG_SESSION_TYPE=x11`, `XDG_CURRENT_DESKTOP=i3`, `/usr/bin/i3` | ok |
| Rust | rustc 1.97.1 / cargo 1.97.1 | ok |
| SQLite FTS5 | system sqlite3 has `ENABLE_FTS5` (we bundle anyway) | ok |
| RTX 3060 laptop | RTX 3060 Laptop, **6144 MiB VRAM**, driver 595.84 | ok |
| Qwen3 1.7B Q4 | ~1.1 GB — fits with room for a 4B fallback profile | ok |
| Openers (nvim, ghostty, gtk-launch, xdg-open) | all present | ok |
| Disk | 72 GB free on `/` | ok |
| RAM | 26 GB | ok |

Setup is **complete and verified** — see [`00b-machine-baseline.md`](00b-machine-baseline.md)
for the measured state, including the llama.cpp CUDA benchmark (pp2048 ≈ 6555 t/s,
tg128 ≈ 168 t/s), which comfortably clears the spec's latency targets.

The honest risk is not feasibility — it is **scope**. The spec is a V1 checklist of ~22
items plus 8 phases plus 6 "future" sections. Built end-to-end that is a multi-month
solo project. The plan below front-loads the part that decides whether the product is
worth finishing (Stages 0–2, roughly the spec's "minimal vertical slice") and treats
everything after that as optional increments, each independently shippable.

## Stage files

| # | File | Outcome when done |
|---|---|---|
| — | [`00-setup.md`](00-setup.md) | Everything to install before writing code — **done** |
| — | [`00b-machine-baseline.md`](00b-machine-baseline.md) | **Verified machine state + measured numbers. Read this first.** |
| — | [`10-kickoff.md`](10-kickoff.md) | **The first coding session, commit by commit** |
| 0 | [`01-stage-0-dock.md`](01-stage-0-dock.md) | ✅ **done** — `$mod+a` shows a focused, floating, top-right dock with a streamed mock answer |
| 1 | [`02-stage-1-index-search.md`](02-stage-1-index-search.md) | Type a question, jump to the right Markdown section in nvim |
| 2 | [`03-stage-2-llm.md`](03-stage-2-llm.md) | Grounded streamed answer from Qwen3 over retrieved sections |
| 3 | [`04-stage-3-actions.md`](04-stage-3-actions.md) | `Alt+1..9` jumps to note, code, video timestamp, app, project |
| 4 | [`05-stage-4-context.md`](05-stage-4-context.md) | Same query ranks differently depending on the focused window |
| 5 | [`06-stage-5-semantic.md`](06-stage-5-semantic.md) | Paraphrases retrieve the right note |
| 6 | [`07-stage-6-corrections.md`](07-stage-6-corrections.md) | Correct once; similar future questions use the correction |
| 7 | [`08-stage-7-benchmark.md`](08-stage-7-benchmark.md) | Recall/MRR/latency numbers gate every further change |
| — | [`09-decisions.md`](09-decisions.md) | Stack rationale, deviations from the spec, open risks |

**Stages 0–2 are the product.** If that slice does not feel good, stop and rethink rather
than continuing to Stage 3.

## Stack (final)

Mostly the spec's recommendation. Where this plan differs, see [`09-decisions.md`](09-decisions.md).

| Layer | Choice | Note |
|---|---|---|
| Language | Rust (edition 2024) | as spec |
| Async | Tokio (multi-thread) — **backend threads only** | Slint owns the main thread |
| UI | Slint, `renderer-femtovg` | as spec |
| X11 | `x11rb` (`randr`, `xproto`) + `raw-window-handle` | direct XID manipulation |
| Compositing | `picom` | **required** for rounded/transparent dock |
| DB | `rusqlite` (`bundled` feature) + WAL | bundled pins the FTS5/SQLite version |
| Lexical search | FTS5 + `bm25()` column weights | as spec |
| Markdown | `pulldown-cmark` `into_offset_iter()` | byte offsets → line numbers |
| Front matter | `serde_yaml_ng` (or pulldown metadata blocks) | |
| Walk / ignore | `ignore` + `globset` | ripgrep's engine, gitignore-aware |
| Watch | `notify` + `notify-debouncer-full` | handles atomic-save renames |
| Hash | `blake3` | change gate before reparse |
| IPC | Unix socket + JSON Lines (`tokio_util` `LinesCodec`) | as spec |
| Inference | `llama-server` child process, HTTP/SSE | Option A from spec §22 |
| LLM | Qwen3 1.7B **Q5_K_M** (not Q4) | 6 GB VRAM makes Q4 an unforced quality loss |
| Embeddings (St. 5) | Qwen3-Embedding-0.6B GGUF, MRL-truncated to 512d | second llama-server instance |
| Vector search (St. 5) | brute-force f32 + `rayon` | 40k×512d ≈ 80 MB, <10 ms; no vector DB |
| Reranker (St. 7+) | Qwen3-Reranker-0.6B, off by default | only if the benchmark justifies it |
| Config | `toml` + `serde` + `etcetera` (XDG) | |
| Logging | `tracing` + `tracing-subscriber` | one span per query |
| Errors | `thiserror` (libs) + `anyhow` (bins) | |

## Repository layout

Fewer crates than the spec's 14. Split later, when a boundary actually hurts.

```text
brain-dock/
├── Cargo.toml                  # workspace
├── crates/
│   ├── brain-core/             # ids, Document, Section, Action, Answer, config, errors
│   ├── brain-proto/            # IPC message enums, framing, socket path  (shared by all 3 bins)
│   ├── brain-index/            # db, migrations, parsers, indexer, watcher
│   ├── brain-engine/           # retrieval, ranking, prompt, llm client, actions, corrections
│   ├── brain-x11/              # EWMH, RandR, active-window/context, window control
│   ├── brain-daemon/           # bin
│   ├── brain-dock/             # bin  (Slint)
│   └── brainctl/               # bin
├── ui/                         # dock.slint, components/, tokens.slint
├── migrations/                 # 001_initial.sql …
├── config/brain.example.toml
├── benchmarks/retrieval.yaml
├── tests/fixtures/vault/       # reproducible test corpus
└── scripts/                    # dev.sh, models.sh, bench.sh
```

## How to use these files

Each stage file is written to be handed to an implementer (human or agent) on its own.
It contains: goal, prerequisites, deliverables, the techniques and gotchas that matter,
a definition of done, and shell commands that verify it. Work them in order; do not
start a stage before its predecessor's verification passes.
