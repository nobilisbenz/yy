# Brain Dock

An instant, floating, local knowledge assistant for X11 + i3. Press one
shortcut, ask how you did something, get a concise answer from your own files,
then jump straight to the note, code, video, or app behind it.

Full specification: [`brain-dock-spec.md`](brain-dock-spec.md).
Build plan: [`plan/`](plan/README.md).

---

## Status: Stage 0 complete

The interaction is proven end to end. There is no index and no model yet — the
query pipeline is a mock whose delays mirror measured hardware timings.

| Stage | | |
|---|---|---|
| 0 | Dock, IPC, window control, streaming | **done** |
| 1 | SQLite + FTS5 index, Markdown parser, file watcher | next |
| 2 | Qwen3 answers via llama-server | |
| 3–7 | Actions, desktop context, embeddings, corrections, benchmark | |

### What works

```
$mod+a                  toggle the dock          (10–15 ms, measured)
type + Enter            query → sources → streamed answer
Esc                     hide, keeping the answer for the next summon
Ctrl+L / Ctrl+C         clear query / copy answer to clipboard
Up / Down               query history
Tab / Shift+Tab         cycle action buttons
Alt+1..9                activate an action       (wired, targets land in Stage 3)
brainctl status|doctor|ask|toggle|show|hide
```

## Building

Prerequisites are in [`plan/00-setup.md`](plan/00-setup.md); the verified state
of this machine is in [`plan/00b-machine-baseline.md`](plan/00b-machine-baseline.md).

```bash
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Running

```bash
cp config/brain.example.toml ~/.config/brain/config.toml
picom -b                              # required for rounded corners and shadow
./target/release/brain-daemon --mock &
./target/release/brain-dock --hidden &
./target/release/brainctl toggle
```

`--mock` answers from a canned script. Drop it once Stage 2 lands; keep it
around for tuning UI timing without loading a model.

### i3

```i3
for_window [class="BrainDock"] floating enable, border pixel 0, sticky enable
focus_on_window_activation smart

exec --no-startup-id brain-daemon
exec --no-startup-id brain-dock --hidden
bindsym $mod+a exec --no-startup-id brainctl toggle
```

`focus_on_window_activation` is not optional: without it i3 ignores the
`_NET_ACTIVE_WINDOW` request the dock uses to take keyboard focus, and the dock
appears with no caret in the input field.

## Architecture

```
$mod+a ──► brainctl ──┐
                      │ unix socket, JSON Lines
                      ▼
                brain-daemon ──────► index, model, caches (all hot, all resident)
                      │
                      ▼
                 brain-dock  ──────► Slint + X11
```

Three binaries and five libraries. The daemon stays hot so summoning the dock
never waits on a model; the dock stays thin so it can be resident from login.

| Crate | |
|---|---|
| `brain-core` | ids and domain types |
| `brain-proto` | the IPC contract — the only thing the three binaries share |
| `brain-index` | database, parsers, watcher (Stage 1) |
| `brain-engine` | retrieval, ranking, prompts (Stages 1–2) |
| `brain-x11` | EWMH, RandR, window control, desktop context |

### Invariants

These are structural, not stylistic. Changing one is a design decision.

- **The LLM never creates an action.** `ActionKind` has no `RunShell` variant and
  will not get one. Buttons come from parsed metadata in the database, so a note
  containing hostile text cannot become a command.
- **No `sh -c`.** `Command::new` with separate arguments, always.
- **Retrieval is the brain; the model narrates.** Sources are emitted *before*
  generation starts, so the path and buttons are usable while the model is still
  warming up — and often they were all you needed.
- **Every query-scoped event carries its id.** An abandoned query keeps streaming
  for a moment; without the id its tail lands in the next answer.
- **The daemon owns visibility.** That is what lets `brainctl` be a stateless
  one-shot binary that knows nothing about windows.
- **Visual constants live in `ui/tokens.slint`.** No colour, radius, or duration
  in Rust.
- **Tunables live in `config/`.** Stage 7 settles ranking by benchmark sweep, and
  a literal in a source file cannot be swept.

## Measured on this machine

RTX 3060 Laptop (5806 MiB), AMD Cezanne iGPU driving X, Ubuntu 26.04, i3.

```
llama.cpp        build 10358 (030ebb558), CUDA
Qwen3-1.7B-Q5_K_M   pp2048  6555 t/s      →  ~310 ms prefill for a full context pack
                    tg128    168 t/s      →  ~2.1 s for a 350-token answer
dock summon      10–15 ms  (target < 50 ms), including the brainctl process spawn
mock query       91 ms retrieval / 401 ms TTFT / 663 ms total
```

The X server runs on the AMD iGPU, so the dock renders on Mesa and the 3060 is
left entirely to llama.cpp. Do not set `__NV_PRIME_RENDER_OFFLOAD` for the dock.

## Non-goals

X11 and i3 only. No Wayland, no Windows, no macOS, no cloud sync, no browser UI,
no autonomous agent, no shell execution. The spec's §2 list is deliberate: the
constraint is what lets this integrate deeply with one real environment instead
of carrying portability complexity from day one.
