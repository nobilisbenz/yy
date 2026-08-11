//! `brain-dock` — the window.
//!
//! Deliberately thin. It renders what the daemon sends and forwards what the
//! user does; it holds no index, no model, and no opinions about retrieval.
//! Keeping it that way is what lets it stay resident and appear instantly.
//!
//! Threading: Slint owns the main thread and Tokio gets its own. The X11
//! controller is reachable only from the UI thread — it is not `Send`, and
//! every caller is a Slint callback or an `invoke_from_event_loop` closure,
//! both of which already run there.

mod ipc;
mod keys;
mod platform;
mod stream;
mod window;

use std::cell::{Cell, RefCell};

use anyhow::{Context, Result};
use brain_proto::{ClientRequest, ServerEvent};
use clap::Parser;
use slint::Model as _;
use window::{DockGeometry, WindowController};

slint::include_modules!();

thread_local! {
    /// UI-thread-only. See the module comment.
    static WINDOW: RefCell<Option<WindowController>> = const { RefCell::new(None) };

    /// The query currently on screen. Events for any other are discarded.
    static CURRENT_QUERY: Cell<Option<uuid::Uuid>> = const { Cell::new(None) };

    static HISTORY: RefCell<keys::History> = RefCell::new(keys::History::new());
}

fn current_query() -> Option<uuid::Uuid> {
    CURRENT_QUERY.with(Cell::get)
}

#[derive(Parser, Debug)]
#[command(name = "brain-dock", version, about = "Brain Dock window")]
struct Args {
    /// Start unmapped. This is how i3 launches it: resident from login,
    /// mapped by `brainctl toggle`.
    #[arg(long)]
    hidden: bool,

    /// Override the UI scale factor. Slint's guess is wrong on some panels;
    /// 1.0 matches an unscaled 96 DPI X session.
    #[arg(long, default_value = "1.0")]
    scale: f32,

    /// Give focus back to the previously focused window on hide (spec §42).
    #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
    restore_focus: bool,

    #[arg(long, env = "BRAIN_LOG", default_value = "info")]
    log: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(&args.log);

    platform::install(args.scale)?;
    let dock = Dock::new().context("creating the dock window")?;

    // `show()` only *schedules* window creation; winit realises it on the first
    // event-loop iteration, so the XID does not exist yet and adopting here
    // fails with "the underlying handle cannot be represented". Adoption
    // therefore happens from inside the loop — see `adopt_when_ready`.
    dock.show().context("realising the dock window")?;
    adopt_when_ready(&dock, args.hidden, args.restore_focus);

    let to_daemon = ipc::spawn({
        let weak = dock.as_weak();
        move |event| {
            let weak = weak.clone();
            // Hop to the UI thread. This is the only supported way to touch a
            // Slint component from elsewhere.
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(dock) = weak.upgrade() {
                    apply(&dock, event);
                }
            });
        }
    });

    wire_callbacks(&dock, to_daemon);

    slint::run_event_loop().context("running the Slint event loop")?;
    Ok(())
}

/// Take ownership of the X11 window once winit has created it.
///
/// Scheduled as a zero-delay timer, which fires on the first event-loop
/// iteration — after `Resumed`, and so after the window exists. The retry is
/// not superstition: window creation is asynchronous and the exact iteration it
/// lands on is a winit implementation detail we should not depend on.
fn adopt_when_ready(dock: &Dock, start_hidden: bool, restore_focus: bool) {
    try_adopt(dock.as_weak(), start_hidden, restore_focus, 0);
}

