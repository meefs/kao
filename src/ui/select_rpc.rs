use iced::border::Radius;
use iced::keyboard;
use iced::widget::operation::focus as focus_widget;
use iced::widget::{Space, column, container, mouse_area, row, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Subscription, Task};

use crate::settings;
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{
    auth_background, auth_card, black, bold, error_text, hint_pill, kao_hero, link_button, mono,
    primary_button, screen_subtitle, screen_title, text_input_style, vspace,
};

pub const CUSTOM_RPC_INPUT_ID: &str = "custom_rpc_input";

#[derive(Debug, Clone)]
pub enum Message {
    UseDefaultsPressed,
    PickCustomPressed,
    CustomInput(String),
    SubmitCustom,
    BackToOptions,
    KeyboardEvent(keyboard::Event),
}

/// Outcome signals emitted by this screen to its parent.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Keep the curated default RPC list. App should not touch the rpcs setting.
    UseDefaults,
    /// Replace the RPC list with the user-provided URL.
    Custom(String),
}

#[derive(Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Choose,
    Custom,
}

#[derive(Debug, Default)]
pub struct SelectRpcScreen {
    mode: Mode,
    custom_input: String,
    error: Option<String>,
}

impl SelectRpcScreen {
    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Outcome>) {
        match message {
            Message::UseDefaultsPressed => (Task::none(), Some(Outcome::UseDefaults)),
            Message::PickCustomPressed => {
                self.mode = Mode::Custom;
                self.error = None;
                (focus_widget(CUSTOM_RPC_INPUT_ID), None)
            }
            Message::CustomInput(s) => {
                self.custom_input = s;
                (Task::none(), None)
            }
            Message::SubmitCustom => (Task::none(), self.try_submit_custom()),
            Message::BackToOptions => {
                self.mode = Mode::Choose;
                self.error = None;
                (Task::none(), None)
            }
            Message::KeyboardEvent(keyboard::Event::KeyPressed { key, .. }) => {
                match (&self.mode, &key) {
                    (Mode::Choose, keyboard::Key::Character(c)) if c.as_str() == "1" => {
                        (Task::none(), Some(Outcome::UseDefaults))
                    }
                    (Mode::Choose, keyboard::Key::Character(c)) if c.as_str() == "2" => {
                        self.mode = Mode::Custom;
                        self.error = None;
                        (focus_widget(CUSTOM_RPC_INPUT_ID), None)
                    }
                    (Mode::Custom, keyboard::Key::Named(keyboard::key::Named::Escape)) => {
                        self.mode = Mode::Choose;
                        self.error = None;
                        (Task::none(), None)
                    }
                    _ => (Task::none(), None),
                }
            }
            Message::KeyboardEvent(_) => (Task::none(), None),
        }
    }

    fn try_submit_custom(&mut self) -> Option<Outcome> {
        let trimmed = self.custom_input.trim();
        if trimmed.is_empty() {
            self.error = Some("Please enter an RPC URL.".into());
            return None;
        }
        if !(trimmed.starts_with("https://") || trimmed.starts_with("http://")) {
            self.error = Some("RPC URL must start with http:// or https://".into());
            return None;
        }
        self.error = None;
        Some(Outcome::Custom(trimmed.to_string()))
    }

    pub fn view(&self) -> Element<'_, Message> {
        let t = KaoTheme::for_kind(settings::theme());
        match self.mode {
            Mode::Choose => self.view_choose(t),
            Mode::Custom => self.view_custom(t),
        }
    }

    fn view_choose(&self, t: KaoTheme) -> Element<'_, Message> {
        let defaults = settings::default_rpcs();
        let defaults_card = self.option_card(
            t,
            t.ab1,
            t.a1,
            "1",
            "Use Default RPC",
            "LlamaRPC mainnet — verified by Helios",
            defaults,
            Message::UseDefaultsPressed,
        );
        let custom_card = self.option_card(
            t,
            t.ab2,
            t.a2,
            "2",
            "Custom RPC",
            "Provide your own Ethereum RPC URL",
            &[],
            Message::PickCustomPressed,
        );

        let hint = container(
            row![
                hint_pill(t, "1"),
                Space::new().width(4),
                hint_pill(t, "2"),
                Space::new().width(8),
                text("pick a source").size(11).color(t.sub).font(mono()),
            ]
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        let content = column![
            kao_hero(t, "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧", 52.0),
            vspace(10),
            screen_title(t, "Choose Your RPC"),
            vspace(6),
            screen_subtitle(t, "Where should Kao fetch on-chain data from?"),
            vspace(22),
            defaults_card,
            vspace(10),
            custom_card,
            vspace(16),
            hint,
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        auth_background(t, auth_card(t, 460.0, content.into()))
    }

    fn view_custom(&self, t: KaoTheme) -> Element<'_, Message> {
        let url_input = text_input("https://my-node.example/rpc", &self.custom_input)
            .id(CUSTOM_RPC_INPUT_ID)
            .on_input(Message::CustomInput)
            .on_submit(Message::SubmitCustom)
            .padding(Padding::from([12, 14]))
            .size(14)
            .font(mono())
            .style(move |_theme, status| text_input_style(t, status));

        let submit_btn = primary_button(t, "Use This RPC →", true).on_press(Message::SubmitCustom);

        let hint = container(
            row![
                hint_pill(t, "Enter"),
                Space::new().width(6),
                text("to confirm · ").size(11).color(t.sub),
                hint_pill(t, "Esc"),
                Space::new().width(6),
                text("to go back").size(11).color(t.sub),
            ]
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        let mut content = column![
            kao_hero(t, "( •̀ ω •́ )y", 56.0),
            vspace(10),
            screen_title(t, "Custom RPC"),
            vspace(6),
            screen_subtitle(t, "Paste an Ethereum execution-layer RPC endpoint."),
            vspace(22),
            url_input,
            vspace(18),
            submit_btn,
            vspace(14),
            hint,
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        if let Some(e) = &self.error {
            content = content.push(vspace(10)).push(error_text(t, e));
        }

        let card = auth_card(t, 520.0, content.into());

        let back_bar = container(link_button(t, "← Back").on_press(Message::BackToOptions))
            .padding(Padding::from([12, 14]))
            .width(Length::Fill);

        let layout = column![back_bar, card]
            .width(Length::Fill)
            .height(Length::Fill);

        auth_background(t, layout.into())
    }

    fn option_card<'a>(
        &self,
        t: KaoTheme,
        bg: Color,
        accent: Color,
        number: &'a str,
        label: &'a str,
        sub: &'a str,
        extras: &'a [&'a str],
        on_press: Message,
    ) -> Element<'a, Message> {
        let mut info = column![
            row![
                container(text(number).size(11).color(accent).font(black()))
                    .padding(Padding::from([2, 7]))
                    .style(move |_| container::Style {
                        background: Some(Background::Color(bg)),
                        border: Border {
                            color: with_alpha(accent, 0.3),
                            width: 1.0,
                            radius: Radius::from(6),
                        },
                        text_color: Some(accent),
                        ..container::Style::default()
                    }),
                Space::new().width(8),
                text(label).size(15).color(t.text).font(black()),
            ]
            .align_y(Alignment::Center),
            Space::new().height(2),
            text(sub).size(12).color(t.sub),
        ]
        .spacing(0);

        for line in extras {
            info = info.push(Space::new().height(4));
            info = info.push(text(*line).size(11).color(accent).font(mono()));
        }

        let row_content = row![
            container(info).width(Length::Fill),
            text("→").size(18).color(accent).font(bold()),
        ]
        .align_y(Alignment::Center)
        .width(Length::Fill);

        let styled = container(row_content)
            .padding(Padding::from([14, 16]))
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(bg)),
                border: Border {
                    color: with_alpha(accent, 0.25),
                    width: 1.5,
                    radius: Radius::from(15),
                },
                text_color: Some(t.text),
                ..container::Style::default()
            });

        mouse_area(styled)
            .on_press(on_press)
            .interaction(iced::mouse::Interaction::Pointer)
            .into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        keyboard::listen().map(Message::KeyboardEvent)
    }
}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;

    #[test]
    fn use_defaults_emits_outcome() {
        let mut s = SelectRpcScreen::default();
        let (_, outcome) = s.update(Message::UseDefaultsPressed);
        assert!(matches!(outcome, Some(Outcome::UseDefaults)));
    }

    #[test]
    fn pick_custom_switches_mode_without_outcome() {
        let mut s = SelectRpcScreen::default();
        let (_, outcome) = s.update(Message::PickCustomPressed);
        assert!(outcome.is_none());
        assert_eq!(s.mode, Mode::Custom);
    }

    #[test]
    fn empty_custom_url_errors() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustomPressed);
        let (_, outcome) = s.update(Message::SubmitCustom);
        assert!(outcome.is_none());
        assert!(s.error.is_some());
    }

    #[test]
    fn rejects_non_http_scheme() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustomPressed);
        s.update(Message::CustomInput("ws://my-node.example".into()));
        let (_, outcome) = s.update(Message::SubmitCustom);
        assert!(outcome.is_none());
        assert!(s.error.unwrap().contains("http"));
    }

    #[test]
    fn accepts_https_url() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustomPressed);
        s.update(Message::CustomInput("  https://my-node.example/rpc  ".into()));
        let (_, outcome) = s.update(Message::SubmitCustom);
        match outcome {
            Some(Outcome::Custom(url)) => assert_eq!(url, "https://my-node.example/rpc"),
            other => panic!("expected Custom outcome, got {other:?}"),
        }
        assert!(s.error.is_none());
    }

    #[test]
    fn back_returns_to_choose_mode() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustomPressed);
        let (_, outcome) = s.update(Message::BackToOptions);
        assert!(outcome.is_none());
        assert_eq!(s.mode, Mode::Choose);
    }
}
