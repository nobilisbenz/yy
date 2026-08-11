# Decisions, deviations, and risks

Why the stack is what it is, where this plan departs from `brain-dock-spec.md`, and what is
most likely to go wrong.

---

## Where this plan agrees with the spec

The spec's architectural core is sound and is adopted unchanged:

- **Hot daemon, thin UI.** The single most important decision in the document. It is what
  makes the <50 ms summon possible at all.
- **Retrieval is the brain, LLM is the narrator** (§3.2). Correct, and the reason a 1.7B
  model is viable.
- **Actions are trusted data, never model output** (§3.3, §48). Structurally enforced by
  having no `RunShell` variant in the enum.
- **Sections, not fixed-size chunks** (§14). This is what makes `nvim +line` precise and
  what makes heading-weighted BM25 work.
- **FTS5 before embeddings** (§16). Prevents weeks of semantic-search tuning before the UX
  is proven.
- **llama-server child process over FFI** (§22 Option A). Right call at this scale.
- **Correction memory before fine-tuning** (§33, §35). Facts belong in the knowledge store.
- **Build your own retrieval benchmark** (§56). More valuable than any public benchmark.

---

## Deviations

### 0. iced, not Slint — the UI toolkit

The spec does not name a toolkit; the original plan chose Slint and Stage 0 shipped on it.
The dock is now moving to **iced 0.14 + `iced_wgpu`**.

Two reasons. The author prefers working in iced, which for a solo multi-month project is a
real input and not a soft one. And the graph panel (`PLAN.md` §7) needs custom wgpu inside
the dock window — on Slint that meant `unstable-wgpu-28` forcing the whole renderer onto
wgpu, stacked on the two unstable features Stage 0 already depended on.

iced 0.14 covers every hard requirement with **stable** API: `application_id` for
`WM_CLASS` before first map, `Mode::Hidden` for show/hide, `widget::shader` for the panel,
`transparent` for the ARGB visual, `raw_id()` for the XID, and `iced::daemon()`
for a runtime that starts windowless. Net effect is three unstable dependencies removed.

Cost: `brain-dock` and `ui/` are rewritten, and Stage 0's measurements must be re-earned.
`brain-core`, `brain-proto`, `brain-daemon`, `brainctl`, and nearly all of `brain-x11` are
untouched — the i3 and X11 findings below are toolkit-independent. Full accounting in
`PLAN.md` §1.

### 0b. `brain-index` wraps `yalive` — it does not reimplement it

Stage 1 as written has this workspace build its own `migrations/001_initial.sql`, its own
`sections` table, its own `pulldown-cmark` parser, its own FTS5, and its own watcher. **It
does none of that.** `brain-index` is a wrapper over `yalive::db` and `yalive::parser`
(`PLAN.md` §2.2, option A).

The reason is identity, not effort. `yalive` keys everything on `section_uid`, and
`relations`, `cards`, and `review_state` hang off it. A second parser with its own notion
of a section would mean the graph could not be shared with `yGraphy`, review state could
not inform ranking, and `Alt+1` would jump to a line computed by a *different parser* than
the one that built the graph on screen.

**What this changes about Stage 1.** Everything below that is genuinely better than what
`yalive` had — `tokenchars '_-.'`, external-content FTS5, a migration runner, FTS5 query
escaping, `heading_path`, one-writer/N-readers — is still built, but as an improvement
*there*, where `yGraphy` and `yReviewy` inherit it. Same work, spent once. **All of it has
now landed**, plus front-matter `status:`, which the `[search.status_weight]` block had
been configuring into a void since it was never parsed.

One placement in this section did not survive contact — see §11.

**What it does not change.** `yy` deliberately indexes more than a vault — `~/projects`,
code, PDFs, up to 50k files — and `yalive` is a Markdown vault indexer. Two databases is
fine; two definitions of a section is not. The vault DB is read through `yalive`, and
non-vault sources get a second store whose rows carry `source_kind` and no `section_uid`.
Retrieval fuses both; only vault rows participate in graph expansion and review-state
ranking.

### 1. Q5_K_M instead of Q4 — quantization

Spec §21 specifies Q4. With 6 GB of VRAM and a 1.7B model, Q4 (~1.1 GB) versus Q5_K_M
(~1.3 GB) is 200 MB of memory you are not short of, in exchange for measurably less
quantization damage on a model that is already small enough to be fragile. Q4 is the right
default for a 7B+ model on constrained hardware; here it is an unforced loss.

