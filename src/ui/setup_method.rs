use iced::border::Radius;
use iced::keyboard;
use iced::widget::{Space, column, container, mouse_area, row, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Subscription, Task};

use crate::settings;
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{
    auth_background, auth_card, black, bold, hint_pill, kao_hero, link_button, mono,
    screen_subtitle, screen_title, vspace,
};

#[derive(Debug, Clone)]
pub enum Message {
    ImportFromSeed,
    ImportFromPrivateKey,
    CreateNewWallet,
    ConnectHardwareWallet,
    WatchAddress,
    AddSafe,
    BackPressed,
    KeyboardEvent(keyboard::Event),
}

/// The setup method chosen by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupMethod {
    ImportFromSeed,
    ImportFromPrivateKey,
    CreateNewWallet,
    ConnectHardwareWallet,
    WatchAddress,
    AddSafe,
}

/// What happened when the user interacted with the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Selected(SetupMethod),
    /// User asked to leave the setup flow (Esc). The parent decides whether
    /// this is meaningful — fresh setup ignores it; add-account mode treats
    /// it as a return-to-dashboard signal.
    Cancel,
}

#[derive(Debug, Default)]
pub struct SetupMethodScreen {
    /// The method the user selected, once they choose.
    selected: Option<SetupMethod>,
}

struct MethodCard<'a> {
    bg: Color,
    accent: Color,
    number: &'a str,
    label: &'a str,
    sub: &'a str,
    on_press: Message,
}

impl SetupMethodScreen {
    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Outcome>) {
        let pick = |this: &mut Self, m: SetupMethod| {
            this.selected = Some(m);
            (Task::none(), Some(Outcome::Selected(m)))
        };
        match message {
            Message::ImportFromSeed => pick(self, SetupMethod::ImportFromSeed),
            Message::ImportFromPrivateKey => pick(self, SetupMethod::ImportFromPrivateKey),
            Message::CreateNewWallet => pick(self, SetupMethod::CreateNewWallet),
            Message::ConnectHardwareWallet => pick(self, SetupMethod::ConnectHardwareWallet),
            Message::WatchAddress => pick(self, SetupMethod::WatchAddress),
            Message::AddSafe => pick(self, SetupMethod::AddSafe),
            Message::BackPressed => (Task::none(), Some(Outcome::Cancel)),
            Message::KeyboardEvent(event) => match event {
                keyboard::Event::KeyPressed { key, .. } => match &key {
                    keyboard::Key::Character(c) if c.as_str() == "1" => {
                        pick(self, SetupMethod::ImportFromSeed)
                    }
                    keyboard::Key::Character(c) if c.as_str() == "2" => {
                        pick(self, SetupMethod::ImportFromPrivateKey)
                    }
                    keyboard::Key::Character(c) if c.as_str() == "3" => {
                        pick(self, SetupMethod::CreateNewWallet)
                    }
                    keyboard::Key::Character(c) if c.as_str() == "4" => {
                        pick(self, SetupMethod::ConnectHardwareWallet)
                    }
                    keyboard::Key::Character(c) if c.as_str() == "5" => {
                        pick(self, SetupMethod::WatchAddress)
                    }
                    keyboard::Key::Character(c) if c.as_str() == "6" => {
                        pick(self, SetupMethod::AddSafe)
                    }
                    keyboard::Key::Named(keyboard::key::Named::Escape) => {
                        (Task::none(), Some(Outcome::Cancel))
                    }
                    _ => (Task::none(), None),
                },
                _ => (Task::none(), None),
            },
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let t = KaoTheme::for_kind(settings::theme());

        let seed_card = self.method_card(
            t,
            MethodCard {
                bg: t.ab1,
                accent: t.a1,
                number: "1",
                label: "Import from Seed",
                sub: "Restore using a 12 or 24-word phrase",
                on_press: Message::ImportFromSeed,
            },
        );
        let key_card = self.method_card(
            t,
            MethodCard {
                bg: t.ab2,
                accent: t.a2,
                number: "2",
                label: "Import Private Key",
                sub: "Restore from a raw 32-byte hex key",
                on_press: Message::ImportFromPrivateKey,
            },
        );
        let create_card = self.method_card(
            t,
            MethodCard {
                bg: t.ab3,
                accent: t.a3,
                number: "3",
                label: "Create New Wallet",
                sub: "Generate a fresh seed phrase",
                on_press: Message::CreateNewWallet,
            },
        );
        let hardware_card = self.method_card(
            t,
            MethodCard {
                bg: t.ab1,
                accent: t.a1,
                number: "4",
                label: "Hardware Wallet",
                sub: "Connect a Ledger or Trezor device",
                on_press: Message::ConnectHardwareWallet,
            },
        );
        let watch_card = self.method_card(
            t,
            MethodCard {
                bg: t.ab2,
                accent: t.a2,
                number: "5",
                label: "Watch an Address",
                sub: "Track any wallet read-only — view-only mode",
                on_press: Message::WatchAddress,
            },
        );
        let safe_card = self.method_card(
            t,
            MethodCard {
                bg: t.ab3,
                accent: t.a3,
                number: "6",
                label: "Add a Safe",
                sub: "Onboard a Gnosis Safe multisig as signer or observer",
                on_press: Message::AddSafe,
            },
        );

        let hint = container(
            row![
                hint_pill(t, "1"),
                Space::new().width(4),
                hint_pill(t, "2"),
                Space::new().width(4),
                hint_pill(t, "3"),
                Space::new().width(4),
                hint_pill(t, "4"),
                Space::new().width(4),
                hint_pill(t, "5"),
                Space::new().width(4),
                hint_pill(t, "6"),
                Space::new().width(8),
                text("pick a method").size(11).color(t.sub).font(mono()),
            ]
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        let content = column![
            kao_hero(t, "٩(◕‿◕｡)۶", 56.0),
            vspace(10),
            screen_title(t, "Set Up Your Wallet"),
            vspace(6),
            screen_subtitle(t, "Choose how you'd like to get started."),
            vspace(22),
            seed_card,
            vspace(10),
            key_card,
            vspace(10),
            create_card,
            vspace(10),
            hardware_card,
            vspace(10),
            watch_card,
            vspace(10),
            safe_card,
            vspace(16),
            hint,
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        let card = auth_card(t, 460.0, content.into());

        let back_bar = container(link_button(t, "← Back").on_press(Message::BackPressed))
            .padding(Padding::from([12, 14]))
            .width(Length::Fill);

        let layout = column![back_bar, card]
            .width(Length::Fill)
            .height(Length::Fill);

        auth_background(t, layout.into())
    }

    fn method_card<'a>(&self, t: KaoTheme, card: MethodCard<'a>) -> Element<'a, Message> {
        let MethodCard {
            bg,
            accent,
            number,
            label,
            sub,
            on_press,
        } = card;
        let info = column![
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

    /// Keyboard subscription for number key shortcuts.
    pub fn subscription(&self) -> Subscription<Message> {
        keyboard::listen().map(Message::KeyboardEvent)
    }
}
