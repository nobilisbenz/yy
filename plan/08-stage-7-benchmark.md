# Stage 7 — Benchmark, tune, and V1 close-out

**Goal:** every further change to ranking, prompting, or models is decided by a number
rather than by an impression.

Spec §56 is right that this matters more than any generic model benchmark. It is listed
last because it is the least fun; **build the skeleton earlier** — ideally right after
Stage 2, and certainly before Stage 5. Semantic search and rerankers are unfalsifiable
without it.

---

## Deliverables

- `benchmarks/retrieval.yaml` with 40+ real questions from your own usage.
- `brain-bench` binary: Recall@1/3/5, MRR, latency p50/p95.
- Latency benchmark over the full query path.
- Debug source view (`Ctrl+Shift+S`) showing per-stage scores and ranks.
- Config-driven ranking experiments.
- The V1 checklist (spec §68) verified end to end.

---

## 7.1 The benchmark set

Format per spec §56, extended with the Stage 4 context field.

**Source the questions from `query_history`, not from imagination.** After Stage 6 you have
a log of what you actually asked and which sources you actually opened. Export the ones
where you clicked through to a source — that click is a ground-truth label you already
collected for free.

```bash
brainctl bench export --from-history --min-queries 40 > benchmarks/retrieval.yaml
```

Then hand-correct it. Forty real questions beat four hundred invented ones.

Run against a **frozen fixture vault** (`tests/fixtures/vault/`) for regression testing, and
against your live vault for tuning. Two different jobs: the fixture catches regressions in
CI-style runs; the live vault tells you whether the system is actually good.

## 7.2 The harness

```bash
cargo run -p brain-bench -- --mode hybrid --config benchmarks/tuning/a.toml
```

Output a table and a JSON report:

```text
mode=hybrid  n=47
Recall@1  0.62      MRR      0.71
Recall@3  0.83      p50      94 ms
Recall@5  0.89      p95     210 ms
```

Two features that make it worth using:

- **Diff against the previous run.** Print which questions moved up and which moved down.
  An aggregate that improves by 2% while breaking five queries you care about is not an
  improvement, and only a per-question diff shows that.
- **Sweep mode.** Take a config with ranges and grid-search the boost weights:
  `--sweep search.context_boost.active_app=1.0,1.25,1.5`. This is the whole reason spec §19
  insists the weights live in config.

Separate retrieval from generation (spec §57). The harness measures retrieval only.
Answer quality gets its own small, manual, held-out set — there is no honest automated
metric for it at this scale, and an LLM-judge over 40 items is noise.

## 7.3 Debug view

`Ctrl+Shift+S` opens a developer panel the normal UI never shows (spec §44):

```text
1. blender/rigging.md · Mirror bones
   bm25 -8.42 (rank 1)   cos 0.71 (rank 3)   rrf 0.0310   ctx ×1.25   final 0.0388
2. ...

timings: ctx 4ms  corr 2ms  fts 11ms  vec 8ms  fuse 1ms  prompt 6ms  ttft 340ms  gen 890ms
cache: retrieval MISS  answer MISS   index_gen 4127
```

Behind `[logging] debug_ui = true`. You will use this more than the benchmark for
diagnosing a single bad result.

## 7.4 Tuning order

Work the biggest lever first:

1. **BM25 column weights.** `bm25(sections_fts, H, P, B)` — sweep heading and heading-path
   weights. Usually the largest single win on a notes corpus.
2. **Stopword list and prefix matching** in query normalization.
3. **Stemmer on/off** (the question left open in Stage 1).
4. **Chunk sizes.** `max_section_tokens` / `subchunk_target_tokens`. Requires a reindex per
   run, so batch these.
5. **Fusion weights and `k`.**
6. **Context boosts.**
7. **`context_sections`** — more sources is not better; it dilutes attention and raises TTFT.

Record every run's config and result in `benchmarks/results/`. Commit them. Six months
later you will want to know why `active_app = 1.25`.

## 7.5 Only now: the extras

Spec §20 and §58 both say do not add a reranker until hybrid works. Stronger version: do
not add one until the benchmark shows hybrid retrieval is your bottleneck and generation is
not.

**Reranker** — Qwen3-Reranker-0.6B, over the top 20–40 candidates, `enabled = false` by
default. It costs 50–150 ms. Ship it only if Recall@1 improves enough to be worth that on
your set.

**Quality model profile** — swap in Qwen3-4B-Instruct-2507-Q4_K_M via `[llm] profile =
"quality"`. Manual switch only; no automatic routing (spec §39).

**Everything else** — PDF ingestion, tree-sitter code symbols (spec §63), OCR, voice input
(spec §62), the relationship graph (spec §64), LoRA (spec §66) — is post-V1. Each is a
project. None of them are needed for the loop in spec §69, which is the thing that decides
whether Brain Dock is good.

## 7.6 V1 checklist

Verify spec §68 item by item, honestly, and check them off in the repo README:

- [ ] Ubuntu/X11/i3 only
- [ ] Super+Space toggles dock
- [ ] Dock opens at top-right of active monitor
- [ ] Frameless and floating
- [ ] Markdown roots configured through TOML
- [ ] Incremental indexing
- [ ] Section-based Markdown parsing
- [ ] SQLite FTS5 search
- [ ] Qwen3 through llama.cpp
- [ ] Model stays loaded
- [ ] Answer streaming
- [ ] Source path and heading shown
- [ ] Open note at exact line in Neovim
- [ ] Open linked video/timestamp
- [ ] Launch referenced desktop app
- [ ] Open referenced project/code
- [ ] Query and retrieval caches
- [ ] Correct/wrong feedback
- [ ] Editable correction memory
- [ ] Active X11 application context
- [ ] Graceful lexical-only fallback if LLM fails
- [ ] Personal retrieval benchmark
- [ ] `brainctl status` diagnostics

Plus two the spec omits but you will want before calling it done:

- [ ] User systemd unit for `brain-daemon` (spec §52 mentions it; do it — i3 `exec` gives no
      restart-on-crash and no journal)
- [ ] `brainctl doctor` catches a broken config, missing model, missing opener binary, and a
      dead llama-server, with actionable messages

---

## Definition of done

```text
Retrieval quality is a number you can move on purpose, and V1 is checked off.
```

- `cargo run -p brain-bench` runs clean and its numbers are committed.
- A ranking change can be justified with a before/after diff.
- The frozen-fixture run is wired into `cargo nextest` as a regression test with a floor
  (e.g. assert Recall@3 ≥ 0.75) so a parser or ranking regression fails the test suite.
