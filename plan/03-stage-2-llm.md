# Stage 2 — Grounded answers from Qwen3

**Goal:** question → retrieved sections → short grounded answer streaming into the dock.

This closes the loop. Stages 0–2 together are the spec's "minimal vertical slice" (§60) and
the point at which you can honestly judge whether Brain Dock is worth building further.

**Prerequisite:** Stage 1 verification passes.

---

## Deliverables

- `llama-server` supervised as a daemon child process, with health checks and restart.
- Context pack builder with a real token budget.
- Restrictive system prompt, versioned.
- SSE streaming through the daemon to the dock.
- Answer + retrieval caches.
- Graceful degradation to Stage 1 behaviour when the LLM is unavailable.
- `brainctl status` reporting model state, backend, context, last TTFT.

---

## 2.1 Process supervision

Spec §22 Option A — child `llama-server` over localhost HTTP. Correct call: the FFI
binding path costs build complexity and buys nothing at this scale.

All flags below are **verified present** in the pinned build (10358 / `030ebb558`) — see
[`00b-machine-baseline.md`](00b-machine-baseline.md). Note the model's actual filename.

```bash
llama-server \
  --model ~/.local/share/brain/models/Qwen_Qwen3-1.7B-Q5_K_M.gguf \
  --alias brain-fast \
  --host 127.0.0.1 --port 8177 \
  --ctx-size 4096 \
  --n-gpu-layers 99 \
  --flash-attn on \
  --cache-reuse 256 \
  --slots \
  --parallel 1 \
  --no-webui
```

Notes on each flag that matters:

- **`--n-gpu-layers 99`** — full offload. The model is 1.37 GiB against 5806 MiB of VRAM,
  and on this machine X runs on the AMD iGPU, so **nothing else is using the 3060**. Full
  offload is free here; `llama-bench` already confirmed it loads and runs on CUDA.
- **`--flash-attn on`** — large win on prefill, which is what you are actually paying for.
- **`--cache-reuse 256`** — this is the important one. Keep the system prompt as a stable
  prefix and llama.cpp reuses its KV cache across queries instead of reprocessing it every
  time. Order the prompt **system → sources → question** so the invariant part is first.
- **`--port`** — bind `127.0.0.1` only, and pick the port from config. If the port is taken,
  fail with a clear message rather than assuming the existing server is yours.

Supervision in the daemon:

- `tokio::process::Command` with `.kill_on_drop(true)`, plus an explicit process group so
  the server dies with the daemon rather than orphaning and holding VRAM.
- Poll `GET /health` until ready. First load of a 1.3 GB model is a few seconds; the daemon
  must start and serve lexical search during that window, not block.
- Restart on exit with exponential backoff, capped, with a circuit breaker after N failures
  that flips the daemon into permanent lexical-only mode and says so in `brainctl status`.
- **Do not lazily load on first query** (spec §37). The model loads at daemon startup and
  stays resident. This is the whole reason a daemon exists.

Config should also allow `backend = "external"`, meaning "a llama-server is already running
at this URL, do not manage it". Invaluable when debugging prompts by hand.

## 2.2 Context pack

Budget from a 4096 context: reserve ~350 for output, ~250 for system prompt and scaffolding,
leaving ~3400 for sources. `[search] context_sections = 5` is the starting point.

**Measured prefill on this machine is 6555 t/s**, so a full ~2000-token pack costs ~310 ms
of prefill — the dominant term in TTFT, but well inside the 500 ms target. This means the
context pack is *not* the thing to squeeze for latency. Size it for retrieval quality and
revisit only if Stage 7 shows more sources hurting answer quality (they often do, by
diluting attention — a quality argument, not a speed one).

Build order:

1. Take the top N sections from Stage 1 retrieval.
2. Deduplicate: if a parent section and its subchunk both appear, keep the subchunk but
   render the parent's heading path.
3. Merge adjacent sections from the same document into one block — a section and the one
   after it read as continuous prose, and splitting them wastes tokens on repeated headers.
4. Fill greedily by score until the budget is spent. If the top section alone exceeds the
   budget, truncate it at a paragraph boundary and mark it truncated.

**Count tokens properly.** Do not ship the `len/3.6` estimate into the prompt path — call
llama-server's `POST /tokenize` (local, sub-millisecond) for the assembled prompt and trim
if over budget. An overflowing prompt gets silently truncated *from the left* by the server,
which decapitates your system prompt and produces baffling behaviour.

Render exactly as spec §23 — `path`, `heading`, `lines`, `text` per numbered source, plus
the desktop context line and the ACTIONS list.

**Sources are untrusted input** (spec §48). A note containing "ignore previous instructions"
is a document you wrote or downloaded, and the model will read it. Two cheap mitigations:

- Wrap each source in explicit delimiters and state in the system prompt that everything
  between them is data, never instruction.
