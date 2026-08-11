//! The dock's view. Spec §4's states and §41's geometry.
//!
//! Structure mirrors what `ui/dock.slint` described declaratively: a card, an
//! always-present query row, and — in every state but `Input` — a body that
//! expands downward while the top-right anchor stays put.
//!
//! **Opacity is applied per colour, through [`Palette`].** iced has no opacity
//! widget and no layer alpha, so the summon fade is the whole palette scaled
//! towards zero alpha. Every colour in here therefore comes from the palette,
//! never from `tokens` directly — a token read straight from this file would
//! stay fully opaque while everything around it faded, which is exactly the
//! kind of defect that only shows up in motion.

use iced::widget::{Id, Space, button, column, container, row, scrollable, text, text_input};
use iced::{Alignment, Border, Color, Element, Length, Padding};

use crate::tokens;
use crate::{Dock, DockState, Message};

/// Focus target for `widget::operation::focus`. 0.14 has one `widget::Id` for
/// every widget rather than a per-widget id type.
pub fn query_input_id() -> Id {
    Id::from("dock-query")
}

/// The token palette at a given opacity.
///
/// `Copy`, so the style closures that need it can take it by value without
/// borrowing the dock.
#[derive(Debug, Clone, Copy)]
struct Palette {
    bg: Color,
    border: Color,
    divider: Color,
    fg: Color,
    fg_dim: Color,
    fg_faint: Color,
    accent: Color,
    danger: Color,
    action_bg: Color,
    action_bg_hover: Color,
    action_bg_focus: Color,
}

impl Palette {
    fn at(opacity: f32) -> Self {
        let fade = |color: Color| Color {
            a: color.a * opacity,
            ..color
        };

        Self {
            bg: fade(tokens::BG),
            border: fade(tokens::BORDER),
            divider: fade(tokens::DIVIDER),
            fg: fade(tokens::FG),
            fg_dim: fade(tokens::FG_DIM),
            fg_faint: fade(tokens::FG_FAINT),
            accent: fade(tokens::ACCENT),
            danger: fade(tokens::DANGER),
            action_bg: fade(tokens::ACTION_BG),
            action_bg_hover: fade(tokens::ACTION_BG_HOVER),
            action_bg_focus: fade(tokens::ACTION_BG_FOCUS),
        }
    }
}

pub fn view(dock: &Dock) -> Element<'_, Message> {
    let palette = Palette::at(dock.opacity());

    let mut card = column![query_row(dock, palette)].width(Length::Fill);

    if dock.state != DockState::Input {
        card = card.push(divider(palette));
        card = card.push(body(dock, palette));
    }

    if let Some(panel) = dock.graph.as_ref().filter(|_| dock.graph_visible) {
        card = card.push(divider(palette));
        card = card.push(graph_panel(panel));
    }

    container(card)
        .width(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(palette.bg.into()),
            border: Border {
                radius: tokens::RADIUS.into(),
                width: 1.0,
                color: palette.border,
            },
            ..Default::default()
        })
        .into()
}

/// The graph, under the answer.
///
/// `ygraphy::panel::graph_view` returns the shader widget with its labels stacked over
/// it, so there is nothing to embed here — it is a widget in a column like any other
/// (`PLAN.md` §7.1). The height is fixed by a token; see `tokens::GRAPH_HEIGHT` for why.
fn graph_panel(panel: &crate::graph::GraphPanel) -> Element<'_, Message> {
    container(ygraphy::panel::graph_view(
        &panel.layout,
        panel.camera,
        panel.selected,
        panel.viewport,
    ))
    .width(Length::Fill)
    .height(Length::Fixed(tokens::GRAPH_HEIGHT))
    .into()
}

fn query_row(dock: &Dock, palette: Palette) -> Element<'_, Message> {
    let input = text_input("Ask how you did something…", &dock.query)
        .id(query_input_id())
        .on_input(Message::QueryChanged)
        .on_submit(Message::Submit)
        .font(tokens::FONT)
        .size(tokens::FONT_QUERY)
        .padding(0)
        .style(move |_theme, _status| text_input::Style {
            background: Color::TRANSPARENT.into(),
            border: Border::default(),
            icon: palette.fg_dim,
            placeholder: palette.fg_faint,
            value: palette.fg,
            selection: palette.action_bg_focus,
        });

    container(input)
        .width(Length::Fill)
        .height(Length::Fixed(tokens::INPUT_HEIGHT))
        .padding(Padding::from([0.0, tokens::PAD_X]))
        .align_y(Alignment::Center)
        .into()
}

