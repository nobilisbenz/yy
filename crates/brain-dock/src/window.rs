//! Bridging iced's window to `brain-x11`.
//!
//! Under Slint the XID only existed after the first `show()`, so adoption
//! retried on a timer. iced hands it over as a `Task`: `window::raw_id(id)`
//! resolves once the window exists, and that `u64` *is* the XID. There is
//! nothing to poll.
//!
//! Everything below this line is unchanged from the Slint build — it is EWMH
//! and i3 behaviour, which no toolkit choice affects.

use anyhow::{Context, Result};
use brain_x11::{Anchor, DockWindow, Placement};

/// Geometry from config (spec §7). Logical pixels; with the scale factor
/// pinned to 1.0 these are also physical pixels.
#[derive(Debug, Clone, Copy)]
pub struct DockGeometry {
    pub anchor: Anchor,
    pub margin_top: i32,
    pub margin_side: i32,
    pub width: u32,
}

impl Default for DockGeometry {
    fn default() -> Self {
        Self {
            anchor: Anchor::TopRight,
            // polybar owns the top 30px on this display. `_NET_WORKAREA`
            // already accounts for it, so this is a gap below the bar rather
            // than an offset from the screen edge.
            margin_top: 8,
            margin_side: 22,
            width: crate::tokens::DOCK_WIDTH as u32,
        }
    }
}

pub struct WindowController {
    x11: DockWindow,
    geometry: DockGeometry,
    /// Last width we positioned against. iced owns the actual size; we track it
    /// only to know when the right edge needs recomputing.
    width: u32,
    restore_focus: bool,
}

impl WindowController {
    /// Adopt the X11 window behind an iced window, given the id from
    /// `iced::window::raw_id`.
    pub fn adopt(xid: u64, geometry: DockGeometry) -> Result<Self> {
        let xid = u32::try_from(xid).context(
            "the window id does not fit in an X11 window id. Brain Dock is X11-only \
             (spec §2); check that iced did not select the Wayland backend.",
        )?;

        let x11 = DockWindow::adopt(xid).context("adopting the dock's X11 window")?;
        x11.apply_persistent_properties()
            .context("applying window properties")?;

        tracing::debug!(xid = format!("0x{xid:x}"), "adopted X11 window");

        Ok(Self {
            x11,
            geometry,
            width: geometry.width,
            restore_focus: true,
        })
    }

    pub fn set_restore_focus(&mut self, restore: bool) {
        self.restore_focus = restore;
    }

    pub fn is_visible(&self) -> bool {
        self.x11.is_mapped()
    }

    /// Position the window and note what had focus, before the map.
    pub fn prepare_show(&mut self, width: u32) -> Result<()> {
        self.width = width.max(1);
        self.x11
            .prepare_show(&self.placement())
            .context("positioning the dock")
    }

    /// Raise and take focus, after the map.
    pub fn finish_show(&mut self) -> Result<()> {
        self.x11.finish_show().context("showing the dock")
    }

    /// Park off-screen and give focus back, after the unmap.
    pub fn finish_hide(&mut self) -> Result<()> {
        self.x11
            .finish_hide(self.restore_focus)
            .context("hiding the dock")
    }

    /// Keep the anchored edge fixed when the window resizes.
    ///
    /// For a top-right anchor only a width change moves the left edge; height
    /// growth extends downward on its own, which is exactly what spec §41 asks
    /// for. So this is a no-op in the common case of an answer expanding, and
    /// it deliberately skips the X11 round trip then — it is called on every
    /// resize, including once per token batch while streaming.
    pub fn follow_resize(&mut self, width: u32) -> Result<()> {
        let width = width.max(1);
        if width == self.width {
            return Ok(());
        }
        self.width = width;
        if self.x11.is_mapped() {
            self.x11
                .resize(&self.placement())
                .context("repositioning the dock after a resize")?;
        }
        Ok(())
    }

    fn placement(&self) -> Placement {
        Placement {
            anchor: self.geometry.anchor,
            margin_top: self.geometry.margin_top,
            margin_side: self.geometry.margin_side,
            width: self.width,
            // Unused for a top anchor; carried for completeness so a
            // bottom-anchored variant needs no signature change.
            height: 0,
        }
    }
}
