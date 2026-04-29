//! Top header strip — page title, address pill, network row, mood pill.

use alloy::primitives::Address;
use iced::border::Radius;
use iced::widget::{Space, column, container, mouse_area, row, text};
use iced::{Alignment, Background, Border, Element, Length, Padding};

use crate::net::VerificationStatus;
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{bold, kao_text, mono, mono_bold, verification_badge};
use crate::wallet::short_address;

use super::{MOOD, Message, Nav};

pub fn view<'a>(
    t: KaoTheme,
    nav: Nav,
    address: Address,
    verification: VerificationStatus,
) -> Element<'a, Message> {
    let title = match nav {
        Nav::Home => "Portfolio",
        Nav::Activity => "Activity",
        Nav::Settings => "Settings",
    };
    let addr_short = short_address(address);
    // Address pill: clickable trigger that opens the account dropdown.
    let addr_pill = container(
        row![
            text(addr_short).size(22).color(t.text).font(mono_bold()),
            Space::new().width(8),
            text("▾").size(14).color(t.sub),
        ]
        .align_y(Alignment::Center),
    )
    .padding(Padding::from([4, 10]))
    .style(move |_| container::Style {
        background: Some(Background::Color(t.ab1)),
        border: Border {
            color: with_alpha(t.a1, 0.18),
            width: 1.0,
            radius: Radius::from(10),
        },
        text_color: Some(t.text),
        ..container::Style::default()
    });
    let addr_trigger: Element<'_, Message> = mouse_area(addr_pill)
        .on_press(Message::OpenAccountDropdown)
        .interaction(iced::mouse::Interaction::Pointer)
        .into();

    let net_row = row![
        text("Ethereum Mainnet").size(11).color(t.sub).font(mono()),
        Space::new().width(8),
        verification_badge(t, verification),
    ]
    .align_y(Alignment::Center);

    let title_col = column![
        text(title).size(17).color(t.text).font(bold()),
        Space::new().height(2),
        addr_trigger,
        Space::new().height(2),
        net_row,
    ]
    .spacing(1);

    let mood_pill = container(kao_text(t, MOOD, 15.0))
        .padding(Padding::from([6, 13]))
        .style(move |_| container::Style {
            background: Some(Background::Color(t.ab1)),
            border: Border {
                color: with_alpha(t.a1, 0.2),
                width: 1.0,
                radius: Radius::from(10),
            },
            ..container::Style::default()
        });

    container(
        row![title_col, Space::new().width(Length::Fill), mood_pill]
            .align_y(Alignment::Center)
            .width(Length::Fill),
    )
    .padding(Padding::from([14, 24]))
    .width(Length::Fill)
    .style(move |_| container::Style {
        border: Border {
            color: t.border,
            width: 1.0,
            radius: Radius::from(0),
        },
        ..container::Style::default()
    })
    .into()
}