- Strip control characters and cap any single source's length.

You cannot fully solve prompt injection here, and you do not need to: the model has no
tools, no filesystem access, and cannot create actions. The blast radius of a successful
injection is a wrong sentence of text.

## 2.3 Prompt

Take spec §23's rules verbatim; they are well-constructed. Add:

```text
8. Answer in at most 4 sentences unless the source describes numbered steps.
9. Never repeat the question back.
10. If sources conflict, prefer the one marked most recent and say they conflict.
```

**Version the prompt.** `prompt_version` in `meta`, included in the answer cache key
(spec §36, §67). Changing the prompt without bumping it means serving cached answers from
the old one.

**Qwen3 thinking mode.** Qwen3-1.7B is a hybrid model and will emit `<think>…</think>` by
default, which is fatal for a sub-500 ms TTFT target. Disable it:

- request body: `"chat_template_kwargs": {"enable_thinking": false}`, and/or
- server flag: `--reasoning-budget 0`.

Belt and braces: strip any `<think>…</think>` span from the stream in the daemon before
forwarding tokens. Verify against the llama.cpp build you pinned — this API has moved
around.

Generation params from spec §21: `temperature 0.15`, `top_p 0.85`, `max_tokens 350`. Also
set `"cache_prompt": true`.

## 2.4 Streaming

`POST /v1/chat/completions` with `"stream": true` → SSE. `reqwest` + `bytes_stream()`, parse
`data: ` lines yourself (it is ~30 lines and avoids an SSE crate's opinions about
reconnection).

Pipeline the events so the user sees progress:

```text
QueryAccepted → RetrievalStarted → RetrievalComplete{n} → Sources → GenerationStarted
              → Token … Token → Actions → Complete{timing}
```

**Send `Sources` before generation starts.** Retrieval finishes in ~100 ms; generation
takes an order of magnitude longer. Showing the source path and actions immediately, then
filling in the prose, makes the whole thing feel instant and is strictly better UX than
waiting. This is a change worth making relative to the spec's ordering.

**Cancellation.** A new query cancels the previous one: `tokio_util::sync::CancellationToken`,
drop the SSE stream, and send `POST /slots/{id}?action=erase` if a slot is held. Every event
carries its query id and the dock discards events for stale ids.

## 2.5 Caching

Spec §36's five layers, but only build L3 and L5 now. L1 and L2 are premature.

**Retrieval cache** — key: `normalized_query + context_bucket + index_generation + retrieval_config_version`.
In-memory LRU (`moka` or `lru`), ~500 entries. Invalidated wholesale by the generation
counter; source-aware invalidation is a Stage 7 refinement if it ever matters.

**Answer cache** — key: `hash(section_ids + their content_hashes) + model_id + prompt_version + gen_params + correction_version`.
Persist to SQLite so it survives a daemon restart. Including the section content hashes is
what stops a stale answer outliving an edited note, and it is the reason this is worth
doing at all.

A cache hit should render **immediately, not replayed token by token**. Fake streaming a
cached answer is a lie that costs latency.

## 2.6 Degradation

Spec §51. The dock must never become unusable because inference broke.

| Failure | Behaviour |
|---|---|
| llama-server not yet loaded | Sources + actions render; answer area shows "model loading" |
| llama-server crashed | Sources + actions only; status line notes lexical-only mode |
| Generation timed out (>15 s) | Cancel, keep whatever streamed, mark incomplete |
| Retrieval confidence below threshold | Spec §45 no-answer card with closest matches — **do not call the model at all** |

The no-answer path is a feature, not an error. `general_knowledge_fallback = false` is the
default and the model must never be asked to improvise from pretraining unless the user
turns that on.

Set the confidence threshold on the top BM25 score and tune it in Stage 7. Getting a
confident "I don't have that" is worth more than a plausible fabrication.

---

## Definition of done

```text
Question → retrieved sections → short grounded answer → source → Alt+1 opens the note.
```

Measured on your real vault, recorded in the repo:

```bash
brainctl status
# llm            qwen3-1.7b-q5_k_m
# backend        CUDA          (or Vulkan)
# model state    loaded
# context        4096
# last query     84 ms retrieval / NNN ms TTFT
```

- TTFT is reported honestly, whatever it is. Spec §21 targets <500 ms warm; if your build
  lands at 800 ms, write that down and decide whether to switch backends or shrink the
  context pack. Do not paper over it.
- Kill `llama-server` mid-session: the dock keeps working, degraded, with no error dialog.
- Ask the same question twice: the second is a cache hit and renders instantly.
- Ask something absent from the vault: you get the no-answer card, not an invention.
- Ask something whose source contains no command: the answer does not contain a command.

**Stop here and use it for a week before starting Stage 3.** Stages 3–7 are all
refinements of a loop that this stage either validates or does not.
