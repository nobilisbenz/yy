//! Every visual constant in Brain Dock lives here.
//!
//! Ported from `ui/tokens.slint`. The invariant is unchanged — nothing outside
//! this module names a colour, a radius, or a duration (spec §40) — but the
//! Slint compiler used to enforce it and now review does, so a literal
//! anywhere else is a defect rather than a style preference.

use std::time::Duration;

use iced::{Color, Font};

// --- geometry (spec §41) ---------------------------------------------------

pub const DOCK_WIDTH: f32 = 560.0;
pub const INPUT_HEIGHT: f32 = 62.0;
pub const ANSWER_MAX_HEIGHT: f32 = 500.0;

/// How tall the graph panel is when open.
///
/// Fixed rather than proportional: the panel shows a seed and its neighbours — tens of
/// nodes — and giving it a share of a card whose height already varies with the answer
/// would make it resize under the user while they are reading it.
pub const GRAPH_HEIGHT: f32 = 320.0;

/// How long the panel's force simulation may run before it is declared settled.
///
/// A dock resident from login must not hold the GPU at the refresh rate; after this the
/// panel stops asking for frames entirely (`PLAN.md` §7.4).
pub const GRAPH_SETTLE_SECONDS: f32 = 8.0;
pub const RADIUS: f32 = 22.0;

pub const PAD_X: f32 = 22.0;
pub const PAD_Y: f32 = 16.0;
pub const GAP: f32 = 10.0;
pub const GAP_TIGHT: f32 = 6.0;

// --- type ------------------------------------------------------------------

pub const FONT: Font = Font::with_name("Fira Code");
pub const FONT_QUERY: f32 = 17.0;
pub const FONT_ANSWER: f32 = 15.0;
pub const FONT_META: f32 = 12.0;

// --- colour ----------------------------------------------------------------
//
// Tuned against the doom-one palette already in the i3 config, so the dock
// reads as part of the desktop rather than a visitor.

/// `#1c1f24e8` — the card. Alpha is load-bearing: picom composites it, which is
/// what the depth-32 ARGB visual from Stage 0′ §0.0 Q4 exists for.
pub const BG: Color = rgba(0x1c, 0x1f, 0x24, 0xe8);
pub const BORDER: Color = rgba(0xff, 0xff, 0xff, 0x14);
pub const DIVIDER: Color = rgba(0xff, 0xff, 0xff, 0x0f);

pub const FG: Color = rgb(0xdf, 0xdf, 0xdf);
pub const FG_DIM: Color = rgb(0x7f, 0x84, 0x90);
pub const FG_FAINT: Color = rgb(0x5b, 0x62, 0x68);
pub const ACCENT: Color = rgb(0x51, 0xaf, 0xef);
pub const DANGER: Color = rgb(0xff, 0x6c, 0x6b);

pub const ACTION_BG: Color = rgba(0xff, 0xff, 0xff, 0x0f);
pub const ACTION_BG_HOVER: Color = rgba(0xff, 0xff, 0xff, 0x1f);
pub const ACTION_BG_FOCUS: Color = rgba(0x51, 0xaf, 0xef, 0x2e);

// --- motion (spec §41) -----------------------------------------------------
//
// Show and hide only. Tokens and buttons do not animate; per-token animation
// reads as lag, not polish. Neither does the card's growth: the answer expands
// the *window*, and animating that means resizing it every frame — expensive,
// and visibly steppy because X sizes are whole pixels.

/// Fade in on summon. Slower than the fade out: arriving should feel
/// deliberate, leaving should feel immediate.
pub const SHOW: Duration = Duration::from_millis(100);
pub const HIDE: Duration = Duration::from_millis(80);

/// How often batched tokens reach the screen. ~30 Hz — fast enough to read as
/// continuous, slow enough that layout cost stays irrelevant.
pub const TOKEN_FLUSH_MS: u64 = 33;

/// Flush early once a batch is this big, so the first words appear promptly
/// rather than waiting out a full tick.
pub const TOKEN_FLUSH_CHARS: usize = 24;

// --- helpers ---------------------------------------------------------------

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    rgba(r, g, b, 0xff)
}

/// `const` so these stay compile-time constants rather than lazy statics;
/// `Color::from_rgba8` is not const in iced 0.14.
const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: a as f32 / 255.0,
    }
}
