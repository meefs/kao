//! Send modal — multi-step wizard (recipient → amount → review → success).
//!
//! The pane carries no signer or RPC access. It bubbles `QuoteRequested` /
//! `BroadcastRequested` outcomes upward to the dashboard, which holds the
//! `KaoSigner` and `BalanceFetcher::provider()` and runs the actual
//! `wallet::tx::build_quote` / `wallet::tx::sign_and_send`. Results flow back
//! through `QuoteFetched` / `BroadcastDone` messages.

use std::str::FromStr;

use alloy::primitives::utils::format_units;
use alloy::primitives::{Address, TxHash};
use iced::border::Radius;
use iced::keyboard;
use iced::widget::{Space, column, container, mouse_area, row, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Subscription, Task};

use crate::decode::render::DecodedCall;
use crate::ens;
use crate::portfolio::LiveToken;
use crate::ui::kao_theme::KaoTheme;
use crate::ui::kao_widgets::{
    avatar, black, bold, colored_address, kao_fit, kao_text, kaomoji_for_index, modal_wrapper,
    mono, mono_black, primary_button, review_row, secondary_button, text_input_style,
};
use crate::ui::wallet_dashboard::function_panel;
use crate::wallet::tx::{SendPlan, SendToken, TxQuote, parse_amount_units};

#[derive(Debug, Clone, Copy)]
struct Contact {
    name: &'static str,
    addr: &'static str,
    kao: &'static str,
}

const CONTACTS: &[Contact] = &[
    Contact {
        name: "vitalik.eth",
        addr: "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045",
        kao: "(◕‿◕✿)",
    },
    Contact {
        name: "friend.eth",
        addr: "0xAbC1234567890ABCdef1234567890aBcDef1234567",
        kao: "( ´ ▽ ` )ﾉ",
    },
];

#[derive(Debug, Clone)]
pub enum Message {
    SetTo(String),
    PickContact(usize),
    SetAmount(String),
    SetToken(usize),
    Max,
    Step(u8),
    Confirm,
    QuoteFetched(Result<TxQuote, String>),
    BroadcastDone(Result<TxHash, String>),
    /// Result of a clear-signing decode spawned by the dashboard.
    /// `seq` is the decode-generation counter; stale results dropped.
    DecodedReady {
        seq: u64,
        decoded: Box<DecodedCall>,
    },
    /// Result of an ENS forward-resolution task spawned by the dashboard.
    /// `seq` is the input-generation counter that was current when the task
    /// was spawned; results carrying a stale seq are dropped so the user's
    /// most recent typing always wins.
    EnsResolved {
        seq: u64,
        name: String,
        result: Result<Option<Address>, String>,
    },
    CopyHash,
    CopyEtherscan,
    Close,
    BoxClickIgnored,
    Key(keyboard::Event),
}

/// Resolution state of the recipient input. Tracks both the literal user
/// input and any ENS lookup that resulted from it.
#[derive(Debug, Clone)]
enum Resolution {
    /// Empty input.
    Empty,
    /// User typed something that's not a valid address and not ENS-shaped
    /// (no dot). Continue is disabled.
    Invalid,
    /// User pasted a valid hex address — no network round-trip needed.
    Address(Address),
    /// User typed an ENS-shaped name and a lookup is in flight.
    Resolving { name: String },
    /// ENS lookup succeeded.
    Resolved { name: String, addr: Address },
    /// ENS lookup returned no address record.
    NotFound { name: String },
    /// ENS lookup errored (network, RPC, decoding).
    Error { name: String, msg: String },
}