fn try_adopt(weak: slint::Weak<Dock>, start_hidden: bool, restore_focus: bool, attempt: u32) {
    const RETRY_EVERY: std::time::Duration = std::time::Duration::from_millis(10);
    const GIVE_UP_AFTER: u32 = 50; // ~500 ms

    // A repeating timer would need to stop itself, and `Timer` is neither
    // `Clone` nor reachable from its own callback. Chaining single-shots
    // sidesteps that: each failure schedules exactly one more attempt.
    slint::Timer::single_shot(
        if attempt == 0 {
            std::time::Duration::ZERO
        } else {
            RETRY_EVERY
        },
        move || {
            let Some(dock) = weak.upgrade() else { return };

            match WindowController::adopt(dock.window(), DockGeometry::default()) {
                Ok(mut controller) => {
                    controller.set_restore_focus(restore_focus);

                    let width = dock.window().size().width;
                    let result = if start_hidden {
                        controller.hide()
                    } else {
                        controller.show(width)
                    };
                    if let Err(err) = result {
                        tracing::error!("{err:#}");
                    }

                    WINDOW.with(|slot| *slot.borrow_mut() = Some(controller));
                    tracing::info!(hidden = start_hidden, attempt, "window ready");

                    if start_hidden {
                        reassert_hidden(0);
                    }
                }
                Err(err) if attempt + 1 >= GIVE_UP_AFTER => {
                    tracing::error!(
                        "{err:#}\nGiving up on the X11 window. The dock will render but \
                         cannot be positioned or toggled."
                    );
                }
                Err(_) => try_adopt(weak, start_hidden, restore_focus, attempt + 1),
            }
        },
    );
}

/// Keep a `--hidden` dock hidden through toolkit startup.
///
/// Slint maps the window itself, asynchronously, and that map can arrive after
/// our unmap — a race we lose about half the time, which showed up as the dock
/// appearing at login despite `--hidden`. The window is parked off-screen while
/// hidden so the flash is invisible either way; this closes the window state
/// itself over the first few hundred milliseconds, then stops.
fn reassert_hidden(attempt: u32) {
    const EVERY: std::time::Duration = std::time::Duration::from_millis(30);
    const ATTEMPTS: u32 = 10;

    slint::Timer::single_shot(EVERY, move || {
        // Only re-hide if nobody has legitimately summoned us in the meantime.
        if visible() {
            return;
        }
        with_window(WindowController::hide);
        if attempt + 1 < ATTEMPTS {
            reassert_hidden(attempt + 1);
        }
    });
}

/// Apply a daemon event to the UI. Runs on the UI thread.
fn apply(dock: &Dock, event: ServerEvent) {
    // Drop anything belonging to a query we have moved on from. The daemon
    // cancels superseded queries, but events already in flight still arrive.
    if let Some(id) = event.query_id()
        && Some(id) != current_query()
    {
        tracing::trace!(%id, "discarding event from a superseded query");
        return;
    }

    match event {
        ServerEvent::ShowDock { .. } => show(dock),
        ServerEvent::HideDock => hide(dock),

        ServerEvent::QueryAccepted { id } => stream::begin(id),

        ServerEvent::RetrievalStarted { .. } => {
            dock.set_state(DockState::Searching);
            dock.set_status_line("Searching…".into());
        }

        // Sources land before generation starts, so the path and the action
        // buttons are on screen while the model is still warming up.
        ServerEvent::Sources { items, .. } => {
            let primary = items.first();
            dock.set_source_path(
                primary
                    .map(|s| s.path.display().to_string())
                    .unwrap_or_default()
                    .into(),
            );
            dock.set_source_heading(
                primary
                    .map(|s| s.heading_path.clone())
                    .unwrap_or_default()
                    .into(),
            );
            dock.set_extra_sources(items.len().saturating_sub(1) as i32);
        }

        ServerEvent::Actions { items, .. } => {
            let actions: Vec<ActionItem> = items
                .iter()
                .map(|action| ActionItem {
                    label: action.label.clone().into(),
                    enabled: action.enabled,
                })
                .collect();
            dock.set_actions(slint::ModelRc::new(slint::VecModel::from(actions)));
            dock.set_selected_action(-1);
        }

        ServerEvent::GenerationStarted { .. } => {
            dock.set_answer(Default::default());
            dock.set_state(DockState::Answer);
        }

        ServerEvent::Token { id, text } => {
            let weak = dock.as_weak();
            stream::push(id, &text, move |answer| {
                if let Some(dock) = weak.upgrade() {
                    dock.set_answer(answer.into());
                }
            });
        }

        ServerEvent::Complete { timing, cache, .. } => {
            let weak = dock.as_weak();
            // Flush the last partial batch, or the answer loses its final words.
            stream::flush(move |answer| {
                if let Some(dock) = weak.upgrade() {
                    dock.set_answer(answer.into());
                }
            });
            dock.set_state(DockState::Answer);
            tracing::info!(
                retrieval_ms = timing.retrieval_ms,
                ttft_ms = timing.ttft_ms,
                total_ms = timing.total_ms,
                tokens = timing.output_tokens,
                answer_cached = cache.answer_hit,
                "query complete"
            );
        }

        ServerEvent::NoAnswer { closest, .. } => {
            // A confident "not in your files" is a feature, not an error
            // (spec §45). The model was never called.
            dock.set_answer("I couldn't find a reliable answer in your indexed files.".into());
            dock.set_state(DockState::NoAnswer);
            if let Some(first) = closest.first() {
                dock.set_source_path(first.path.display().to_string().into());
                dock.set_source_heading(first.heading_path.clone().into());
                dock.set_extra_sources(closest.len().saturating_sub(1) as i32);
            }
        }

        ServerEvent::Error { message, .. } => {
            tracing::warn!(message, "daemon reported an error");
            dock.set_status_line(message.into());
            dock.set_state(DockState::Searching);
        }

        other => tracing::trace!(?other, "unhandled event"),
    }

    // Any state change can relayout the card. For a top-right anchor only a
    // width change matters; height growth extends downward by design.
    with_window(|controller| controller.follow_resize(dock.window().size().width));
}

