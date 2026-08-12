# Stage 6 — Correction memory

> **Status: the exact-match path works and ships on; the fuzzy path is built, measured, and
> off by default.**
>
> ```text
> correct "how do I deploy?" once
> re-ask, same question        → the corrected answer, 2 ms, no model call
> re-ask, "how should I deploy" → same (stopwords normalise away)
> ask about backups            → correction does not leak
> edit a source note           → marked stale within seconds; brainctl status reports it
> ```
>
> **The fuzzy path does not work at this model size, and that is the finding.** A reworded
> question does match — the lookup logs `matched=Some(("deploy", Fuzzy))` — and the
> correction is injected. Qwen3-1.7B then answers from the retrieved sources and ignores it.
> Tried, in order: an explicit system rule (rule 11), moving the block from the top of the
> prompt to immediately before the question, and cutting the competing sources from five to
> one. None changed the answer. So `[corrections] fuzzy` defaults to **false**: exact matches
> need no model and are exact, and the fuzzy path waits for `profile = "quality"` or for the
> embeddings that would justify returning it verbatim.
>
> This stage's stated prerequisite is Stage 5, which is deferred, so matching is lexical —
> §6.3's "cheap parallel path" without the semantic one beside it. A question that shares no
> vocabulary with the stored one is missed, which is recorded as a test rather than hidden.
>
> Not built: `query_sources` with per-stage ranks (§6.1) beyond what `provenance` already
> records, and trust weighting (§6.5).

**Goal:** correct an answer once; a semantically similar future question uses the
correction.

This is spec §33's "how the assistant learns immediately", and it is the right mechanism.
Note what it is *not*: fine-tuning. Facts go in the knowledge store; corrections go in
correction memory; only *behaviour and style* would ever justify a LoRA, and not yet
(spec §35).

**Prerequisite:** Stage 5 verification passes. Corrections need semantic matching to be
useful — a lexical-only correction lookup only fires on near-identical wording, which is
rare enough not to be worth the UI.

---

## Deliverables

- `✓ correct` / `✗ wrong` / `✎ edit` feedback on every answer.
- Correction editor UI state (`Ctrl+E`, `Ctrl+Enter` to save).
- `corrections` + `correction_sources` tables, embedded and retrievable.
- Correction injection into the context pack.
- Staleness detection.
- `query_history` populated for every query.

---

## 6.1 Feedback capture

Spec §34's tables. Write a `query_history` row for **every** query from this stage on,
regardless of feedback, plus `query_sources` with lexical / semantic / final ranks.

This table is the dataset for Stage 7 evaluation and any future LoRA work. It cannot be
reconstructed later. Start writing it before you need it.

Respect `[logging] log_queries` — if the user turns it off, store the query id, timing, and
section ids but not the query text or answer.

## 6.2 Correction editor

`Ctrl+E` on an answer opens the Correction state (spec §4). The current answer is
pre-loaded into an editable text area; the user edits it; `Ctrl+Enter` saves; `Esc`
cancels.

Save: `question`, `normalized_question`, `bad_answer`, `good_answer`, timestamps, plus a
`correction_sources` row for each section that was in the context pack. Embed the
normalized question immediately (blocking, ~10 ms) rather than queueing it — the user's
next action may be re-asking, and a correction that does not apply on the very next query
feels broken.

`✗ wrong` without an edit is also valuable: it stores a negative signal against that
(query, section set) pair for Stage 7, without creating a correction.

## 6.3 Retrieval and injection

At query time, before lexical/semantic retrieval:

1. Look up corrections by cosine similarity of the query embedding against
   `normalized_question` embeddings, plus an FTS match as a cheap parallel path.
2. If the best match exceeds `[corrections] match_threshold` (start at 0.82, tune it), take it.
3. Inject it as source `[0]` in the context pack under an explicit marker:

```text
USER-CONFIRMED CORRECTION (authoritative, overrides the sources below):
{good_answer}
```

4. Add a system prompt rule: *"If a USER-CONFIRMED CORRECTION is present and answers the
   question, use it as the basis for your answer."*

Note the deliberate choice: the correction goes **into the prompt**, it is not returned
verbatim. Returning stored text bypasses the model entirely and gives a canned answer to a
differently-phrased question. Letting the model adapt it keeps the answer responsive to
what was actually asked, at the cost of one generation.

Exception: on an exact `normalized_question` match, returning the stored answer directly is
correct and instant. Do that.

## 6.4 Staleness

Spec §33 is explicit: do not return a correction forever.

- Every `correction_sources` row snapshots the section's `content_hash` at save time.
- When the indexer rewrites a section, mark any correction referencing it as
  `stale = 1`.
- A stale correction is still injected, but with a weaker marker and a lower boost, and
  `brainctl status` reports the count.
- `brainctl corrections list --stale` lets you review and either re-confirm or delete.

Rationale: the source changed, so the correction *might* be obsolete — but it might also
still be right, and silently dropping a user's explicit correction is worse than showing a
possibly-outdated one.

`correction_version` (a counter in `meta`) is part of the answer cache key from Stage 2.
Bump it on every correction write or the cache will keep serving the answer that was just
corrected. This is the single easiest bug to ship in this stage.

## 6.5 Trust weighting

Spec §46's ordering, applied as ranking multipliers:

```text
user correction > current personal note > current code > old note > imported reference
```

Add a `trust_class` column to `documents`, defaulted from which `[[sources]]` block the
file came from. Do not build a trust engine — the schema just needs room, as the spec says.

---

## Definition of done

```text
Correct an answer once; a semantically similar future question uses the correction.
```

- Ask a question, get a wrong answer, `Ctrl+E`, fix it, save.
- Re-ask the **same** question → the corrected answer, instantly (exact-match path).
- Ask a **paraphrase** → the answer reflects the correction.
- Ask an unrelated question → the correction does not leak in.
- Edit the source note the correction was based on → the correction is marked stale and
  `brainctl status` shows it.
- `query_history` and `query_sources` have a row for every query made since the stage landed.
- With `log_queries = false`, no query text is persisted anywhere.
