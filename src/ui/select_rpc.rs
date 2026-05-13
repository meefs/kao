//! Pick the execution-layer RPC the wallet uses. First step of fresh setup.
//!
//! UI: a single screen with four cards. Clicking "Use Default RPC" emits
//! an outcome immediately. Clicking Alchemy, dRPC, or Custom RPCs expands
//! the chosen card inline to show its input form. The Custom RPCs card
//! holds two parallel blocks of per-chain inputs — execution and consensus
//! — with one labeled row per chain (Mainnet / Base / Optimism). All URL
//! fields come pre-typed with sensible defaults so the user can submit
//! straight away or tweak any single chain. Picking Alchemy or dRPC and
//! entering a key produces per-chain URLs for **all three chains**; the
//! indexer screen then auto-detects the same key off the Mainnet RPC, so
//! the user only types it once.
//!
//! `Outcome::Custom` carries a full `PerChain` of exec + consensus URLs.
//! The app coordinator persists each chain's slot whose value is
//! non-empty; empty slots leave that chain unconfigured (no L2 fetch
//! attempted in the dashboard).

use iced::border::Radius;
use iced::keyboard;
use iced::widget::operation::focus as focus_widget;
use iced::widget::{Column, Space, column, container, mouse_area, row, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Subscription, Task};

use crate::chain::{Chain, PerChain};
use crate::settings;
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{
    auth_background, auth_card, black, bold, error_text, hint_pill, kao_hero, link_button, mono,
    primary_button, screen_subtitle, screen_title, text_input_style, vspace,
};

pub const ALCHEMY_KEY_INPUT_ID: &str = "rpc_alchemy_key_input";
pub const DRPC_KEY_INPUT_ID: &str = "rpc_drpc_key_input";
/// Focus target when expanding the Custom card. Maps to the Mainnet
/// execution-RPC input — the only required field on that card.
pub const CUSTOM_RPC_INPUT_ID: &str = "custom_rpc_input";

#[derive(Debug, Clone)]
pub enum Message {
    PickDefaults,
    PickAlchemy,
    PickDrpc,
    PickCustom,
    AlchemyKeyInput(String),
    DrpcKeyInput(String),
    CustomExecInput(Chain, String),
    CustomConsensusInput(Chain, String),
    SubmitCurrent,
    Back,
    KeyboardEvent(keyboard::Event),
}

/// Outcome signals emitted by this screen to its parent.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Keep the curated default RPC list. App should not touch the
    /// rpcs setting.
    UseDefaults,
    /// Replace the per-chain RPC lists with these URLs. The app
    /// coordinator persists each chain's slot whose string is non-empty
    /// and leaves the rest unset. Used for both the Alchemy and Custom
    /// paths — the Alchemy path generates URLs for all three chains
    /// from a single key (and seeds the consensus slots from each
    /// chain's `default_consensus_url`), the Custom path uses whatever
    /// the user typed.
    Custom {
        exec: PerChain<String>,
        consensus: PerChain<String>,
    },
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
    Drpc,
    Custom,
}

#[derive(Debug)]
pub struct SelectRpcScreen {
    expanded: Expanded,
    alchemy_key_input: String,
    drpc_key_input: String,
    custom_exec: PerChain<String>,
    custom_consensus: PerChain<String>,
    error: Option<String>,
}

