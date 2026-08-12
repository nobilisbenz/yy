//! `brain-dock` on iced — the window.
//!
//! Deliberately thin. It renders what the daemon sends and forwards what the
//! user does; it holds no index, no model, and no opinions about retrieval.
//! Keeping it that way is what lets it stay resident and appear instantly.
//!
//! Runtime: `iced::daemon()` rather than `iced::application()`. A daemon starts
//! with no window and does not exit when its windows close, which is exactly
//! "resident from login, summoned on a keystroke".
//!
//! The window is nonetheless opened at boot in both cases, invisible
//! (`Settings.visible = false`). Deferring the open would defer the XID with
//! it, and every X11 property has to be on the window *before* its first map —
//! i3 evaluates `for_window` at map time. `--hidden` therefore means "do not
//! map yet", and the Slint-era race against the toolkit's own async map is gone
//! rather than worked around.
//!
//! Visibility is the daemon's to decide (that is what keeps `brainctl` a
//! stateless one-shot binary), so nothing here toggles the window on its own;
//! it acts on `ShowDock`/`HideDock`.

mod graph;
mod ipc;
mod keys;
mod layout;
mod platform;
mod tokens;
mod view;
mod window;

use std::time::Instant;

use brain_proto::{ActionView, ClientRequest, DesktopContext, ServerEvent, SourceRef};
use clap::Parser;
use iced::futures::channel::mpsc;
use iced::{Element, Subscription, Task, Theme};
use uuid::Uuid;

use window::{DockGeometry, WindowController};

/// Spec §4's states, minus Hidden — hiding is an X11 unmap, not a UI state, so
/// the window has nothing to draw for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockState {
    Input,
    Searching,
    Answer,
    NoAnswer,
}

/// Quiet period after a keystroke before retrieval runs.
///
/// 80 ms is comfortably under the threshold where typing feels laggy and comfortably over
/// the interval between keystrokes at speed, so a fast typist sends one query per word
/// rather than one per letter.
const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(80);

/// The retrieved source shown under the answer.
pub struct Source {
    pub path: String,
    pub heading: String,
    /// Empty for a source with no vault identity; the graph panel skips those.
    pub section_uid: String,
    /// Why this section was retrieved — `matched heading · 1 hop to contradicts`.
    ///
    /// A feature and the debugger for graph retrieval out of one implementation: when the
    /// wrong section comes back, this says whether the seed or the expansion was at fault.
    pub explain: String,
}

#[derive(Parser, Debug)]
#[command(name = "brain-dock", version, about = "Brain Dock window")]
struct Args {
    /// Start hidden. This is how i3 launches it: resident from login, revealed
    /// by `brainctl toggle`.
    #[arg(long)]
    hidden: bool,

    /// Override the UI scale factor. 1.0 matches an unscaled 96 DPI X session,
    /// which is what the geometry tokens are drawn against.
    #[arg(long, default_value = "1.0")]
    scale: f32,

    /// Give focus back to the previously focused window on hide (spec §42).
    #[arg(long, default_value = "true", action = clap::ArgAction::Set)]
    restore_focus: bool,

    #[arg(long, env = "BRAIN_LOG", default_value = "info")]
    log: String,
}

