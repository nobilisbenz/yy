//! Backend setup that has to happen before the first window exists.
//!
//! Only one thing is left here after the iced port. `WM_CLASS` moved into
//! `window::Settings.platform_specific.application_id` (Stage 0′ §0.0 Q3), and
//! transparency and decorations are ordinary settings. The scale factor is not
//! settable through iced at all, so it stays an environment variable.

/// Pin the scale factor winit reports.
///
/// winit guesses 1.5 on this laptop panel even though the X session is a plain
/// unscaled 96 DPI with no `Xft.dpi` set. Measured on the iced build: the window
/// comes up 840×93 for a `Settings.size` of 560×62.
///
/// This is deliberately *not* `iced::Daemon::scale_factor`. That hook multiplies
/// on top of whatever winit reports, so it can only scale the content inside an
/// already-oversized window — the window itself would stay 840px wide.
/// `WINIT_X11_SCALE_FACTOR` changes the number at the source, so the window,
/// every later resize, and the layout all agree.
///
/// Honour anything the user has already set: this is the documented override
/// and a HiDPI user may legitimately want a different value.
pub fn pin_scale_factor(scale: f32) {
    const KEY: &str = "WINIT_X11_SCALE_FACTOR";

    if std::env::var_os(KEY).is_some() {
        tracing::debug!(KEY, "already set; leaving it alone");
        return;
    }

    // SAFETY: called at the top of main, before iced starts the event loop and
    // before any other thread exists to observe the environment.
    unsafe { std::env::set_var(KEY, scale.to_string()) };
    tracing::debug!(scale, "scale factor pinned");
}
