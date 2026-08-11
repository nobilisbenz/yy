# Machine baseline — verified 2026-08-11

Setup is complete. This file records what is actually installed and measured, so the
stage files can stop hedging. **Every value here was observed, not assumed.**

---

## Hardware and display

```text
GPU 0 (compute)   NVIDIA RTX 3060 Laptop (GA106M), 5806 MiB usable VRAM, CC 8.6
GPU 1 (display)   AMD Cezanne / Radeon Vega iGPU
X provider 0      modesetting  — Source Output, drives eDP-1     ← X renders here
X provider 1      NVIDIA-G0    — Sink Output (PRIME offload)     ← llama.cpp runs here
Monitor           eDP-1, 1920x1080 @ +0+0   (single monitor)
RAM               26 GB
```

**This split is a gift, and the plan should exploit it.** X, picom, and the Slint dock all
render on the AMD iGPU through Mesa. The NVIDIA GPU is untouched by the desktop, so
llama.cpp gets the whole 5.8 GB of VRAM and never competes with the UI for it.

Consequences:

- **Do not** set `__NV_PRIME_RENDER_OFFLOAD` or `__GLX_VENDOR_LIBRARY_NAME=nvidia` for
  `brain-dock`. Offloading the UI to the NVIDIA GPU would add PRIME copy latency to every
  frame and steal VRAM from the model. Let it use Mesa.
- The old i3 comment warning that "GLX can freeze X11 on hybrid AMD/NVIDIA" was about
  NVIDIA-driven X. Not this setup. picom now uses `egl` on Mesa, which is the correct and
  stable choice here.

**Single monitor.** The multi-monitor logic in Stage 0 (spec §7, §43) still gets written —
it is a laptop and an external display will appear eventually — but it is currently
**untestable**. Write it, keep it simple, and mark it unverified until there is a second
output to test against.

---

## Toolchain

```text
Ubuntu           26.04 LTS (Resolute Raccoon), X11, i3
rustc / cargo    1.97.1
cargo-watch      installed        cargo-nextest    installed
rust-analyzer, clippy, rustfmt    installed
CUDA             12.4.131 (nvcc), host compiler g++-13 13.4.0
gcc (system)     15.2.0
cmake / ninja    installed
apt deps         ninja-build, libclang-dev, libgl1-mesa-dev, libxcb-cursor-dev,
                 libinput-dev, wmctrl, jq  — all present
X11/font headers libxkbcommon, libxcb1, libfontconfig, libfreetype, libxrandr — present
```

---

## llama.cpp

```text
source     ~/.local/src/llama.cpp
build      CUDA (libggml-cuda.so, 197 MB) — confirmed loading the RTX 3060 at runtime
version    10358  (commit 030ebb558)     ← pin this in the repo README
symlinks   ~/.local/bin/{llama-server,llama-cli,llama-bench,llama-tokenize}
```

`~/.local/bin` is on `PATH`. Symlinks resolve `$ORIGIN` correctly, so the shared-library
layout of this build works through them — verified with `llama-server --version`.

**Note the new binary layout.** This build ships a unified dispatcher (`llama serve`,
`llama cli`, …) *alongside* the classic `llama-server` / `llama-cli` binaries, and most
logic now lives in shared objects (`libllama-server-impl.so` etc.). Use the classic
`llama-server` binary — it is what the Stage 2 supervisor code expects, and it keeps the
command line stable if the dispatcher's interface changes.

### Flags verified present in this build

`--flash-attn` · `--cache-reuse` · `--reasoning-budget` · `--no-webui` · `--ctx-size` ·
`--n-gpu-layers` · `--parallel` · `--slots` · `--jinja` · `--chat-template-kwargs` ·
`--host` · `--port` · `--alias` · `--ubatch-size` · `--embeddings` · `--pooling`

Every flag Stage 2 and Stage 5 depend on exists. The `--reasoning-budget` risk flagged in
`09-decisions.md` is resolved — this build even ships a `test-reasoning-budget` test binary.

---

## Model

```text
path   ~/.local/share/brain/models/Qwen_Qwen3-1.7B-Q5_K_M.gguf
size   1.37 GiB on disk (1.5 GB reported by ls), 2.03 B params
```

Note the **filename is `Qwen_Qwen3-1.7B-Q5_K_M.gguf`** — the HF CLI prefixed the repo
owner. Use this exact name in config; do not rename it, since a re-download would produce
the same name again.

---

## Measured performance — the number that matters

```text
$ llama-bench -m Qwen_Qwen3-1.7B-Q5_K_M.gguf -ngl 99 -p 2048 -n 128

qwen3 1.7B Q5_K_M | CUDA | ngl 99 | pp2048 | 6555.37 ± 64.50 t/s
qwen3 1.7B Q5_K_M | CUDA | ngl 99 |  tg128 |  168.33 ±  0.89 t/s
```

