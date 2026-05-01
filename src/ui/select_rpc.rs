//! Pick the execution-layer RPC the wallet uses. First step of fresh setup.
//!
//! UI: a single screen with three cards. Clicking "Use Default RPC" emits an
//! outcome immediately. Clicking Alchemy or Custom expands the chosen card
//! inline to show its input field plus a submit button — the other cards
//! stay collapsed. Picking Alchemy and entering a key produces an Alchemy
//! mainnet URL; the indexer screen then auto-detects the same key off the
//! RPC, so the user only types it once.

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

pub const ALCHEMY_KEY_INPUT_ID: &str = "rpc_alchemy_key_input";
pub const CUSTOM_RPC_INPUT_ID: &str = "custom_rpc_input";

#[derive(Debug, Clone)]
pub enum Message {
    PickDefaults,
    PickAlchemy,
    PickCustom,
    AlchemyKeyInput(String),
    CustomInput(String),
    SubmitCurrent,
    Back,
    KeyboardEvent(keyboard::Event),
}

/// Outcome signals emitted by this screen to its parent.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Keep the curated default RPC list. App should not touch the rpcs setting.
    UseDefaults,
    /// Replace the RPC list with this URL. Used for both the Alchemy and
    /// Custom paths — the Alchemy path simply formats the URL for the user.
    Custom(String),
    /// User pressed Esc / "← Back" with nothing expanded — step back to
    /// the password screen.
    Back,
}

/// Which card (if any) is currently expanded, showing its input form.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
enum Expanded {
    #[default]
    None,
    Alchemy,
    Custom,
}

#[derive(Debug, Default)]
pub struct SelectRpcScreen {
    expanded: Expanded,
    alchemy_key_input: String,
    custom_input: String,
    error: Option<String>,
}

