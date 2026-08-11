# Stage 0 — The dock ✅ complete on iced

> **Status: shipped on Slint (commits C1–C9), then ported to iced 0.14 and re-proved.**
> The Slint build is gone; it is in git history at `44c50ad` if any of it is wanted back.
> The toolkit decision and its accounting are in [`../../PLAN.md`](../../PLAN.md) §1.
>
> **Still unmeasured on iced:** end-to-end summon latency against the 50 ms target (the
> Slint build's 10–15 ms is the bar), and the multi-monitor requirement — this machine
> reports a single output, so "appears on the monitor with the focused window, 20/20 times"
> cannot be tested here at all.
>
> ### What running it taught us — the X11/i3 half (permanent)
>
> None of this is toolkit-specific. It survives the port unchanged and is the most
> valuable content in this document.
>
> | Assumption | Reality |
> |---|---|
> | `_NET_WORKAREA` gives the usable area | **i3 does not publish it.** Derived from `_NET_WM_STRUT_PARTIAL` instead — and dock windows are i3's *grandchildren*, so the tree walk needs 3 levels |
> | `_NET_WM_STATE_ABOVE` keeps it on top | **i3 ignores it**, plus SKIP_TASKBAR/SKIP_PAGER. Floating + an explicit `ConfigureWindow` raise is what works |
> | Transparency is the big risk | Works on both. Slint/femtovg and `iced_wgpu`/Vulkan alike: depth-32 ARGB visual, picom on `egl`. On iced it also needs `Daemon::style` to return a transparent `background_color`, and the surface reporting `alpha modes: [Opaque]` turns out not to matter |
>
> ### What running it taught us — the toolkit half (re-verified on iced)
>
> These were Slint facts. Each has an iced counterpart, and every one was re-measured
> rather than assumed — two of them did not behave as expected. **This is Stage 0′ §0.0
> plus the findings below it.**
>
> | Slint finding | what iced actually does |
> |---|---|
> | The XID is not available after `show()` — window creation is deferred to the first event-loop iteration, so adoption retries on a timer | `window::raw_id()` is a `Task<u64>` that resolves once the window exists — no polling loop |
> | `hide()`/`show()` **destroys the window and exits the event loop**; map/unmap via `x11rb` was the workaround | `window::set_mode(id, Mode::Hidden)` is winit `set_visible(false)` = an X11 unmap, XID preserved. ✅ measured — **and it is mandatory, not preferred**: an `x11rb` map behind winit's back does not stick |
> | `WM_CLASS` before map needed the `unstable-winit-030` attributes hook | `window::Settings.platform_specific.application_id`, stable |
> | `SLINT_SCALE_FACTOR` pins scale only at creation; `WINIT_X11_SCALE_FACTOR` needed for resizes | `WINIT_X11_SCALE_FACTOR` is still the fix. ❌ `Daemon::scale_factor` is *not* an override — it multiplies on top of winit's guess, shrinking the content inside an already-oversized window |
> | `--hidden` races Slint's async map, so the window is parked off-screen and the state re-asserted | ✅ gone, but not for the expected reason. The window is opened at boot either way, with `Settings { visible: false }` — properties must land before the first map, and `raw_id` resolves only after iced maps it. There is no toolkit map left to race |


**Goal:** prove the interaction. `$mod+a` (Super+A) puts a focused, frameless, floating
panel at the top-right of the active monitor, instantly, every time.

> Read [`00b-machine-baseline.md`](00b-machine-baseline.md) first. It settles the
> keybinding, the compositor backend, the anchor offset, and two i3 hazards that affect
> this stage directly.

**No AI. No database. No search.** A hard-coded fake answer is the deliverable.

This is the highest-risk stage in the project despite looking like the easiest one. Every
hard problem here is a toolkit/WM integration problem, and none of them get easier later.
Budget more time than feels reasonable.

---

## Deliverables

- Cargo workspace with `brain-core`, `brain-proto`, `brain-x11`, and the three binaries.
- `brain-dock`: iced window, hidden at startup with `--hidden`.
- `brain-daemon`: Unix socket server, JSON Lines, streams fake tokens.
- `brainctl`: `toggle | show | hide | ask | status`.
- i3 rules applied and working.
- The six UI states from spec §4 wired as one Rust enum in the dock's model (only Hidden /
  Input / Searching / Answer need to work; Correction and Sources can be stubs).

---

## Build order

### 0.0 The spike — half a day, before any UI code

