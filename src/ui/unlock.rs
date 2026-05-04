use iced::widget::{Space, column, container, row, stack, text, text_input};
use iced::{Alignment, Element, Length, Padding, Task};
use secrecy::SecretString;
use zeroize::Zeroizing;

use crate::settings;
use crate::ui::kao_theme::KaoTheme;
use crate::ui::kao_widgets::{
    auth_background, auth_card, error_text, hint_pill, kao_hero, modal_wrapper, mono,
    primary_button, screen_subtitle, screen_title, secondary_button, text_input_style, vspace,
};
use crate::wallet::{self, WalletDescriptor, WalletError};

pub const PASSWORD_INPUT_ID: &str = "unlock_password_input";

/// Structured failure modes from the unlock async task. We classify here
/// rather than storing a `String` so the update handler can dispatch
/// distinct UI behaviour for each: wrong-password just refreshes the
/// inline error label, KeyringMissingEntry pops the security modal, and
/// Other falls back to a generic error string.
#[derive(Debug, Clone)]
pub enum UnlockError {
    WrongPassword,
    /// The wallet file exists but the OS keyring has no record for it on
    /// this machine. Could be a legitimate restore from backup OR an
    /// attacker who copied the file — the user must explicitly resolve.
    KeyringMissingEntry { file_epoch: u64 },
    /// Anything else worth showing verbatim (KeyringUnavailable,
    /// Rollback, IO, …). Pre-formatted so the view doesn't have to
    /// pattern-match on `WalletError`.
    Other(String),
}

#[derive(Debug, Clone)]
pub enum Message {
    PasswordInput(String),
    UnlockPressed,
    PasswordSubmitted,
    UnlockResult(Result<WalletDescriptor, UnlockError>),
    /// User clicked "Accept" on the keyring-missing-entry warning modal.
    /// Re-runs the unlock through the bypass loader, which seeds the
    /// keyring with the file's current epoch on success.
    KeyringWarningAccept,
    /// User clicked "Quit" on the warning modal — exit the app.
    KeyringWarningQuit,
    /// Backdrop click on the warning modal. Closes the modal and drops
    /// the user back at the password input. Doesn't accept anything;
    /// re-submitting the password just re-shows the modal. Explicit by
    /// design — backdrop must never silently dismiss a security prompt.
    KeyringWarningDismiss,
    /// Box click on the warning modal — eaten so it doesn't bubble up
    /// to the backdrop's dismiss handler.
    KeyringWarningModalNoop,
}

/// Outcome signals emitted by this screen to its parent.
#[derive(Debug)]
pub enum Outcome {
    Unlocked {
        passphrase: SecretString,
        descriptor: WalletDescriptor,
    },
}

/// Modal state shown when load_descriptor surfaces `KeyringMissingEntry`.
/// Carrying the file's epoch lets the view echo it back to the user so
/// they can sanity-check ("does this match my backup's vintage?").
#[derive(Debug)]
struct KeyringWarningState {
    file_epoch: u64,
}

/// Which loader the unlock task should call. The bypass loader is only
/// reachable via the security-modal "Accept" path, so wrong-password
/// users can't accidentally seed the keyring through ordinary unlock.
#[derive(Debug, Clone, Copy)]
enum LoadMode {
    Strict,
    AcceptingKeyringReset,
}

/// Map a `WalletError` from the loader into the unlock screen's
/// dispatch-friendly error enum.
fn classify(e: WalletError) -> UnlockError {
    match e {
        // Encryption is the variant the wallet store uses for
        // wrong-password (the AEAD auth-check fails to decrypt). Any
        // other Encryption error is rare and bug-shaped — for the
        // unlock screen, "looks like a wrong password" is a fine
        // heuristic since the user can re-try and a real corruption
        // would surface persistently.
        WalletError::Encryption(_) => UnlockError::WrongPassword,
        WalletError::KeyringMissingEntry { file_epoch } => {
            UnlockError::KeyringMissingEntry { file_epoch }
        }
        other => UnlockError::Other(other.to_string()),
    }
}

#[derive(Debug, Default)]
pub struct UnlockScreen {
    /// Live `text_input` buffer. `Zeroizing<String>` zeros the heap allocation
    /// each time the input is replaced (every keystroke) and on screen drop.
    password: Zeroizing<String>,
    error: Option<String>,
    unlocking: bool,
    keyring_warning: Option<KeyringWarningState>,
}

