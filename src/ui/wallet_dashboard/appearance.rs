//! Appearance settings sub-screen — theme picker. Owned by the dashboard's
//! Settings nav slot.

use iced::border::Radius;
use iced::widget::{Space, column, container, mouse_area, row, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding};

use crate::ui::kao_theme::{KaoTheme, ThemeKind};
use crate::ui::kao_widgets::{black, bold, card_style, section};

use super::Message;

pub fn view<'a>(t: KaoTheme, current: ThemeKind) -> Element<'a, Message> {
    let header = row![
        mouse_area(
            container(text("← Back").size(12).color(t.sub).font(bold()))
                .padding(Padding::from([4, 0])),
        )
        .on_press(Message::CloseAppearanceSettings),
        Space::new().width(Length::Fill),
        text("Appearance").size(14).color(t.text).font(black()),
        Space::new().width(Length::Fill),
        text("    ").size(12),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);

    let mut swatches = column![].spacing(8);
    for k in ThemeKind::ALL {
        swatches = swatches.push(theme_row(t, current, k));
    }

    let theme_section = section(
        t,
        "Theme",
        "(｡◕‿◕｡)",
        "Pick a palette. Applies instantly across the app.",
        swatches.into(),
    );

    let body = column![header, Space::new().height(16), theme_section]
        .spacing(0)
        .width(Length::Fill);

    iced::widget::scrollable(
        container(body)
            .padding(Padding::from([22, 24]))
            .width(Length::Fill),
    )
    .height(Length::Fill)
    .width(Length::Fill)
    .into()
}

fn theme_row<'a>(t: KaoTheme, current: ThemeKind, k: ThemeKind) -> Element<'a, Message> {
    let selected = current == k;
    let theme = KaoTheme::for_kind(k);
    let swatch_color = k.swatch();

    let outline_col = if selected { t.a1 } else { Color::TRANSPARENT };
    let swatch = container(Space::new().width(36).height(36))
        .width(Length::Fixed(40.0))
        .height(Length::Fixed(40.0))
        .style(move |_| container::Style {
            background: Some(Background::Color(swatch_color)),
            border: Border {
                color: outline_col,
                width: 2.0,
                radius: Radius::from(10),
            },
            ..container::Style::default()
        });

    let info = column![
        text(theme.name).size(14).color(t.text).font(bold()),
        text(theme.icon).size(11).color(t.sub),
    ]
    .spacing(0);

    let check = if selected {
        text("✓").size(16).color(t.a1).font(bold())
    } else {
        text(" ").size(16).color(Color::TRANSPARENT)
    };

    let row = row![
        swatch,
        Space::new().width(14),
        column![info].width(Length::Fill),
        check,
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);

    let card: Element<'a, Message> = container(row)
        .padding(Padding::from([12, 14]))
        .width(Length::Fill)
        .style(move |_| card_style(t))
        .into();

    mouse_area(card)
        .on_press(Message::SelectTheme(k))
        .interaction(iced::mouse::Interaction::Pointer)
        .into()
}
