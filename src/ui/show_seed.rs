use alloy::primitives::Address;
use iced::border::Radius;
use iced::clipboard;
use iced::keyboard;
use iced::widget::{Space, column, container, row, text};
use iced::{Alignment, Background, Border, Element, Length, Padding, Subscription, Task};
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

use crate::settings;
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{
    auth_background, auth_card, bold, hint_pill, kao_hero, link_button, mono, mono_bold,
    primary_button, screen_subtitle, screen_title, secondary_button, vspace,
};
use crate::wallet;

#[derive(Debug, Clone)]
pub enum Message {
    CopySeed,
    SeedCopied,
    Continue,
    BackPressed,
    KeyboardEvent(keyboard::Event),
}

/// Outcome signals emitted by this screen to its parent.
#[derive(Debug, Clone)]
pub enum Outcome {
    Continue,
    Back,
}

#[derive(Debug)]
pub struct ShowSeedScreen {
    seed_phrase: SecretString,
    key_bytes: Zeroizing<[u8; 32]>,
    address: Address,
    /// True once the user has clicked Copy on the current visit. Drives the
    /// auto-clear-clipboard-on-Continue behavior so we don't clobber unrelated
    /// clipboard contents when the user never copied. Reset per-instance.
    did_copy: bool,
}

impl ShowSeedScreen {
    /// Create a new screen by generating a fresh mnemonic.
    pub fn generate() -> Result<Self, wallet::WalletError> {
        let (phrase, signer) = wallet::generate_mnemonic()?;
        let key_bytes = Zeroizing::new(signer.to_bytes().into());
        let address = wallet::signer_address(&signer);
        Ok(Self {
            seed_phrase: phrase,
            key_bytes,
            address,
            did_copy: false,
        })
    }

    /// Reconstruct a ShowSeedScreen when navigating back from VerifySeed.
    pub fn from_existing(
        phrase: SecretString,
        key_bytes: Zeroizing<[u8; 32]>,
        address: Address,
    ) -> Self {
        Self {
            seed_phrase: phrase,
            key_bytes,
            address,
            did_copy: false,
        }
    }

    /// Consume the screen and return data needed by the verify screen.
    pub fn into_wallet_data(self) -> (SecretString, Zeroizing<[u8; 32]>, Address) {
        (self.seed_phrase, self.key_bytes, self.address)
    }

    /// Best-effort wipe of the clipboard slot we wrote the seed into. Returns
    /// an empty task when we never copied, so we don't clobber unrelated
    /// clipboard contents. Only clears the current slot, not clipboard-manager
    /// history.
    fn clear_clipboard_task(&self) -> Task<Message> {
        if self.did_copy {
            clipboard::write(String::new()).map(|_: ()| Message::SeedCopied)
        } else {
            Task::none()
        }
    }

    /// Build the (Task, Outcome) pair that moves the user *past* this screen,
    /// overwriting the clipboard if we put the seed there.
    fn continue_outcome(&self) -> (Task<Message>, Option<Outcome>) {
        (self.clear_clipboard_task(), Some(Outcome::Continue))
    }