impl UnlockScreen {
    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Outcome>) {
        match message {
            Message::PasswordInput(p) => {
                self.password = Zeroizing::new(p);
                (Task::none(), None)
            }
            Message::PasswordSubmitted | Message::UnlockPressed => {
                (self.try_unlock(LoadMode::Strict), None)
            }
            Message::UnlockResult(Ok(descriptor)) => {
                self.unlocking = false;
                self.keyring_warning = None;
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
            Message::UnlockResult(Err(UnlockError::WrongPassword)) => {
                self.unlocking = false;
                self.error = Some("Incorrect password".to_string());
                (Task::none(), None)
            }
            Message::UnlockResult(Err(UnlockError::KeyringMissingEntry { file_epoch })) => {
                self.unlocking = false;
                // Don't pre-fill `self.error` — the modal IS the error
                // surface for this case. Leaving the inline label empty
                // keeps the post-modal screen clean if the user
                // dismisses the modal.
                self.keyring_warning = Some(KeyringWarningState { file_epoch });
                (Task::none(), None)
            }
            Message::UnlockResult(Err(UnlockError::Other(msg))) => {
                self.unlocking = false;
                self.error = Some(msg);
                (Task::none(), None)
            }
            Message::KeyringWarningAccept => (self.try_unlock(LoadMode::AcceptingKeyringReset), None),
            Message::KeyringWarningQuit => (iced::exit(), None),
            Message::KeyringWarningDismiss => {
                self.keyring_warning = None;
                (Task::none(), None)
            }
            Message::KeyringWarningModalNoop => (Task::none(), None),
        }
    }

    fn try_unlock(&mut self, mode: LoadMode) -> Task<Message> {
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
                // `load_descriptor` runs Argon2id (~250–500ms on desktop).
                // Without `spawn_blocking` it stalls the iced runtime —
                // input events queue up and the unlocking-spinner state
                // never gets to repaint. Match `save_descriptor_task` in
                // `app/mod.rs` which already handles this for the symmetric
                // save path.
                let join = tokio::task::spawn_blocking(move || {
                    let passphrase = SecretString::new(Box::from(password.as_str()));
                    match mode {
                        LoadMode::Strict => wallet::load_descriptor(&passphrase),
                        LoadMode::AcceptingKeyringReset => {
                            wallet::load_descriptor_accepting_keyring_reset(&passphrase)
                        }
                    }
                })
                .await;
                match join {
                    Ok(Ok(desc)) => Ok(desc),
                    Ok(Err(e)) => Err(classify(e)),
                    Err(join_err) => Err(UnlockError::Other(format!(
                        "unlock task panicked: {join_err}"
                    ))),
                }
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

        let base = auth_background(t, auth_card(t, 400.0, content.into()));

        match &self.keyring_warning {
            None => base,
            Some(warning) => {
                let modal = self.keyring_warning_modal(t, warning);
                stack![base, modal].into()
            }
        }
    }

    /// Security warning shown when the wallet file exists but the OS
    /// keyring has no record of it on this machine. Two actions: "Quit"
    /// (default, safe — exits the app) and "Accept" (re-runs unlock
    /// through the bypass loader, which seeds the keyring). Backdrop
    /// click dismisses the modal back to the password screen WITHOUT
    /// accepting; the next unlock attempt re-shows it.
    fn keyring_warning_modal<'a>(
        &self,
        t: KaoTheme,
        warning: &KeyringWarningState,
    ) -> Element<'a, Message> {
        let body = column![
            kao_hero(t, "(°□°)", 44.0),
            vspace(14),
            screen_title(t, "Unrecognized wallet"),
            vspace(8),
            screen_subtitle(
                t,
                "This machine has no record of the wallet file on disk. \
                 Accept only if you are restoring from your own backup. \
                 If you didn't expect to see this, choose Quit.",
            ),
            vspace(10),
            container(
                text(format!("File epoch: {}", warning.file_epoch))
                    .size(11)
                    .font(mono())
                    .color(t.sub),
            )
            .width(Length::Fill)
            .center_x(Length::Fill),
            vspace(20),
            // "Quit" is primary (filled) so it's the visually obvious
            // default. "Accept" is secondary so the destructive choice
            // requires deliberate aim — matches the "safe default is
            // quit" policy.
            primary_button(t, "Quit", true).on_press(Message::KeyringWarningQuit),
            vspace(10),
            secondary_button(t, "Accept and unlock anyway")
                .on_press(Message::KeyringWarningAccept),
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        modal_wrapper(
            t,
            440.0,
            // No animation: a security prompt that's still fading in
            // invites accidental clicks. Render at full progress so the
            // surface is fully opaque from frame 1.
            1.0,
            Message::KeyringWarningDismiss,
            Message::KeyringWarningModalNoop,
            body.into(),
        )
    }
}
