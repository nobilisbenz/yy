# 00 — Setup: what to install before writing code

Everything here was checked against this machine. Commands are copy-pasteable.
Total download ≈ **4–5 GB** (most of it the CUDA toolkit), or **≈ 1.5 GB** on the
Vulkan path.

---

## 1. System packages

Already present: `cmake`, `gcc/g++`, `git`, `pkg-config`, `sqlite3`, `xdotool`, `xprop`,
`gtk-launch`, `nvim`, `ghostty`, `picom`, and the X11/font dev headers iced needs
(`libxkbcommon-dev`, `libxcb1-dev`, `libfontconfig-dev`, `libfreetype-dev`,
`libxrandr-dev`, `libssl-dev`).

Missing — install these:

```bash
sudo apt update
sudo apt install -y \
  ninja-build \
  libclang-dev \
  libgl1-mesa-dev \
  libxcb-cursor-dev \
  libinput-dev \
  wmctrl \
  curl jq
```

- `ninja-build` — llama.cpp builds much faster with it.
- `libclang-dev` — needed by `bindgen`, pulled in transitively by several crates.
- `libgl1-mesa-dev` — GL/Mesa dev target. `iced_wgpu` prefers **Vulkan** on this box, so
  confirm `mesa-vulkan-drivers` and `vulkan-tools` are installed as well.
- `wmctrl`, `jq`, `curl` — debugging only (`wmctrl -lx` to inspect WM_CLASS, `curl` to poke llama-server).

---

## 2. GPU inference backend — pick one

### Path A (recommended): CUDA

The distro toolkit is **CUDA 12.4**, whose `nvcc` refuses this machine's **gcc 15.2**.
Install `g++-13` alongside it and point `nvcc` at it explicitly. This works; it is just
not discoverable, so it is written out here.

```bash
sudo apt install -y nvidia-cuda-toolkit g++-13 gcc-13
nvcc --version   # expect release 12.4
```

Then when building llama.cpp (§3), pass:

```bash
-DGGML_CUDA=ON -DCMAKE_CUDA_HOST_COMPILER=/usr/bin/g++-13
```

RTX 3060 Laptop is Ampere → `-DCMAKE_CUDA_ARCHITECTURES=86` cuts build time a lot by
skipping every other architecture.

Download: ~3.5 GB.

### Path B (zero friction): Vulkan

No CUDA toolkit, no compiler pinning. On a 3060 with a 1.7B model, token generation is
close to CUDA; **prompt prefill is meaningfully slower**, and prefill is what drives
time-to-first-token here (you feed it several retrieved sections every query). Use this
to get unblocked on day 1, and re-measure against CUDA at Stage 2.

```bash
sudo apt install -y libvulkan-dev glslc vulkan-tools
vulkaninfo --summary | head -20   # confirm the NVIDIA device is listed
```

Build flag: `-DGGML_VULKAN=ON`. Download: ~200 MB.

> Do not build the CPU-only backend and plan to "add GPU later" — the whole latency
> budget in spec §37 assumes GPU offload.

---

## 3. llama.cpp

Build from source. Prebuilt release binaries exist but are not guaranteed to carry the
backend you chose.

```bash
mkdir -p ~/.local/src && cd ~/.local/src
git clone https://github.com/ggml-org/llama.cpp
cd llama.cpp

# --- Path A: CUDA ---
cmake -B build -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DGGML_CUDA=ON \
  -DCMAKE_CUDA_HOST_COMPILER=/usr/bin/g++-13 \
  -DCMAKE_CUDA_ARCHITECTURES=86 \
  -DLLAMA_CURL=ON

# --- Path B: Vulkan ---
# cmake -B build -G Ninja -DCMAKE_BUILD_TYPE=Release -DGGML_VULKAN=ON -DLLAMA_CURL=ON

cmake --build build --config Release -j"$(nproc)"
```

Put the binaries on `PATH`:

```bash
mkdir -p ~/.local/bin
ln -sf ~/.local/src/llama.cpp/build/bin/llama-server ~/.local/bin/llama-server
ln -sf ~/.local/src/llama.cpp/build/bin/llama-cli    ~/.local/bin/llama-cli
ln -sf ~/.local/src/llama.cpp/build/bin/llama-bench  ~/.local/bin/llama-bench
llama-server --version
```

`LLAMA_CURL=ON` lets `llama-server -hf <repo>` pull models directly, which is the
easiest way to fetch GGUFs (§4).

Pin the commit you built (`git rev-parse HEAD`) in the repo README. llama.cpp's server
API changes; you want to know which build your prompts were tuned against.

---

## 4. Models

```bash
mkdir -p ~/.local/share/brain/models
cd ~/.local/share/brain/models
```

### Answer model — get this now

**Qwen3-1.7B, `Q5_K_M`** (~1.3 GB). The spec says Q4; with 6 GB of VRAM and a 1.7B model,
Q4 gives up measurable quality for memory you are not short of. Q5_K_M is the better default.

```bash
pip install --user -U "huggingface_hub[cli]"   # or: pipx install huggingface_hub
hf download Qwen/Qwen3-1.7B-GGUF Qwen3-1.7B-Q5_K_M.gguf \
  --local-dir ~/.local/share/brain/models
```

Verify it runs and get a real number for your machine:

