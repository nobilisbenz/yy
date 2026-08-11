//! Backend setup that has to happen before the first window exists.
//!
//! Two things can only be done here, and both are invisible failures if you get
//! them wrong:
//!
//! - **WM_CLASS** must be set before the window is first mapped. i3 evaluates
//!   `for_window` criteria at map time, so a class assigned afterwards matches
//!   nothing and the dock silently arrives tiled, bordered, and non-sticky.
//! - **Scale factor** has to be settled before layout, because every size in
//!   `tokens.slint` is logical.

use anyhow::{Context, Result};

/// What i3 matches on. `for_window [class="BrainDock"]` in the i3 config.
pub const WM_CLASS_INSTANCE: &str = "brain-dock";
pub const WM_CLASS_GENERAL: &str = "BrainDock";

/// Install the winit backend with our window attributes.
///
/// Must be called before any `Dock::new()`.
pub fn install(scale: f32) -> Result<()> {
    apply_scale_factor(scale);

    let selector = slint::BackendSelector::new().with_winit_window_attributes_hook(|attributes| {
        #[cfg(target_os = "linux")]
        {
            use slint::winit_030::winit::platform::x11::WindowAttributesExtX11;
            // winit's X11 `with_name(general, instance)` writes WM_CLASS as
            // `instance\0general\0`. Left alone, winit derives both from the
            // binary name, which gives `brain-dock`/`brain-dock` — close
            // enough to look right in `wmctrl` and wrong enough that the i3
            // rules never fire.
            let attributes = attributes.with_name(WM_CLASS_GENERAL, WM_CLASS_INSTANCE);
            // A transparent window needs a 32-bit ARGB visual; without this the
            // compositor has nothing to blend and the rounded corners come out
            // as black wedges.
            attributes
                .with_transparent(true)
                .with_decorations(false)
        }
        #[cfg(not(target_os = "linux"))]
        attributes
    });

    selector
        .select()
        .context("selecting the winit backend — is a display server reachable?")
}

/// Slint guesses a scale factor from the display, and on this class of laptop
/// panel it guesses 1.5 even when the X session is a plain unscaled 96 DPI.
/// That would render the dock half again larger than every other window.
///
/// `SLINT_SCALE_FACTOR` is Slint's own documented override, so honour an
/// existing value and only fill in the default.
fn apply_scale_factor(scale: f32) {
    if std::env::var_os("SLINT_SCALE_FACTOR").is_some() {
        tracing::debug!("SLINT_SCALE_FACTOR is already set; leaving it alone");
        return;
    }
    // SAFETY: called at the top of main, before any other thread exists and
    // before the Slint backend reads it.
    unsafe { std::env::set_var("SLINT_SCALE_FACTOR", scale.to_string()) };
    tracing::debug!(scale, "scale factor pinned");
}