Four questions. Answer them in a throwaway binary with a bare iced window; do not start
porting `brain-dock` until all four have measured answers. Every one of them was a
surprise on Slint, and each has a documented fallback.

| # | Question | Fallback if it fails |
|---|---|---|
| 1 | Does `Mode::Hidden` ⇄ `Mode::Windowed` **preserve the XID** and keep the wgpu surface valid? | `x11rb` `map_window`/`unmap_window` on the raw XID — already implemented in `brain-x11` |
| 2 | Does that round-trip land **inside 50 ms**? | as above; measure both paths and keep the faster |
| 3 | Does `platform_specific.application_id` produce a `WM_CLASS` i3's `for_window` actually matches? | set the property with `x11rb` before the first map |
| 4 | Does `transparent: true` give a **depth-32 ARGB visual** that picom rounds and shadows, through `iced_wgpu`/Vulkan? | opaque card + picom `corner-radius`; documented and acceptable |

```bash
# 1 and 3
xprop -name "Brain Dock" WM_CLASS          # expect: "brain-dock", "brain-dock"
xwininfo -id $XID | grep -E "Depth|Map State"   # expect Depth: 32; IsViewable ⇄ IsUnMapped

# 2
# time the round-trip from inside the app with `tracing`, not from the shell
```

### Answers — measured 2026-08-11 (`crates/dock-spike`, since deleted)

```
Q1  XID preserved across Mode::Hidden ⇄ Windowed : YES
Q2  set_mode round-trip                          : p50 91µs, worst 135µs (20 samples)
Q3  WM_CLASS from application_id                 : ["brain-dock", "brain-dock"]
Q4  visual depth                                 : 32  (ARGB)
```

**Q1 — pass, and it is the important one.** `Mode::Hidden` does *not* destroy the window.
The XID is identical across twenty hide/show cycles, so the Slint workaround is not needed.
(Building on this turned out to be stronger than "not needed": `set_mode` is the *only*
thing that works — see finding 1 below.)

**Q2 — pass, with a caveat about what was measured.** 91 µs is the `set_mode` task's
round-trip through iced's runtime, not photons on screen. What it proves is the *absence of
window recreation* — a destroy-and-recreate cannot happen in 91 µs. The real summon
latency is still the whole `brainctl toggle` → mapped → focused path, and that stays
unmeasured until C6/C7. The Slint build's 10–15 ms is the bar.

**Q3 — works, but not the way the i3 config expects.** `application_id` sets *both* halves
of `WM_CLASS` to the same string, giving `"brain-dock", "brain-dock"` rather than Stage 0's
`"brain-dock", "BrainDock"`. i3 matches `class` against the second field, so
**`for_window [class="brain-dock"]` will silently stop matching** — the exact failure this
document warns about under "for_window rules match on map".

Two fixes; take the first:

1. **Change the i3 rule to `for_window [class="brain-dock"]`.** One line, and update
   `README.md`, `brain-dock-spec.md`, and the verification commands with it.
2. Override `WM_CLASS` with `x11rb` before the first map to restore the capitalised class.
   More code, for cosmetics.

**Q4 — pass.** `transparent: true` yields a depth-32 ARGB visual through `iced_wgpu`, so
picom composites it. This was the highest-risk unknown of the port and it is closed.

**Conclusion: no fallbacks needed.** Proceed with the port on `set_mode` and
`application_id`, changing the i3 class string.

### What the port then taught us — measured 2026-08-11, `crates/brain-dock`

The spike answered its four questions correctly and none of the answers changed. Building
on top of them surfaced five more findings, none of which are guessable from the
documentation.

**1. The toolkit must own the map. This is the important one.**

Stage 0 drove `map_window`/`unmap_window` through `x11rb` because Slint's `hide()` destroyed
the window. Carrying that forward to iced does not work: **winit tracks its own visibility,
and a `MapWindow` sent behind its back does not stick.** Measured — i3 honours the request,
manages the window, sets `I3_FLOATING_WINDOW`, and it is withdrawn again immediately;
`WM_STATE` reads `Withdrawn` and even `xdotool windowmap` cannot map it.

So `window::set_mode(id, Mode::Windowed | Mode::Hidden)` is not merely the preferred
primitive, it is the only one that works, and `brain-x11`'s `show`/`hide` had to be split
around it:

```
prepare_show()  →  previous focus, work area, position   (window still unmapped)
set_mode(Windowed)                                       ← iced maps it
finish_show()   →  raise, _NET_WM_STATE_ABOVE, focus     (window now mapped)
```

