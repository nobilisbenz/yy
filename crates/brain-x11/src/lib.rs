//! X11 and EWMH integration.
//!
//! Two jobs: control the dock's own window (`DockWindow`), and read what the
//! user was doing when they summoned it (Stage 4's desktop context).
//!
//! Everything here is X11-only and unapologetically so. The spec's first
//! version targets X11 and i3 (§2), and carrying a portability layer for a
//! platform we do not support would cost clarity for nothing.

pub mod atoms;
pub mod dock_window;
pub mod geometry;

pub use atoms::Atoms;
pub use dock_window::DockWindow;
pub use geometry::{Anchor, Placement, Rect};

#[derive(Debug, thiserror::Error)]
pub enum X11Error {
    #[error("cannot connect to the X display: {0}")]
    Connect(#[from] x11rb::errors::ConnectError),

    #[error("X11 connection failed: {0}")]
    Connection(#[from] x11rb::errors::ConnectionError),

    #[error("X11 request failed: {0}")]
    Reply(#[from] x11rb::errors::ReplyError),

    #[error("the X server reported no monitors")]
    NoMonitors,
}
