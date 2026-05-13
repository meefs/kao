//! Pick the third-party indexer (transaction history + unverified balances)
//! used by the wallet. Shown right after `select_rpc` during fresh setup.
//!
//! UI: a single screen with five cards. Clicking Alchemy/dRPC/Blockscout/
//! Etherscan expands the chosen card inline to show its input fields plus
//! a submit button — the other cards stay collapsed. Clicking "No Indexer"
//! (or Alchemy/dRPC when their key can be reused from the RPC URL) emits an
//! outcome immediately, no expansion needed.

use iced::border::Radius;
use iced::keyboard;
use iced::widget::operation::focus as focus_widget;
use iced::widget::{Column, Space, column, container, mouse_area, row, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Subscription, Task};

use crate::settings;
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{
    auth_background, auth_card, black, bold, error_text, hint_pill, kao_hero, link_button, mono,
    primary_button, screen_subtitle, screen_title, text_input_style, vspace,
};

pub const ALCHEMY_KEY_INPUT_ID: &str = "indexer_alchemy_key_input";
pub const DRPC_KEY_INPUT_ID: &str = "indexer_drpc_key_input";
pub const BLOCKSCOUT_URL_INPUT_ID: &str = "indexer_blockscout_url_input";
pub const ETHERSCAN_KEY_INPUT_ID: &str = "indexer_etherscan_key_input";

#[derive(Debug, Clone)]
pub enum Message {
    PickAlchemy,
    PickDrpc,
    PickBlockscout,
    PickEtherscan,
    PickNone,
    AlchemyKeyInput(String),
    DrpcKeyInput(String),
    BlockscoutUrlInput(String),
    BlockscoutKeyInput(String),
    EtherscanKeyInput(String),
    SubmitCurrent,
    Back,
    KeyboardEvent(keyboard::Event),
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// Use Alchemy with this API key.
    Alchemy { api_key: String },
    /// Use dRPC's Wallet API with this API key.
    Drpc { api_key: String },
    /// Use Blockscout. `base_url`/`api_key` are `None` when the user left
    /// the field blank — the indexer falls back to its built-in defaults.
    Blockscout {
        base_url: Option<String>,
        api_key: Option<String>,
    },
    /// Use Etherscan with this API key.
    Etherscan { api_key: String },
    /// Disable the indexer entirely.
    NoIndexer,
    /// User pressed Esc with nothing expanded — step back to the RPC picker.
    Back,
}

/// Which card (if any) is currently expanded, showing its input form.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
enum Expanded {
    #[default]
    None,
    Alchemy,
    Drpc,
    Blockscout,
    Etherscan,
}

#[derive(Debug, Default)]
pub struct SelectIndexerScreen {
    expanded: Expanded,
    /// Alchemy key extracted from the user's RPC URL, if their RPC is an
    /// Alchemy endpoint. Drives the "Reuse key from your RPC" affordance
    /// and short-circuits the Alchemy card so picking it emits the outcome
    /// without needing to expand for an input.
    alchemy_key_from_rpc: Option<String>,
    alchemy_key_input: String,
    /// dRPC key extracted from the user's RPC URL when it points at
    /// `lb.drpc.live/{chain}/{key}`. Same short-circuit as Alchemy:
    /// picking the card with a known key emits an outcome immediately.
    drpc_key_from_rpc: Option<String>,
    drpc_key_input: String,
    blockscout_url_input: String,
    blockscout_key_input: String,
    etherscan_key_input: String,
    error: Option<String>,
}