### 2. Show/hide without destroying the window

Spec §6 says "hidden rather than destroyed when dismissed" but does not say how. Toolkit
hide/show can recreate the underlying window, losing the XID and every property set on it
— **Slint's `hide()` did exactly that**, and also exited the event loop, which is why
Stage 0 drove `map_window`/`unmap_window` through `x11rb` instead.

On iced 0.14 the intended primitive is `window::set_mode(id, Mode::Hidden)`, which
should be winit `set_visible(false)` — an X11 unmap that preserves the window. **Verify
this before building on it** (Stage 0′). If it destroys or recreates the window, fall back
to the proven path: take the XID from `iced::window::raw_id()` and drive
map/unmap with `x11rb`, exactly as `brain-x11` does today. Either way the window lives for
the whole session and only its mapping toggles; that is what makes the latency target
reachable and the properties stable.

### 3. Send `Sources` and `Actions` before generation, not after

Spec §25 lists `Actions` after the token stream. But retrieval completes in ~100 ms and
generation takes many hundreds of milliseconds. Rendering the source path and action
buttons the instant retrieval finishes, then filling in prose, makes the tool feel
immediate — and the actions are frequently all the user needed. It also means a failed
generation still leaves a useful result on screen.

### 4. Search-as-you-type, not submit-on-Enter

Spec §5 has `Enter` submit the query. Keep that for generation, but run *retrieval* live on
an 80 ms debounce. FTS5 is fast enough that live results are free, and it reframes the LLM
answer as a bonus on top of instant search rather than the only output.

### 5. Explicit FTS5 query escaping

Not mentioned in the spec, and it will break on the first query containing punctuation.
`MATCH` takes an expression language, not a string. Stage 1 §1.6 specifies the tokenize →
quote → OR construction. This is the most likely thing to be discovered as a crash in
week one.

### 6. `tokenchars '_-.'` in the FTS5 tokenizer

Spec §15 uses plain `unicode61`. That splits `calculate_pivot` into two tokens and destroys
precision on any corpus containing code or filenames — which is the explicit use case in
spec §63. One tokenizer option, large effect.

### 7. Brute-force vector search instead of a vector index

Spec §16 says "pluggable". Concretely: at 40k sections with 512-dim vectors, brute force in
`rayon` is single-digit milliseconds against a 30 ms budget. An ANN index adds a
dependency, an index-rebuild problem, and approximation, for no benefit until ~500k
sections. Consistent with spec §69's warning about vector-DB shopping.

### 8. Fewer crates at the start

Spec §9 lists 14 crates; §9 itself then says not to over-split on day one. This plan starts
with 5 libraries and 3 binaries. Crate boundaries are cheap to add and expensive to guess
wrong.

### 9. Build the benchmark harness early, not at Stage 7

Spec §58 puts it in Phase 7. But Stages 5 and 7 both make changes that are unfalsifiable
without it. Build the skeleton after Stage 2.

### 10. systemd user unit for the daemon

Spec §52 mentions it as "better later". Do it as part of V1: i3 `exec` gives no
restart-on-crash and no journal, and a daemon that silently dies is the worst failure mode
this product has.

### 11. FTS5 escaping lives in `yalive::search`, not `brain-index::fts`

§0b put the escaping in `yy` on the reasoning that the expression language is `yy`'s problem
while the schema is `yalive`'s. That line does not hold: **what counts as a term is decided
by the tokenizer**, `tokenchars '_-.'`, which is part of the schema. Two copies of the
escaping would be two things that have to keep agreeing with one tokenizer, and they would
drift the first time the tokenizer changed.

`yalive` cannot depend on `yy`, so the shared implementation has to live in `yalive`.
`brain-index::fts` is now a re-export. This also fixed a real bug for free: `yalive`'s own
TUI search built its expression by hand and threw on the first query containing an
apostrophe.

### 12. Retrieval joins query terms with `OR`, the TUI filter with `AND`

Stage 1 §1.6 specifies `OR`; the first implementation used `AND`. Both are right, for
different callers, so `yalive::search::Mode` names the choice:

- **`Mode::All`** — the TUI's filter list, where each keystroke should *remove* rows.
- **`Mode::Any`** — retrieval, with stopwords dropped first.

Measured on the real vault, and the reason this is not a matter of taste:

```text
"how do I schedule a nightly backup?"   All:  0 hits
                                        Any: 13 hits, "Nightly pg_dump to S3" first
```

