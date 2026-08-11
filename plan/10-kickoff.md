# Kickoff — the first coding session

Setup is done and verified. This file turns Stage 0 into an ordered sequence of small
commits, each of which leaves the tree building and independently verifiable.

Read [`00b-machine-baseline.md`](00b-machine-baseline.md) and
[`01-stage-0-dock.md`](01-stage-0-dock.md) first; this is the execution order, not a
replacement for either.

---

## Before the first commit

**Put real notes in `~/brain`.** It is empty. Write three or four genuine notes about
things you actually did recently — an OBS setup, a Blender workflow, a build flag you had
to look up twice. Include headings, at least one with three levels, and at least one
`@video` / `@file` / `@app` line.

This matters more than it looks. Stage 1's whole value judgement is "does it find the
right section", and synthetic notes cannot answer that — they are written by the same mind
that writes the query, in the same words, so BM25 always wins and you learn nothing.

```bash
ls ~/brain          # should not be empty when you reach Stage 1
```

**Start the compositor and confirm it survives.** Run it and use the machine normally for
an hour before you build UI on top of it.

```bash
picom -b --config ~/.config/picom/picom.conf
```

If it misbehaves, switch `backend = "egl"` → `"xrender"` and set the rule's
`blur-background = false`.

---

## Commit sequence

### C1 — workspace skeleton

```bash
cd ~/Dev/tools/yy
git init
mkdir -p crates ui/components migrations config scripts tests/fixtures benchmarks
```