impl Default for SelectRpcScreen {
    fn default() -> Self {
        // Pre-type sensible URLs so the Custom card lands ready to submit;
        // the user can edit any single chain or wipe a row to opt out.
        let mut custom_exec = PerChain::<String>::default();
        let mut custom_consensus = PerChain::<String>::default();
        for chain in Chain::ALL {
            custom_exec.set(chain, chain.default_exec_url().to_string());
            custom_consensus.set(chain, chain.default_consensus_url().to_string());
        }
        Self {
            expanded: Expanded::None,
            alchemy_key_input: String::new(),
            drpc_key_input: String::new(),
            custom_exec,
            custom_consensus,
            error: None,
        }
    }
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
            Message::PickDrpc => self.toggle(Expanded::Drpc, DRPC_KEY_INPUT_ID),
            Message::PickCustom => self.toggle(Expanded::Custom, CUSTOM_RPC_INPUT_ID),
            Message::AlchemyKeyInput(s) => {
                self.alchemy_key_input = s;
                (Task::none(), None)
            }
            Message::DrpcKeyInput(s) => {
                self.drpc_key_input = s;
                (Task::none(), None)
            }
            Message::CustomExecInput(chain, s) => {
                self.custom_exec.set(chain, s);
                (Task::none(), None)
            }
            Message::CustomConsensusInput(chain, s) => {
                self.custom_consensus.set(chain, s);
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
                self.toggle(Expanded::Drpc, DRPC_KEY_INPUT_ID)
            }
            (_, keyboard::Key::Character(c)) if c.as_str() == "4" => {
                self.toggle(Expanded::Custom, CUSTOM_RPC_INPUT_ID)
            }
            (Expanded::Alchemy | Expanded::Drpc | Expanded::Custom, keyboard::Key::Named(n))
                if *n == keyboard::key::Named::Escape =>
            {
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
                let key = self.alchemy_key_input.trim();
                if key.is_empty() {
                    self.error = Some("Please enter your Alchemy API key.".into());
                    return None;
                }
                self.error = None;
                Some(Outcome::Custom {
                    exec: alchemy_exec_urls(key),
                    consensus: default_consensus_urls(),
                })
            }
            Expanded::Drpc => {
                let key = self.drpc_key_input.trim();
                if key.is_empty() {
                    self.error = Some("Please enter your dRPC API key.".into());
                    return None;
                }
                self.error = None;
                Some(Outcome::Custom {
                    exec: drpc_exec_urls(key),
                    consensus: default_consensus_urls(),
                })
            }
            Expanded::Custom => {
                let mainnet_raw = self.custom_exec.get(Chain::Mainnet).trim();
                if mainnet_raw.is_empty() {
                    self.error = Some("Please enter a Mainnet RPC URL.".into());
                    return None;
                }
                let Some(mainnet_url) = parse_rpc_input(mainnet_raw) else {
                    self.error =
                        Some("Mainnet RPC: enter an https:// URL, hostname, or IP address.".into());
                    return None;
                };
                // L2 exec inputs are optional — only validate non-empty
                // entries. Normalize each (parse_rpc_input wraps bare
                // hosts/IPs) so the persisted setting is always a full URL.
                let mut exec = PerChain::<String>::default();
                exec.set(Chain::Mainnet, mainnet_url);
                for chain in [Chain::Base, Chain::Optimism] {
                    let s = self.custom_exec.get(chain).trim();
                    if s.is_empty() {
                        continue;
                    }
                    let Some(url) = parse_rpc_input(s) else {
                        self.error = Some(format!(
                            "{}: enter an https:// URL, hostname, or IP address.",
                            chain.label()
                        ));
                        return None;
                    };
                    exec.set(chain, url);
                }
                // Per-chain consensus URLs are optional; HTTPS-only when
                // set because a plain-HTTP LC bootstrap can be MITM'd
                // before the consensus signatures get verified.
                let mut consensus = PerChain::<String>::default();
                for chain in Chain::ALL {
                    let s = self.custom_consensus.get(chain).trim();
                    if s.is_empty() {
                        continue;
                    }
                    if !is_https_url(s) {
                        self.error = Some(format!(
                            "{} consensus: enter an https:// URL.",
                            chain.label()
                        ));
                        return None;
                    }
                    consensus.set(chain, s.to_string());
                }
                self.error = None;
                Some(Outcome::Custom { exec, consensus })
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let t = KaoTheme::for_kind(settings::theme());

        // Per-card body: only the expanded card renders one. `None` keeps
        // the card showing just its header.
        let alchemy_body = (self.expanded == Expanded::Alchemy).then(|| self.alchemy_body(t));
        let drpc_body = (self.expanded == Expanded::Drpc).then(|| self.drpc_body(t));
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
        let drpc_card = self.card(
            t,
            t.ab1,
            t.a1,
            "3",
            "dRPC",
            "Decentralized · Requires API key",
            &[],
            self.expanded == Expanded::Drpc,
            drpc_body,
            Message::PickDrpc,
        );
        let custom_card = self.card(
            t,
            t.ab2,
            t.a2,
            "4",
            "Custom RPCs",
            "Bring your own Mainnet / Base / Optimism endpoints",
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
                Space::new().width(4),
                hint_pill(t, "4"),
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
            drpc_card,
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

    fn custom_body(&self, t: KaoTheme) -> Element<'_, Message> {
        // Execution RPCs — one labeled input per chain, pre-typed with the
        // chain's default URL so the form is submit-ready out of the box.
        let mut exec_block: Column<'_, Message> = column![subsection_label(t, "Execution RPCs")]
            .spacing(6)
            .width(Length::Fill);
        for chain in Chain::ALL {
            let value = self.custom_exec.get(chain);
            let mut input = text_input("https://…", value)
                .on_input(move |s| Message::CustomExecInput(chain, s))
                .on_submit(Message::SubmitCurrent)
                .padding(Padding::from([10, 12]))
                .size(13)
                .font(mono())
                .style(move |_theme, status| text_input_style(t, status));
            // Mainnet is the field we focus when the card opens.
            if matches!(chain, Chain::Mainnet) {
                input = input.id(CUSTOM_RPC_INPUT_ID);
            }
            exec_block = exec_block.push(labeled_row(t, chain.label(), input.into()));
        }

        // Consensus RPCs — same shape as the exec block. Empty rows are
        // tolerated; the wallet falls back to the curated mainnet LC for
        // any chain whose slot is left blank.
        let mut consensus_block: Column<'_, Message> =
            column![subsection_label(t, "Consensus RPCs")]
                .spacing(6)
                .width(Length::Fill);
        for chain in Chain::ALL {
            let value = self.custom_consensus.get(chain);
            let input = text_input("https://…", value)
                .on_input(move |s| Message::CustomConsensusInput(chain, s))
                .on_submit(Message::SubmitCurrent)
                .padding(Padding::from([10, 12]))
                .size(13)
                .font(mono())
                .style(move |_theme, status| text_input_style(t, status));
            consensus_block = consensus_block.push(labeled_row(t, chain.label(), input.into()));
        }

        let has_mainnet = parse_rpc_input(self.custom_exec.get(Chain::Mainnet)).is_some();
        let mut submit = primary_button(t, "Use These RPCs →", has_mainnet);
        if has_mainnet {
            submit = submit.on_press(Message::SubmitCurrent);
        }

        column![exec_block, vspace(12), consensus_block, vspace(12), submit,]
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

/// Build a per-chain map of Alchemy exec URLs from a single API key.
/// Alchemy's hostnames (`eth-mainnet`, `base-mainnet`, `opt-mainnet`)
/// follow the same `{slug}.g.alchemy.com/v2/{key}` pattern, so one key
/// covers every chain we support.
fn alchemy_exec_urls(key: &str) -> PerChain<String> {
    let mut out = PerChain::<String>::default();
    out.set(
        Chain::Mainnet,
        format!("https://eth-mainnet.g.alchemy.com/v2/{key}"),
    );
    out.set(
        Chain::Base,
        format!("https://base-mainnet.g.alchemy.com/v2/{key}"),
    );
    out.set(
        Chain::Optimism,
        format!("https://opt-mainnet.g.alchemy.com/v2/{key}"),
    );
    out
}

/// Build a per-chain map of dRPC exec URLs from a single API key.
/// dRPC's load-balanced endpoint takes the chain slug and key as path
/// segments — the same key authenticates the JSON-RPC and the Wallet
/// API, so the indexer screen can detect it off the Mainnet RPC.
fn drpc_exec_urls(key: &str) -> PerChain<String> {
    let mut out = PerChain::<String>::default();
    for chain in Chain::ALL {
        let slug = match chain {
            Chain::Mainnet => "ethereum",
            Chain::Base => "base",
            Chain::Optimism => "optimism",
        };
        out.set(chain, format!("https://lb.drpc.live/{slug}/{key}"));
    }
    out
}

/// Per-chain consensus URL defaults. Mirrors the chain enum's own
/// `default_consensus_url`. Used by the Alchemy path so picking Alchemy
/// also lights up L2 consensus (operationsolarstorm.org's L2 LC proxies)
/// without the user needing to also visit the Networks pane.
fn default_consensus_urls() -> PerChain<String> {
    let mut out = PerChain::<String>::default();
    for chain in Chain::ALL {
        out.set(chain, chain.default_consensus_url().to_string());
    }
    out
}

fn subsection_label<'a>(t: KaoTheme, label: &'a str) -> Element<'a, Message> {
    text(label).size(11).color(t.sub).font(mono()).into()
}

/// Fixed-width chain label (e.g. "Mainnet") next to a URL input. Used to
/// stack the per-chain rows so the inputs line up under one another.
fn labeled_row<'a>(
    t: KaoTheme,
    label: &'a str,
    input: Element<'a, Message>,
) -> Element<'a, Message> {
    let label_box =
        container(text(label).size(12).color(t.text).font(bold())).width(Length::Fixed(72.0));
    row![label_box, input]
        .spacing(8)
        .align_y(Alignment::Center)
        .width(Length::Fill)
        .into()
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