`hide` is the mirror: `set_mode(Hidden)`, then `finish_hide()` parks it off-screen and
restores focus. `DockWindow::show`/`hide` are kept intact for the Slint build and remain the
documented fallback.

**2. Create the window with `visible: false`.** `raw_id` resolves *after* iced has mapped
the window, so a window that starts visible is already mapped when we adopt it — and
`_NET_WM_STATE` written with `ChangeProperty` at that point is ignored by the WM (the
`debug_assert` in `set_state_flags` catches this, loudly). Creating it unmapped puts every
persistent property on before the first map, which is also what `for_window` needs. It
retires the Slint-era `--hidden` race outright: there is no toolkit map to lose to.

**3. `iced::Daemon::scale_factor` is not a scale-factor override.** winit still guesses 1.5
on this panel, and the hook multiplies *on top* of that — the window comes up 840×93 for a
`Settings.size` of 560×62 and only the content shrinks. `WINIT_X11_SCALE_FACTOR` set before
the event loop is what fixes it (`platform.rs`), exactly as on Slint minus
`SLINT_SCALE_FACTOR`.

**4. Transparency needs the program `style` too, and the surface alpha mode is a red
herring.** `transparent: true` gives the depth-32 visual, but the theme paints an opaque
background over the whole window unless `Daemon::style` returns
`background_color: Color::TRANSPARENT`. Separately, `iced_wgpu` logs
`Available alpha modes: [Opaque]` on this NVIDIA/Vulkan stack, which looks fatal and is not
— picom composites the ARGB visual regardless. **Verified visually**, rounded corners and
all: Q4 is closed on evidence rather than on the depth number alone.

**5. iced does not size a window to its content.** Slint grew the window as the card grew;
nothing in iced 0.14 reports a view's measured size, and `window::resize` is the only way to
change it. `layout.rs` computes the height from the same tokens `view.rs` lays out with,
measuring the one variable part — the wrapped answer — with the renderer's own shaper
(`iced::advanced::graphics::text::Paragraph::min_bounds`). The two files must be read
together; a mismatch clips the answer or leaves a translucent skirt below the card.

### 0.1 Workspace

```toml
# Cargo.toml
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
edition = "2024"
rust-version = "1.90"

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
uuid = { version = "1", features = ["v4", "serde"] }
```

Plus the UI stack:

```toml
iced = { version = "0.14", features = ["wgpu", "advanced", "tokio"] }
x11rb = { version = "0.14", features = ["randr"] }
raw-window-handle = "0.6"
```

`iced` 0.14 pulls **wgpu 27, winit 0.30, raw-window-handle 0.6**. `yGraphy` must match
that wgpu version when the graph panel lands (`PLAN.md` §7.2).

Set `[profile.dev] opt-level = 1` and `[profile.dev.package."*"] opt-level = 3` — wgpu and
the text shaping stack are unusably slow in an unoptimised debug build, and you will be
iterating on UI feel constantly in this stage.

### 0.2 `brain-proto` — the IPC contract

Define this **before** either process, and never let the daemon and dock share anything
else. JSON Lines over a Unix socket, as spec §26.

```rust
// crates/brain-proto/src/lib.rs
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    Query { id: Uuid, text: String, context: DesktopContext },
    Toggle, Show, Hide,
    Cancel { id: Uuid },
    Status,
    Reindex,
    Subscribe,                 // dock announces itself as the UI
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    QueryAccepted { id: Uuid },
    RetrievalStarted { id: Uuid },
    RetrievalComplete { id: Uuid, source_count: usize },
    GenerationStarted { id: Uuid },
    Token { id: Uuid, text: String },
    Sources { id: Uuid, items: Vec<SourceRef> },
    Actions { id: Uuid, items: Vec<ActionView> },
    Complete { id: Uuid, timing: TimingInfo },
    Error { id: Option<Uuid>, message: String },
    ShowDock { context: DesktopContext },   // daemon → dock, from `brainctl toggle`
    HideDock,
    Status(StatusReport),
}
```

Two things the spec leaves implicit and you must fix now:

1. **Every event carries the query id.** Otherwise a slow query that the user has already
   abandoned will stream tokens into the next answer.
2. **`ShowDock`/`HideDock` flow daemon → dock.** `brainctl toggle` talks only to the
   daemon; the daemon owns the visibility state and relays it. This keeps `brainctl` a
   stateless one-shot binary.

Socket path: `$XDG_RUNTIME_DIR/brain/brain.sock`. On startup, unlink a stale socket only
after failing to connect to it — never unconditionally, or a second daemon silently
steals the first one's socket.