impl Resolution {
    fn recipient(&self) -> Option<Address> {
        match self {
            Resolution::Address(a) | Resolution::Resolved { addr: a, .. } => Some(*a),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Closed,
    /// User clicked one of the success-step copy buttons. Coordinator
    /// runs `iced::clipboard::write`.
    CopyText(String),
}

#[derive(Debug)]
pub struct SendPane {
    /// Sender's address — held so we can build a `SendPlan` without
    /// passing the signer around.
    from: Address,
    step: u8,
    to: String,
    /// Parsed/resolved recipient state. Inputs that are valid hex
    /// addresses skip the network; ENS-shaped inputs go through a
    /// dashboard-coordinated resolver. The `recipient()` accessor pulls a
    /// concrete `Address` out only when the state is settled.
    resolution: Resolution,
    /// Bumped on every recipient-input change. ENS lookups tag their
    /// results with the seq they were spawned at; stale results are dropped.
    resolution_seq: u64,
    /// Highest seq for which the dashboard has already spawned a task. Lets
    /// `take_pending_ens` return `Some` once per fresh input change without
    /// the dashboard having to track per-pane state.
    last_dispatched_seq: Option<u64>,
    amount: String,
    token_idx: usize,
    busy: bool,
    quote: Option<TxQuote>,
    quote_loading: bool,
    /// Latest broadcast/quote error. Cleared on user action.
    error: Option<String>,
    /// Set by `BroadcastDone(Ok(_))`; rendered on the success step.
    last_tx_hash: Option<TxHash>,
    /// Clear-signing result for the current SendPlan. `None` while a
    /// decode is in flight (with `decoded_loading = true`) or when the
    /// plan has empty calldata (native send — no decode needed).
    decoded: Option<Box<DecodedCall>>,
    decoded_loading: bool,
    /// Bumped each time the dashboard kicks a fresh decode. Stale
    /// results (slow decoder finishing after the plan changed) are
    /// dropped via this same sequence-number pattern as ENS resolves.
    decoded_seq: u64,
}

impl SendPane {
    pub fn new(from: Address) -> Self {
        Self {
            from,
            step: 0,
            to: String::new(),
            resolution: Resolution::Empty,
            resolution_seq: 0,
            last_dispatched_seq: None,
            amount: String::new(),
            token_idx: 0,
            busy: false,
            quote: None,
            quote_loading: false,
            error: None,
            last_tx_hash: None,
            decoded: None,
            decoded_loading: false,
            decoded_seq: 0,
        }
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn token_idx(&self) -> usize {
        self.token_idx
    }

    pub fn quote(&self) -> Option<TxQuote> {
        self.quote
    }

    /// Coordinator hook: returns `Some((seq, name))` exactly once per
    /// recipient-input change that landed on an ENS-shaped value. The
    /// dashboard spawns a forward-resolution task tagged with `seq`, and a
    /// later `EnsResolved` carries the result back. After the first
    /// dispatch this returns `None` until the user types something that
    /// bumps the seq again.
    pub fn take_pending_ens(&mut self) -> Option<(u64, String)> {
        match &self.resolution {
            Resolution::Resolving { name }
                if self.last_dispatched_seq != Some(self.resolution_seq) =>
            {
                let seq = self.resolution_seq;
                self.last_dispatched_seq = Some(seq);
                Some((seq, name.clone()))
            }
            _ => None,
        }
    }

    fn set_to(&mut self, raw: String) {
        self.to = raw;
        self.resolution_seq = self.resolution_seq.wrapping_add(1);
        let trimmed = self.to.trim();
        self.resolution = if trimmed.is_empty() {
            Resolution::Empty
        } else if let Ok(addr) = Address::from_str(trimmed) {
            Resolution::Address(addr)
        } else if ens::looks_like_ens(trimmed) {
            Resolution::Resolving {
                name: trimmed.to_string(),
            }
        } else {
            Resolution::Invalid
        };
    }

    /// Coordinator-driven Max: the dashboard knows the active token's raw
    /// balance and (when a quote is loaded) the expected ETH gas cost, so it
    /// computes the max sendable amount and pumps it back as a formatted
    /// string. We just slot it in.
    pub fn apply_max(&mut self, amount_str: String) {
        self.amount = amount_str;
    }

    pub fn update(&mut self, msg: Message) -> (Task<Message>, Option<Outcome>) {
        match msg {
            Message::SetTo(s) => {
                self.set_to(s);
                (Task::none(), None)
            }
            Message::PickContact(i) => {
                if let Some(c) = CONTACTS.get(i) {
                    self.set_to(c.addr.into());
                }
                (Task::none(), None)
            }
            Message::EnsResolved { seq, name, result } => {
                if seq != self.resolution_seq {
                    return (Task::none(), None);
                }
                // Only apply the result if the resolution slot is still
                // pointing at the same name we dispatched for. The user
                // could have typed a different ENS-shaped value at the
                // same seq if rapid edits straddled wrapping boundaries —
                // unlikely in practice, but cheap to guard.
                let still_relevant = matches!(
                    &self.resolution,
                    Resolution::Resolving { name: pending } if pending == &name,
                );
                if !still_relevant {
                    return (Task::none(), None);
                }
                self.resolution = match result {
                    Ok(Some(addr)) => Resolution::Resolved { name, addr },
                    Ok(None) => Resolution::NotFound { name },
                    Err(msg) => Resolution::Error { name, msg },
                };
                (Task::none(), None)
            }
            Message::SetAmount(s) => {
                self.amount = s;
                self.error = None;
                (Task::none(), None)
            }
            Message::SetToken(i) => {
                self.token_idx = i;
                // A different token invalidates any existing quote — gas
                // cost is the same call but the calldata differs, and the
                // user shouldn't see a stale 21k gas line for an ERC-20.
                self.quote = None;
                (Task::none(), None)
            }
            Message::Max => (Task::none(), None),
            Message::Step(s) => {
                if s <= 3 {
                    self.step = s;
                }
                (Task::none(), None)
            }
            Message::Confirm => {
                // The dashboard intercepts this message *before* forwarding
                // to us so it can move the signer into a broadcast task.
                // Our role is just to flip into the busy state. Refuse to
                // mark busy if no quote is loaded — the dashboard would
                // also refuse to spawn the task in that case, so we'd
                // wedge the UI.
                if !self.busy && self.quote.is_some() {
                    self.busy = true;
                    self.error = None;
                }
                (Task::none(), None)
            }
            Message::QuoteFetched(result) => {
                self.quote_loading = false;
                match result {
                    Ok(q) => {
                        self.quote = Some(q);
                        self.error = None;
                    }
                    Err(e) => {
                        self.error = Some(e);
                    }
                }
                (Task::none(), None)
            }
            Message::BroadcastDone(result) => {
                self.busy = false;
                match result {
                    Ok(hash) => {
                        self.last_tx_hash = Some(hash);
                        self.step = 3;
                        self.error = None;
                    }
                    Err(e) => {
                        self.error = Some(e);
                    }
                }
                (Task::none(), None)
            }
            Message::DecodedReady { seq, decoded } => {
                // Drop stale results — the user might have backed out
                // of the review step and built a different plan before
                // this future resolved.
                if seq == self.decoded_seq {
                    self.decoded_loading = false;
                    self.decoded = Some(decoded);
                }
                (Task::none(), None)
            }
            Message::CopyHash => match self.last_tx_hash {
                Some(h) => (
                    Task::none(),
                    Some(Outcome::CopyText(format!("{h:#x}"))),
                ),
                None => (Task::none(), None),
            },
            Message::CopyEtherscan => match self.last_tx_hash {
                Some(h) => (
                    Task::none(),
                    Some(Outcome::CopyText(format!(
                        "https://etherscan.io/tx/{h:#x}"
                    ))),
                ),
                None => (Task::none(), None),
            },
            Message::Close => (Task::none(), Some(Outcome::Closed)),
            Message::BoxClickIgnored => (Task::none(), None),
            Message::Key(keyboard::Event::KeyPressed { key, .. }) => {
                if let keyboard::Key::Named(keyboard::key::Named::Escape) = key {
                    if matches!(self.step, 1 | 2) && !self.busy {
                        self.step -= 1;
                        (Task::none(), None)
                    } else {
                        (Task::none(), Some(Outcome::Closed))
                    }
                } else {
                    (Task::none(), None)
                }
            }
            Message::Key(_) => (Task::none(), None),
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        keyboard::listen().map(Message::Key)
    }

    /// Coordinator hook: called when the user presses "Review →" so the
    /// dashboard can fetch a quote against the same plan the pane will
    /// later broadcast. Returns `None` if the current state can't be
    /// turned into a valid plan (input parses missing).
    pub fn build_plan(&self, portfolio: &[LiveToken]) -> Option<SendPlan> {
        let recipient = self.resolution.recipient()?;
        let token = portfolio.get(self.token_idx)?;
        let amount_units = parse_amount_units(&self.amount, token.decimals).ok()?;
        if amount_units.is_zero() {
            return None;
        }
        if amount_units > token.balance_raw {
            return None;
        }
        let send_token = match token.contract {
            None => SendToken::Native,
            Some(addr) => SendToken::Erc20 { contract: addr },
        };
        Some(SendPlan {
            from: self.from,
            recipient,
            token: send_token,
            amount_units,
            chain: token.chain,
        })
    }

    /// Mark a quote fetch in flight. Called by the dashboard right after
    /// it spawns the quote task so the review step renders a "loading"
    /// indicator rather than a missing-quote state.
    pub fn quote_started(&mut self) {
        self.quote_loading = true;
        self.error = None;
    }

    /// Bump the decode seq, mark in flight, and return the new seq.
    /// The dashboard tags its `decode_call` task with this value; the
    /// matching `DecodedReady` message carries it back, and we drop
    /// any result whose seq doesn't match the latest.
    pub fn decode_started(&mut self) -> u64 {
        self.decoded_seq = self.decoded_seq.wrapping_add(1);
        self.decoded_loading = true;
        self.decoded = None;
        self.decoded_seq
    }

    pub fn view<'a>(
        &'a self,
        t: KaoTheme,
        portfolio: &'a [LiveToken],
        progress: f32,
    ) -> Element<'a, Message> {
        let inner: Element<'_, Message> = match self.step {
            0 => self.step_recipient(t),
            1 => self.step_amount(t, portfolio),
            2 => self.step_review(t, portfolio),
            _ => self.step_success(t, portfolio),
        };

        let step_kao = match self.step {
            0 => "(・・;)ゞ",
            1 => "( •̀ω•́ )✧",
            2 => "(・_・ヾ",
            _ => "ヽ(・∀・)ﾉ",
        };

        let mut head_col = column![].spacing(2);
        head_col = head_col.push(text("Send").size(22).color(t.text).font(black()));
        if self.step < 3 {
            head_col = head_col.push(
                text(format!("Step {} of 3", self.step + 1))
                    .size(12)
                    .color(t.sub)
                    .font(mono()),
            );
        }

        let head = row![
            head_col,
            Space::new().width(Length::Fill),
            kao_text(t, step_kao, 30.0),
        ]
        .align_y(Alignment::Start)
        .width(Length::Fill);

        let mut content = column![head].spacing(0);
        content = content.push(Space::new().height(20));
        if self.step < 3 {
            content = content.push(self.progress_bar(t));
            content = content.push(Space::new().height(16));
        }
        content = content.push(inner);

        modal_wrapper(
            t,
            440.0,
            progress,
            Message::Close,
            Message::BoxClickIgnored,
            content.into(),
        )
    }

    fn progress_bar<'a>(&self, t: KaoTheme) -> Element<'a, Message> {
        let mut r = row![].spacing(5).width(Length::Fill);
        for i in 0..3u8 {
            let col = if i <= self.step { t.a1 } else { t.border };
            r = r.push(
                container(Space::new().width(Length::Fill).height(4))
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(Background::Color(col)),
                        border: Border {
                            color: col,
                            width: 0.0,
                            radius: Radius::from(2),
                        },
                        ..container::Style::default()
                    }),
            );
        }
        r.into()
    }

    fn step_recipient<'a>(&'a self, t: KaoTheme) -> Element<'a, Message> {
        let label = text("TO").size(11).color(t.sub).font(bold());

        let input = text_input("0x… address or name.eth", &self.to)
            .on_input(Message::SetTo)
            .padding(Padding::from([12, 14]))
            .size(15)
            .font(mono())
            .style(move |_theme, status| text_input_style(t, status));

        let parse_hint: Element<'_, Message> = match &self.resolution {
            Resolution::Empty => Space::new().height(0).into(),
            Resolution::Address(_) => container(
                text("✓ valid address").size(11).color(t.up).font(bold()),
            )
            .padding(Padding::from([4, 0]))
            .into(),
            Resolution::Resolved { name, addr } => container(
                row![
                    text(format!("✓ {name} →  ")).size(11).color(t.up).font(bold()),
                    text(short_address_str(&format!("{addr:#x}")))
                        .size(11)
                        .color(t.sub)
                        .font(mono()),
                ]
                .align_y(Alignment::Center),
            )
            .padding(Padding::from([4, 0]))
            .into(),
            Resolution::Resolving { name } => container(
                text(format!("(；・∀・) resolving {name}…"))
                    .size(11)
                    .color(t.sub)
                    .font(bold()),
            )
            .padding(Padding::from([4, 0]))
            .into(),
            Resolution::NotFound { name } => container(
                text(format!("ENS name “{name}” has no address record"))
                    .size(11)
                    .color(t.down)
                    .font(bold()),
            )
            .padding(Padding::from([4, 0]))
            .into(),
            Resolution::Error { name, msg } => container(
                text(format!("ENS lookup for “{name}” failed: {msg}"))
                    .size(11)
                    .color(t.down)
                    .font(bold()),
            )
            .padding(Padding::from([4, 0]))
            .into(),
            Resolution::Invalid => container(
                text("Not a valid 0x… address or ENS name")
                    .size(11)
                    .color(t.down)
                    .font(bold()),
            )
            .padding(Padding::from([4, 0]))
            .into(),
        };

        let recent_label = text("RECENT").size(11).color(t.sub).font(bold());

        let mut contacts_col = column![].spacing(2);
        for (i, c) in CONTACTS.iter().enumerate() {
            contacts_col = contacts_col.push(self.contact_row(t, i, *c));
        }

        let can_continue = self.resolution.recipient().is_some();
        let continue_btn =
            primary_button(t, "Continue →", can_continue).on_press_maybe(if can_continue {
                Some(Message::Step(1))
            } else {
                None
            });

        column![
            label,
            Space::new().height(6),
            input,
            parse_hint,
            Space::new().height(12),
            recent_label,
            Space::new().height(4),
            contacts_col,
            Space::new().height(16),
            continue_btn,
        ]
        .width(Length::Fill)
        .into()
    }

    fn contact_row<'a>(&self, t: KaoTheme, i: usize, c: Contact) -> Element<'a, Message> {
        let selected = self.to == c.addr;
        let bg = if selected { t.ab2 } else { Color::TRANSPARENT };

        let row_content = row![
            avatar(t, c.kao, 34.0, t.ab2),
            Space::new().width(12),
            column![
                text(c.name).size(14).color(t.text).font(bold()),
                text(short_form(c.addr)).size(11).color(t.sub).font(mono()),
            ]
            .spacing(0)
            .width(Length::Fill),
            text(if selected { "✓" } else { " " }).size(16).color(t.a2),
        ]
        .align_y(Alignment::Center)
        .width(Length::Fill);

        let styled = container(row_content)
            .padding(Padding::from([9, 10]))
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(bg)),
                border: Border {
                    color: Color::TRANSPARENT,
                    width: 0.0,
                    radius: Radius::from(11),
                },
                text_color: Some(t.text),
                ..container::Style::default()
            });

        mouse_area(styled)
            .on_press(Message::PickContact(i))
            .interaction(iced::mouse::Interaction::Pointer)
            .into()
    }

    fn step_amount<'a>(
        &'a self,
        t: KaoTheme,
        portfolio: &'a [LiveToken],
    ) -> Element<'a, Message> {
        let recipient = self.resolution.recipient();
        let recipient_kao = "(￣ω￣)";

        let recipient_summary: Element<'_, Message> = match recipient {
            Some(addr) => {
                let mut col = column![
                    container(avatar(t, recipient_kao, 52.0, t.ab2))
                        .width(Length::Fill)
                        .center_x(Length::Fill),
                    Space::new().height(8),
                ]
                .align_x(Alignment::Center);
                // Show the resolved ENS name above the chunked address
                // when the user typed a name rather than a raw address.
                if let Resolution::Resolved { name, .. } = &self.resolution {
                    col = col.push(
                        container(text(name.clone()).size(13).color(t.text).font(bold()))
                            .width(Length::Fill)
                            .center_x(Length::Fill),
                    );
                    col = col.push(Space::new().height(4));
                }
                col = col.push(colored_address(t, addr));
                col.into()
            }
            None => column![
                container(text("Recipient parse failed").size(13).color(t.down).font(bold()))
                    .width(Length::Fill)
                    .center_x(Length::Fill),
            ]
            .into(),
        };

        let mut tabs = row![].spacing(7).width(Length::Fill);
        for (i, tk) in portfolio.iter().take(4).enumerate() {
            tabs = tabs.push(self.token_tab(t, i, tk));
        }

        let token = portfolio.get(self.token_idx);
        let token_bal = token.map(|t| t.balance.as_str()).unwrap_or("0");
        let token_sym = token.map(|t| t.symbol.as_str()).unwrap_or("ETH");
        let amount_input = text_input("0.00", &self.amount)
            .on_input(Message::SetAmount)
            .padding(14)
            .size(34)
            .font(mono_black())
            .align_x(Alignment::Center)
            .style(move |_theme, status| text_input_style(t, status));

        // Live amount validation. Rejects unparseable input, zero, and
        // amounts above balance.
        let parsed_amount = token
            .and_then(|tk| parse_amount_units(&self.amount, tk.decimals).ok());
        let amount_valid = match (parsed_amount, token) {
            (Some(amt), Some(tk)) => !amt.is_zero() && amt <= tk.balance_raw,
            _ => false,
        };
        let amount_hint: Element<'_, Message> = if self.amount.trim().is_empty() {
            Space::new().height(0).into()
        } else if !amount_valid {
            container(
                text(match (parsed_amount, token) {
                    (None, _) => "Not a valid amount".to_string(),
                    (Some(amt), Some(tk)) if amt > tk.balance_raw => {
                        format!("Exceeds balance ({} {})", tk.balance, tk.symbol)
                    }
                    _ => "Amount must be > 0".to_string(),
                })
                .size(11)
                .color(t.down)
                .font(bold()),
            )
            .padding(Padding::from([4, 0]))
            .into()
        } else {
            Space::new().height(0).into()
        };

        let bal_line = row![
            text(format!("Balance: {} {}", token_bal, token_sym))
                .size(12)
                .color(t.sub),
            Space::new().width(Length::Fill),
            mouse_area(text("Max").size(12).color(t.a1).font(bold()))
                .on_press(Message::Max)
                .interaction(iced::mouse::Interaction::Pointer),
        ]
        .width(Length::Fill);

        let back_btn = secondary_button(t, "← Back").on_press(Message::Step(0));
        let review_btn = primary_button(t, "Review →", amount_valid).on_press_maybe(
            if amount_valid {
                Some(Message::Step(2))
            } else {
                None
            },
        );

        let action_row = row![
            container(back_btn).width(Length::FillPortion(1)),
            Space::new().width(9),
            container(review_btn).width(Length::FillPortion(2)),
        ]
        .width(Length::Fill);

        column![
            recipient_summary,
            Space::new().height(20),
            tabs,
            Space::new().height(14),
            amount_input,
            amount_hint,
            Space::new().height(7),
            bal_line,
            Space::new().height(16),
            action_row,
        ]
        .width(Length::Fill)
        .into()
    }

    fn token_tab<'a>(&self, t: KaoTheme, i: usize, tk: &'a LiveToken) -> Element<'a, Message> {
        let active = i == self.token_idx;
        let border_col = if active { t.a1 } else { t.border };
        let bg = if active { t.ab1 } else { t.card };
        let inner = column![
            kao_text(t, kaomoji_for_index(i), 11.0),
            Space::new().height(1),
            text(&tk.symbol).size(12).color(t.text).font(bold()),
        ]
        .align_x(Alignment::Center)
        .spacing(0);

        let styled = container(inner)
            .width(Length::Fill)
            .center_x(Length::Fill)
            .padding(Padding::from([8, 4]))
            .style(move |_| container::Style {
                background: Some(Background::Color(bg)),
                border: Border {
                    color: border_col,
                    width: 1.5,
                    radius: Radius::from(10),
                },
                text_color: Some(t.text),
                ..container::Style::default()
            });

        mouse_area(container(styled).width(Length::Fill))
            .on_press(Message::SetToken(i))
            .interaction(iced::mouse::Interaction::Pointer)
            .into()
    }

    fn step_review<'a>(
        &'a self,
        t: KaoTheme,
        portfolio: &'a [LiveToken],
    ) -> Element<'a, Message> {
        let token = portfolio.get(self.token_idx);
        let token_sym = token.map(|t| t.symbol.as_str()).unwrap_or("ETH");
        let recipient = self.resolution.recipient();
        // Drive the network label off the actual token's chain so the user
        // can't review a Base USDC send under a "Mainnet" label.
        let chain = token.map(|t| t.chain).unwrap_or_default();

        // Sending row + chain id label below.
        let sending_row = column![
            review_row(
                t,
                "Sending",
                &format!("{} {}", self.amount, token_sym),
                true,
                false,
            ),
            row![
                text("").size(11),
                Space::new().width(Length::Fill),
                text(format!("{} · chain {}", chain.display_name(), chain.chain_id()))
                    .size(10)
                    .color(t.sub)
                    .font(mono()),
            ]
            .width(Length::Fill),
        ]
        .spacing(2);

        // To row: full checksum address rendered with per-chunk colors.
        // When the user typed an ENS name we put the name above the
        // chunked address — the chunked address is still the load-bearing
        // identifier the user is signing for, so the ENS name is
        // supporting context, not a substitute.
        let to_block: Element<'_, Message> = match recipient {
            Some(addr) => {
                let mut col = column![text("To").size(13).color(t.sub), Space::new().height(4)];
                if let Resolution::Resolved { name, .. } = &self.resolution {
                    col = col.push(text(name.clone()).size(13).color(t.text).font(bold()));
                    col = col.push(Space::new().height(4));
                }
                col = col.push(colored_address(t, addr));
                col.into()
            }
            None => row![
                text("To").size(13).color(t.sub),
                Space::new().width(Length::Fill),
                text("(invalid)").size(13).color(t.down).font(bold()),
            ]
            .width(Length::Fill)
            .into(),
        };

        // Gas fee row: real numbers when a quote is loaded, shimmer when in
        // flight, dash when neither.
        let gas_row: Element<'_, Message> = if self.quote_loading {
            review_row(t, "Gas fee", "(；・∀・) estimating…", false, true)
        } else {
            match self.quote {
                Some(q) => {
                    let eth_str = format_units(q.eth_cost_wei, 18u8)
                        .unwrap_or_else(|_| "—".into());
                    // Trim trailing zeros for display.
                    let eth_short = trim_eth_display(&eth_str);
                    let usd = portfolio
                        .first()
                        .map(|eth_tk| eth_tk.usd_price)
                        .unwrap_or(0.0);
                    let eth_f = eth_str.parse::<f64>().unwrap_or(0.0);
                    let usd_cost = eth_f * usd;
                    let display = if usd > 0.0 {
                        format!("≈ {} ETH (${:.2})", eth_short, usd_cost)
                    } else {
                        format!("≈ {} ETH", eth_short)
                    };
                    review_row(t, "Gas fee (｡•́︿•̀｡)", &display, false, false)
                }
                None => review_row(t, "Gas fee", "—", false, true),
            }
        };

        // Native ETH only: warn if amount + gas > balance.
        let insufficient_eth_warning: Element<'_, Message> = match (token, self.quote) {
            (Some(tk), Some(q)) if tk.contract.is_none() => {
                if let Ok(amt) = parse_amount_units(&self.amount, tk.decimals) {
                    if amt.saturating_add(q.eth_cost_wei) > tk.balance_raw {
                        container(
                            text("Insufficient ETH for amount + gas")
                                .size(12)
                                .color(t.down)
                                .font(bold()),
                        )
                        .padding(Padding::from([6, 0]))
                        .into()
                    } else {
                        Space::new().height(0).into()
                    }
                } else {
                    Space::new().height(0).into()
                }
            }
            _ => Space::new().height(0).into(),
        };

        // Clear-signing panel: only renders when the call has calldata.
        // Native ETH transfers return `None` from `function_panel::view`
        // and we keep the surrounding vertical rhythm consistent so the
        // review card doesn't pop when the panel lands.
        let function_block: Element<'_, Message> = match function_panel::view::<Message>(
            t,
            self.decoded.as_deref(),
            self.decoded_loading,
        ) {
            Some(panel) => column![Space::new().height(14), panel].spacing(0).into(),
            None => Space::new().height(0).into(),
        };

        // No dividers between sections — `divider()` renders as a
        // ~25px solid bar (its 12px vertical padding gets the border
        // color baked in), which on the review card overwhelms the
        // actual content. Plain vertical space gives the same visual
        // separation without the heavy strip.
        let review_box = column![
            sending_row,
            Space::new().height(14),
            to_block,
            Space::new().height(14),
            gas_row,
            function_block,
            insufficient_eth_warning,
        ]
        .spacing(0);

        let review_card = container(review_box)
            .padding(Padding::from([18, 20]))
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(t.card_alt)),
                border: Border {
                    color: t.border,
                    width: 0.0,
                    radius: Radius::from(16),
                },
                text_color: Some(t.text),
                ..container::Style::default()
            });

        let error_block: Element<'_, Message> = match &self.error {
            Some(msg) => container(
                text(format!("(╥﹏╥) {msg}"))
                    .size(12)
                    .color(t.down)
                    .font(bold()),
            )
            .padding(Padding::from([10, 4]))
            .into(),
            None => Space::new().height(0).into(),
        };

        let back_btn = secondary_button(t, "← Back").on_press(Message::Step(1));
        let confirm_enabled = !self.busy && self.quote.is_some();
        let confirm_label = if self.busy {
            "(・・;)ゞ sending…"
        } else {
            "Confirm Send ✓"
        };
        let confirm_btn = primary_button(t, confirm_label, confirm_enabled).on_press_maybe(
            if confirm_enabled {
                Some(Message::Confirm)
            } else {
                None
            },
        );

        let action_row = row![
            container(back_btn).width(Length::FillPortion(1)),
            Space::new().width(9),
            container(confirm_btn).width(Length::FillPortion(2)),
        ]
        .width(Length::Fill);

        column![
            review_card,
            error_block,
            Space::new().height(16),
            action_row,
        ]
        .width(Length::Fill)
        .into()
    }

    fn step_success<'a>(
        &'a self,
        t: KaoTheme,
        portfolio: &'a [LiveToken],
    ) -> Element<'a, Message> {
        let token_sym = portfolio
            .get(self.token_idx)
            .map(|t| t.symbol.as_str())
            .unwrap_or("ETH");
        let big_kao = container(kao_fit(t, "ヽ(・∀・)ﾉ", 320.0, 76.0))
            .width(Length::Fill)
            .center_x(Length::Fill);

        let title = container(text("Sent!").size(26).color(t.text).font(black()))
            .width(Length::Fill)
            .center_x(Length::Fill);

        // Prefer the ENS name on the success screen — the user already
        // saw the chunked checksum address on the review step, and the
        // human-readable name is what they remember acting on.
        let recipient_short = match &self.resolution {
            Resolution::Resolved { name, .. } => name.clone(),
            _ => self
                .resolution
                .recipient()
                .map(|a| short_address_str(&format!("{a:#x}")))
                .unwrap_or_else(|| self.to.clone()),
        };
        let detail = container(
            text(format!(
                "{} {} → {}",
                self.amount, token_sym, recipient_short,
            ))
            .size(15)
            .color(t.sub),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        let hash_str = match self.last_tx_hash {
            Some(h) => format!("{} · pending", short_address_str(&format!("{h:#x}"))),
            None => "—".into(),
        };
        let hash = container(text(hash_str).size(12).color(t.sub).font(mono()))
            .width(Length::Fill)
            .center_x(Length::Fill);

        let copy_btn = secondary_button(t, "Copy hash").on_press_maybe(
            if self.last_tx_hash.is_some() {
                Some(Message::CopyHash)
            } else {
                None
            },
        );
        let etherscan_btn = secondary_button(t, "Copy Etherscan link").on_press_maybe(
            if self.last_tx_hash.is_some() {
                Some(Message::CopyEtherscan)
            } else {
                None
            },
        );
        let action_row = row![
            container(copy_btn).width(Length::FillPortion(1)),
            Space::new().width(9),
            container(etherscan_btn).width(Length::FillPortion(1)),
        ]
        .width(Length::Fill);

        let close_btn = primary_button(t, "Close (ﾉ◕ヮ◕)ﾉ*:･ﾟ✧", true).on_press(Message::Close);
        let close_wrap = container(close_btn)
            .width(Length::Fill)
            .center_x(Length::Fill);

        column![
            Space::new().height(8),
            big_kao,
            Space::new().height(16),
            title,
            Space::new().height(6),
            detail,
            Space::new().height(8),
            hash,
            Space::new().height(14),
            action_row,
            Space::new().height(16),
            close_wrap,
        ]
        .width(Length::Fill)
        .into()
    }
}