fn show(dock: &Dock) {
    with_window(|controller| controller.show(dock.window().size().width));
    dock.set_revealed(true);

    // Focus the field *after* mapping: focusing an unmapped window is a no-op,
    // and the dock would come up with no caret.
    dock.invoke_focus_query();
}

fn hide(dock: &Dock) {
    dock.set_revealed(false);

    // Let the fade finish before unmapping, or the card vanishes instead of
    // fading. Unmapping is what actually hides it; the animation is only worth
    // waiting for because it is shorter than a frame budget's worth of delay.
    let weak = dock.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(80), move || {
        // If the user re-summoned during the fade, leave it alone.
        if weak.upgrade().is_some_and(|dock| dock.get_revealed()) {
            return;
        }
        with_window(WindowController::hide);
    });

    // Deliberately does not clear the query or answer. Reopening a second
    // later should show what was there (spec §42).
}

fn wire_callbacks(dock: &Dock, to_daemon: tokio::sync::mpsc::UnboundedSender<ClientRequest>) {
    dock.on_dismiss({
        let weak = dock.as_weak();
        let tx = to_daemon.clone();
        move || {
            if !visible() {
                return;
            }
            // Route through the daemon rather than hiding locally, so the
            // daemon's `visible` flag stays in step. Otherwise the next
            // `brainctl toggle` would try to hide an already-hidden dock.
            let _ = tx.send(ClientRequest::Hide);
            // Hide immediately as well: waiting for the round trip would show
            // a perceptible delay on a keypress that should feel instant.
            if let Some(dock) = weak.upgrade() {
                hide(&dock);
            }
        }
    });

    dock.on_submit({
        let weak = dock.as_weak();
        let tx = to_daemon.clone();
        move |text| {
            if text.trim().is_empty() {
                return;
            }

            // Abandon whatever is running. The daemon supersedes it too, but
            // saying so explicitly means a slow query stops costing tokens the
            // moment the user asks something else.
            if let Some(previous) = current_query() {
                let _ = tx.send(ClientRequest::Cancel { id: previous });
            }

            let id = uuid::Uuid::new_v4();
            CURRENT_QUERY.with(|cell| cell.set(Some(id)));
            stream::begin(id);
            HISTORY.with(|h| h.borrow_mut().push(&text));

            if let Some(dock) = weak.upgrade() {
                dock.set_state(DockState::Searching);
                dock.set_status_line("Searching…".into());
                dock.set_answer(Default::default());
                dock.set_source_path(Default::default());
                dock.set_source_heading(Default::default());
                dock.set_extra_sources(0);
                dock.set_actions(slint::ModelRc::new(slint::VecModel::from(
                    Vec::<ActionItem>::new(),
                )));
            }

            let _ = tx.send(ClientRequest::Query {
                id,
                text: text.to_string(),
                context: Default::default(),
                retrieval_only: false,
            });
        }
    });

    dock.on_edited(|_text| {
        // Live retrieval lands in Stage 1, where there is an index to search.
    });

    dock.on_activate_action(|index| {
        tracing::info!(index, "action activated (Stage 3)");
    });

    dock.on_shortcut({
        let weak = dock.as_weak();
        let tx = to_daemon.clone();
        move |name| {
            let Some(dock) = weak.upgrade() else { return };
            let Some(command) = keys::Command::parse(&name) else {
                tracing::debug!(%name, "unknown shortcut");
                return;
            };
            handle_shortcut(&dock, command, &tx);
        }
    });

    // Slint resizes the window as the answer grows. Keep the anchored edge
    // fixed when that changes the width.
    dock.window().on_close_requested(|| {
        // A frameless dock has no close button, but a WM can still send this.
        // Hiding rather than quitting keeps the process resident.
        with_window(WindowController::hide);
        slint::CloseRequestResponse::KeepWindowShown
    });
}