Framing: `tokio_util::codec::Framed` with `LinesCodec::new_with_max_length(1 << 20)`.

### 0.3 `brain-x11` — window control

This is the crate that makes or breaks the stage. `x11rb` with features
`["randr", "allow-unsafe-code"]`.

**Getting the XID.** `iced::window::raw_id::<Message>(id)` returns a `Task<u64>` that resolves once the window
exists — that `u64` **is** the XID. (`window::run(id, f)` gives the full `&dyn Window` if
you need more than the id.) Cache it; it does not change for the window's lifetime.

This replaces the Slint-era retry timer — the `Task` *is* the "window now exists" signal,
so there is nothing to poll.

**Setting WM_CLASS.** Must be set *before* the window is mapped or i3's `for_window` rules
will not match. On iced this is a plain setting, applied at window creation:

```rust
let mut settings = iced::window::Settings::default();
settings.platform_specific.application_id = "brain-dock".to_string();
```

Confirm the result — do not assume iced sets both halves of `WM_CLASS` the way i3 wants.
If only the instance name lands, set the property yourself with `x11rb` on the
still-unmapped window before the first map:

```bash
xprop -name "Brain Dock" WM_CLASS      # expect: "brain-dock", "brain-dock"
wmctrl -lx                             # expect: brain-dock.brain-dock
```

**Window properties to set** (all via `ChangeProperty` on the XID, atoms interned once):

| Property | Value | Why |
|---|---|---|
| `_NET_WM_WINDOW_TYPE` | `_NET_WM_WINDOW_TYPE_UTILITY` | i3 floats it without a rule |
| `_NET_WM_STATE` | `_ABOVE`, `_STICKY`, `_SKIP_TASKBAR`, `_SKIP_PAGER` | always-on-top, all workspaces |
| `_NET_WM_DESKTOP` | `0xFFFFFFFF` | sticky |
| `_MOTIF_WM_HINTS` | decorations off | frameless belt-and-braces |

When the window is **unmapped**, set `_NET_WM_STATE` directly with `ChangeProperty`. When
it is **mapped**, the WM owns it — you must send a `ClientMessage` to the root window
instead. Getting this backwards is the classic "always-on-top randomly stops working" bug.

**Show/hide.** The whole latency target depends on the window living for the entire
session and only its *mapping* toggling. On iced there is exactly one primitive for it:

```rust
iced::window::set_mode(id, iced::window::Mode::Windowed)  // show
iced::window::set_mode(id, iced::window::Mode::Hidden)    // hide
```

`Mode::Hidden` is winit `set_visible(false)` — an X11 unmap that preserves the window, and
§0.0 Q1 measured the XID surviving twenty round trips. **Slint's equivalent destroyed the
window and exited the event loop**, which is why Stage 0 used `x11rb` map/unmap instead.