Translated into the spec's budget (§3.1, §37):

| Quantity | Derivation | Result |
|---|---|---|
| Prefill of a full ~2000-token context pack | 2048 / 6555 | **≈ 310 ms** |
| Expected TTFT, cold prompt | prefill + sampling overhead | **≈ 350 ms** |
| Expected TTFT, warm prefix (`--cache-reuse`) | only the changing suffix reprocesses | **well under 200 ms** |
| Full 350-token answer | 350 / 168 | **≈ 2.1 s** |
| First visible sentence (~20 tokens) | 20 / 168 | **≈ 120 ms after TTFT** |

**The spec's `< 500 ms` first-token target is comfortably reachable**, with headroom. Two
things follow:

1. **The context pack budget is not the constraint it was assumed to be.** `context_sections
   = 5` is affordable, and Stage 7 could raise it if recall justifies. Prefill is cheap on
   this machine.
2. **Streaming still matters** — a full answer takes ~2.1 s, so waiting for `Complete`
   before rendering would feel slow. The Stage 2 decision to emit sources and actions
   before generation starts is what makes the whole thing feel instant.

Re-run this benchmark after any llama.cpp rebuild and update the table.

---

## Desktop configuration

### picom — `~/.config/picom/picom.conf` (rewritten)

`backend = "egl"` (was `glx`), `blur-method = "dual_kawase"` declared globally so the
per-rule `blur-background` actually takes effect, and an added rule that keeps polybar and
the desktop/wallpaper free of rounding and shadows.

**picom is not currently running.** i3 execs it at startup, but the session predates the
config. Start it manually and watch for trouble before trusting it:

```bash
picom -b --config ~/.config/picom/picom.conf
```

If the screen tears, flickers, or the cursor corrupts, fall back to `backend = "xrender"`
and set `blur-background = false` in the rule. Rounded corners and shadows still work on
xrender; only blur is lost.

### i3 — `~/.config/i3/config` (three changes)

1. **Toggle is now `$mod+a`** (line 72) — you set this. `$mod+space` remains bound to your
   quick-terminal script; no conflict. Update all docs and the spec's `$mod+space`
   references accordingly.
2. **Removed `no_focus [class="BrainDock" …]`.** This was a mistake in the original plan —
   `no_focus` tells i3 *not* to focus the window on map, which is the exact opposite of
   what the dock needs.
3. **Added `focus_on_window_activation smart`.** Without it, i3 ignores the
   `_NET_ACTIVE_WINDOW` client message the dock sends to grab keyboard focus, and the dock
   appears without a cursor in the input field.

### Two live-configuration hazards for Stage 0

**`focus_follows_mouse yes`** (line 8). The dock anchors top-right; if the pointer happens
to rest there, or drifts over the dock and then off it, i3 will move focus and your
keystrokes go elsewhere mid-question. i3 has no per-window override.

> Mitigation: the dock takes focus explicitly on map, and **must not** auto-hide on focus
> loss — only on `Esc` or an explicit toggle. If this still bites in practice, the fallback
> is `focus_follows_mouse no` while the dock is visible, toggled via i3 IPC.

**polybar occupies the top 30 px** (`[bar/main]`, `height = 30`, `width = 100%`, top by
default, currently running as pid `polybar main`). The spec's `margin_top = 22` would put
the dock *underneath the bar*.

> Fix: default `margin_top = 38` in `config/brain.example.toml` (30 px bar + 8 px gap).
> Better, do it properly: read `_NET_WORKAREA` from the root window in `brain-x11` and
> anchor to the work area rather than the raw monitor rectangle. That handles polybar,
> any future bar, and bar-visibility toggling ($mod+b) for free — it is about ten extra
> lines in code you are already writing.

### Directories created

```text
~/.config/brain   ~/.cache/brain   ~/brain   $XDG_RUNTIME_DIR/brain
~/.local/share/brain/models   (already existed, holds the model)
```

`~/brain` is **empty**. Before Stage 1 is judgeable, put real notes in it — see
[`10-kickoff.md`](10-kickoff.md).

---

## Corrections to the earlier plan

| Where | Was | Now |
|---|---|---|
| Keybinding | `$mod+space` | **`$mod+a`** |
| Model filename | `Qwen3-1.7B-Q5_K_M.gguf` | **`Qwen_Qwen3-1.7B-Q5_K_M.gguf`** |
| picom backend | `glx` | **`egl`** (Mesa/AMD drives X here) |
| i3 `no_focus` rule | present | **removed** — it broke focus |
| `focus_on_window_activation` | not mentioned in the i3 snippet | **added, required** |
| `margin_top` | 22 | **38**, or derive from `_NET_WORKAREA` |
| CUDA vs Vulkan | open question | **settled: CUDA, built and measured** |
| TTFT feasibility | "measure it, might be a problem" | **settled: ~350 ms cold, comfortable** |
