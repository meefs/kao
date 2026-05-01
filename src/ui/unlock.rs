use iced::widget::{Space, column, container, row, text, text_input};
use iced::{Alignment, Element, Length, Padding, Task};
use secrecy::SecretString;
use zeroize::Zeroizing;

use crate::settings;
use crate::ui::kao_theme::KaoTheme;
use crate::ui::kao_widgets::{
    auth_background, auth_card, error_text, hint_pill, kao_hero, mono, primary_button,
    screen_subtitle, screen_title, text_input_style, vspace,
};
use crate::wallet::{self, WalletDescriptor};

pub const PASSWORD_INPUT_ID: &str = "unlock_password_input";

#[derive(Debug, Clone)]
pub enum Message {
    PasswordInput(String),
    UnlockPressed,
    PasswordSubmitted,
    UnlockResult(Result<WalletDescriptor, String>),
}

/// Outcome signals emitted by this screen to its parent.
#[derive(Debug)]
pub enum Outcome {
    Unlocked {
        passphrase: SecretString,
        descriptor: WalletDescriptor,
    },
}

#[derive(Debug, Default)]
pub struct UnlockScreen {
    /// Live `text_input` buffer. `Zeroizing<String>` zeros the heap allocation
    /// each time the input is replaced (every keystroke) and on screen drop.
    password: Zeroizing<String>,
    error: Option<String>,
    unlocking: bool,
}

impl UnlockScreen {
    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Outcome>) {
        match message {
            Message::PasswordInput(p) => {
                self.password = Zeroizing::new(p);
                (Task::none(), None)
            }
            Message::PasswordSubmitted | Message::UnlockPressed => (self.try_unlock(), None),
            Message::UnlockResult(Ok(descriptor)) => {
                self.unlocking = false;
                // Take the buffer so the previous heap allocation gets
                // zeroed on drop. `Box::from(&str)` reallocates into a
                // fresh buffer that `SecretString` then owns and zeros.
                let taken = std::mem::take(&mut self.password);
                let passphrase = SecretString::new(Box::from(taken.as_str()));
                (
                    Task::none(),
                    Some(Outcome::Unlocked {
                        passphrase,
                        descriptor,
                    }),
                )
            }
            Message::UnlockResult(Err(e)) => {
                self.unlocking = false;
                self.error = Some(e);
                (Task::none(), None)
            }
        }
    }

    fn try_unlock(&mut self) -> Task<Message> {
        if self.password.is_empty() || self.unlocking {
            return Task::none();
        }
        self.error = None;
        self.unlocking = true;

        // Clone into another `Zeroizing<String>` so the async-task copy
        // gets wiped after the SecretString takes over. The original
        // `self.password` stays put so the user doesn't have to retype on
        // a wrong-password error.
        let password = self.password.clone();
        Task::perform(
            async move {
                let passphrase = SecretString::new(Box::from(password.as_str()));
                wallet::load_descriptor(&passphrase).map_err(|e| match e {
                    wallet::WalletError::Encryption(_) => "Incorrect password".to_string(),
                    other => other.to_string(),
                })
            },
            Message::UnlockResult,
        )
    }

    pub fn view(&self) -> Element<'_, Message> {
        let t = KaoTheme::for_kind(settings::theme());

        let password_input = text_input("Password", self.password.as_str())
            .id(PASSWORD_INPUT_ID)
            .secure(true)
            .on_input(Message::PasswordInput)
            .on_submit(Message::PasswordSubmitted)
            .padding(Padding::from([12, 14]))
            .size(14)
            .font(mono())
            .style(move |_theme, status| text_input_style(t, status));

        let (btn_label, enabled) = if self.unlocking {
            ("(・・;)ゞ unlocking…", false)
        } else {
            ("Unlock ✓", true)
        };
        let unlock_btn = {
            let mut b = primary_button(t, btn_label, enabled);
            if enabled {
                b = b.on_press(Message::UnlockPressed);
            }
            b
        };

        let hint = container(
            row![
                hint_pill(t, "Enter"),
                Space::new().width(6),
                text("to unlock").size(11).color(t.sub),
            ]
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        let mut content = column![
            kao_hero(t, "(˘ᵕ˘)", 56.0),
            vspace(10),
            screen_title(t, "Welcome Back"),
            vspace(6),
            screen_subtitle(
                t,
                "Enter your password to decrypt the wallet on this device."
            ),
            vspace(24),
            password_input,
            vspace(20),
            unlock_btn,
            vspace(14),
            hint,
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        if let Some(e) = &self.error {
            content = content.push(vspace(10)).push(error_text(t, e));
        }

        auth_background(t, auth_card(t, 400.0, content.into()))
    }
}
