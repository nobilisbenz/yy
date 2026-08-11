//! Direct control of the dock's X11 window.
//!
//! The toolkit creates the window; from then on we drive it ourselves. Two
//! decisions are load-bearing:
//!
//! **Map/unmap, not toolkit show/hide.** Slint's `hide()` tears down the winit
//! window — the XID changes, every property set on it is lost, and with the last
//! window gone the event loop exits, turning `Esc` into "quit". Mapping and
//! unmapping the existing window instead keeps the whole thing alive for the
//! session and makes a summon cost one round trip.
//!
//! **`_NET_WM_STATE` is set two different ways depending on map state.** While
//! the window is unmapped it is ours and a plain `ChangeProperty` works. Once
//! it is mapped the window manager owns the property, and the only legitimate
//! way to change it is a `ClientMessage` to the root. Getting this backwards is
//! the classic "always-on-top works, until it randomly doesn't" bug.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, PropMode,
    StackMode, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::atoms::Atoms;
use crate::geometry::{self, Placement, Rect};
use crate::X11Error;

/// `_NET_WM_STATE` client message actions (EWMH).
const STATE_REMOVE: u32 = 0;
const STATE_ADD: u32 = 1;

/// Source indication for EWMH client messages. 2 = "pager", which window
/// managers trust for focus requests; 1 = "application" is treated with more
/// suspicion and i3 may ignore it depending on `focus_on_window_activation`.
const SOURCE_PAGER: u32 = 2;

/// `_NET_WM_DESKTOP` value meaning "all desktops".
const ALL_DESKTOPS: u32 = 0xFFFF_FFFF;

pub struct DockWindow {
    conn: RustConnection,
    root: Window,
    window: Window,
    atoms: Atoms,
    mapped: bool,
    /// Window that had focus when we were last summoned, so it can be given
    /// focus back on hide (spec §42).
    previous_focus: Option<Window>,
}

impl DockWindow {
    /// Adopt an existing window by XID.
    ///
    /// Opens its own X11 connection rather than borrowing the toolkit's: the
    /// toolkit's connection belongs to the UI thread and is not ours to use
    /// from anywhere else, and a second connection to the same display costs
    /// almost nothing.
    pub fn adopt(window: Window) -> Result<Self, X11Error> {
        let (conn, screen_index) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen_index].root;
        let atoms = Atoms::intern(&conn)?;

        let mapped = conn
            .get_window_attributes(window)?
            .reply()
            .map(|attributes| {
                attributes.map_state != x11rb::protocol::xproto::MapState::UNMAPPED
            })
            .unwrap_or(false);

