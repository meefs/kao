use alloy::primitives::Address;
use iced::keyboard;
use iced::widget::{Space, column, container, row, text, text_input};
use iced::{Alignment, Element, Length, Padding, Subscription, Task};

use crate::settings;
use crate::ui::kao_theme::KaoTheme;
use crate::ui::kao_widgets::{
    auth_background, auth_card, error_text, hint_pill, kao_hero, link_button, mono, primary_button,
    screen_subtitle, screen_title, text_input_style, vspace,
};

pub const ADDRESS_INPUT_ID: &str = "view_only_address_input";

#[derive(Debug, Clone)]
pub enum Message {
    AddressInput(String),
    AddPressed,
    BackPressed,
    KeyboardEvent(keyboard::Event),
}

/// Outcome signals emitted by this screen to its parent.
#[derive(Debug, Clone)]
pub enum Outcome {
    Imported { address: Address },
    Back,
}

#[derive(Debug, Default)]
pub struct ImportAddressScreen {
    address_input: String,
    error: Option<String>,
}

impl ImportAddressScreen {
    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Outcome>) {
        match message {
            Message::AddressInput(s) => {
                self.address_input = s;
                (Task::none(), None)
            }
            Message::AddPressed => (Task::none(), self.try_import()),
            Message::BackPressed => (Task::none(), Some(Outcome::Back)),
            Message::KeyboardEvent(keyboard::Event::KeyPressed { key, .. }) => match key {
                keyboard::Key::Named(keyboard::key::Named::Escape) => {
                    (Task::none(), Some(Outcome::Back))
                }
                _ => (Task::none(), None),
            },
            Message::KeyboardEvent(_) => (Task::none(), None),
        }
    }

    fn try_import(&mut self) -> Option<Outcome> {
        let trimmed = self.address_input.trim();
        if trimmed.is_empty() {
            self.error = Some("Please enter an Ethereum address.".into());
            return None;
        }
        match trimmed.parse::<Address>() {
            Ok(address) => {
                self.error = None;
                Some(Outcome::Imported { address })
            }
            Err(_) => {
                self.error = Some(
                    "Invalid address — expected 20 bytes as 40 hex characters (0x prefix optional)."
                        .into(),
                );
                None
            }
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        keyboard::listen().map(Message::KeyboardEvent)
    }

    pub fn view(&self) -> Element<'_, Message> {
        let t = KaoTheme::for_kind(settings::theme());

        let address_input = text_input("0x…", &self.address_input)
            .id(ADDRESS_INPUT_ID)
            .on_input(Message::AddressInput)
            .on_submit(Message::AddPressed)
            .padding(Padding::from([12, 14]))
            .size(14)
            .font(mono())
            .style(move |_theme, status| text_input_style(t, status));

        let add_btn = primary_button(t, "Watch Address →", true).on_press(Message::AddPressed);

        let hint = container(
            row![
                hint_pill(t, "Enter"),
                Space::new().width(6),
                text("to add · ").size(11).color(t.sub),
                hint_pill(t, "Esc"),
                Space::new().width(6),
                text("to go back").size(11).color(t.sub),
            ]
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        let mut content = column![
            kao_hero(t, "(◉‿◉)", 56.0),
            vspace(10),
            screen_title(t, "Watch an Address"),
            vspace(6),
            screen_subtitle(t, "Track any wallet read-only — no signing, no risk."),
            vspace(22),
            address_input,
            vspace(18),
            add_btn,
            vspace(14),
            hint,
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        if let Some(e) = &self.error {
            content = content.push(vspace(10)).push(error_text(t, e));
        }

        let card = auth_card(t, 520.0, content.into());

        let back_bar = container(link_button(t, "← Back").on_press(Message::BackPressed))
            .padding(Padding::from([12, 14]))
            .width(Length::Fill);

        let layout = column![back_bar, card]
            .width(Length::Fill)
            .height(Length::Fill);

        auth_background(t, layout.into())
    }
}
