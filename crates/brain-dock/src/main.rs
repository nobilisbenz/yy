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
mod platform;
mod window;

use std::cell::RefCell;

use anyhow::{Context, Result};
use brain_proto::{ClientRequest, ServerEvent};
use clap::Parser;
use window::{DockGeometry, WindowController};

slint::include_modules!();

thread_local! {
    /// UI-thread-only. See the module comment.
    static WINDOW: RefCell<Option<WindowController>> = const { RefCell::new(None) };
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
    match event {
        ServerEvent::ShowDock { .. } => show(dock),
        ServerEvent::HideDock => hide(dock),

        ServerEvent::Error { message, .. } => {
            tracing::warn!(message, "daemon reported an error");
            dock.set_status_line(message.into());
            dock.set_state(DockState::Searching);
        }

        // Query events arrive in C8.
        other => tracing::trace!(?other, "unhandled event"),
    }

    // Any state change can relayout the card. For a top-right anchor only a
    // width change matters; height growth extends downward by design.
    with_window(|controller| controller.follow_resize(dock.window().size().width));
}

fn show(dock: &Dock) {
    with_window(|controller| controller.show(dock.window().size().width));

    // Focus the field *after* mapping: focusing an unmapped window is a no-op,
    // and the dock would come up with no caret.
    dock.invoke_focus_query();
}

fn hide(dock: &Dock) {
    with_window(WindowController::hide);
    // Deliberately does not clear the query or answer. Reopening a second
    // later should show what was there (spec §42).
    let _ = dock;
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
            if let Some(dock) = weak.upgrade() {
                dock.set_state(DockState::Searching);
                dock.set_status_line("Searching…".into());
            }
            let _ = tx.send(ClientRequest::Query {
                id: uuid::Uuid::new_v4(),
                text: text.to_string(),
                context: Default::default(),
                retrieval_only: false,
            });
        }
    });

    dock.on_edited(|_text| {
        // Live retrieval lands in Stage 1, where there is an index to search.
    });

    dock.on_clear_query({
        let weak = dock.as_weak();
        move || {
            if let Some(dock) = weak.upgrade() {
                dock.invoke_clear();
            }
        }
    });

    dock.on_activate_action(|index| {
        tracing::info!(index, "action activated (Stage 3)");
    });

    dock.on_copy_answer(|| {
        tracing::info!("copy (C9)");
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