        Ok(Self {
            conn,
            root,
            window,
            atoms,
            mapped,
            previous_focus: None,
        })
    }

    pub fn is_mapped(&self) -> bool {
        self.mapped
    }

    /// Everything that should be true for the whole session.
    ///
    /// Call once, as early as possible. Any of these applied after the first
    /// map is at best a flicker and at worst ignored.
    pub fn apply_persistent_properties(&self) -> Result<(), X11Error> {
        self.set_window_type()?;
        self.set_motif_undecorated()?;
        self.set_all_desktops()?;
        self.set_state_flags()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Place and show, taking keyboard focus.
    ///
    /// Position is set while unmapped so the dock never appears at the wrong
    /// coordinates for a frame.
    pub fn show(&mut self, placement: &Placement) -> Result<(), X11Error> {
        self.previous_focus = geometry::active_window(&self.conn, self.root, &self.atoms)
            .ok()
            .flatten()
            .filter(|&w| w != self.window);

        let area = geometry::active_work_area(&self.conn, self.root, &self.atoms)?;
        self.move_resize(placement, &area)?;

        if !self.mapped {
            self.conn.map_window(self.window)?;
            self.mapped = true;
        }

        // Re-assert stacking on every summon: another window mapped since the
        // last one would otherwise sit above us.
        //
        // The explicit raise is what actually keeps the dock on top under i3,
        // which ignores `_NET_WM_STATE_ABOVE` entirely — reasonably, since a
        // tiling WM has no always-on-top concept. Verified: after a summon the
        // window reports only STICKY and FOCUSED, yet sits last in
        // `_NET_CLIENT_LIST_STACKING`. The ABOVE hint is still re-sent for
        // window managers that do honour it.
        self.raise()?;
        self.request_above()?;
        self.focus()?;
        self.conn.flush()?;
        Ok(())
    }

    /// Hide without destroying. The window, its XID, and its properties all
    /// survive for the next summon.
    pub fn hide(&mut self, restore_focus: bool) -> Result<(), X11Error> {
        // Unmap unconditionally rather than trusting `self.mapped`.
        //
        // We adopt the window before the toolkit has finished bringing it up,
        // so the map state sampled at adoption is stale almost immediately —
        // and a `--hidden` dock that skipped the unmap on that basis came up
        // visible on login. Unmapping an unmapped window is a no-op at the
        // server, which makes the cheap defensive call the right one.
        self.conn.unmap_window(self.window)?;
        self.mapped = false;
        self.park_offscreen()?;

        if restore_focus && let Some(previous) = self.previous_focus.take() {
            // Best-effort: the window may have closed while we were open.
            if let Err(err) = self.activate(previous) {
                tracing::debug!(%err, "could not restore focus to the previous window");
            }
        }

        self.conn.flush()?;
        Ok(())
    }

    /// Everything a summon needs *before* the window is mapped.
    ///
    /// Split out of [`show`](Self::show) for callers whose toolkit owns the map
    /// itself — iced maps through winit, and a window winit believes is hidden
    /// stays unmapped no matter who sends the `MapWindow`: measured on i3, the
    /// request is honoured, the window is managed, and it is withdrawn again
    /// immediately. Pair with [`finish_show`](Self::finish_show) around the
    /// toolkit's own show call.
    ///
    /// Positioning belongs here: doing it while unmapped is what keeps the dock
    /// from appearing at the wrong coordinates for a frame.
    pub fn prepare_show(&mut self, placement: &Placement) -> Result<(), X11Error> {
        self.previous_focus = geometry::active_window(&self.conn, self.root, &self.atoms)
            .ok()
            .flatten()
            .filter(|&w| w != self.window);

        let area = geometry::active_work_area(&self.conn, self.root, &self.atoms)?;
        self.move_resize(placement, &area)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Stacking and focus, once the toolkit has mapped the window.
    ///
    /// See [`show`](Self::show) for why the raise is what actually keeps the
    /// dock on top under i3.
    pub fn finish_show(&mut self) -> Result<(), X11Error> {
        self.mapped = true;
        self.raise()?;
        self.request_above()?;
        self.focus()?;
        self.conn.flush()?;
        Ok(())
    }

    /// The other half of [`hide`](Self::hide), for the same split callers: run
    /// once the toolkit has unmapped the window.
    pub fn finish_hide(&mut self, restore_focus: bool) -> Result<(), X11Error> {
        self.mapped = false;
        self.park_offscreen()?;

        if restore_focus && let Some(previous) = self.previous_focus.take() {
            // Best-effort: the window may have closed while we were open.
            if let Err(err) = self.activate(previous) {
                tracing::debug!(%err, "could not restore focus to the previous window");
            }
        }

        self.conn.flush()?;
        Ok(())
    }

    /// Resize in place, keeping the anchor fixed.
    ///
    /// The answer expands downward; the top-right corner must not move
    /// (spec §41), so this recomputes x from the new width rather than leaving
    /// the old left edge in place.
    pub fn resize(&mut self, placement: &Placement) -> Result<(), X11Error> {
        let area = geometry::active_work_area(&self.conn, self.root, &self.atoms)?;
        self.move_resize(placement, &area)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Position only — never size.
    ///
    /// Slint sizes the window from its own content (the answer expands the
    /// card, which expands the window). Configuring width and height here too
    /// would mean two parties resizing the same window every frame, which
    /// shows up as flicker and occasionally as a resize loop. So the toolkit
    /// owns size, we own position, and `placement.width` is only used to work
    /// out where the right edge should land.
    fn move_resize(&self, placement: &Placement, area: &Rect) -> Result<(), X11Error> {
        let (x, y) = placement.position_in(area);
        self.conn
            .configure_window(self.window, &ConfigureWindowAux::new().x(x).y(y))?;
        Ok(())
    }

    /// Move the window far off-screen while it is hidden.
    ///
    /// Belt and braces against a map we did not ask for. The toolkit maps the
    /// window itself during startup, asynchronously, and that map can land
    /// after our unmap — which showed up as a `--hidden` dock flashing into
    /// view at login. Parked off-screen, any such map is invisible, and
    /// `show()` always repositions before mapping so nothing is left stale.
    fn park_offscreen(&self) -> Result<(), X11Error> {
        const FAR_AWAY: i32 = -32000;
        self.conn.configure_window(
            self.window,
            &ConfigureWindowAux::new().x(FAR_AWAY).y(FAR_AWAY),
        )?;
        Ok(())
    }

    fn raise(&self) -> Result<(), X11Error> {
        self.conn.configure_window(
            self.window,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        Ok(())
    }

    /// Ask the window manager for keyboard focus.
    ///
    /// i3 honours this when `focus_on_window_activation` is `smart` or `focus`.
    /// Setting the input focus directly with `SetInputFocus` would fight the
    /// WM's own idea of what is focused and desynchronise its tree.
    fn focus(&self) -> Result<(), X11Error> {
        self.activate(self.window)
    }

    fn activate(&self, window: Window) -> Result<(), X11Error> {
        let event = ClientMessageEvent::new(
            32,
            window,
            self.atoms._NET_ACTIVE_WINDOW,
            [SOURCE_PAGER, x11rb::CURRENT_TIME, self.window, 0, 0],
        );
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            event,
        )?;
        Ok(())
    }

    /// UTILITY rather than NORMAL: window managers float utility windows and
    /// keep them out of tiling layouts without needing a rule, so the dock
    /// behaves even on a machine whose i3 config was never edited.
    fn set_window_type(&self) -> Result<(), X11Error> {
        self.conn.change_property32(
            PropMode::REPLACE,
            self.window,
            self.atoms._NET_WM_WINDOW_TYPE,
            AtomEnum::ATOM,
            &[self.atoms._NET_WM_WINDOW_TYPE_UTILITY],
        )?;
        Ok(())
    }

    fn set_all_desktops(&self) -> Result<(), X11Error> {
        self.conn.change_property32(
            PropMode::REPLACE,
            self.window,
            self.atoms._NET_WM_DESKTOP,
            AtomEnum::CARDINAL,
            &[ALL_DESKTOPS],
        )?;
        Ok(())
    }

    /// Belt and braces alongside winit's `with_decorations(false)`: some window
    /// managers only honour the Motif hint.
    fn set_motif_undecorated(&self) -> Result<(), X11Error> {
        // flags = MWM_HINTS_DECORATIONS, decorations = none.
        const MWM_HINTS_DECORATIONS: u32 = 1 << 1;
        self.conn.change_property32(
            PropMode::REPLACE,
            self.window,
            self.atoms._MOTIF_WM_HINTS,
            self.atoms._MOTIF_WM_HINTS,
            &[MWM_HINTS_DECORATIONS, 0, 0, 0, 0],
        )?;
        Ok(())
    }

    /// Declare the initial state, before the window is ever mapped.
    ///
    /// While unmapped the property is ours to write directly; once mapped the
    /// window manager owns it and only a `ClientMessage` will do. This is
    /// called from `apply_persistent_properties`, so the unmapped path is the
    /// correct one — see the module comment.
    fn set_state_flags(&self) -> Result<(), X11Error> {
        debug_assert!(!self.mapped, "state flags must be declared before mapping");

        self.conn.change_property32(
            PropMode::REPLACE,
            self.window,
            self.atoms._NET_WM_STATE,
            AtomEnum::ATOM,
            &[
                self.atoms._NET_WM_STATE_ABOVE,
                self.atoms._NET_WM_STATE_STICKY,
                self.atoms._NET_WM_STATE_SKIP_TASKBAR,
                self.atoms._NET_WM_STATE_SKIP_PAGER,
            ],
        )?;
        Ok(())
    }

    /// Re-request always-on-top on a mapped window.
    ///
    /// Only `_ABOVE`, deliberately. Re-sending the whole set on every summon
    /// appends a duplicate `_NET_WM_STATE_STICKY` each time under i3, since
    /// sticky is already held and ADD is not idempotent there.
    fn request_above(&self) -> Result<(), X11Error> {
        let event = ClientMessageEvent::new(
            32,
            self.window,
            self.atoms._NET_WM_STATE,
            [STATE_ADD, self.atoms._NET_WM_STATE_ABOVE, 0, SOURCE_PAGER, 0],
        );
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            event,
        )?;
        Ok(())
    }

    /// Clear the state we set, for a clean exit.
    #[allow(dead_code)]
    pub fn clear_state_flags(&self) -> Result<(), X11Error> {
        for state in [
            self.atoms._NET_WM_STATE_ABOVE,
            self.atoms._NET_WM_STATE_STICKY,
        ] {
            let event = ClientMessageEvent::new(
                32,
                self.window,
                self.atoms._NET_WM_STATE,
                [STATE_REMOVE, state, 0, SOURCE_PAGER, 0],
            );
            self.conn.send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )?;
        }
        self.conn.flush()?;
        Ok(())
    }
}
