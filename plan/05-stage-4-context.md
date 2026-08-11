# Stage 4 — Desktop context

> **Status: the definition of done passes.** Capture measured at **373 µs** against the
> 10 ms budget, including the `/proc` descent — which correctly walked
> `com.mitchellh.ghostty.agent` down to the `tmux: client` running inside it.
>
> The same query, with ghostty focused:
>
> ```text
> with context     [1] Splitting panes in ghostty   matched heading · matches the focused app
>                  [2] Splitting windows in vim
> --no-context     [1] Splitting windows in vim
>                  [2] Splitting panes in ghostty
> ```
>
> Both results are present either way — context reorders, it never filters (§4.2).
>
> **Found while verifying:** the alias table looked correct and did nothing. Ghostty's
> terminal reports `WM_CLASS = com.mitchellh.ghostty.agent`, while the obvious config entry
> is `com.mitchellh.ghostty`; an exact-match lookup missed, fell back to the full reverse-DNS
> string, and matched no note text. Aliases now also match a reverse-DNS *sub*-identifier,
> and the unaliased fallback uses the name a person would actually write (`ghostty`, not
> `com.mitchellh.ghostty.agent`).
>
> Not built: `recent_source` boosting, which needs the action-open history that does not
> exist yet, and the benchmark's `context:` field, which waits on the Stage 7 harness.

**Goal:** the same vague query returns different, better results depending on what you were
doing when you pressed the shortcut.

Spec §18 calls this "one of the most valuable custom features" and that is right. It is
also cheap — most of the X11 plumbing already exists from Stage 0.

**Prerequisite:** Stage 3 verification passes.

---

## Deliverables

- `DesktopContext` captured at summon time and carried through the query.
- Context-derived ranking boosts.
- `--no-context` flag and a config kill switch.

---

## 4.1 Capture

Spec §18's struct. Captured by the **daemon**, at `toggle` time, before the dock maps —
not by the dock, which by then is itself the active window.

Via `brain-x11`, all cheap property reads:

| Field | Source |
|---|---|
| `active_window_id` | `_NET_ACTIVE_WINDOW` on root |
| `wm_class` | `WM_CLASS` on that window |
| `window_title` | `_NET_WM_NAME` (UTF-8), fall back to `WM_NAME` |
| `pid` | `_NET_WM_PID` |
| `process_name` | `/proc/<pid>/comm` |
| `cwd` | `/proc/<pid>/cwd` symlink |
| `workspace` | i3 IPC `get_workspaces`, or `_NET_CURRENT_DESKTOP` |

**The terminal problem.** For ghostty running nvim, `_NET_WM_PID` is the terminal's pid and
its `cwd` is wherever it was launched. The useful cwd belongs to the foreground child.
Walk `/proc/<pid>/task/*/children` down to the deepest descendant and read *its* `cwd`.
Guard with a depth cap and a 5 ms budget — this runs on every summon.

Everything here is best-effort. Every field is `Option`. A failed read logs at debug and
returns `None`; it never fails a query.

Total budget: **under 10 ms** (spec §37). Time it with a span and assert it in a test.

## 4.2 Ranking boosts

Spec §18's key constraint: context **boosts**, it does not filter. If you hard-filter to the
active app, asking "how did I do X in Blender" while Blender is open works fine and asking
anything else while Blender is open silently returns nothing.

Applied as multipliers on the fused score, all from config:

```toml
[search.context_boost]
active_app       = 1.25   # section tags/apps front matter contains the wm_class
current_project  = 1.35   # document path is under the resolved cwd
heading_match    = 1.30
recent_source    = 1.10   # opened via an action in the last N days
```

Mapping `wm_class` → app identity needs an alias table, because `WM_CLASS` is not what
people write in notes:

```toml
[context.aliases]
blender      = ["blender", "Blender"]
obs          = ["obs", "com.obsproject.Studio"]
ghostty      = ["ghostty", "com.mitchellh.ghostty"]
```

Seed it from your actual `wmctrl -lx` output rather than guessing.

**Explicit override.** If the query itself names an app ("in Blender", "in OBS"), the
query's app wins over the focused window. A one-line check against the alias table before
applying boosts.

**Escape hatch.** `[context] enabled = true` in config plus `brainctl ask --no-context`.
When context ranking misfires you need to confirm that is what happened, and the only way
is to turn it off and re-ask.

## 4.3 Context in the cache key and the prompt

- The retrieval cache key already includes a `context_bucket` (Stage 2.5). Bucket it
  coarsely — `(app_alias, project_root)` — not the full struct, or the cache never hits.
- The prompt gets one line: `active_app=obs` (spec §23). Nothing more. Window titles can
  contain anything, including someone else's text; they are untrusted and they are not
  worth the tokens.

## 4.4 Evaluation

This is the first stage where "did it help?" is genuinely non-obvious. Add context to the
benchmark format now, even though the harness is Stage 7:

```yaml
- question: "where is the sprite importer?"
  context: { app: ghostty, cwd: ~/projects/game }
  expected:
    - path: ~/projects/game/tools/importer/src/sprite.rs
```

Record every query's context and final ranks into `query_sources` (spec §34) from this
stage on. That table is what makes Stage 7 possible, and you cannot backfill it.

---

## Definition of done

```text
The same vague query gives better results depending on whether Blender or your game
project is active.
```

- Focus Blender, ask "how do I mirror bones" → Blender notes rank first.
- Focus a terminal in `~/projects/game`, ask the same → project files rank first.
- Focus something unrelated, ask the same → still returns sensible results, nothing filtered away.
- Context capture measured under 10 ms including the `/proc` descent.
- Kill the X11 property reads (test with a window that has no `_NET_WM_PID`): queries still work.