Root `Cargo.toml` per [`README.md`](README.md#repository-layout), with the dev-profile
override that keeps Slint usable:

```toml
[profile.dev]
opt-level = 1
[profile.dev.package."*"]
opt-level = 3
```

Create all five library crates and three binary crates as empty stubs that compile.
Move `brain-dock-spec.md` and `plan/` into the repo, add `.gitignore` (`/target`,
`*.db`, `*.db-wal`, `*.db-shm`).

**Verify:** `cargo build --workspace` succeeds.

### C2 — `brain-proto`

The full `ClientRequest` / `ServerEvent` enums from Stage 0 §0.2, socket path resolution
from `$XDG_RUNTIME_DIR/brain/brain.sock`, and the `Framed` + `LinesCodec` helpers both
ends will use.

Deps: `serde`, `serde_json`, `uuid`, `tokio`, `tokio-util` (`codec` feature), `thiserror`.

**Verify:** a round-trip test — serialize every enum variant, deserialize, assert equality.

### C3 — `brainctl` + a daemon that only echoes

`brain-daemon` binds the socket and accepts connections; every request gets a
`ServerEvent::Error{ "not implemented" }` except `Status`. `brainctl` connects, sends one
request, prints the reply, exits non-zero if the socket is missing.

This is the smallest end-to-end IPC proof and it takes an hour. Do it before touching
Slint, so that when the UI misbehaves you already know the transport works.

Deps (daemon): `tokio`, `tracing`, `tracing-subscriber`, `anyhow`.

**Verify:**

```bash
cargo run -p brain-daemon &
cargo run -p brainctl -- status          # prints a stub report
kill %1; cargo run -p brainctl -- status # exits non-zero with a clear message
```

### C4 — `brain-dock` window, on screen, ugly

Minimum Slint: a `Window` with a text input and nothing else. No IPC, no positioning, no
X11 properties. Get it to appear.

Deps: `slint` (build-dep `slint-build`), `ui/dock.slint`, `ui/tokens.slint`.

**Verify:** `cargo run -p brain-dock` shows a window and accepts typing.

### C5 — WM_CLASS and i3 rules

Set `WM_CLASS` to `brain-dock` / `BrainDock` **before first map** (winit attributes hook;
fallback in Stage 0 §0.3). This is the commit where the i3 rules already in your config
start applying.

**Verify:**

```bash
xprop -name "Brain Dock" WM_CLASS        # "brain-dock", "BrainDock"
wmctrl -lx | grep BrainDock
i3-msg -t get_tree | jq '.. | select(.window_properties?.class? == "BrainDock")
                          | {floating, border, sticky}'
```

Floating, border 0, sticky — all from the config, no code.

### C6 — `brain-x11`: properties, position, map/unmap

The core of the stage. EWMH atoms, `_NET_WM_STATE` (above/sticky/skip-taskbar/skip-pager),
`_NET_WM_WINDOW_TYPE_UTILITY`, `_MOTIF_WM_HINTS`, `_NET_WORKAREA` anchoring,
`MapWindow`/`UnmapWindow`, `_NET_ACTIVE_WINDOW` focus request, and the RandR monitor pick.

Deps: `x11rb` (`randr`), `raw-window-handle`.

Remember the mapped/unmapped split: `ChangeProperty` while unmapped, root `ClientMessage`
while mapped.

**Verify:** the dock appears at top-right *below polybar*, focused, above other windows,
and follows you across workspaces. Time `map → focused` with a `tracing` span; target
< 50 ms.

### C7 — wire the toggle

Daemon owns `visible: bool`; `brainctl toggle` flips it; the daemon sends
`ShowDock`/`HideDock` to the subscribed UI connection. Dock's tokio thread reconnects with
backoff if the daemon is not up yet.

This is where the Slint/tokio threading model from Stage 0 §0.4 gets built — main thread
runs `ui.run()`, background thread runs the runtime, backend→UI goes through
`slint::invoke_from_event_loop`.

**Verify:** `$mod+a` from i3 toggles the dock. Restart the daemon while the dock is
running; the dock reconnects on its own.

### C8 — fake streaming answer

Daemon's `--mock` pipeline from Stage 0 §0.5: fake retrieval delay, then a fixed sentence
word by word. Dock renders the six UI states, with the ~30 ms token batching in place from
the start.

**Verify:** type a question, watch it stream smoothly, no stutter, no per-token jank.
`Esc` hides without clearing; reopening shows the previous answer.

### C9 — polish and lock the stage

Keyboard map struct (`Enter`, `Esc`, `Ctrl+L`, `Ctrl+C`, `Up`/`Down`), show/hide
animations, `tokens.slint` holding every visual constant, `brainctl doctor` checking the
compositor and socket.

**Verify:** the full Stage 0 definition-of-done checklist.

---

## What to build first inside C6

C6 is the risky one, so front-load its two unknowns before writing the rest of it:

1. **Does transparency work?** Set `background: transparent` on the Slint `Window`, request
   a transparent winit window, run with picom active. Half an hour. If it fails, take the
   opaque fallback immediately and move on — do not spend a day on it.
2. **Does map/unmap survive?** Unmap and remap the window ten times and confirm the XID,
   the EWMH properties, and the Slint rendering all survive. If Slint's renderer objects to
   being unmapped, fall back to moving the window off-screen instead, and note it.

Both answers change the shape of the code, so get them before writing the code.

---

## Definition of "ready for Stage 1"

- All nine commits landed, `cargo clippy --workspace -- -D warnings` clean.
- `$mod+a` works from a cold boot (i3 `exec` lines start both processes).
- `~/brain` has real notes waiting.
- The measured summon latency is recorded in the repo README next to the llama-bench
  numbers.

---

## Standing rules for the build

- **Instrument from commit one.** Every stage boundary gets a `tracing` span. Retrofitting
  after a latency problem appears means guessing.
- **Config over constants.** Anything you might tune — weights, limits, margins, timeouts —
  goes in `config/brain.example.toml`, never a literal. Stage 7 depends on this.
- **No `sh -c`, ever.** `Command::new` with separate args. There is no `RunShell` action.
- **Golden tests before parser features** (Stage 1). The parser is the one component whose
  regressions are invisible.
- **Stop after Stage 2 and use it for a week.** Stages 3–7 are refinements of a loop that
  Stage 2 either validates or does not.
