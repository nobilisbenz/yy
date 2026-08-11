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

/// Pin the scale factor.
///
/// winit guesses 1.5 on this laptop panel even though the X session is a plain
/// unscaled 96 DPI with no `Xft.dpi` set, which renders the dock half again
/// larger than every other window.
///
/// Both variables are needed, and the reason is worth recording:
///
/// - `SLINT_SCALE_FACTOR` is applied only to the *initial* window attributes.
///   It gets the window created at the right size and then stops mattering.
/// - `WINIT_X11_SCALE_FACTOR` changes what winit itself reports, so every
///   later resize uses it too. Without this one, the card is correct at 560px
///   until the first answer arrives and then snaps to 840px — which is exactly
///   the bug this pair fixes.
///
/// Honour anything the user has already set; these are the documented
/// overrides and a HiDPI user may legitimately want a different value.
fn apply_scale_factor(scale: f32) {
    for key in ["SLINT_SCALE_FACTOR", "WINIT_X11_SCALE_FACTOR"] {
        if std::env::var_os(key).is_some() {
            tracing::debug!(key, "already set; leaving it alone");
            continue;
        }
        // SAFETY: called at the top of main, before any other thread exists
        // and before the Slint backend reads it.
        unsafe { std::env::set_var(key, scale.to_string()) };
    }
    tracing::debug!(scale, "scale factor pinned");
}