`All` returns nothing at exactly the moment the user asks a real question, because no single
section contains every word of a natural sentence. BM25 does the separating, and the seeds
feed graph expansion, so recall is what matters at this stage.

The stopword list is compared against a **punctuation-trimmed** form. Testing the raw word
lets `for?` through as a content term, which then earns a heading-match boost on every note
whose heading contains "for" — observed on the real vault before it was fixed.

---

## Risks, ranked

### High — transparency and window control on X11 (re-opened by the iced port)

The dock's whole visual identity depends on a translucent, rounded, shadowed window. That
needs an ARGB visual through winit, a running compositor, and the toolkit cooperating. If
any link fails you fall back to an opaque card, which is fine but is not the reference
design.

Stage 0 **proved this on Slint** (depth-32 ARGB visual, picom on `egl`). The iced port
re-opens it: `iced_wgpu` renders through wgpu/Vulkan rather than femtovg/GL, and
`window::Settings.transparent` has to produce the same ARGB visual.

*Mitigation:* prove it in the first hours of Stage 0′, before building anything on it —
together with `Mode::Hidden` (deviation 2) and `application_id` → `WM_CLASS`. The opaque
fallback is documented and acceptable.

### High — scope

Spec §68's V1 is 22 items across 8 phases, with 6 further "future" sections. This is a
multi-month solo project if built end to end.

*Mitigation:* Stages 0–2 are the product; ship and live with them before continuing. Every
stage after that is independently useful and independently abandonable.

### ~~Medium — TTFT with a real context pack~~ → resolved

Measured: **pp2048 = 6555 t/s**, so a full ~2000-token context pack prefills in ~310 ms and
TTFT lands around 350 ms cold, lower with `--cache-reuse`. The spec's <500 ms target has
headroom, and `context_sections` can be chosen for retrieval quality rather than speed.
Full numbers in [`00b-machine-baseline.md`](00b-machine-baseline.md).

### Medium — Qwen3-1.7B answer quality

A 1.7B model with good retrieved context is fine for "restate this note concisely". It is
not fine for synthesis across sources, and it will occasionally ignore the "say you don't
know" instruction.

*Mitigation:* the prompt contract is deliberately narrow; the no-answer path is triggered
by a retrieval-confidence threshold *before* the model is called, not by the model's
judgement. The 4B quality profile exists as a config switch.

### Medium — desktop context extraction for terminals

`_NET_WM_PID` on a terminal gives the terminal, not the nvim inside it. The `/proc` child
descent works but is fiddly and varies by terminal.

*Mitigation:* every field is `Option`; context only boosts, never filters; there is a kill
switch.

### Low — FTS5 external-content drift

Hand-maintained external-content FTS tables desynchronize silently. Solved by using
triggers (Stage 1 §1.2).

### Low — llama.cpp API churn

The server's flags and request fields move. `--reasoning-budget`, `chat_template_kwargs`,
and `--flash-attn`'s spelling have all changed within releases.

*Mitigation:* pin the built commit in the README, and verify each flag against
`llama-server --help` on your build rather than trusting this document. **Done for build
10358 (`030ebb558`)** — every flag Stages 2 and 5 need is present, including
`--reasoning-budget` and `--chat-template-kwargs`. Re-run that check after any rebuild.

---

## Open questions to settle during the build

| Question | Decide at | How |
|---|---|---|
| ~~CUDA or Vulkan backend~~ | ~~Setup~~ | **Settled: CUDA. pp2048 = 6555 t/s, tg128 = 168 t/s** |
| Porter stemmer on or off | Stage 7 | benchmark sweep |
| 512 vs 1024 embedding dims | Stage 5 | benchmark; 512 is the default |
| Is the reranker worth 100 ms | Stage 7 | benchmark; default off |
| Live results vs submit-only | Stage 1 | use it for a week |
| Does context boosting actually help | Stage 4/7 | benchmark with and without |
| Should corrections return verbatim or via the model | Stage 6 | exact match verbatim, paraphrase via model |

---

## The thing to keep in view

Spec §69 gets the last word, and it is the correct framing:

```text
shortcut → ask → correct source → short useful answer → one-keystroke jump
```

Every decision above should be checked against whether it makes that loop faster or more
reliable. Nothing else in the spec — not embeddings, not rerankers, not LoRA, not the
knowledge graph — matters until that loop is good.