```bash
llama-bench -m ~/.local/share/brain/models/Qwen3-1.7B-Q5_K_M.gguf -ngl 99 -p 2048 -n 128
```

Record `pp2048` (prefill — drives TTFT) and `tg128` (generation). These are your
baseline for Stage 2. **Do not write a token/s number into docs or code**; measure.

### Quality-profile model — optional, get later

**Qwen3-4B-Instruct-2507, `Q4_K_M`** (~2.5 GB). Fits in 6 GB alongside a 4096 context.
Wire it as the `quality` profile in config (spec §39) and switch by hand.

```bash
hf download Qwen/Qwen3-4B-Instruct-2507-GGUF Qwen3-4B-Instruct-2507-Q4_K_M.gguf \
  --local-dir ~/.local/share/brain/models
```

### Embedding model — Stage 5 only, do not download yet

**Qwen3-Embedding-0.6B, `Q8_0`** (~650 MB). Noted here so the plan is complete.

```bash
# hf download Qwen/Qwen3-Embedding-0.6B-GGUF Qwen3-Embedding-0.6B-Q8_0.gguf \
#   --local-dir ~/.local/share/brain/models
```

---

## 5. Compositor (required for the dock's look)

`picom` is installed but **not running**. Without a compositor the dock window cannot be
translucent and cannot have a shadow; rounded corners have to be faked.

Add to `~/.config/picom.conf`:

```conf
backend = "glx";
vsync = true;

corner-radius = 0;
shadow = false;

rules = (
  {
    match = "class_g = 'brain-dock'";
    corner-radius = 22;
    shadow = true;
    shadow-radius = 28;
    shadow-opacity = 0.45;
    shadow-offset-x = -14;
    shadow-offset-y = 10;
    blur-background = true;
  }
);
```

And to `~/.config/i3/config`:

```i3
exec --no-startup-id picom
```

Start it now to confirm nothing in your setup breaks (tearing, cursor artifacts):

```bash
picom -b
```

If picom causes problems you do not want, Stage 0 has a documented opaque fallback — the
dock still works, it just loses the shadow and blur.

---

## 6. i3 configuration

Add to `~/.config/i3/config`. The `for_window` rules can go in now; the `exec` lines only
matter once binaries exist.

```i3
# --- Brain Dock ---
for_window [class="brain-dock"] floating enable, border pixel 0, sticky enable
no_focus  [class="brain-dock" window_role="none"]

exec --no-startup-id brain-daemon
exec --no-startup-id brain-dock --hidden
bindsym $mod+space exec --no-startup-id brainctl toggle
```

**Check first:** `$mod+space` is i3's default `focus mode_toggle` binding. Confirm what
your config currently binds it to before overriding — you may want a different key.

```bash
grep -n 'mod+space' ~/.config/i3/config
```

Also make sure i3 will let the dock take focus when it maps:

```i3
focus_on_window_activation smart
```

---

## 7. Rust toolchain extras

Toolchain is current (1.97.1). Add the dev tools:

```bash
rustup component add rust-analyzer clippy rustfmt
cargo install cargo-watch cargo-nextest --locked
```

- `cargo-watch` — `cargo watch -x 'run -p brain-daemon'` during Stages 1–2.
- `cargo-nextest` — the parser golden tests and retrieval tests run much faster and
  isolate panics per test.

Optional but useful later:

```bash
cargo install cargo-flamegraph --locked   # profiling the retrieval path in Stage 7
```

---

## 8. Directories

```bash
mkdir -p ~/.config/brain \
         ~/.local/share/brain/models \
         ~/.cache/brain \
         "${XDG_RUNTIME_DIR:?}/brain" \
         ~/brain
```

`~/brain` is the default note vault. Put two or three real Markdown notes in it now —
you need a genuine corpus to judge Stage 1, and synthetic test notes will mislead you.

---

## 9. Verification

Run this before starting Stage 0. Everything should print a version or a path.

```bash
set -e
echo "session:   $XDG_SESSION_TYPE / $XDG_CURRENT_DESKTOP"
rustc --version && cargo --version
cmake --version | head -1 && ninja --version
llama-server --version 2>&1 | head -2
nvidia-smi --query-gpu=name,memory.total --format=csv,noheader
pgrep -a picom || echo "WARNING: picom not running"
ls -lh ~/.local/share/brain/models/*.gguf
sqlite3 :memory: "pragma compile_options;" | grep FTS5
for b in nvim ghostty gtk-launch xdg-open xprop; do command -v $b; done
```

---

## Summary of what to download

| Item | Size | When |
|---|---|---|
| apt packages (§1) | ~150 MB | now |
| CUDA toolkit + g++-13 (§2A) | ~3.5 GB | now |
| *or* Vulkan dev (§2B) | ~200 MB | now |
| llama.cpp source + build | ~300 MB | now |
| Qwen3-1.7B-Q5_K_M.gguf | ~1.3 GB | now |
| cargo-watch, cargo-nextest | ~50 MB | now |
| Qwen3-4B-Instruct-2507-Q4_K_M.gguf | ~2.5 GB | Stage 2, optional |
| Qwen3-Embedding-0.6B-Q8_0.gguf | ~650 MB | Stage 5 |
| Qwen3-Reranker-0.6B GGUF | ~650 MB | Stage 7, only if benchmarked |

Rust crates are all from crates.io and resolve on first `cargo build` — nothing to
pre-fetch.
