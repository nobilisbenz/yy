# Stage 5 — Semantic retrieval

**Goal:** a paraphrased question finds the right note even when it shares almost no words
with it.

**Prerequisite:** Stage 4 verification passes, **and** the Stage 7 benchmark harness exists
in at least skeleton form. Adding semantic search without a way to measure it means you
will never know whether it helped. If you have not built the harness yet, build it now —
see [`08-stage-7-benchmark.md`](08-stage-7-benchmark.md).

Spec §16 is right that this is not required for a good V1. Do not start it because it feels
like the "real" way to do search. Start it when you have logged real queries that BM25
missed.

---

## Deliverables

- Embedding service (second llama-server instance).
- `section_embeddings` table, background embedding queue.
- Brute-force vector search.
- Reciprocal rank fusion of lexical + semantic.
- Query embedding cache.

---

## 5.1 Embedding model

**Qwen3-Embedding-0.6B, Q8_0** (~650 MB). Strong on retrieval benchmarks, has official
GGUF, and — the reason it is the right pick here — supports **Matryoshka** dimension
truncation.

```bash
llama-server \
  --model ~/.local/share/brain/models/Qwen3-Embedding-0.6B-Q8_0.gguf \
  --embeddings --pooling last \
  --host 127.0.0.1 --port 8178 \
  --ctx-size 1024 --n-gpu-layers 99 \
  --ubatch-size 512
```

Two model-specific requirements that will produce quietly terrible results if missed:

1. **`--pooling last`.** Qwen3-Embedding uses last-token pooling. Mean pooling produces
   embeddings that look plausible and rank badly.
2. **Queries get an instruction prefix, documents do not.** Something like
   `Instruct: Given a question, retrieve the note section that answers it\nQuery: {q}`.
   This is asymmetric by design; embedding both sides identically loses several points of
   recall.

VRAM: 1.3 GB (answer model) + 0.7 GB (embedder) + KV caches ≈ 3 GB of your 6. Comfortable.
If it ever is not, run the embedder on CPU — it only runs on the indexing path and for one
short query string.

## 5.2 Storage and dimensionality

Native output is 1024 dims. **Truncate to 512 and re-normalize (L2).** Qwen3-Embedding is
trained with MRL so the leading dimensions carry most of the signal; you halve memory and
search time for a small, measurable recall cost. Measure it on your benchmark and keep
whichever wins — but 512 is the right default.

Schema from spec §16, with `dimensions` recorded per row so a model change is detectable.

Store as a raw little-endian `f32` blob. At 40k sections × 512 dims that is **80 MB** —
load it all into one contiguous `Vec<f32>` in RAM at startup, with a parallel
`Vec<SectionId>`.

## 5.3 Search: brute force, deliberately

No vector database. No HNSW. No `sqlite-vec`.

40k × 512 f32 dot products is 20M multiply-adds. With `rayon` over chunks and normalized
vectors (so cosine = dot product) that is **single-digit milliseconds**, against the
spec's 30 ms budget. An ANN index would add a dependency, a build step, an index-staleness
problem, and approximate results, to save time you do not need.

```rust
// vectors are L2-normalized, so cosine similarity == dot product
let scores: Vec<(SectionId, f32)> = matrix
    .par_chunks(DIM * 1024)
    .enumerate()
    .flat_map(|(chunk_i, chunk)| { /* dot products */ })
    .collect();
// then partial_sort for top-K
```

Revisit only if the corpus passes ~500k sections. Spec §69 lists "which vector DB is
fastest" as premature optimization; it is right.

## 5.4 Embedding queue

Spec §28's key property: **a saved note is searchable via FTS before its embedding
exists.** Never block the index transaction on the embedder.

- After a section is committed, enqueue its id.
- A background worker batches ~32 sections per `/v1/embeddings` call, low priority.
- Queue depth is reported by `brainctl status`.
- Queue survives restarts: derive it from `sections LEFT JOIN section_embeddings WHERE
  section_embeddings.section_id IS NULL` at startup rather than persisting a queue table.
- On model or dimension change, re-embed everything in the background while continuing to
  serve with the old vectors (spec §16). Keyed by `model_id` + `model_revision`.

**Query embeddings are cached** (spec §36 L2) — an LRU keyed on the normalized query. The
same question asked twice should not pay for the embedding twice.

## 5.5 Fusion

Reciprocal Rank Fusion, spec §19:

```text
RRF(d) = Σ_i  weight_i / (k + rank_i(d))     k = 60
```

Combine **ranks**, not scores. BM25 and cosine live on incompatible scales and any attempt
to normalize them into a shared range is a source of subtle, permanent ranking bugs.

```toml
[search.fusion]
k = 60
lexical_weight  = 1.0
semantic_weight = 1.0
```

Retrieve `lexical_limit = 30` and `semantic_limit = 30`, fuse, apply the Stage 4 context
multipliers to the fused score, take `fused_limit = 20`, and pass `context_sections = 5`
to the prompt.

Keep lexical-only reachable at runtime (`brainctl ask --lexical`) — both for degradation
when the embedder is down (spec §51) and for A/B comparison on the benchmark.

---

## Definition of done

```text
Paraphrased questions retrieve notes even when they share few exact words.
```

Gated on numbers, not on feel:

```bash
cargo run -p brain-bench -- --mode lexical
cargo run -p brain-bench -- --mode hybrid
```

- Hybrid beats lexical on Recall@3 and MRR over your real benchmark set. **If it does not,
  do not ship it** — fix the prefix/pooling first, and if it still does not, that is a real
  answer about your corpus.
- Vector search stays under 30 ms at your corpus size.
- Embedding queue drains without stalling indexing; a note saved now is FTS-searchable
  immediately and semantically searchable within seconds.
- Kill the embedding server: queries continue lexical-only with no user-visible error.
