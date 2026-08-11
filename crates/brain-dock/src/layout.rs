//! How tall the dock window has to be.
//!
//! Slint sized the window to the card for us; iced does not. `window::resize`
//! is the only way to change it, and nothing in iced 0.14 reports the measured
//! size of a view — so the card's height is computed here, from the same
//! tokens `view.rs` lays it out with, and the two must be read together.
//!
//! Getting it wrong is visible: too short clips the answer, too tall leaves a
//! translucent skirt below the card catching clicks. So the one genuinely
//! variable part — the wrapped answer — is *measured* with the renderer's own
//! text shaper rather than estimated from a character count.

use iced::Size;
use iced::advanced::graphics::text::Paragraph as GraphicsParagraph;
use iced::advanced::text::{LineHeight, Paragraph as _, Shaping, Text, Wrapping};

use crate::tokens;
use crate::{Dock, DockState};

/// Height of one line of `size`-point text, matching the `text` widget's
/// default line height.
fn line(size: f32) -> f32 {
    (size * 1.3).ceil()
}

/// The width text wraps at inside the card.
fn content_width() -> f32 {
    tokens::DOCK_WIDTH - 2.0 * tokens::PAD_X
}

/// Total window height for the dock's current state.
///
/// Always at least `INPUT_HEIGHT`: the query row is present in every state, and
/// a window shorter than its own text input looks broken during the frame
/// between a state change and the resize landing.
pub fn window_height(dock: &Dock) -> f32 {
    let mut height = tokens::INPUT_HEIGHT;

    if dock.state != DockState::Input {
        // 1.0 is the divider in `view::divider`.
        height += 1.0 + body_height(dock);
    }

    // The panel is a fixed height, so unlike the answer it needs no measuring — but it
    // does need adding, or the window clips it and the graph is drawn into nothing.
    if dock.graph_visible && dock.graph.is_some() {
        height += 1.0 + tokens::GRAPH_HEIGHT;
    }

    height
}

fn body_height(dock: &Dock) -> f32 {
    let mut height = 0.0_f32;
    let mut blocks: u32 = 0;

    match dock.state {
        DockState::Searching => {
            height += line(tokens::FONT_ANSWER);
            blocks += 1;
        }
        DockState::Answer | DockState::NoAnswer => {
            // Bounded here as well as by `view`'s `max_height`, or a long answer
            // would ask for a window taller than the screen instead of scrolling.
            height += measure(&dock.answer, tokens::FONT_ANSWER).min(tokens::ANSWER_MAX_HEIGHT);
            blocks += 1;
        }
        DockState::Input => {}
    }

    if let Some(source) = &dock.source {
        height += line(tokens::FONT_META);
        // `view::source_badge` stacks the explanation under the path, so it is a second
        // line. Missing it here clips the window by exactly that much.
        if !source.explain.is_empty() {
            height += line(tokens::FONT_META) + tokens::GAP_TIGHT;
        }
        blocks += 1;
    }

    if !dock.actions.is_empty() {
        // `view::action_row`'s buttons: 6px of padding above and below the label.
        height += line(tokens::FONT_META) + 12.0;
        blocks += 1;
    }

    // `column.spacing` puts a gap *between* blocks, not around them.
    height += tokens::GAP * (blocks.saturating_sub(1)) as f32;

    height + 2.0 * tokens::PAD_Y
}

/// Shape `content` exactly as the renderer will and report how tall it came out.
///
/// This is the same shaper the text widget uses, through the process-wide font
/// system, so the answer is measured with the real font, the real wrapping, and
/// the real ligatures rather than an average glyph width.
fn measure(content: &str, size: f32) -> f32 {
    if content.is_empty() {
        return 0.0;
    }

    GraphicsParagraph::with_text(Text {
        content,
        bounds: Size::new(content_width(), f32::INFINITY),
        size: size.into(),
        line_height: LineHeight::default(),
        font: tokens::FONT,
        align_x: iced::advanced::text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: Shaping::default(),
        wrapping: Wrapping::default(),
    })
    .min_bounds()
    .height
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dock(state: DockState, answer: &str) -> Dock {
        let mut dock = Dock::for_test();
        dock.state = state;
        dock.answer = answer.to_string();
        dock
    }

    #[test]
    fn input_state_is_exactly_the_query_row() {
        assert_eq!(
            window_height(&dock(DockState::Input, "")),
            tokens::INPUT_HEIGHT
        );
    }

    #[test]
    fn a_longer_answer_is_a_taller_window() {
        let short = window_height(&dock(DockState::Answer, "one line"));
        let long = window_height(&dock(
            DockState::Answer,
            &"a sentence that certainly wraps several times over. ".repeat(20),
        ));
        assert!(long > short, "{long} should exceed {short}");
    }

    #[test]
    fn a_very_long_answer_stops_growing() {
        let huge = window_height(&dock(DockState::Answer, &"word ".repeat(20_000)));
        // The cap, plus the chrome that is never part of the scrollable region.
        assert!(
            huge <= tokens::INPUT_HEIGHT + 1.0 + tokens::ANSWER_MAX_HEIGHT + 2.0 * tokens::PAD_Y,
            "{huge} exceeds the bounded height"
        );
    }
}