impl SelectRpcScreen {
    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Outcome>) {
        match message {
            Message::PickDefaults => {
                self.expanded = Expanded::None;
                self.error = None;
                (Task::none(), Some(Outcome::UseDefaults))
            }
            Message::PickAlchemy => self.toggle(Expanded::Alchemy, ALCHEMY_KEY_INPUT_ID),
            Message::PickCustom => self.toggle(Expanded::Custom, CUSTOM_RPC_INPUT_ID),
            Message::AlchemyKeyInput(s) => {
                self.alchemy_key_input = s;
                (Task::none(), None)
            }
            Message::CustomInput(s) => {
                self.custom_input = s;
                (Task::none(), None)
            }
            Message::SubmitCurrent => (Task::none(), self.try_submit_current()),
            Message::Back => {
                self.expanded = Expanded::None;
                self.error = None;
                (Task::none(), Some(Outcome::Back))
            }
            Message::KeyboardEvent(keyboard::Event::KeyPressed { key, .. }) => {
                self.handle_key(key)
            }
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

    fn handle_key(&mut self, key: keyboard::Key) -> (Task<Message>, Option<Outcome>) {
        match (&self.expanded, &key) {
            (_, keyboard::Key::Character(c)) if c.as_str() == "1" => {
                self.expanded = Expanded::None;
                self.error = None;
                (Task::none(), Some(Outcome::UseDefaults))
            }
            (_, keyboard::Key::Character(c)) if c.as_str() == "2" => {
                self.toggle(Expanded::Alchemy, ALCHEMY_KEY_INPUT_ID)
            }
            (_, keyboard::Key::Character(c)) if c.as_str() == "3" => {
                self.toggle(Expanded::Custom, CUSTOM_RPC_INPUT_ID)
            }
            (Expanded::Alchemy | Expanded::Custom, keyboard::Key::Named(n))
                if *n == keyboard::key::Named::Escape =>
            {
                self.expanded = Expanded::None;
                self.error = None;
                (Task::none(), None)
            }
            (Expanded::None, keyboard::Key::Named(n))
                if *n == keyboard::key::Named::Escape =>
            {
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
                let key = self.alchemy_key_input.trim();
                if key.is_empty() {
                    self.error = Some("Please enter your Alchemy API key.".into());
                    return None;
                }
                self.error = None;
                Some(Outcome::Custom(format!(
                    "https://eth-mainnet.g.alchemy.com/v2/{key}"
                )))
            }
            Expanded::Custom => {
                let trimmed = self.custom_input.trim();
                if trimmed.is_empty() {
                    self.error = Some("Please enter an RPC URL.".into());
                    return None;
                }
                let Some(url) = parse_rpc_input(trimmed) else {
                    self.error = Some("Enter an https:// URL, hostname, or IP address.".into());
                    return None;
                };
                self.error = None;
                Some(Outcome::Custom(url))
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let t = KaoTheme::for_kind(settings::theme());

        // Per-card body: only the expanded card renders one. `None` keeps
        // the card showing just its header.
        let alchemy_body = (self.expanded == Expanded::Alchemy).then(|| self.alchemy_body(t));
        let custom_body = (self.expanded == Expanded::Custom).then(|| self.custom_body(t));

        let defaults = settings::default_rpcs();
        let defaults_card = self.card(
            t,
            t.ab1,
            t.a1,
            "1",
            "Use Default RPC",
            "LlamaRPC mainnet — verified by Helios",
            defaults,
            false,
            None,
            Message::PickDefaults,
        );
        let alchemy_card = self.card(
            t,
            t.ab2,
            t.a2,
            "2",
            "Alchemy",
            "Fast & accurate · Requires API key",
            &[],
            self.expanded == Expanded::Alchemy,
            alchemy_body,
            Message::PickAlchemy,
        );
        let custom_card = self.card(
            t,
            t.ab1,
            t.a1,
            "3",
            "Custom RPC",
            "Provide your own Ethereum RPC URL",
            &[],
            self.expanded == Expanded::Custom,
            custom_body,
            Message::PickCustom,
        );

        let hint = container(
            row![
                hint_pill(t, "1"),
                Space::new().width(4),
                hint_pill(t, "2"),
                Space::new().width(4),
                hint_pill(t, "3"),
                Space::new().width(8),
                text("pick a source").size(11).color(t.sub).font(mono()),
            ]
            .align_y(Alignment::Center),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        let mut content = column![
            kao_hero(t, "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧", 52.0),
            vspace(10),
            screen_title(t, "Choose Your RPC"),
            vspace(6),
            screen_subtitle(t, "Where should Kao fetch on-chain data from?"),
            vspace(22),
            defaults_card,
            vspace(10),
            alchemy_card,
            vspace(10),
            custom_card,
            vspace(16),
            hint,
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        if let Some(e) = &self.error {
            content = content.push(vspace(10)).push(error_text(t, e));
        }

        let card = auth_card(t, 460.0, content.into());

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

    fn custom_body(&self, t: KaoTheme) -> Element<'_, Message> {
        let url_input = text_input("https://my-node.example/rpc", &self.custom_input)
            .id(CUSTOM_RPC_INPUT_ID)
            .on_input(Message::CustomInput)
            .on_submit(Message::SubmitCurrent)
            .padding(Padding::from([10, 12]))
            .size(13)
            .font(mono())
            .style(move |_theme, status| text_input_style(t, status));
        let has_url = parse_rpc_input(&self.custom_input).is_some();
        let mut submit = primary_button(t, "Use This RPC →", has_url);
        if has_url {
            submit = submit.on_press(Message::SubmitCurrent);
        }
        column![url_input, vspace(10), submit]
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
        extras: &'a [&'a str],
        is_expanded: bool,
        body: Option<Element<'a, Message>>,
        on_header_click: Message,
    ) -> Element<'a, Message> {
        let arrow = if is_expanded { "↓" } else { "→" };
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

/// Validate and normalize a custom RPC input. Accepts:
/// - an explicit `https://` URL (kept as-is),
/// - a bare hostname with optional `:port` and `/path` (wrapped as `https://`),
/// - a bare IP address with optional `:port` and `/path` (wrapped as `http://`,
///   since local nodes typically do not have TLS).
///
/// Explicit non-https schemes (`http://`, `ws://`, …) are rejected so users
/// cannot downgrade themselves; if they want plain http they must type the
/// host without a scheme and we will pick the right scheme automatically.
fn parse_rpc_input(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Only treat the input as a URL when it carries an explicit scheme;
    // `url::Url::parse` would otherwise misread "host:port/path" as
    // scheme+path and reject perfectly fine bare hostnames.
    if s.contains("://") {
        let url = url::Url::parse(s).ok()?;
        if url.scheme() != "https" {
            return None;
        }
        let host = url.host_str()?;
        if !is_plausible_host(host) {
            return None;
        }
        return Some(s.to_string());
    }
    let (host_port, _) = s.find('/').map_or((s, ""), |i| (&s[..i], &s[i..]));
    let host = match host_port.rsplit_once(':') {
        Some((host, port)) => {
            if port.parse::<u16>().is_err() {
                return None;
            }
            host
        }
        None => host_port,
    };
    if host.parse::<std::net::IpAddr>().is_ok() || host.eq_ignore_ascii_case("localhost") {
        return Some(format!("http://{s}"));
    }
    if !is_plausible_host(host) {
        return None;
    }
    Some(format!("https://{s}"))
}

/// A host is plausible if it's an IP, `localhost`, or a multi-label hostname
/// (something with at least one dot). Single-label garbage like "d" or "asdf"
/// would parse fine but never resolve to a real RPC, so we reject it up-front
/// to keep the submit button honest.
fn is_plausible_host(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s.eq_ignore_ascii_case("localhost") || s.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    if !s.contains('.') {
        return false;
    }
    is_valid_hostname(s)
}

fn is_valid_hostname(s: &str) -> bool {
    if s.is_empty() || s.len() > 253 {
        return false;
    }
    s.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;

    #[test]
    fn use_defaults_emits_outcome() {
        let mut s = SelectRpcScreen::default();
        let (_, outcome) = s.update(Message::PickDefaults);
        assert!(matches!(outcome, Some(Outcome::UseDefaults)));
        assert_eq!(s.expanded, Expanded::None);
    }

    #[test]
    fn pick_custom_expands_without_outcome() {
        let mut s = SelectRpcScreen::default();
        let (_, outcome) = s.update(Message::PickCustom);
        assert!(outcome.is_none());
        assert_eq!(s.expanded, Expanded::Custom);
    }

    #[test]
    fn pick_alchemy_expands_without_outcome() {
        let mut s = SelectRpcScreen::default();
        let (_, outcome) = s.update(Message::PickAlchemy);
        assert!(outcome.is_none());
        assert_eq!(s.expanded, Expanded::Alchemy);
    }

    #[test]
    fn re_clicking_expanded_card_collapses_it() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickAlchemy);
        assert_eq!(s.expanded, Expanded::Alchemy);
        s.update(Message::PickAlchemy);
        assert_eq!(s.expanded, Expanded::None);
    }

    #[test]
    fn switching_cards_replaces_expansion() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickAlchemy);
        assert_eq!(s.expanded, Expanded::Alchemy);
        s.update(Message::PickCustom);
        assert_eq!(s.expanded, Expanded::Custom);
    }

    #[test]
    fn empty_custom_url_errors() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.is_some());
    }

    #[test]
    fn rejects_non_http_scheme() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        s.update(Message::CustomInput("ws://my-node.example".into()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.unwrap().contains("http"));
    }

    #[test]
    fn accepts_https_url() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        s.update(Message::CustomInput("  https://my-node.example/rpc  ".into()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Custom(url)) => assert_eq!(url, "https://my-node.example/rpc"),
            other => panic!("expected Custom outcome, got {other:?}"),
        }
        assert!(s.error.is_none());
    }

    #[test]
    fn empty_alchemy_key_errors() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickAlchemy);
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.is_some());
    }

    #[test]
    fn alchemy_key_emits_mainnet_url() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickAlchemy);
        s.update(Message::AlchemyKeyInput("  abc123  ".into()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Custom(url)) => {
                assert_eq!(url, "https://eth-mainnet.g.alchemy.com/v2/abc123");
            }
            other => panic!("expected Custom outcome, got {other:?}"),
        }
        assert!(s.error.is_none());
    }

    #[test]
    fn parse_accepts_bare_ip() {
        assert_eq!(parse_rpc_input("192.168.1.5"), Some("http://192.168.1.5".into()));
    }

    #[test]
    fn parse_accepts_ip_with_port_and_path() {
        assert_eq!(
            parse_rpc_input("192.168.1.5:8545/rpc"),
            Some("http://192.168.1.5:8545/rpc".into())
        );
    }

    #[test]
    fn parse_accepts_bare_hostname() {
        assert_eq!(
            parse_rpc_input("my-node.example"),
            Some("https://my-node.example".into())
        );
    }

    #[test]
    fn parse_accepts_hostname_with_port_and_path() {
        assert_eq!(
            parse_rpc_input("my-node.example:8545/rpc"),
            Some("https://my-node.example:8545/rpc".into())
        );
    }

    #[test]
    fn parse_rejects_empty_and_invalid_port() {
        assert_eq!(parse_rpc_input(""), None);
        assert_eq!(parse_rpc_input("   "), None);
        assert_eq!(parse_rpc_input("my-node.example:abc"), None);
        assert_eq!(parse_rpc_input("--bad-label.example"), None);
    }

    #[test]
    fn parse_rejects_single_label_hosts() {
        assert_eq!(parse_rpc_input("https://d"), None);
        assert_eq!(parse_rpc_input("d"), None);
        assert_eq!(parse_rpc_input("asdf"), None);
        assert_eq!(parse_rpc_input("https://asdf"), None);
    }

    #[test]
    fn parse_accepts_localhost_as_http() {
        assert_eq!(parse_rpc_input("localhost"), Some("http://localhost".into()));
        assert_eq!(
            parse_rpc_input("localhost:8545"),
            Some("http://localhost:8545".into())
        );
    }

    #[test]
    fn parse_accepts_explicit_https_localhost() {
        assert_eq!(
            parse_rpc_input("https://localhost:8545"),
            Some("https://localhost:8545".into())
        );
    }

    #[test]
    fn parse_rejects_http_url() {
        assert_eq!(parse_rpc_input("http://my-node.example"), None);
    }

    #[test]
    fn accepts_ip_via_submit() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        s.update(Message::CustomInput("10.0.0.2:8545".into()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Custom(url)) => assert_eq!(url, "http://10.0.0.2:8545"),
            other => panic!("expected Custom outcome, got {other:?}"),
        }
        assert!(s.error.is_none());
    }
}