/// "0xabcd…ef01" condenser. Used for the success step's hash + recipient
/// display, where the full hash isn't actionable.
fn short_address_str(s: &str) -> String {
    if s.len() >= 12 {
        format!("{}…{}", &s[..6], &s[s.len() - 4..])
    } else {
        s.to_string()
    }
}

/// Same shape as `short_address_str` but takes the placeholder `0xd8dA…6045`
/// fixed-width contact address as input — collapses any internal `…` so the
/// CONTACTS list rows display tidily.
fn short_form(addr: &str) -> String {
    if addr.contains('…') {
        addr.to_string()
    } else {
        short_address_str(addr)
    }
}

/// Compact an ether-formatted decimal string for display next to a USD
/// total. Used for gas — values are typically sub-millieth, where the
/// raw `format_units` output runs to 18 fractional digits and wraps to
/// two lines on the review card.
///
/// Strategy: keep up to 3 significant digits past the leading zeros in
/// the fractional part, then trim trailing zeros. So
/// `"0.000014239683110688"` becomes `"0.0000142"` and
/// `"0.000210000000000000"` stays `"0.00021"`.
fn trim_eth_display(s: &str) -> String {
    let Some(dot) = s.find('.') else {
        return s.to_string();
    };
    let (int_part, dot_frac) = s.split_at(dot);
    let frac = &dot_frac[1..];
    let leading_zeros = frac.bytes().take_while(|b| *b == b'0').count();
    let keep = leading_zeros + 3;
    let truncated: String = frac.chars().take(keep).collect();
    let final_frac = truncated.trim_end_matches('0');
    if final_frac.is_empty() {
        int_part.to_string()
    } else {
        format!("{int_part}.{final_frac}")
    }
}