/// True iff `s` parses as an HTTPS URL. Used to gate the per-chain consensus
/// URL inputs — bare-host shortcuts aren't allowed there because LC bootstrap
/// integrity depends on TLS.
fn is_https_url(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    matches!(url::Url::parse(s), Ok(url) if url.scheme() == "https")
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
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
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
    fn defaults_are_pretyped_into_custom_inputs() {
        let s = SelectRpcScreen::default();
        // Every chain's exec slot ships with a real URL the user can edit.
        for chain in Chain::ALL {
            assert_eq!(s.custom_exec.get(chain), chain.default_exec_url());
            assert_eq!(s.custom_consensus.get(chain), chain.default_consensus_url());
        }
    }

    #[test]
    fn pretyped_defaults_submit_cleanly() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Custom { exec, consensus }) => {
                // Every chain's exec slot ships pre-typed, so all three
                // propagate to settings on submit.
                for chain in Chain::ALL {
                    assert_eq!(exec.get(chain), chain.default_exec_url());
                    assert_eq!(consensus.get(chain), chain.default_consensus_url());
                }
            }
            other => panic!("expected Custom outcome, got {other:?}"),
        }
        assert!(s.error.is_none());
    }

    #[test]
    fn cleared_mainnet_exec_errors() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        s.update(Message::CustomExecInput(Chain::Mainnet, String::new()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.is_some());
    }

    #[test]
    fn rejects_non_http_scheme_on_mainnet() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        s.update(Message::CustomExecInput(
            Chain::Mainnet,
            "ws://my-node.example".into(),
        ));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.is_some());
    }

    #[test]
    fn accepts_https_mainnet() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        s.update(Message::CustomExecInput(
            Chain::Mainnet,
            "  https://my-node.example/rpc  ".into(),
        ));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Custom { exec, .. }) => {
                assert_eq!(exec.get(Chain::Mainnet), "https://my-node.example/rpc");
            }
            other => panic!("expected Custom outcome, got {other:?}"),
        }
        assert!(s.error.is_none());
    }

    #[test]
    fn invalid_l2_exec_errors_even_when_mainnet_is_valid() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        s.update(Message::CustomExecInput(Chain::Base, "ws://nope".into()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.as_deref().unwrap().contains("Base"));
    }

    #[test]
    fn cleared_l2_slots_submit_cleanly() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        s.update(Message::CustomExecInput(Chain::Base, String::new()));
        s.update(Message::CustomExecInput(Chain::Optimism, String::new()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Custom { exec, .. }) => {
                assert!(!exec.get(Chain::Mainnet).is_empty());
                assert!(exec.get(Chain::Base).is_empty());
                assert!(exec.get(Chain::Optimism).is_empty());
            }
            other => panic!("expected Custom outcome, got {other:?}"),
        }
        assert!(s.error.is_none());
    }

    #[test]
    fn consensus_inputs_are_independent_per_chain() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        let mainnet_before = s.custom_consensus.mainnet.clone();
        s.update(Message::CustomConsensusInput(
            Chain::Base,
            "https://cl-base.example".into(),
        ));
        // Typing into Base's slot must not bleed into Mainnet's slot.
        assert_eq!(s.custom_consensus.mainnet, mainnet_before);
        assert_eq!(s.custom_consensus.base, "https://cl-base.example");
        assert_eq!(
            s.custom_consensus.optimism,
            Chain::Optimism.default_consensus_url()
        );
    }

    #[test]
    fn non_https_consensus_errors() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickCustom);
        s.update(Message::CustomConsensusInput(
            Chain::Mainnet,
            "http://not-tls.example".into(),
        ));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.as_deref().unwrap().contains("Mainnet"));
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
    fn pick_drpc_expands_without_outcome() {
        let mut s = SelectRpcScreen::default();
        let (_, outcome) = s.update(Message::PickDrpc);
        assert!(outcome.is_none());
        assert_eq!(s.expanded, Expanded::Drpc);
    }

    #[test]
    fn empty_drpc_key_errors() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickDrpc);
        let (_, outcome) = s.update(Message::SubmitCurrent);
        assert!(outcome.is_none());
        assert!(s.error.is_some());
    }

    #[test]
    fn drpc_key_emits_per_chain_urls() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickDrpc);
        s.update(Message::DrpcKeyInput("  abc123  ".into()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Custom { exec, consensus }) => {
                assert_eq!(
                    exec.get(Chain::Mainnet),
                    "https://lb.drpc.live/ethereum/abc123"
                );
                assert_eq!(exec.get(Chain::Base), "https://lb.drpc.live/base/abc123");
                assert_eq!(
                    exec.get(Chain::Optimism),
                    "https://lb.drpc.live/optimism/abc123"
                );
                for chain in Chain::ALL {
                    assert_eq!(consensus.get(chain), chain.default_consensus_url());
                }
            }
            other => panic!("expected Custom outcome, got {other:?}"),
        }
        assert!(s.error.is_none());
    }

    #[test]
    fn alchemy_key_emits_per_chain_urls() {
        let mut s = SelectRpcScreen::default();
        s.update(Message::PickAlchemy);
        s.update(Message::AlchemyKeyInput("  abc123  ".into()));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Custom { exec, consensus }) => {
                assert_eq!(
                    exec.get(Chain::Mainnet),
                    "https://eth-mainnet.g.alchemy.com/v2/abc123"
                );
                assert_eq!(
                    exec.get(Chain::Base),
                    "https://base-mainnet.g.alchemy.com/v2/abc123"
                );
                assert_eq!(
                    exec.get(Chain::Optimism),
                    "https://opt-mainnet.g.alchemy.com/v2/abc123"
                );
                // Consensus comes from the per-chain defaults so picking
                // Alchemy lights up L2 verification too — Alchemy doesn't
                // host beacon LC endpoints.
                for chain in Chain::ALL {
                    assert_eq!(consensus.get(chain), chain.default_consensus_url());
                }
            }
            other => panic!("expected Custom outcome, got {other:?}"),
        }
        assert!(s.error.is_none());
    }

    #[test]
    fn parse_accepts_bare_ip() {
        assert_eq!(
            parse_rpc_input("192.168.1.5"),
            Some("http://192.168.1.5".into())
        );
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
        assert_eq!(
            parse_rpc_input("localhost"),
            Some("http://localhost".into())
        );
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
        s.update(Message::CustomExecInput(
            Chain::Mainnet,
            "10.0.0.2:8545".into(),
        ));
        let (_, outcome) = s.update(Message::SubmitCurrent);
        match outcome {
            Some(Outcome::Custom { exec, .. }) => {
                assert_eq!(exec.get(Chain::Mainnet), "http://10.0.0.2:8545");
            }
            other => panic!("expected Custom outcome, got {other:?}"),
        }
        assert!(s.error.is_none());
    }
}
