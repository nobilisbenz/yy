//! Bridging Slint's window to `brain-x11`.
//!
//! The XID only exists after the window has been realised, which under Slint
//! means after the first `show()`. So the sequence is: show once to bring the
//! X11 window into being, grab its id, apply our properties, then immediately
//! unmap if we were asked to start hidden. The window then lives for the whole
//! session and only its mapping changes.

use anyhow::{Context, Result};
use brain_x11::{Anchor, DockWindow, Placement};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

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
            width: 560,
        }
    }
}

pub struct WindowController {
    x11: DockWindow,
    geometry: DockGeometry,
    /// Last width we positioned against. Slint owns the actual size; we track
    /// it only to know when the right edge needs recomputing.
    width: u32,
    restore_focus: bool,
}

impl WindowController {
    /// Adopt the Slint window's underlying X11 window.
    pub fn adopt(window: &slint::Window, geometry: DockGeometry) -> Result<Self> {
        // The intermediate must outlive the borrow it hands out.
        let slint_handle = window.window_handle();
        let handle = slint_handle
            .window_handle()
            .context("the Slint window has no native handle yet — call after show()")?;

        let xid = match handle.as_raw() {
            RawWindowHandle::Xlib(handle) => handle.window as u32,
            RawWindowHandle::Xcb(handle) => handle.window.get(),
            other => anyhow::bail!(
                "expected an X11 window, got {other:?}. Brain Dock is X11-only \
                 (spec §2); check that SLINT_BACKEND is not forcing Wayland."
            ),
        };

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

    pub fn show(&mut self, width: u32) -> Result<()> {
        self.width = width.max(1);
        self.x11
            .show(&self.placement())
            .context("showing the dock")?;
        Ok(())
    }

    pub fn hide(&mut self) -> Result<()> {
        self.x11
            .hide(self.restore_focus)
            .context("hiding the dock")?;
        Ok(())
    }

    /// Keep the anchored edge fixed when Slint resizes the window.
    ///
    /// For a top-right anchor only a width change moves the left edge; height
    /// growth extends downward on its own, which is exactly what spec §41
    /// asks for. So this is a no-op in the common case of an answer expanding,
    /// and it deliberately skips the X11 round trip then — it is called on
    /// every layout pass, including once per token batch while streaming.
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