    /// Build the (Task, Outcome) pair that takes the user *back* off this
    /// screen. Clears the clipboard on the way out too — leaving Back/Escape
    /// would otherwise strand a copied seed in the clipboard.
    fn back_outcome(&self) -> (Task<Message>, Option<Outcome>) {
        (self.clear_clipboard_task(), Some(Outcome::Back))
    }

    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Outcome>) {
        match message {
            Message::CopySeed => {
                // Clipboard write requires an owned String — the one
                // unavoidable plaintext copy on the path to the OS clipboard.
                // We auto-clear on Continue (see `continue_outcome`).
                let phrase = self.seed_phrase.expose_secret().to_string();
                (
                    clipboard::write(phrase).map(|_: ()| Message::SeedCopied),
                    None,
                )
            }
            Message::SeedCopied => {
                self.did_copy = true;
                (Task::none(), None)
            }
            Message::Continue => self.continue_outcome(),
            Message::BackPressed => self.back_outcome(),
            Message::KeyboardEvent(keyboard::Event::KeyPressed { key, .. }) => match key {
                keyboard::Key::Named(keyboard::key::Named::Escape) => self.back_outcome(),
                keyboard::Key::Named(keyboard::key::Named::Enter) => self.continue_outcome(),
                _ => (Task::none(), None),
            },
            Message::KeyboardEvent(_) => (Task::none(), None),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        keyboard::listen().map(Message::KeyboardEvent)
    }

    pub fn view(&self) -> Element<'_, Message> {
        let t = KaoTheme::for_kind(settings::theme());

        let words: Vec<&str> = self
            .seed_phrase
            .expose_secret()
            .split_whitespace()
            .collect();
        let mut grid = column![].spacing(8);
        for (chunk_idx, chunk) in words.chunks(4).enumerate() {
            let mut row_el = row![].spacing(8);
            for (offset, w) in chunk.iter().enumerate() {
                let idx = chunk_idx * 4 + offset + 1;
                row_el = row_el.push(word_cell(t, idx, Some(*w)));
            }
            grid = grid.push(row_el);
        }

        let warning = container(
            row![
                text("⚠").size(15).color(t.down),
                Space::new().width(8),
                text("Save this seed now — it cannot be exported later.")
                    .size(12)
                    .color(t.text)
                    .font(bold()),
            ]
            .align_y(Alignment::Center),
        )
        .padding(Padding::from([10, 14]))
        .style(move |_| container::Style {
            background: Some(Background::Color(with_alpha(t.down, 0.10))),
            border: Border {
                color: with_alpha(t.down, 0.35),
                width: 1.0,
                radius: Radius::from(12),
            },
            text_color: Some(t.text),
            ..container::Style::default()
        });

        let action_row = row![
            container(secondary_button(t, "Copy ⎘").on_press(Message::CopySeed))
                .width(Length::FillPortion(1)),
            Space::new().width(10),
            container(primary_button(t, "Continue →", true).on_press(Message::Continue))
                .width(Length::FillPortion(2)),
        ]
        .width(Length::Fill);

        let hint = container(
            row![
                hint_pill(t, "Enter"),
                Space::new().width(6),
                text("to continue · ").size(11).color(t.sub),
                hint_pill(t, "Esc"),
                Space::new().width(6),
                text("to go back").size(11).color(t.sub),
            ]
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        let content = column![
            kao_hero(t, "(¬‿¬)", 56.0),
            vspace(10),
            screen_title(t, "Your Seed Phrase"),
            vspace(6),
            screen_subtitle(t, "Write these words down and store them somewhere safe."),
            vspace(22),
            grid,
            vspace(16),
            warning,
            vspace(18),
            action_row,
            vspace(14),
            hint,
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        let card = auth_card(t, 560.0, content.into());

        let back_bar = container(link_button(t, "← Back").on_press(Message::BackPressed))
            .padding(Padding::from([12, 14]))
            .width(Length::Fill);

        let layout = column![back_bar, card]
            .width(Length::Fill)
            .height(Length::Fill);

        auth_background(t, layout.into())
    }
}

fn word_cell<'a>(t: KaoTheme, num: usize, word: Option<&'a str>) -> Element<'a, Message> {
    let label = word.unwrap_or("???");
    let inner = row![
        text(format!("{num:>2}")).size(11).color(t.sub).font(mono()),
        Space::new().width(6),
        text(label).size(13).color(t.text).font(mono_bold()),
    ]
    .align_y(Alignment::Center);

    container(inner)
        .padding(Padding::from([8, 10]))
        .width(Length::Fixed(120.0))
        .style(move |_| container::Style {
            background: Some(Background::Color(t.card_alt)),
            border: Border {
                color: t.border,
                width: 1.0,
                radius: Radius::from(10),
            },
            text_color: Some(t.text),
            ..container::Style::default()
        })
        .into()
}