fn main() -> iced::Result {
    let args = Args::parse();
    init_tracing(&args.log);
    platform::pin_scale_factor(args.scale);

    iced::daemon(
        move || Dock::boot(&args),
        Dock::update,
        Dock::view as fn(&Dock, iced::window::Id) -> Element<'_, Message>,
    )
    .title(|_state: &Dock, _id| String::from("Brain Dock"))
    .theme(|_state: &Dock, _id| Theme::Dark)
    // Without this the theme paints an opaque background over the whole
    // window and the depth-32 ARGB visual buys nothing — the rounded corners
    // and the shadow both come from picom seeing through to the desktop.
    // The card's own translucent fill is `view`'s container style.
    .style(|_state: &Dock, _theme| iced::theme::Style {
        background_color: iced::Color::TRANSPARENT,
        text_color: tokens::FG,
    })
    .subscription(Dock::subscription)
    .run()
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

#[derive(Debug, Clone)]
pub enum Message {
    Ipc(ipc::Event),
    WindowOpened(iced::window::Id),
    /// The XID, from `window::raw_id`. Adoption happens on receipt.
    Adopted(u64),
    QueryChanged(String),
    /// The correction editor's text changed.
    CorrectionChanged(String),
    /// The debounce after a keystroke has elapsed. Carries the generation it was
    /// scheduled for, so only the most recent keystroke's timer does anything.
    SearchDebounceElapsed(u64),
    Submit,
    /// A named shortcut, resolved through `keys::Command`.
    Command(keys::Command),
    Dismiss,
    /// A frame, while the summon fade is running.
    Tick(std::time::Instant),
    /// A frame, while the graph panel's force simulation is still moving.
    GraphTick,
    /// The user did something to the graph panel.
    Graph(ygraphy::panel::Interaction),
    /// The fade-out has had its time; the window may be unmapped now.
    FadeOutElapsed,
    /// The toolkit finished mapping or unmapping the window.
    VisibilityChanged(bool),
    /// The window manager asked the window to close.
    CloseRequested,
    ActivateAction(usize),
}

impl From<ygraphy::panel::Interaction> for Message {
    fn from(interaction: ygraphy::panel::Interaction) -> Self {
        Self::Graph(interaction)
    }
}

pub struct Dock {
    // --- runtime wiring ----------------------------------------------------
    window: Option<iced::window::Id>,
    x11: Option<WindowController>,
    geometry: DockGeometry,
    /// Newest sender from the IPC subscription. `None` while disconnected —
    /// the dock stays usable and simply has nowhere to send.
    requests: Option<mpsc::Sender<ClientRequest>>,
    start_hidden: bool,
    restore_focus: bool,
    /// Height last asked of `window::resize`. Compared against every update so
    /// a token batch that does not add a line costs no X11 traffic.
    requested_height: f32,

    /// The graph panel, once it has been opened at least once. Loading a vault and
    /// laying it out is not work a dock resident from login should do for a panel nobody
    /// asked for, so this stays `None` until the first `SetGraphVisible { true }`.
    pub graph: Option<graph::GraphPanel>,
    pub graph_visible: bool,

    /// The summon fade (spec §41). `true` means "on screen"; the interpolated
    /// value is the card's opacity.
    reveal: iced::Animation<bool>,
    /// Frame time, from the `frames()` subscription. Held in state so `view`
    /// stays a pure function of the model rather than reading the clock.
    now: Instant,

    // --- what is on screen -------------------------------------------------
    pub state: DockState,
    pub query: String,
    pub answer: String,
    pub status_line: String,
    pub source: Option<Source>,
    pub extra_sources: usize,
    pub actions: Vec<ActionView>,
    pub selected_action: Option<usize>,
    /// The body currently holds a failure, not an answer. Not a `DockState` —
    /// spec §4 has six states and this is a colour, not a seventh one — but
    /// "the daemon broke" and "your notes do not say" must not look alike.
    pub failed: bool,

    /// The query currently on screen. Events for any other are discarded: an
    /// abandoned query keeps streaming for a moment, and without this its tail
    /// lands in the next answer.
    current_query: Option<Uuid>,
    history: keys::History,

    /// Bumped on every keystroke. A debounce timer only fires if the generation it was
    /// scheduled for is still the current one, which is what makes the timers
    /// self-cancelling without holding a handle to abort.
    search_generation: u64,
    /// The live retrieval-only query in flight, if any.
    ///
    /// Tracked apart from `current_query` because the two are cancelled at different
    /// moments: a keystroke supersedes the previous *search*, but must not cancel the
    /// generation the user already pressed Enter on.
    live_query: Option<Uuid>,
    /// What `live_query` was searching for, so an unchanged query is not re-sent.
    live_text: String,
    /// The in-progress correction text, when the answer is being edited.
    ///
    /// `Some` *is* spec §4's Correction state — kept here rather than as a `DockState`
    /// variant because the answer stays on screen underneath and the state machine already
    /// distinguishes what is shown, not what is focused.
    pub correcting: Option<String>,
}

impl Dock {
    fn new() -> Self {
        Self {
            window: None,
            x11: None,
            geometry: DockGeometry::default(),
            requests: None,
            start_hidden: false,
            restore_focus: true,
            requested_height: tokens::INPUT_HEIGHT,
            // Starts hidden and fades in on the first summon, whichever way the
            // process was launched: an unmapped window has nothing to show, and
            // a `--hidden`-less start is still an arrival.
            reveal: iced::Animation::new(false).easing(iced::animation::Easing::EaseOut),
            now: Instant::now(),
            state: DockState::Input,
            query: String::new(),
            answer: String::new(),
            status_line: String::new(),
            source: None,
            extra_sources: 0,
            actions: Vec::new(),
            selected_action: None,
            failed: false,
            graph: None,
            graph_visible: false,
            current_query: None,
            history: keys::History::new(),
            search_generation: 0,
            live_query: None,
            live_text: String::new(),
            correcting: None,
        }
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::new()
    }

    fn boot(args: &Args) -> (Self, Task<Message>) {
        let dock = Self {
            start_hidden: args.hidden,
            restore_focus: args.restore_focus,
            ..Self::new()
        };

        // Opened invisible either way; see the module comment.
        let (_id, open) = iced::window::open(window_settings());

        (dock, open.map(Message::WindowOpened))
    }

    fn subscription(&self) -> Subscription<Message> {
        // Only while the card is actually fading. iced is event-driven, so a
        // permanently subscribed `frames()` would hold the GPU at the refresh
        // rate for a window that is resident from login and idle nearly all of
        // it — the exact cost `PLAN.md` §2.5 asks the graph panel to avoid.
        let animating = if self.reveal.is_animating(self.now) {
            iced::window::frames().map(Message::Tick)
        } else {
            Subscription::none()
        };

        // Same rule as the fade: subscribe only while there is motion to show. A
        // settled graph costs nothing, which is what makes a panel that can stay open
        // all day acceptable in a process resident from login.
        let simulating = match &self.graph {
            Some(graph) if self.graph_visible && !graph.is_settled() => {
                iced::window::frames().map(|_| Message::GraphTick)
            }
            _ => Subscription::none(),
        };

        Subscription::batch([
            animating,
            simulating,
            ipc::connect().map(Message::Ipc),
            // `listen_raw`, not `listen`: the text input captures most keys, and
            // Esc / Ctrl+L / Alt+1 have to reach us anyway.
            iced::event::listen_raw(|event, _status, _window| match event {
                iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key, modifiers, ..
                }) => shortcut(&key, modifiers),
                _ => None,
            }),
            // A frameless dock has no close button, but a WM can still ask.
            // Closing would destroy the window and with it the XID and every
            // property on it, so this is always answered by hiding — and it is
            // not `Dismiss`, which steps back through the UI states first.
            iced::window::close_requests().map(|_id| Message::CloseRequested),
        ])
    }

    /// Fold the message in, then reconcile the window with what that produced.
    ///
    /// The resize has to happen here rather than in `view`: iced never tells a
    /// program how tall its view came out, so the window and the card are kept
    /// in step by computing the height from the same state both are drawn from
    /// (`layout::window_height`).
    fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.dispatch(message);
        Task::batch([task, self.fit_window()])
    }

    fn fit_window(&mut self) -> Task<Message> {
        let Some(id) = self.window else {
            return Task::none();
        };

        let height = layout::window_height(self);
        // Sub-pixel differences are not worth a round trip, and comparing
        // floats for equality would make every token batch a resize.
        if (height - self.requested_height).abs() < 1.0 {
            return Task::none();
        }
        self.requested_height = height;

        // Only the width moves the anchored edge — height grows downward by
        // design (spec §41) — so this is a no-op today and stays correct if the
        // dock ever becomes width-adaptive.
        if let Some(x11) = self.x11.as_mut()
            && let Err(err) = x11.follow_resize(tokens::DOCK_WIDTH as u32)
        {
            tracing::error!("{err:#}");
        }

        iced::window::resize(id, iced::Size::new(tokens::DOCK_WIDTH, height))
    }

    fn dispatch(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowOpened(id) => {
                self.window = Some(id);
                iced::window::raw_id::<Message>(id).map(Message::Adopted)
            }

            Message::Adopted(raw) => {
                match WindowController::adopt(raw, self.geometry) {
                    Ok(mut controller) => {
                        controller.set_restore_focus(self.restore_focus);
                        self.x11 = Some(controller);
                        // The window is created unmapped, so `--hidden` needs
                        // nothing done to it. Anything else is an ordinary
                        // summon, properties and all.
                        if self.start_hidden {
                            Task::none()
                        } else {
                            self.set_visible(true)
                        }
                    }
                    Err(err) => {
                        // Not fatal: an un-adopted window is an ordinary
                        // floating window, which is ugly but usable, and saying
                        // so beats exiting at login with no explanation.
                        tracing::error!("{err:#}");
                        Task::none()
                    }
                }
            }

            Message::Ipc(event) => self.on_ipc(event),

            Message::CorrectionChanged(text) => {
                self.correcting = Some(text);
                Task::none()
            }

            Message::QueryChanged(text) => {
                self.query = text;
                self.schedule_search()
            }

            Message::SearchDebounceElapsed(generation) => {
                // A newer keystroke has already scheduled its own timer. Letting this one
                // through would send a query for a prefix the user has moved past.
                if generation != self.search_generation {
                    return Task::none();
                }
                self.search_now()
            }

            Message::Submit => self.submit(),

            Message::Dismiss => {
                // Esc walks back one step rather than hiding outright, and it
                // never clears the answer — reopening a second later should
                // show the previous result (spec §42).
                // Esc walks back one step, and an open editor is the innermost step.
                if self.correcting.take().is_some() {
                    self.status_line = String::from("correction discarded");
                    return Task::none();
                }
                match self.state {
                    DockState::Answer | DockState::NoAnswer | DockState::Searching => {
                        self.state = DockState::Input;
                    }
                    DockState::Input => {
                        if !self.is_visible() {
                            return Task::none();
                        }
                        // Tell the daemon, so its `visible` flag stays in step
                        // and the next `brainctl toggle` does not try to hide an
                        // already-hidden dock — but hide locally right now
                        // rather than waiting out the round trip, which is
                        // perceptible on a keypress that should feel instant.
                        self.send(ClientRequest::Hide);
                        return self.set_visible(false);
                    }
                }
                Task::none()
            }

            Message::Tick(now) => {
                self.now = now;
                Task::none()
            }

            Message::GraphTick => {
                if let Some(graph) = self.graph.as_mut() {
                    graph.tick();
                }
                Task::none()
            }

            Message::Graph(interaction) => {
                let Some(graph) = self.graph.as_mut() else {
                    return Task::none();
                };
                if let Some(section_uid) = graph.on_interaction(interaction) {
                    // Jumping lands in Stage 3 with the rest of the actions; the uid is
                    // logged now so the wiring is visible and the shape does not change
                    // when it arrives.
                    tracing::info!(%section_uid, "graph section activated");
                }
                Task::none()
            }

            Message::FadeOutElapsed => {
                // The user may have re-summoned mid-fade, in which case the
                // window is on its way back in and must not be unmapped.
                if self.reveal.value() {
                    tracing::debug!("re-summoned during the fade; staying up");
                    return Task::none();
                }
                let Some(id) = self.window else {
                    return Task::none();
                };
                iced::window::set_mode::<Message>(id, iced::window::Mode::Hidden)
                    .chain(Task::done(Message::VisibilityChanged(false)))
            }

            Message::VisibilityChanged(visible) => {
                if let Some(x11) = self.x11.as_mut() {
                    let result = if visible {
                        x11.finish_show()
                    } else {
                        x11.finish_hide()
                    };
                    if let Err(err) = result {
                        tracing::error!("{err:#}");
                    }
                }

                if visible {
                    // Both of these need the window to be mapped already:
                    // focusing an unmapped window is a no-op, and fading in one
                    // shows nobody anything.
                    self.animate_reveal(true);
                    return iced::widget::operation::focus(view::query_input_id());
                }
                Task::none()
            }

            Message::CloseRequested => {
                tracing::debug!("close requested; hiding instead");
                self.send(ClientRequest::Hide);
                self.set_visible(false)
            }

            Message::Command(command) => self.on_command(command),

            Message::ActivateAction(index) => {
                self.selected_action = Some(index);
                // Targets land in Stage 3; the wiring is here so the UI does not
                // change shape when they do.
                tracing::info!(index, "action activated");
                Task::none()
            }
        }
    }

    fn view(&self, _id: iced::window::Id) -> Element<'_, Message> {
        view::view(self)
    }

    // -----------------------------------------------------------------------

    fn on_ipc(&mut self, event: ipc::Event) -> Task<Message> {
        match event {
            ipc::Event::Connected(sender) => {
                self.requests = Some(sender);
                Task::none()
            }
            ipc::Event::Disconnected => {
                self.requests = None;
                Task::none()
            }
            ipc::Event::Tokens { id, text } => {
                if self.current_query == Some(id) {
                    self.answer.push_str(&text);
                    self.state = DockState::Answer;
                }
                Task::none()
            }
            ipc::Event::Server(event) => self.on_server(*event),
        }
    }

    fn on_server(&mut self, event: ServerEvent) -> Task<Message> {
        // Discard stragglers from a query that is no longer on screen. Events
        // with no query id (visibility, status) are always ours.
        if let Some(id) = event.query_id()
            && self.current_query != Some(id)
        {
            return Task::none();
        }

        match event {
            // Focus is claimed in `VisibilityChanged`, once the window is
            // actually mapped.
            ServerEvent::ShowDock { context } => {
                tracing::debug!(?context, "shown");
                return self.set_visible(true);
            }
            ServerEvent::HideDock => return self.set_visible(false),

            ServerEvent::SetGraphVisible { visible } => self.set_graph_visible(visible),

            // A live search must not repaint the dock as "searching…" on every keystroke.
            // It resolves in well under a millisecond, so the spinner would be a flicker
            // rather than feedback — and the previous results should stay on screen until
            // the new ones replace them.
            ServerEvent::RetrievalStarted { id } if self.live_query == Some(id) => {}
            ServerEvent::RetrievalStarted { .. } => {
                self.state = DockState::Searching;
                self.status_line = String::from("searching…");
            }
            ServerEvent::RetrievalComplete { source_count, .. } => {
                self.status_line = match source_count {
                    0 => String::from("no sources"),
                    1 => String::from("1 source"),
                    n => format!("{n} sources"),
                };
            }
            ServerEvent::Sources { items, .. } => self.set_sources(&items),
            ServerEvent::Actions { items, .. } => {
                self.selected_action = if items.is_empty() { None } else { Some(0) };
                self.actions = items;
            }
            ServerEvent::GenerationStarted { .. } => {
                self.answer.clear();
                self.state = DockState::Answer;
            }
            // A finished live search leaves its sources on screen without claiming there is
            // an answer to read — there is no prose, and Enter has not been pressed.
            ServerEvent::Complete { id, .. } if self.live_query == Some(id) => {
                self.live_query = None;
                if self.state == DockState::Searching {
                    self.state = DockState::Input;
                }
            }
            ServerEvent::Complete { timing, .. } => {
                self.state = DockState::Answer;
                tracing::debug!(total_ms = timing.total_ms, "query complete");
            }
            // Typing a word that matches nothing yet is the normal path through a query
            // being typed, not a verdict on it. Saying "no reliable answer" mid-word would
            // be wrong more often than right.
            ServerEvent::NoAnswer { id, .. } if self.live_query == Some(id) => {
                self.live_query = None;
                self.source = None;
                self.extra_sources = 0;
                self.actions.clear();
                self.selected_action = None;
                if self.state == DockState::Searching {
                    self.state = DockState::Input;
                }
            }
            ServerEvent::NoAnswer { closest, .. } => {
                // A confident "not in your files" is a feature, not a failure
                // (spec §45). The model was never called.
                self.state = DockState::NoAnswer;
                self.answer = String::from("I couldn't find a reliable answer in your notes.");
                self.set_sources(&closest);
            }
            ServerEvent::Error { message, .. } => {
                tracing::warn!(message, "daemon reported an error");
                self.state = DockState::NoAnswer;
                self.failed = true;
                self.answer = message;
            }

            ServerEvent::QueryAccepted { .. }
            | ServerEvent::Token { .. }
            | ServerEvent::Status(_) => {}
        }

        Task::none()
    }

    fn set_sources(&mut self, items: &[SourceRef]) {
        self.source = items.first().map(|first| Source {
            path: first.path.display().to_string(),
            heading: first.heading_path.clone(),
            section_uid: first.section_uid.clone(),
            explain: first.explain.clone(),
        });
        self.extra_sources = items.len().saturating_sub(1);

        // Re-seed the panel on whatever the answer is actually about. This is the whole
        // point of the panel per `PLAN.md` §7 — the neighbourhood of the answer you are
        // reading, not the vault as a bag of dots — and it is what makes it double as the
        // Phase D retrieval debugger.
        if let (Some(graph), Some(primary)) = (self.graph.as_mut(), items.first())
            && !primary.section_uid.is_empty()
        {
            graph.focus_on(&primary.section_uid);
        }
    }

    /// Open or close the graph panel.
    ///
    /// The first open pays for reading the vault and laying it out; later ones are free.
    /// A failure to load is not fatal — the dock is still a dock — but a keypress that
    /// silently does nothing looks broken, so the failure is surfaced in the status line.
    fn set_graph_visible(&mut self, visible: bool) {
        self.graph_visible = visible;
        if !visible {
            self.status_line = String::from("graph hidden");
            return;
        }

        if self.graph.is_none() {
            match ygraphy::vault::resolve(None).and_then(|vault| graph::GraphPanel::load(&vault)) {
                Ok(panel) => self.graph = Some(panel),
                Err(err) => {
                    tracing::error!("could not open the graph: {err:#}");
                    // `resolve(None)` fails with `reading <path>; run yalive or pass --vault`
                    // when no `last-vault` exists — that single line tells the user both
                    // what went wrong and what to do, so it is shown verbatim.
                    let message = err
                        .chain()
                        .last()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| err.to_string());
                    self.status_line = format!("graph: {message}");
                    self.graph_visible = false;
                    return;
                }
            }
        }

        // Opening onto the current answer's source, if there is one, rather than onto
        // wherever the camera happened to be left.
        if let (Some(graph), Some(source)) = (self.graph.as_mut(), self.source.as_ref()) {
            let uid = source.section_uid.clone();
            if !uid.is_empty() {
                graph.focus_on(&uid);
            }
        }
    }

    /// Start the debounce after a keystroke.
    ///
    /// Retrieval is live (`09-decisions.md` §4): FTS5 answers in well under a millisecond,
    /// so results while typing are effectively free, and they reframe the generated answer
    /// as an addition to instant search rather than the only output. Enter stays reserved
    /// for generation, which is the expensive half.
    fn schedule_search(&mut self) -> Task<Message> {
        self.search_generation = self.search_generation.wrapping_add(1);
        let generation = self.search_generation;

        if self.query.trim().is_empty() {
            // Clearing the box clears the results with it, rather than leaving the last
            // query's sources stranded under an empty input.
            self.cancel_live_query();
            self.live_text.clear();
            self.source = None;
            self.extra_sources = 0;
            self.actions.clear();
            self.selected_action = None;
            if self.state == DockState::Searching {
                self.state = DockState::Input;
            }
            return Task::none();
        }

        Task::perform(
            async move {
                tokio::time::sleep(SEARCH_DEBOUNCE).await;
                generation
            },
            Message::SearchDebounceElapsed,
        )
    }

    /// Issue the live, retrieval-only query.
    fn search_now(&mut self) -> Task<Message> {
        let text = self.query.trim().to_string();
        if text.is_empty() || text == self.live_text {
            // Typing and deleting a character lands back on the same query. Re-running it
            // would repaint identical results for no reason.
            return Task::none();
        }

        self.cancel_live_query();

        let id = Uuid::new_v4();
        self.live_query = Some(id);
        self.live_text = text.clone();
        self.current_query = Some(id);

        self.send(ClientRequest::Query {
            id,
            text,
            context: DesktopContext::default(),
            retrieval_only: true,
        });

        Task::none()
    }

    /// Abandon the live search, if one is running.
    ///
    /// Deliberately does not touch a submitted query: pressing Enter and then typing must
    /// not cancel the generation that Enter started.
    fn cancel_live_query(&mut self) {
        if let Some(previous) = self.live_query.take() {
            self.send(ClientRequest::Cancel { id: previous });
        }
    }

    fn submit(&mut self) -> Task<Message> {
        let text = self.query.trim().to_string();
        if text.is_empty() {
            return Task::none();
        }

        self.history.push(&text);

        // A pending debounce would fire mid-generation and replace the answer's sources
        // with its own. Bumping the generation makes that timer a no-op.
        self.search_generation = self.search_generation.wrapping_add(1);
        self.cancel_live_query();
        self.live_text.clear();

        // Abandon whatever is running. The daemon supersedes it anyway, but
        // saying so explicitly means a slow query stops costing tokens the
        // moment the user asks something else.
        if let Some(previous) = self.current_query {
            self.send(ClientRequest::Cancel { id: previous });
        }

        let id = Uuid::new_v4();
        self.current_query = Some(id);
        self.answer.clear();
        self.source = None;
        self.extra_sources = 0;
        self.actions.clear();
        self.selected_action = None;
        self.failed = false;
        self.state = DockState::Searching;
        self.status_line = String::from("searching…");

        self.send(ClientRequest::Query {
            id,
            text,
            context: DesktopContext::default(),
            retrieval_only: false,
        });

        Task::none()
    }

    fn on_command(&mut self, command: keys::Command) -> Task<Message> {
        use keys::Command;

        match command {
            Command::ClearQuery => {
                self.query.clear();
                return iced::widget::operation::focus(view::query_input_id());
            }
            Command::CopyAnswer => {
                if self.answer.is_empty() {
                    return Task::none();
                }
                // iced's clipboard, not `arboard`: X11 selection ownership
                // needs a live owner for as long as the paste might happen, and
                // iced's runtime already holds one for the process lifetime.
                tracing::info!(chars = self.answer.len(), "answer copied");
                return iced::clipboard::write(self.answer.clone());
            }
            Command::HistoryPrevious => {
                if let Some(entry) = self.history.previous(&self.query) {
                    self.query = entry;
                }
            }
            Command::HistoryNext => {
                if let Some(entry) = self.history.next() {
                    self.query = entry;
                }
            }
            Command::SelectNextAction => self.move_selection(1),
            Command::SelectPreviousAction => self.move_selection(-1),
            Command::Activate(index) => {
                if index < self.actions.len() {
                    return self.dispatch(Message::ActivateAction(index));
                }
            }
            Command::Correct => {
                // Pre-load the answer so a correction is an edit rather than a retype —
                // most corrections change a flag or a path, not the whole sentence.
                if self.answer.is_empty() || self.current_query.is_none() {
                    return Task::none();
                }
                self.correcting = Some(self.answer.clone());
                self.status_line = String::from("editing · Ctrl+Enter saves, Esc cancels");
                return iced::widget::operation::focus(view::correction_input_id());
            }
            Command::SaveCorrection => {
                let (Some(answer), Some(id)) = (self.correcting.take(), self.current_query)
                else {
                    return Task::none();
                };
                if answer.trim().is_empty() {
                    return Task::none();
                }
                self.answer = answer.clone();
                self.send(ClientRequest::SaveCorrection { id, answer });
                self.status_line = String::from("correction saved");
                return Task::none();
            }
            Command::Rate(good) => {
                let Some(id) = self.current_query else {
                    return Task::none();
                };
                // Deliberately silent about whether it landed: the daemon may have
                // forgotten the query, and a rating is a hint, not a transaction.
                self.send(ClientRequest::RateAnswer { id, good });
                self.status_line = if good {
                    String::from("rated good")
                } else {
                    String::from("rated bad")
                };
                return Task::none();
            }
            Command::Retry => {
                if !self.query.trim().is_empty() {
                    return self.submit();
                }
            }
            // Both need somewhere to put the result: the correction editor
            // (Stage 6) and the sources panel (Stage 1).
            Command::ShowSources | Command::EditAnswer => {
                tracing::debug!(?command, "not wired yet");
            }
        }

        Task::none()
    }

    fn move_selection(&mut self, delta: isize) {
        if self.actions.is_empty() {
            return;
        }
        let count = self.actions.len() as isize;
        let current = self.selected_action.unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(count);
        self.selected_action = Some(next as usize);
    }

    /// The card's opacity right now. Drives the summon fade in `view`.
    pub fn opacity(&self) -> f32 {
        self.reveal.interpolate(0.0, 1.0, self.now)
    }

    /// Whether the dock counts as on screen.
    ///
    /// A window that is mapped but already fading out does **not** — otherwise
    /// a second `Esc` during the fade would send a second `Hide` and the next
    /// `brainctl toggle` would show a dock the daemon believes is hidden.
    fn is_visible(&self) -> bool {
        self.reveal.value() && self.x11.as_ref().is_some_and(WindowController::is_visible)
    }

    /// Show or hide the dock.
    ///
    /// The map itself goes through iced (`Mode::Windowed` / `Mode::Hidden`),
    /// not through `x11rb`. That is the Stage 0′ §0.0 Q1 primitive, and it is
    /// also the only one that works: winit tracks its own visibility, and a
    /// `MapWindow` sent behind its back is honoured by i3 and then withdrawn
    /// again within the same second. `brain-x11` brackets the toolkit's map
    /// with the parts iced has no API for — placement, stacking, focus.
    ///
    /// The fade sits on opposite sides of that map in each direction: show
    /// maps first and fades in once there is a window to fade (the fade starts
    /// in `VisibilityChanged`), hide fades first and unmaps after, or the card
    /// would vanish instead of leaving.
    fn set_visible(&mut self, visible: bool) -> Task<Message> {
        let Some(id) = self.window else {
            return Task::none();
        };
        tracing::debug!(visible, "setting dock visibility");

        if visible {
            if let Some(x11) = self.x11.as_mut()
                && let Err(err) = x11.prepare_show(self.geometry.width)
            {
                tracing::error!("{err:#}");
            }
            iced::window::set_mode::<Message>(id, iced::window::Mode::Windowed)
                .chain(Task::done(Message::VisibilityChanged(true)))
        } else {
            self.animate_reveal(false);
            // Sleeping rather than waiting for the animation to report itself
            // done: `frames()` stops arriving the moment the window stops being
            // drawn, so an animation-driven completion could never fire.
            Task::perform(tokio::time::sleep(tokens::HIDE), |()| {
                Message::FadeOutElapsed
            })
        }
    }

    /// Start the fade towards `visible`.
    ///
    /// `Animation::duration` consumes the animation, and show and hide are
    /// deliberately different lengths, so the animation is taken out and put
    /// back rather than mutated in place.
    fn animate_reveal(&mut self, visible: bool) {
        let duration = if visible { tokens::SHOW } else { tokens::HIDE };
        self.now = Instant::now();

        let reveal = std::mem::replace(&mut self.reveal, iced::Animation::new(visible));
        self.reveal = reveal.duration(duration).go(visible, self.now);
    }

    /// Fire-and-forget. A full or absent channel means the daemon is gone,
    /// which the subscription is already handling — dropping the request is
    /// better than blocking the UI on a socket.
    fn send(&mut self, request: ClientRequest) {
        let Some(sender) = self.requests.as_mut() else {
            tracing::debug!("not connected; dropping request");
            return;
        };
        if let Err(err) = sender.try_send(request) {
            tracing::warn!(%err, "could not queue request");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dock with no daemon connection. `send` drops requests when disconnected, so the
    /// state machine can be driven directly without a socket or a running event loop.
    fn dock() -> Dock {
        Dock::new()
    }

    fn type_query(dock: &mut Dock, text: &str) {
        dock.query = text.to_string();
        let _ = dock.schedule_search();
    }

    #[test]
    fn a_stale_debounce_does_not_search_for_a_prefix_the_user_typed_past() {
        let mut dock = dock();
        type_query(&mut dock, "wire");
        let stale = dock.search_generation;
        type_query(&mut dock, "wireguard");

        // The timer scheduled for "wire" fires late. Honouring it would replace the
        // results for "wireguard" with results for a prefix already moved past — the
        // characteristic search-as-you-type bug.
        let _ = dock.dispatch(Message::SearchDebounceElapsed(stale));
        assert!(dock.live_query.is_none(), "a stale timer issued a query");

        let _ = dock.dispatch(Message::SearchDebounceElapsed(dock.search_generation));
        assert!(dock.live_query.is_some(), "the current timer issued nothing");
        assert_eq!(dock.live_text, "wireguard");
    }

    #[test]
    fn retyping_the_same_query_does_not_search_again() {
        let mut dock = dock();
        type_query(&mut dock, "wireguard");
        let _ = dock.dispatch(Message::SearchDebounceElapsed(dock.search_generation));
        let first = dock.live_query;

        // Typing a character and deleting it lands back on the same text.
        type_query(&mut dock, "wireguardx");
        type_query(&mut dock, "wireguard");
        let _ = dock.dispatch(Message::SearchDebounceElapsed(dock.search_generation));

        assert_eq!(dock.live_query, first, "an unchanged query was re-sent");
    }

    #[test]
    fn clearing_the_box_clears_the_results_with_it() {
        let mut dock = dock();
        type_query(&mut dock, "wireguard");
        let _ = dock.dispatch(Message::SearchDebounceElapsed(dock.search_generation));
        dock.set_sources(&[source_ref("obs#root")]);
        assert!(dock.source.is_some());

        type_query(&mut dock, "");
        assert!(dock.source.is_none(), "results outlived the query that found them");
        assert!(dock.live_query.is_none());
    }

    #[test]
    fn a_pending_debounce_cannot_disturb_a_submitted_query() {
        // Type, then press Enter before the debounce elapses. The timer must not fire and
        // replace the answer's sources with a live search's, and it must not cancel the
        // generation Enter started.
        let mut dock = dock();
        type_query(&mut dock, "wireguard");
        let pending = dock.search_generation;

        let _ = dock.submit();
        let submitted = dock.current_query.expect("Enter did not start a query");

        let _ = dock.dispatch(Message::SearchDebounceElapsed(pending));
        assert!(dock.live_query.is_none(), "the stale debounce ran");
        assert_eq!(
            dock.current_query,
            Some(submitted),
            "the debounce displaced the submitted query"
        );
    }

    #[test]
    fn a_live_search_finishing_does_not_claim_there_is_an_answer() {
        let mut dock = dock();
        type_query(&mut dock, "wireguard");
        let _ = dock.dispatch(Message::SearchDebounceElapsed(dock.search_generation));
        let live = dock.live_query.expect("no live query");

        dock.state = DockState::Searching;
        let _ = dock.on_server(ServerEvent::Complete {
            id: live,
            timing: Default::default(),
            cache: Default::default(),
        });

        // `Answer` would render the empty answer body as though generation had produced
        // nothing, rather than leaving the input alone with its sources under it.
        assert_eq!(dock.state, DockState::Input);
    }

    #[test]
    fn a_live_search_matching_nothing_is_silent_rather_than_a_verdict() {
        let mut dock = dock();
        type_query(&mut dock, "wireg");
        let _ = dock.dispatch(Message::SearchDebounceElapsed(dock.search_generation));
        let live = dock.live_query.expect("no live query");

        let _ = dock.on_server(ServerEvent::NoAnswer {
            id: live,
            closest: Vec::new(),
        });

        // Mid-word, "no reliable answer in your notes" is wrong more often than right.
        assert_ne!(dock.state, DockState::NoAnswer);
        assert!(dock.answer.is_empty());
    }

    #[test]
    fn correcting_preloads_the_answer_so_it_is_an_edit_not_a_retype() {
        // Most corrections change a flag or a path, not the whole sentence.
        let mut dock = dock();
        dock.current_query = Some(Uuid::new_v4());
        dock.answer = "Use rsync to deploy.".into();

        let _ = dock.dispatch(Message::Command(keys::Command::Correct));
        assert_eq!(dock.correcting.as_deref(), Some("Use rsync to deploy."));
    }

    #[test]
    fn there_is_nothing_to_correct_without_an_answer() {
        let mut dock = dock();
        dock.current_query = Some(Uuid::new_v4());
        let _ = dock.dispatch(Message::Command(keys::Command::Correct));
        assert!(dock.correcting.is_none());
    }

    #[test]
    fn escape_closes_the_editor_before_it_closes_anything_else() {
        // Esc walks back one step, and an open editor is the innermost step — dismissing
        // the whole dock instead would throw away what was being typed.
        let mut dock = dock();
        dock.current_query = Some(Uuid::new_v4());
        dock.answer = "Use rsync.".into();
        dock.state = DockState::Answer;

        let _ = dock.dispatch(Message::Command(keys::Command::Correct));
        let _ = dock.dispatch(Message::Dismiss);

        assert!(dock.correcting.is_none(), "the editor stayed open");
        assert_eq!(dock.state, DockState::Answer, "the answer was dismissed too");
    }

    #[test]
    fn saving_a_correction_replaces_the_answer_on_screen() {
        let mut dock = dock();
        dock.current_query = Some(Uuid::new_v4());
        dock.answer = "Use rsync.".into();

        let _ = dock.dispatch(Message::Command(keys::Command::Correct));
        let _ = dock.dispatch(Message::CorrectionChanged("Use the container.".into()));
        let _ = dock.dispatch(Message::Command(keys::Command::SaveCorrection));

        assert!(dock.correcting.is_none());
        assert_eq!(dock.answer, "Use the container.");
    }

    #[test]
    fn an_empty_correction_is_not_saved() {
        let mut dock = dock();
        dock.current_query = Some(Uuid::new_v4());
        dock.answer = "Use rsync.".into();

        let _ = dock.dispatch(Message::Command(keys::Command::Correct));
        let _ = dock.dispatch(Message::CorrectionChanged("   ".into()));
        let _ = dock.dispatch(Message::Command(keys::Command::SaveCorrection));

        assert_eq!(dock.answer, "Use rsync.", "the answer was blanked");
    }

    fn source_ref(uid: &str) -> SourceRef {
        SourceRef {
            section_id: brain_proto::SectionId(0),
            section_uid: uid.into(),
            path: "/tmp/a.md".into(),
            heading_path: "A > B".into(),
            start_line: 1,
            end_line: 1,
            score: 1.0,
            snippet: String::new(),
            explain: "matched heading".into(),
        }
    }
}

/// Window settings proven by the Stage 0′ spike (`plan/01-stage-0-dock.md` §0.0).
fn window_settings() -> iced::window::Settings {
    iced::window::Settings {
        size: iced::Size::new(tokens::DOCK_WIDTH, tokens::INPUT_HEIGHT),
        decorations: false,
        // Created unmapped, and mapped by `brain-x11` once the properties are
        // on it. Two things depend on this. `_NET_WM_STATE` is ours to write
        // directly only while unmapped — after the map the WM owns it and a
        // plain `ChangeProperty` is ignored — and `raw_id` resolves *after*
        // iced has mapped the window, so letting iced map it means every
        // persistent property lands too late. It also removes the login flash
        // that `--hidden` had to work around on Slint.
        visible: false,
        // A WM close request must hide the dock, not destroy the window: the
        // XID and every property on it would go with it. `subscription`
        // answers the request; iced must not act on it first.
        exit_on_close_request: false,
        // Needs a depth-32 ARGB visual, which is what lets picom draw the
        // rounded corners and shadow. Verified: Q4.
        transparent: true,
        level: iced::window::Level::AlwaysOnTop,
        platform_specific: iced::window::settings::PlatformSpecific {
            // Sets *both* fields of WM_CLASS to this string, so the i3 rule is
            // `for_window [class="brain-dock"]`. Verified: Q3.
            application_id: String::from("brain-dock"),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Map a key press to a named command, or to `Dismiss`.
///
/// Names rather than raw keys, so the binding table stays in `keys.rs` and
/// becomes config-driven without touching this function's callers (spec §5).
fn shortcut(key: &iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> Option<Message> {
    use iced::keyboard::key::Named;

    if let iced::keyboard::Key::Named(named) = key {
        match named {
            Named::Enter if modifiers.command() => return command("save-correction"),
            Named::Escape => return Some(Message::Dismiss),
            Named::ArrowUp if !modifiers.command() => {
                return command("history-previous");
            }
            Named::ArrowDown if !modifiers.command() => {
                return command("history-next");
            }
            Named::Tab if modifiers.shift() => return command("action-previous"),
            Named::Tab => return command("action-next"),
            _ => return None,
        }
    }

    let iced::keyboard::Key::Character(c) = key else {
        return None;
    };

    if modifiers.command() {
        return match c.as_str() {
            "l" => command("clear"),
            "c" => command("copy-answer"),
            "s" => command("show-sources"),
            // Ctrl+Up / Ctrl+Down would collide with history; a thumbs pair on adjacent
            // keys keeps rating a reflex rather than something to look up.
            "g" => command("rate-good"),
            "b" => command("rate-bad"),
            "e" => command("correct"),
            _ => None,
        };
    }

    if modifiers.alt() {
        // `Alt+1..9`. One-based here because that is what the buttons show.
        let digit: usize = c.parse().ok()?;
        return command(&format!("action-{digit}"));
    }

    None
}

fn command(name: &str) -> Option<Message> {
    keys::Command::parse(name).map(Message::Command)
}