fn handle_shortcut(
    dock: &Dock,
    command: keys::Command,
    tx: &tokio::sync::mpsc::UnboundedSender<ClientRequest>,
) {
    use keys::Command;

    match command {
        Command::ClearQuery => {
            dock.invoke_clear();
            dock.set_state(DockState::Input);
        }

        Command::CopyAnswer => {
            // Copy what has streamed so far, not the last flushed batch:
            // Ctrl+C mid-answer should give you everything on screen.
            let answer = stream::current();
            if answer.is_empty() {
                return;
            }
            match copy_to_clipboard(&answer) {
                Ok(()) => tracing::info!(chars = answer.len(), "answer copied"),
                Err(err) => tracing::error!("{err:#}"),
            }
        }

        Command::HistoryPrevious => {
            let current = dock.get_query().to_string();
            if let Some(entry) = HISTORY.with(|h| h.borrow_mut().previous(&current)) {
                dock.set_query(entry.into());
            }
        }
        Command::HistoryNext => {
            if let Some(entry) = HISTORY.with(|h| h.borrow_mut().next()) {
                dock.set_query(entry.into());
            }
        }

        Command::SelectNextAction | Command::SelectPreviousAction => {
            let count = dock.get_actions().row_count() as i32;
            if count == 0 {
                return;
            }
            let step = if command == Command::SelectNextAction { 1 } else { -1 };
            // Wraps in both directions; `rem_euclid` keeps -1 at the end
            // rather than off the front.
            let next = (dock.get_selected_action() + step).rem_euclid(count);
            dock.set_selected_action(next);
        }

        Command::Activate(index) => {
            if index < dock.get_actions().row_count() {
                dock.invoke_activate_action(index as i32);
            }
        }

        Command::Retry => {
            let query = dock.get_query().to_string();
            if !query.trim().is_empty() {
                dock.invoke_submit(query.into());
            }
        }

        // Both need somewhere to put the result, which arrives with the
        // correction editor (Stage 6) and the sources panel (Stage 1).
        Command::EditAnswer | Command::ShowSources => {
            tracing::info!(?command, "not implemented yet");
        }
    }

    let _ = tx;
}

/// X11 clipboard ownership requires a live process holding the selection, so
/// the copy has to outlive this function. `arboard` keeps a background thread
/// for exactly that; the handle is parked for the process lifetime rather than
/// recreated per copy, which would drop the selection each time.
fn copy_to_clipboard(text: &str) -> Result<()> {
    thread_local! {
        static CLIPBOARD: RefCell<Option<arboard::Clipboard>> = const { RefCell::new(None) };
    }

    CLIPBOARD.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(arboard::Clipboard::new().context("opening the clipboard")?);
        }
        slot.as_mut()
            .expect("just initialised")
            .set_text(text)
            .context("writing to the clipboard")
    })
}

fn visible() -> bool {
    WINDOW.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(WindowController::is_visible)
    })
}

fn with_window(action: impl FnOnce(&mut WindowController) -> Result<()>) {
    WINDOW.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(controller) = slot.as_mut() else {
            tracing::error!("window controller is not initialised");
            return;
        };
        if let Err(err) = action(controller) {
            tracing::error!("{err:#}");
        }
    });
}

fn init_tracing(filter: &str) {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_timer(fmt::time::uptime())
        .init();
}