impl SelectIndexerScreen {
    /// Build the screen, parsing the chosen RPC URL for an Alchemy key so
    /// option 1 can offer to reuse it. Pass `None` if no custom RPC was
    /// set (defaults are in use).
    pub fn new(rpc_url: Option<&str>) -> Self {
        let alchemy_key_from_rpc = rpc_url.and_then(extract_alchemy_key);
        let drpc_key_from_rpc = rpc_url.and_then(extract_drpc_key);
        Self {
            expanded: Expanded::default(),
            alchemy_key_input: alchemy_key_from_rpc.clone().unwrap_or_default(),
            alchemy_key_from_rpc,
            drpc_key_input: drpc_key_from_rpc.clone().unwrap_or_default(),
            drpc_key_from_rpc,
            blockscout_url_input: String::new(),
            blockscout_key_input: String::new(),
            etherscan_key_input: String::new(),
            error: None,
        }
    }

    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Outcome>) {
        match message {
            Message::PickAlchemy => self.toggle_alchemy(),
            Message::PickDrpc => self.toggle_drpc(),
            Message::PickBlockscout => self.toggle(Expanded::Blockscout, BLOCKSCOUT_URL_INPUT_ID),
            Message::PickEtherscan => self.toggle(Expanded::Etherscan, ETHERSCAN_KEY_INPUT_ID),
            Message::PickNone => {
                self.expanded = Expanded::None;
                self.error = None;
                (Task::none(), Some(Outcome::NoIndexer))
            }
            Message::AlchemyKeyInput(s) => {
                self.alchemy_key_input = s;
                (Task::none(), None)
            }
            Message::DrpcKeyInput(s) => {
                self.drpc_key_input = s;
                (Task::none(), None)
            }
            Message::BlockscoutUrlInput(s) => {
                self.blockscout_url_input = s;
                (Task::none(), None)
            }
            Message::BlockscoutKeyInput(s) => {
                self.blockscout_key_input = s;
                (Task::none(), None)
            }
            Message::EtherscanKeyInput(s) => {
                self.etherscan_key_input = s;
                (Task::none(), None)
            }
            Message::SubmitCurrent => (Task::none(), self.try_submit_current()),
            Message::Back => {
                self.expanded = Expanded::None;
                self.error = None;
                (Task::none(), Some(Outcome::Back))
            }
            Message::KeyboardEvent(keyboard::Event::KeyPressed { key, .. }) => self.handle_key(key),
            Message::KeyboardEvent(_) => (Task::none(), None),
        }
    }

    fn toggle(
        &mut self,
        target: Expanded,
        focus_id: &'static str,
    ) -> (Task<Message>, Option<Outcome>) {
        if self.expanded == target {
            // Re-clicking the open card collapses it.
            self.expanded = Expanded::None;
            self.error = None;
            (Task::none(), None)
        } else {
            self.expanded = target;
            self.error = None;
            (focus_widget(focus_id), None)
        }
    }

    fn toggle_alchemy(&mut self) -> (Task<Message>, Option<Outcome>) {
        // RPC carried an Alchemy key — skip expansion entirely and emit.
        // The whole point of detecting the key is keeping the common case
        // friction-free.
        if let Some(key) = self.alchemy_key_from_rpc.clone() {
            self.expanded = Expanded::None;
            self.error = None;
            return (Task::none(), Some(Outcome::Alchemy { api_key: key }));
        }
        self.toggle(Expanded::Alchemy, ALCHEMY_KEY_INPUT_ID)
    }

    fn toggle_drpc(&mut self) -> (Task<Message>, Option<Outcome>) {
        if let Some(key) = self.drpc_key_from_rpc.clone() {
            self.expanded = Expanded::None;
            self.error = None;
            return (Task::none(), Some(Outcome::Drpc { api_key: key }));
        }
        self.toggle(Expanded::Drpc, DRPC_KEY_INPUT_ID)
    }

    fn handle_key(&mut self, key: keyboard::Key) -> (Task<Message>, Option<Outcome>) {
        match (&self.expanded, &key) {
            (_, keyboard::Key::Character(c)) if c.as_str() == "1" => self.toggle_alchemy(),
            (_, keyboard::Key::Character(c)) if c.as_str() == "2" => self.toggle_drpc(),
            (_, keyboard::Key::Character(c)) if c.as_str() == "3" => {
                self.toggle(Expanded::Blockscout, BLOCKSCOUT_URL_INPUT_ID)
            }
            (_, keyboard::Key::Character(c)) if c.as_str() == "4" => {
                self.toggle(Expanded::Etherscan, ETHERSCAN_KEY_INPUT_ID)
            }
            (_, keyboard::Key::Character(c)) if c.as_str() == "5" => {
                self.expanded = Expanded::None;
                self.error = None;
                (Task::none(), Some(Outcome::NoIndexer))
            }
            (
                Expanded::Alchemy | Expanded::Drpc | Expanded::Blockscout | Expanded::Etherscan,
                keyboard::Key::Named(n),
            ) if *n == keyboard::key::Named::Escape => {
                self.expanded = Expanded::None;
                self.error = None;
                (Task::none(), None)
            }
            (Expanded::None, keyboard::Key::Named(n)) if *n == keyboard::key::Named::Escape => {
                self.error = None;
                (Task::none(), Some(Outcome::Back))
            }
            _ => (Task::none(), None),
        }
    }

    fn try_submit_current(&mut self) -> Option<Outcome> {
        match self.expanded {
            Expanded::None => None,
            Expanded::Alchemy => {
                let key = self.alchemy_key_input.trim().to_string();
                if key.is_empty() {
                    self.error = Some("Please enter your Alchemy API key.".into());
                    return None;
                }
                self.error = None;
                Some(Outcome::Alchemy { api_key: key })
            }
            Expanded::Drpc => {
                let key = self.drpc_key_input.trim().to_string();
                if key.is_empty() {
                    self.error = Some("Please enter your dRPC API key.".into());
                    return None;
                }
                self.error = None;
                Some(Outcome::Drpc { api_key: key })
            }
            Expanded::Blockscout => {
                let url_raw = self.blockscout_url_input.trim();
                let key_raw = self.blockscout_key_input.trim();
                let base_url = if url_raw.is_empty() {
                    None
                } else {
                    match url::Url::parse(url_raw) {
                        Ok(u) if u.scheme() == "https" => Some(url_raw.to_string()),
                        _ => {
                            self.error = Some("Custom URL must start with https://".into());
                            return None;
                        }
                    }
                };
                let api_key = if key_raw.is_empty() {
                    None
                } else {
                    Some(key_raw.to_string())
                };
                self.error = None;
                Some(Outcome::Blockscout { base_url, api_key })
            }
            Expanded::Etherscan => {
                let key = self.etherscan_key_input.trim().to_string();
                if key.is_empty() {
                    self.error = Some("Please enter your Etherscan API key.".into());
                    return None;
                }
                self.error = None;
                Some(Outcome::Etherscan { api_key: key })
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let t = KaoTheme::for_kind(settings::theme());

        // Per-card body: only the expanded card renders one. `None` keeps
        // the card showing just its header.
        let alchemy_body = match self.expanded {
            Expanded::Alchemy if self.alchemy_key_from_rpc.is_none() => Some(self.alchemy_body(t)),
            _ => None,
        };
        let drpc_body = match self.expanded {
            Expanded::Drpc if self.drpc_key_from_rpc.is_none() => Some(self.drpc_body(t)),
            _ => None,
        };
        let blockscout_body =
            (self.expanded == Expanded::Blockscout).then(|| self.blockscout_body(t));
        let etherscan_body = (self.expanded == Expanded::Etherscan).then(|| self.etherscan_body(t));

        let alchemy_sub: &str = if self.alchemy_key_from_rpc.is_some() {
            "Reuse the API key from your RPC"
        } else {
            "Fast & accurate · Requires API key"
        };
        let drpc_sub: &str = if self.drpc_key_from_rpc.is_some() {
            "Reuse the API key from your RPC · Wallet API needs a paid plan"
        } else {
            "Decentralized · Wallet API needs a paid plan"
        };

        let alchemy_card = self.card(
            t,
            t.ab1,
            t.a1,
            "1",
            "Alchemy",
            alchemy_sub,
            self.expanded == Expanded::Alchemy && self.alchemy_key_from_rpc.is_none(),
            alchemy_body,
            Message::PickAlchemy,
        );
        let drpc_card = self.card(
            t,
            t.ab2,
            t.a2,
            "2",
            "dRPC",
            drpc_sub,
            self.expanded == Expanded::Drpc && self.drpc_key_from_rpc.is_none(),
            drpc_body,
            Message::PickDrpc,
        );
        let blockscout_card = self.card(
            t,
            t.ab1,
            t.a1,
            "3",
            "Blockscout",
            "Public, no key needed · Custom URL supported",
            self.expanded == Expanded::Blockscout,
            blockscout_body,
            Message::PickBlockscout,
        );
        let etherscan_card = self.card(
            t,
            t.ab2,
            t.a2,
            "4",
            "Etherscan",
            "Requires Pro API key (~$50/month)",
            self.expanded == Expanded::Etherscan,
            etherscan_body,
            Message::PickEtherscan,
        );
        let none_card = self.card(
            t,
            t.ab1,
            t.a1,
            "5",
            "No Indexer",
            "Slower · history limited to txs sent from Kao",
            false,
            None,
            Message::PickNone,
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
                Space::new().width(8),
                text("pick a source").size(11).color(t.sub).font(mono()),
            ]
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        let mut content = column![
            kao_hero(t, "(¬‿¬ )", 52.0),
            vspace(10),
            screen_title(t, "Choose Your Indexer"),
            vspace(6),
            screen_subtitle(t, "Where should Kao fetch transaction history from?"),
            vspace(22),
            alchemy_card,
            vspace(10),
            drpc_card,
            vspace(10),
            blockscout_card,
            vspace(10),
            etherscan_card,
            vspace(10),
            none_card,
            vspace(16),
            hint,
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        if let Some(e) = &self.error {
            content = content.push(vspace(10)).push(error_text(t, e));
        }

        let card = auth_card(t, 480.0, content.into());

        let back_bar = container(link_button(t, "← Back").on_press(Message::Back))
            .padding(Padding::from([12, 14]))
            .width(Length::Fill);

        let layout = column![back_bar, card]
            .width(Length::Fill)
            .height(Length::Fill);

        auth_background(t, layout.into())
    }

    // ── Per-card bodies (rendered inside the expanded card) ──────────────

    fn alchemy_body(&self, t: KaoTheme) -> Element<'_, Message> {
        let key_input = text_input("Your Alchemy API key", &self.alchemy_key_input)
            .id(ALCHEMY_KEY_INPUT_ID)
            .on_input(Message::AlchemyKeyInput)
            .on_submit(Message::SubmitCurrent)
            .padding(Padding::from([10, 12]))
            .size(13)
            .font(mono())
            .style(move |_theme, status| text_input_style(t, status));
        let has_key = !self.alchemy_key_input.trim().is_empty();
        let mut submit = primary_button(t, "Use Alchemy →", has_key);
        if has_key {
            submit = submit.on_press(Message::SubmitCurrent);
        }
        column![key_input, vspace(10), submit]
            .width(Length::Fill)
            .into()
    }

    fn drpc_body(&self, t: KaoTheme) -> Element<'_, Message> {
        let key_input = text_input("Your dRPC API key", &self.drpc_key_input)
            .id(DRPC_KEY_INPUT_ID)
            .on_input(Message::DrpcKeyInput)
            .on_submit(Message::SubmitCurrent)
            .padding(Padding::from([10, 12]))
            .size(13)
            .font(mono())
            .style(move |_theme, status| text_input_style(t, status));
        let has_key = !self.drpc_key_input.trim().is_empty();
        let mut submit = primary_button(t, "Use dRPC →", has_key);
        if has_key {
            submit = submit.on_press(Message::SubmitCurrent);
        }
        column![key_input, vspace(10), submit]
            .width(Length::Fill)
            .into()
    }

    fn blockscout_body(&self, t: KaoTheme) -> Element<'_, Message> {
        let url_input = text_input(
            "https://eth.blockscout.com (optional)",
            &self.blockscout_url_input,
        )
        .id(BLOCKSCOUT_URL_INPUT_ID)
        .on_input(Message::BlockscoutUrlInput)
        .on_submit(Message::SubmitCurrent)
        .padding(Padding::from([10, 12]))
        .size(13)
        .font(mono())
        .style(move |_theme, status| text_input_style(t, status));
        let key_input = text_input(
            "API key (optional, for higher rate limits)",
            &self.blockscout_key_input,
        )
        .on_input(Message::BlockscoutKeyInput)
        .on_submit(Message::SubmitCurrent)
        .padding(Padding::from([10, 12]))
        .size(13)
        .font(mono())
        .style(move |_theme, status| text_input_style(t, status));
        let submit = primary_button(t, "Use Blockscout →", true).on_press(Message::SubmitCurrent);
        column![url_input, vspace(8), key_input, vspace(10), submit]
            .width(Length::Fill)
            .into()
    }

    fn etherscan_body(&self, t: KaoTheme) -> Element<'_, Message> {
        let key_input = text_input("Your Etherscan API key", &self.etherscan_key_input)
            .id(ETHERSCAN_KEY_INPUT_ID)
            .on_input(Message::EtherscanKeyInput)
            .on_submit(Message::SubmitCurrent)
            .padding(Padding::from([10, 12]))
            .size(13)
            .font(mono())
            .style(move |_theme, status| text_input_style(t, status));
        let warning =
            text("Etherscan's address-token-balance endpoint requires the Pro tier (~$50/month).")
                .size(11)
                .color(t.sub);
        let has_key = !self.etherscan_key_input.trim().is_empty();
        let mut submit = primary_button(t, "Use Etherscan →", has_key);
        if has_key {
            submit = submit.on_press(Message::SubmitCurrent);
        }
        column![key_input, vspace(8), warning, vspace(10), submit]
            .width(Length::Fill)
            .into()
    }

    // ── Card renderer ────────────────────────────────────────────────────

    /// Render a single card. The header is always clickable (mouse_area)
    /// and toggles the card's expanded state. The optional body element
    /// is rendered below the header — outside the click target so its
    /// own widgets (text inputs, submit button) receive their events.
    #[allow(clippy::too_many_arguments)]
    fn card<'a>(
        &self,
        t: KaoTheme,
        bg: Color,
        accent: Color,
        number: &'a str,
        label: &'a str,
        sub: &'a str,
        is_expanded: bool,
        body: Option<Element<'a, Message>>,
        on_header_click: Message,
    ) -> Element<'a, Message> {
        let arrow = if is_expanded { "↓" } else { "→" };
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

        let header_row = row![
            container(info).width(Length::Fill),
            text(arrow).size(18).color(accent).font(bold()),
        ]
        .align_y(Alignment::Center)
        .width(Length::Fill);

        let header = mouse_area(header_row)
            .on_press(on_header_click)
            .interaction(iced::mouse::Interaction::Pointer);

        let mut card_col: Column<'a, Message> = column![header].width(Length::Fill);
        if let Some(b) = body {
            card_col = card_col.push(vspace(12)).push(b);
        }

        container(card_col)
            .padding(Padding::from([14, 16]))
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(bg)),
                border: Border {
                    color: with_alpha(accent, if is_expanded { 0.55 } else { 0.25 }),
                    width: if is_expanded { 2.0 } else { 1.5 },
                    radius: Radius::from(15),
                },
                text_color: Some(t.text),
                ..container::Style::default()
            })
            .into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        keyboard::listen().map(Message::KeyboardEvent)
    }
}

