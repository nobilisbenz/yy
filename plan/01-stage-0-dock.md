# Stage 0 — The dock ✅ complete

> **Built and verified.** Commits C1–C9. Summon measured at 10–15 ms against a
> 50 ms target. What running it changed, beyond what is written below:
>
> | Assumption here | Reality |
> |---|---|
> | `_NET_WORKAREA` gives the usable area | **i3 does not publish it.** Derived from `_NET_WM_STRUT_PARTIAL` instead — and dock windows are i3's *grandchildren*, so the tree walk needs 3 levels |
> | `_NET_WM_STATE_ABOVE` keeps it on top | **i3 ignores it**, plus SKIP_TASKBAR/SKIP_PAGER. Floating + an explicit `ConfigureWindow` raise is what works |
> | The XID is available after `show()` | Window creation is deferred to the first event-loop iteration; adoption retries on a timer |
> | Slint hide/show might destroy the window | It does — and exits the event loop. Map/unmap confirmed correct |
> | Transparency is the big risk | Works. Depth-32 ARGB visual, picom on `egl` |
> | `SLINT_SCALE_FACTOR` pins the scale | Only at window creation. `WINIT_X11_SCALE_FACTOR` is needed for resizes too |
> | `--hidden` just skips the map | Races Slint's own async map; window is parked off-screen and the state re-asserted |


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
- `brain-dock`: Slint window, hidden at startup with `--hidden`.
- `brain-daemon`: Unix socket server, JSON Lines, streams fake tokens.
- `brainctl`: `toggle | show | hide | ask | status`.
- i3 rules applied and working.
- The six UI states from spec §4 wired as one Slint state enum (only Hidden / Input /
  Searching / Answer need to work; Correction and Sources can be stubs).

---

## Build order

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

Set `[profile.dev] opt-level = 1` and `[profile.dev.package."*"] opt-level = 3` — Slint
and femtovg are unusably slow in an unoptimised debug build, and you will be iterating on
UI feel constantly in this stage.

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

**Getting the XID.** Slint exposes the underlying window handle. Get it *after* the first
`show()`, via `window.window_handle()` → `raw_window_handle::RawWindowHandle::Xlib` /
`Xcb` → the `window` field. Cache it; it does not change for the window's lifetime.

**Setting WM_CLASS.** Must be set *before* the window is mapped or i3's `for_window` rules
will not match. Slint's `BackendSelector` has a winit window-attributes hook — use it to
call `WindowAttributesExtX11::with_name("brain-dock", "BrainDock")`. Verify the exact API
name against the Slint version you pull; if the hook is unavailable, the fallback is to
set the `WM_CLASS` property with x11rb on an initially-unmapped window and map it
yourself. Confirm with:

```bash
xprop -name "Brain Dock" WM_CLASS      # expect: "brain-dock", "BrainDock"
wmctrl -lx                             # expect: brain-dock.BrainDock
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

**Show/hide.** Do **not** use Slint's `hide()`/`show()`. Depending on backend version that
can destroy and recreate the winit window, which loses the XID, loses your properties,
and adds tens of milliseconds. Instead:

```rust
conn.map_window(xid)?;    // show
conn.unmap_window(xid)?;  // hide
conn.flush()?;
```

The Slint window stays alive the whole session; only its X11 mapping toggles. This is what
buys the `<50 ms` target in spec §3.1.

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

### 0.4 `brain-dock` — Slint

**Threading model. Get this right first, it shapes everything.**

Slint's event loop must own the main thread. Tokio runs on a background thread.

```rust
fn main() -> anyhow::Result<()> {
    let ui = AppWindow::new()?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ClientRequest>();
    let weak = ui.as_weak();

    std::thread::spawn(move || {
        tokio::runtime::Runtime::new().unwrap().block_on(async move {
            ipc_loop(weak, rx).await   // reconnects with backoff if daemon dies
        })
    });

    ui.on_submit({ let tx = tx.clone(); move |text| { let _ = tx.send(/* Query */); } });
    ui.run()?;   // blocks
    Ok(())
}
```

Backend → UI is always `slint::invoke_from_event_loop(move || { ui.upgrade()... })`.

**Batch the tokens.** One `invoke_from_event_loop` per token wakes the event loop ~40×/s
per token and re-renders a growing text block each time; it visibly stutters. Accumulate
tokens in the async task and flush to the UI property on a ~30 ms tick or when the buffer
exceeds ~24 chars, whichever comes first. Do this from the start — retrofitting it means
re-tuning the animations.

**Transparency.** Set `background: transparent` on the Slint `Window` and request a
transparent winit window (ARGB visual). With picom running you get real rounded corners,
blur, and a shadow. Test this on day one of the stage, because if it does not work in your
Slint version you want to know before you build a design around it.

Documented fallback if transparency fails: opaque window, solid `#16161d`-ish fill, corner
radius drawn by picom's `corner-radius` rule instead of by Slint, no blur. It looks fine.

**UI files** — follow spec §40:

```text
ui/tokens.slint       radius, spacing, font sizes, opacity, durations, colors
ui/dock.slint         Window + state machine
ui/components/        SearchInput, AnswerText, ActionButton, SourceBadge, LoadingIndicator
```

Every visual constant lives in `tokens.slint`. No colors or sizes in Rust.

**Geometry** (spec §41): width 560, input height 62, answer max height 520, radius 22,
margin 22. The answer expands **downward** — the top-right anchor must not move. Since the
X11 position is set by you, resizing the Slint window is enough; just make sure you
re-`ConfigureWindow` on height change without touching `x`/`y`.

**Animations:** show = opacity + scale 0.97→1.0 over 100 ms; hide = opacity over 80 ms;
answer expand = height over 150 ms. Nothing else animates.

**Keyboard** (spec §5). Implement in Stage 0: `Enter`, `Esc` (answer → input → hide),
`Ctrl+L`, `Ctrl+C`, `Up`/`Down` history. Stub the rest. Route them through a single
`FocusScope` and a Rust-side keymap struct so they are configurable later — do not scatter
`key == "\u{1b}"` checks through the `.slint` files.

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
  always a dead compositor, not a Slint problem.
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
i3-msg -t get_tree | jq '.. | select(.window_properties?.class? == "BrainDock")
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

If any of the above needs a workaround you are not happy with, fix it now. Stage 1
onwards assumes the window layer is solved and never revisits it.