That `x11rb` path is **not** a fallback here. winit tracks its own visibility and a
`MapWindow` sent behind its back is withdrawn again immediately (measured — see "What the
port then taught us" above). `brain-x11` brackets the toolkit's map instead:
`prepare_show()` positions while unmapped, `set_mode` maps, `finish_show()` raises and
focuses; `hide` is the mirror. `DockWindow::show`/`hide` remain for the Slint build.

**Focus.** After mapping, send `_NET_ACTIVE_WINDOW` as a `ClientMessage` to the root with
`data[0] = 2` (source indication: pager). i3 honours this when
`focus_on_window_activation smart` is set. Do not rely on i3 focusing it automatically.

**Monitor selection** (spec §7), via the RandR extension:

1. `GetProperty(root, _NET_ACTIVE_WINDOW)` → active XID → `GetGeometry` +
   `TranslateCoordinates` to root coords → pick the monitor containing its centre.
2. Fall back to `QueryPointer(root)`.
3. Fall back to the RandR monitor flagged primary (`GetMonitors`).
4. Fall back to monitor 0.

This machine currently has **one monitor** (eDP-1, 1920x1080). Write the logic anyway —
it is a laptop — but treat it as unverified until an external display exists, and keep it
simple rather than clever.

**Anchor to the work area, not the monitor rectangle.** polybar occupies the top 30 px
here, so `y = mon.y + 22` would put the dock underneath it. Read `_NET_WORKAREA` from the
root window and use that rectangle:

```text
x = area.x + area.width - dock.width - margin_right
y = area.y + margin_top
```

This handles polybar, any future bar, and bar toggling ($mod+b) with no special cases, for
roughly ten lines. Default `margin_top = 38` in the example config as a belt-and-braces
value for the case where `_NET_WORKAREA` is missing or wrong.

Set the position with `ConfigureWindow` on the XID **while unmapped**, so the dock never
appears in the wrong place for a frame.

**Restoring focus on hide.** Save the active XID at show-time; on hide, send
`_NET_ACTIVE_WINDOW` for it. Gate behind `restore_previous_focus` in config — it is
occasionally the wrong behaviour and you want to turn it off without recompiling.

### 0.4 `brain-dock` — iced

**Architecture. Get this right first, it shapes everything.**

Use `iced::daemon()`, not `iced::application()`. A daemon starts with **no window** and
does not exit when its windows close — which is exactly "resident from login, summoned on
a keystroke". `--hidden` becomes "do not send the initial `window::open`", so the
Slint-era race between `--hidden` and the toolkit's async map disappears entirely.

The daemon connection is a **`Subscription`**, not a background thread marshalling into
the UI. This is the part that gets simpler:

```rust
fn subscription(&self) -> Subscription<Message> {
    Subscription::run(daemon_events)   // reconnects with backoff if the daemon dies
}
```

`daemon_events` is a stream yielding `Message::Server(ServerEvent)`. No
`invoke_from_event_loop`, no `Weak` upgrades, no cross-thread UI handles — iced's runtime
owns the executor (enable the `tokio` feature so it is the same runtime `brain-proto`
expects).

`update()` folds each `ServerEvent` into the dock's state; `view()` renders it. The six
spec §4 states become one Rust enum in that state.

**Batch the tokens.** One `Message` per token wakes the runtime ~40×/s and re-lays-out a
growing text block each time; it visibly stutters. Accumulate in the stream and yield a
flush on a ~30 ms tick or when the buffer exceeds ~24 chars, whichever comes first. Do this
from the start — retrofitting it means re-tuning the animations. **This requirement is
unchanged from the Slint build and for the same reason.**

**Transparency.** `window::Settings { transparent: true, decorations: false, .. }`, and a
`Theme`/container style whose background carries alpha. With picom running you get real
rounded corners, blur, and a shadow.

**Test this on day one of the stage.** It worked on Slint's femtovg/GL path with a depth-32
ARGB visual; `iced_wgpu` goes through wgpu/Vulkan instead, so it is a genuinely open
question again rather than a formality.

Documented fallback if transparency fails: opaque window, solid `#16161d`-ish fill, corner
radius drawn by picom's `corner-radius` rule instead of by the toolkit, no blur. It looks
fine.

**UI files** — spec §40's structure, translated. They are Rust modules of the dock crate,
not a separate `ui/` tree; Slint needed its own directory because `.slint` files are
compiled by a build script, and Rust modules do not:

```text
crates/brain-dock/src/tokens.rs    radius, spacing, font sizes, durations, colours
crates/brain-dock/src/view.rs      view() and the widget tree
crates/brain-dock/src/layout.rs    how tall the window has to be for that view
crates/brain-dock/src/graph.rs     shader::Program panel (Phase E, not Stage 0)
```

Components did not survive the translation and should not be reintroduced yet. Each of the
five was a Slint file because Slint needs one per component; in iced they are four
short functions in `view.rs` totalling less than the file that would hold them. Split them
out when one grows state of its own.

**Every visual constant lives in `tokens.rs`.** The invariant is unchanged; only its home
moved. In Slint the compiler enforced the separation — here it is a review rule, so a
literal colour or radius anywhere outside `tokens.rs` is a defect. `view.rs` earns one
exemption, documented in its header: opacity is applied by scaling every token's alpha
through a `Palette`, because iced has no opacity widget.

**Geometry** (spec §41): width 560, input height 62, answer max height 520, radius 22,
margin 22. The answer expands **downward** — the top-right anchor must not move. Since the
X11 position is set by you, `window::resize` is enough; just make sure you
re-`ConfigureWindow` on height change without touching `x`/`y`.

**Animations:** show = opacity over 100 ms, ease-out; hide = opacity over 80 ms. Nothing
else animates, and the fade is bracketed correctly against the map — hide fades *then*
unmaps, or the card vanishes instead of leaving.

Two things this asked for did not ship, in the Slint build or the iced one, for the same
reason both times:

- **scale 0.97→1.0 on show.** The window is exactly card-sized, so scaling the card leaves
  a transparent margin the compositor still shadows. Worth revisiting only if the window is
  ever made larger than its card.
- **answer expand over 150 ms.** The card's growth *is* the window's growth, so animating
  it means an X11 `ConfigureWindow` every frame — expensive, and steppy because window
  sizes are whole pixels. The answer appears at its final height.

`window::frames()` drives the fade and is subscribed **only while the animation is
running**; a permanent frame subscription would hold the GPU at the refresh rate for a
window that is idle almost all of its life.

**Keyboard** (spec §5). Implement in Stage 0: `Enter`, `Esc` (answer → input → hide),
`Ctrl+L`, `Ctrl+C`, `Up`/`Down` history. Stub the rest. Route everything through
`iced::event::listen_with` (or `keyboard::on_key_press`) into the **existing keymap struct
in `keys.rs`**, which is toolkit-independent and survives the port — do not scatter key
comparisons through `view()`.

**Esc does not clear the answer** (spec §42). Keep it for a configurable grace period so
reopening a second later shows the previous result.

### 0.5 `brain-daemon` — fake pipeline

`UnixListener` accepting many connections. Track one "UI" connection (the one that sent
`Subscribe`) and any number of one-shot `brainctl` connections.

`toggle` → flip an internal `visible: bool` → send `ShowDock`/`HideDock` to the UI
connection. If no UI is connected, log and no-op (do not spawn one).

`Query` → sleep 120 ms → `RetrievalStarted` → `RetrievalComplete { source_count: 1 }` →
`GenerationStarted` → emit a fixed sentence word-by-word with 25 ms gaps → `Sources` with
one fake `SourceRef` → `Actions` with two fake buttons → `Complete`.

This fake stream is worth keeping permanently behind `--mock`. It is how you will iterate
on UI timing and animation without a model loaded.

### 0.6 `brainctl`

Connect, send one request, print or stream the reply, exit. Exit non-zero with a clear
message if the socket is absent — this is what tells you the daemon died.

`brainctl status` prints the spec §38 table (mostly `n/a` at this stage).

---

## Gotchas, collected

- **`focus_follows_mouse yes` is on** in this i3 config. The dock anchors top-right; if the
  pointer rests there or drifts across the dock, i3 moves focus and keystrokes land
  somewhere else mid-question. So: the dock grabs focus explicitly on map, and **must not
  auto-hide on focus loss** — only `Esc` or an explicit toggle hides it. If it still bites,
  toggle `focus_follows_mouse no` over i3 IPC while the dock is visible.
- **`for_window` rules match on map.** If WM_CLASS is set after mapping, they silently do
  nothing and you will chase a phantom.
- **picom may not be running.** Start it (`picom -b`) before judging the visuals, and
  confirm `pgrep picom` in `brainctl doctor` — "the dock looks flat and square" is almost
  always a dead compositor, not a toolkit problem.
- **Never `override_redirect`** (spec §6). You lose i3 focus handling and gain nothing.
- **Multi-monitor**: recompute position only on summon / monitor change / config reload
  (spec §43). Do not react to focus changes while a query is running.
- **Startup order**: `brain-dock` may start before `brain-daemon`. The dock's IPC loop
  must reconnect with backoff, not exit.
- **`XDG_RUNTIME_DIR`** may be unset in some session setups. Fail loudly with an
  actionable message rather than falling back to `/tmp` (spec §26).

---

## Definition of done

```text
$mod+a reliably shows an attractive focused dock on the active monitor.
```

Concretely, all of these pass:

```bash
pgrep picom || picom -b          # visuals need the compositor

# window identity
brainctl show && xprop -name "Brain Dock" WM_CLASS _NET_WM_STATE _NET_WM_WINDOW_TYPE

# it is floating, borderless, sticky
i3-msg -t get_tree | jq '.. | select(.window_properties?.class? == "brain-dock")
                          | {floating, border, sticky, rect}'

# focus actually landed on it
xdotool getactivewindow getwindowname

# it appears on the monitor with the focused window, on every monitor, 20/20 times
# it survives a workspace switch (sticky) and stays above other windows
# typing is instant; the fake answer streams smoothly with no stutter
# Esc hides; Super+Space reopens with the previous answer still visible
```

And, measured with `tracing` spans:

```text
brainctl toggle → window mapped and focused:   < 50 ms
```

The Slint build measured **10–15 ms**. That is the bar the port has to clear, not the
50 ms spec target — if iced lands materially slower, find out why before moving on.

If any of the above needs a workaround you are not happy with, fix it now. Stage 1
onwards assumes the window layer is solved and never revisits it.