fn divider(palette: Palette) -> Element<'static, Message> {
    container(Space::new().height(Length::Fixed(1.0)))
        .width(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(palette.divider.into()),
            ..Default::default()
        })
        .into()
}

fn body(dock: &Dock, palette: Palette) -> Element<'_, Message> {
    let mut body = column![].spacing(tokens::GAP);

    match dock.state {
        DockState::Searching => {
            body = body.push(
                text(&dock.status_line)
                    .font(tokens::FONT)
                    .size(tokens::FONT_ANSWER)
                    .color(palette.fg_dim),
            );
        }
        DockState::Answer | DockState::NoAnswer => {
            let colour = match (dock.failed, dock.state) {
                (true, _) => palette.danger,
                (false, DockState::NoAnswer) => palette.fg_dim,
                _ => palette.fg,
            };
            body = body.push(
                scrollable(
                    text(&dock.answer)
                        .font(tokens::FONT)
                        .size(tokens::FONT_ANSWER)
                        .color(colour),
                )
                .height(Length::Shrink)
                // Growth is bounded so a long answer scrolls instead of running
                // off the bottom of the screen (spec §41).
                .width(Length::Fill),
            );
        }
        DockState::Input => {}
    }

    if let Some(source) = &dock.source {
        body = body.push(source_badge(source, dock.extra_sources, palette));
    }

    if !dock.actions.is_empty() {
        body = body.push(action_row(dock, palette));
    }

    container(body)
        .width(Length::Fill)
        .max_height(tokens::ANSWER_MAX_HEIGHT)
        .padding(Padding {
            top: tokens::PAD_Y,
            bottom: tokens::PAD_Y,
            left: tokens::PAD_X,
            right: tokens::PAD_X,
        })
        .into()
}

/// What was retrieved. Rendered as soon as `Sources` arrives — before
/// generation starts — because the path is frequently all that was needed.
fn source_badge(source: &crate::Source, extra: usize, palette: Palette) -> Element<'_, Message> {
    let mut line = row![
        text(&source.path)
            .font(tokens::FONT)
            .size(tokens::FONT_META)
            .color(palette.accent),
    ]
    .spacing(tokens::GAP_TIGHT)
    .align_y(Alignment::Center);

    if !source.heading.is_empty() {
        line = line.push(
            text("·")
                .font(tokens::FONT)
                .size(tokens::FONT_META)
                .color(palette.fg_faint),
        );
        line = line.push(
            text(&source.heading)
                .font(tokens::FONT)
                .size(tokens::FONT_META)
                .color(palette.fg_dim),
        );
    }

    if extra > 0 {
        line = line.push(Space::new().width(Length::Fill));
        line = line.push(
            text(format!("{extra} more"))
                .font(tokens::FONT)
                .size(tokens::FONT_META)
                .color(palette.fg_faint),
        );
    }

    if source.explain.is_empty() {
        return line.into();
    }

    // Why this result, under which result. Faintest text in the card: it is there when you
    // look for it and invisible when you are just reading the answer.
    column![
        line,
        text(&source.explain)
            .font(tokens::FONT)
            .size(tokens::FONT_META)
            .color(palette.fg_faint),
    ]
    .spacing(tokens::GAP_TIGHT)
    .into()
}

fn action_row(dock: &Dock, palette: Palette) -> Element<'_, Message> {
    let mut actions = row![].spacing(tokens::GAP_TIGHT);

    for (index, action) in dock.actions.iter().enumerate() {
        let selected = dock.selected_action == Some(index);
        let label = format!("{}  {}", index + 1, action.label);

        let mut control = button(
            text(label)
                .font(tokens::FONT)
                .size(tokens::FONT_META)
                .color(if action.enabled {
                    palette.fg
                } else {
                    palette.fg_faint
                }),
        )
        .padding(Padding::from([6.0, 10.0]))
        .style(move |_theme, status| {
            let background = match (selected, status) {
                (true, _) => palette.action_bg_focus,
                (false, button::Status::Hovered) => palette.action_bg_hover,
                _ => palette.action_bg,
            };
            button::Style {
                background: Some(background.into()),
                text_color: palette.fg,
                border: Border {
                    radius: 8.0.into(),
                    width: if selected { 1.0 } else { 0.0 },
                    color: palette.accent,
                },
                ..Default::default()
            }
        });

        if action.enabled {
            control = control.on_press(Message::ActivateAction(index));
        }

        actions = actions.push(control);
    }

    actions.push(Space::new().width(Length::Fill)).into()
}
