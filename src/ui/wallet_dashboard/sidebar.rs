//! Vertical sidebar — wordmark and nav icons. Stateless view.

use iced::border::Radius;
use iced::widget::{Space, column, container, mouse_area, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding};

use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{bold, kao_fit};

use super::{Message, Nav};

pub fn view<'a>(t: KaoTheme, nav: Nav) -> Element<'a, Message> {
    let wordmark = container(text("KAO").size(24).color(t.a1).font(iced::Font {
        weight: iced::font::Weight::Black,
        ..iced::Font::DEFAULT
    }))
    .width(Length::Fill)
    .center_x(Length::Fill)
    .padding(Padding::from([18, 0]));

    let nav_items = [
        (Nav::Home, "(´｡• ᵕ •｡`)", "Portfolio"),
        (Nav::Activity, "(˘ᵕ˘)", "History"),
        (Nav::Settings, "(・ω・)", "Settings"),
    ];

    let mut nav_col = column![]
        .spacing(18)
        .align_x(Alignment::Center)
        .width(Length::Fill);
    nav_col = nav_col.push(wordmark);
    for (id, kao, label) in nav_items {
        nav_col = nav_col.push(nav_icon(t, nav, id, kao, label));
    }
    nav_col = nav_col.push(Space::new().height(Length::Fill));

    container(nav_col)
        .padding(Padding::from([22, 0]))
        .width(Length::Fixed(100.0))
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(t.sidebar)),
            border: Border {
                color: t.border,
                width: 1.0,
                radius: Radius::from(0),
            },
            text_color: Some(t.text),
            ..container::Style::default()
        })
        .into()
}

fn nav_icon<'a>(
    t: KaoTheme,
    active_nav: Nav,
    id: Nav,
    kao: &'a str,
    label: &'a str,
) -> Element<'a, Message> {
    let active = active_nav == id;
    let bg = if active { t.ab1 } else { Color::TRANSPARENT };
    let border_color = if active {
        with_alpha(t.a1, 0.33)
    } else {
        Color::TRANSPARENT
    };
    let icon_box = container(kao_fit(t, kao, 76.0, 34.0))
        .width(Length::Fixed(92.0))
        .height(Length::Fixed(92.0))
        .align_x(iced::alignment::Horizontal::Center)
        .align_y(iced::alignment::Vertical::Center)
        .style(move |_| container::Style {
            background: Some(Background::Color(bg)),
            border: Border {
                color: border_color,
                width: 1.5,
                radius: Radius::from(20),
            },
            text_color: Some(t.text),
            ..container::Style::default()
        });
    let label_color = if active { t.text } else { t.sub };
    let inner = column![
        icon_box,
        text(label).size(18).color(label_color).font(bold()),
    ]
    .spacing(6)
    .align_x(Alignment::Center);
    mouse_area(inner)
        .on_press(Message::SelectNav(id))
        .interaction(iced::mouse::Interaction::Pointer)
        .into()
}