/// Pull an Alchemy API key out of an RPC URL like
/// `https://eth-mainnet.g.alchemy.com/v2/{key}`. Returns `None` for any
/// non-Alchemy host or a URL whose path doesn't carry a key.
fn extract_alchemy_key(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if !host.ends_with(".g.alchemy.com") {
        return None;
    }
    let mut segs = parsed.path_segments()?;
    if segs.next()? != "v2" {
        return None;
    }
    let key = segs.next()?;
    if key.is_empty() {
        return None;
    }
    Some(key.to_string())
}

/// Pull a dRPC API key out of an RPC URL like
/// `https://lb.drpc.live/{chain}/{key}`. The same key works for both
/// the standard JSON-RPC and the Wallet API endpoints.
fn extract_drpc_key(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "lb.drpc.live" {
        return None;
    }
    let mut segs = parsed.path_segments()?;
    let _chain = segs.next()?;
    let key = segs.next()?;
    if key.is_empty() {
        return None;
    }
    Some(key.to_string())
}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;

    #[test]
    fn extract_alchemy_key_from_v2_url() {
        assert_eq!(
            extract_alchemy_key("https://eth-mainnet.g.alchemy.com/v2/abc123"),
            Some("abc123".to_string()),
        );
        assert_eq!(
            extract_alchemy_key("https://opt-mainnet.g.alchemy.com/v2/XYZ-_."),
            Some("XYZ-_.".to_string()),
        );
    }

    #[test]
    fn extract_alchemy_key_rejects_other_hosts() {
        assert_eq!(extract_alchemy_key("https://eth.llamarpc.com"), None);
        assert_eq!(
            extract_alchemy_key("https://eth-mainnet.example.com/v2/abc"),
            None,
        );
        assert_eq!(extract_alchemy_key("not-a-url"), None);
        assert_eq!(
            extract_alchemy_key("https://eth-mainnet.g.alchemy.com/"),
            None,
        );
    }

    #[test]
    fn pick_none_emits_outcome() {
        let mut s = SelectIndexerScreen::new(None);
        let (_, outcome) = s.update(Message::PickNone);
        assert!(matches!(outcome, Some(Outcome::NoIndexer)));
        assert_eq!(s.expanded, Expanded::None);
    }

    #[test]
    fn pick_alchemy_with_rpc_key_skips_expansion() {
        let mut s = SelectIndexerScreen::new(Some("https://eth-mainnet.g.alchemy.com/v2/MYKEY"));
        let (_, outcome) = s.update(Message::PickAlchemy);
        match outcome {
            Some(Outcome::Alchemy { api_key }) => assert_eq!(api_key, "MYKEY"),
            other => panic!("expected immediate Alchemy outcome, got {other:?}"),
        }
        assert_eq!(s.expanded, Expanded::None);
    }

    #[test]
    fn pick_alchemy_without_rpc_key_expands_card() {
        let mut s = SelectIndexerScreen::new(None);
        let (_, outcome) = s.update(Message::PickAlchemy);
        assert!(outcome.is_none());
        assert_eq!(s.expanded, Expanded::Alchemy);
    }

    #[test]
    fn re_clicking_expanded_card_collapses_it() {
        let mut s = SelectIndexerScreen::new(None);
        s.update(Message::PickBlockscout);
        assert_eq!(s.expanded, Expanded::Blockscout);
        s.update(Message::PickBlockscout);
        assert_eq!(s.expanded, Expanded::None);
    }

    #[test]
    fn switching_cards_replaces_expansion() {
        let mut s = SelectIndexerScreen::new(None);
        s.update(Message::PickEtherscan);
        assert_eq!(s.expanded, Expanded::Etherscan);
        s.update(Message::PickBlockscout);
        assert_eq!(s.expanded, Expanded::Blockscout);
    }

    #[test]
    fn submitting_empty_alchemy_key_errors() {
        let mut s = SelectIndexerScreen::new(None);
        s.update(Message::PickAlchemy);
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.is_some());
    }

    #[test]
    fn submitting_alchemy_key_emits_outcome() {
        let mut s = SelectIndexerScreen::new(None);
        s.update(Message::PickAlchemy);
        s.update(Message::AlchemyKeyInput("  somekey  ".into()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Alchemy { api_key }) => assert_eq!(api_key, "somekey"),
            other => panic!("expected Alchemy outcome, got {other:?}"),
        }
    }

    #[test]
    fn blockscout_with_blank_inputs_emits_defaults() {
        let mut s = SelectIndexerScreen::new(None);
        s.update(Message::PickBlockscout);
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Blockscout { base_url, api_key }) => {
                assert!(base_url.is_none());
                assert!(api_key.is_none());
            }
            other => panic!("expected Blockscout outcome, got {other:?}"),
        }
    }

    #[test]
    fn blockscout_rejects_non_https_url() {
        let mut s = SelectIndexerScreen::new(None);
        s.update(Message::PickBlockscout);
        s.update(Message::BlockscoutUrlInput("http://example.com".into()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.as_deref().is_some_and(|e| e.contains("https")));
    }

    #[test]
    fn blockscout_https_url_passes_through() {
        let mut s = SelectIndexerScreen::new(None);
        s.update(Message::PickBlockscout);
        s.update(Message::BlockscoutUrlInput(
            "https://base.blockscout.com".into(),
        ));
        s.update(Message::BlockscoutKeyInput("KEY".into()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Blockscout { base_url, api_key }) => {
                assert_eq!(base_url.as_deref(), Some("https://base.blockscout.com"));
                assert_eq!(api_key.as_deref(), Some("KEY"));
            }
            other => panic!("expected Blockscout outcome, got {other:?}"),
        }
    }

    #[test]
    fn etherscan_requires_key() {
        let mut s = SelectIndexerScreen::new(None);
        s.update(Message::PickEtherscan);
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.is_some());
    }

    #[test]
    fn extract_drpc_key_from_rpc_url() {
        assert_eq!(
            extract_drpc_key("https://lb.drpc.live/ethereum/abc123"),
            Some("abc123".to_string()),
        );
        assert_eq!(
            extract_drpc_key("https://lb.drpc.live/base/XYZ-_."),
            Some("XYZ-_.".to_string()),
        );
    }

    #[test]
    fn extract_drpc_key_rejects_other_hosts() {
        assert_eq!(extract_drpc_key("https://eth.drpc.org/"), None);
        assert_eq!(extract_drpc_key("https://lb.drpc.live/ethereum/"), None,);
        assert_eq!(extract_drpc_key("not-a-url"), None);
    }

    #[test]
    fn pick_drpc_with_rpc_key_skips_expansion() {
        let mut s = SelectIndexerScreen::new(Some("https://lb.drpc.live/ethereum/MYKEY"));
        let (_, outcome) = s.update(Message::PickDrpc);
        match outcome {
            Some(Outcome::Drpc { api_key }) => assert_eq!(api_key, "MYKEY"),
            other => panic!("expected immediate Drpc outcome, got {other:?}"),
        }
        assert_eq!(s.expanded, Expanded::None);
    }

    #[test]
    fn pick_drpc_without_rpc_key_expands_card() {
        let mut s = SelectIndexerScreen::new(None);
        let (_, outcome) = s.update(Message::PickDrpc);
        assert!(outcome.is_none());
        assert_eq!(s.expanded, Expanded::Drpc);
    }

    #[test]
    fn submitting_empty_drpc_key_errors() {
        let mut s = SelectIndexerScreen::new(None);
        s.update(Message::PickDrpc);
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.is_some());
    }

    #[test]
    fn submitting_drpc_key_emits_outcome() {
        let mut s = SelectIndexerScreen::new(None);
        s.update(Message::PickDrpc);
        s.update(Message::DrpcKeyInput("  somekey  ".into()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Drpc { api_key }) => assert_eq!(api_key, "somekey"),
            other => panic!("expected Drpc outcome, got {other:?}"),
        }
    }
}
