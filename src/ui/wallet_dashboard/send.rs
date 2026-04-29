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

use crate::portfolio::LiveToken;
use crate::ui::kao_theme::KaoTheme;
use crate::ui::kao_widgets::{
    avatar, black, bold, colored_address, divider, kao_fit, kao_text, kaomoji_for_index,
    modal_wrapper, mono, mono_black, primary_button, review_row, secondary_button,
    text_input_style,
};
use crate::wallet::CHAIN_ID;
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
    CopyHash,
    CopyEtherscan,
    Close,
    BoxClickIgnored,
    Key(keyboard::Event),
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
    /// Parsed recipient. `None` until `to` is a valid hex address.
    recipient: Option<Address>,
    amount: String,
    token_idx: usize,
    busy: bool,
    quote: Option<TxQuote>,
    quote_loading: bool,
    /// Latest broadcast/quote error. Cleared on user action.
    error: Option<String>,
    /// Set by `BroadcastDone(Ok(_))`; rendered on the success step.
    last_tx_hash: Option<TxHash>,
}

impl SendPane {
    pub fn new(from: Address) -> Self {
        Self {
            from,
            step: 0,
            to: String::new(),
            recipient: None,
            amount: String::new(),
            token_idx: 0,
            busy: false,
            quote: None,
            quote_loading: false,
            error: None,
            last_tx_hash: None,
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
                self.recipient = Address::from_str(s.trim()).ok();
                self.to = s;
                (Task::none(), None)
            }
            Message::PickContact(i) => {
                if let Some(c) = CONTACTS.get(i) {
                    self.to = c.addr.into();
                    self.recipient = Address::from_str(c.addr).ok();
                }
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
        let recipient = self.recipient?;
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
        })
    }

    /// Mark a quote fetch in flight. Called by the dashboard right after
    /// it spawns the quote task so the review step renders a "loading"
    /// indicator rather than a missing-quote state.
    pub fn quote_started(&mut self) {
        self.quote_loading = true;
        self.error = None;
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

        let input = text_input("0x… address (40 hex chars)", &self.to)
            .on_input(Message::SetTo)
            .padding(Padding::from([12, 14]))
            .size(15)
            .font(mono())
            .style(move |_theme, status| text_input_style(t, status));

        let parse_hint: Element<'_, Message> = if self.to.trim().is_empty() {
            Space::new().height(0).into()
        } else if self.recipient.is_some() {
            container(text("✓ valid address").size(11).color(t.up).font(bold()))
                .padding(Padding::from([4, 0]))
                .into()
        } else {
            container(text("Not a valid 0x… address").size(11).color(t.down).font(bold()))
                .padding(Padding::from([4, 0]))
                .into()
        };

        let recent_label = text("RECENT").size(11).color(t.sub).font(bold());

        let mut contacts_col = column![].spacing(2);
        for (i, c) in CONTACTS.iter().enumerate() {
            contacts_col = contacts_col.push(self.contact_row(t, i, *c));
        }

        let can_continue = self.recipient.is_some();
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
        let recipient = self.recipient;
        let recipient_kao = "(￣ω￣)";

        let recipient_summary: Element<'_, Message> = match recipient {
            Some(addr) => column![
                container(avatar(t, recipient_kao, 52.0, t.ab2))
                    .width(Length::Fill)
                    .center_x(Length::Fill),
                Space::new().height(8),
                colored_address(t, addr),
            ]
            .align_x(Alignment::Center)
            .into(),
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
        let recipient = self.recipient;

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
                text(format!("Ethereum Mainnet · chain {}", CHAIN_ID))
                    .size(10)
                    .color(t.sub)
                    .font(mono()),
            ]
            .width(Length::Fill),
        ]
        .spacing(2);

        // To row: full checksum address rendered with per-chunk colors.
        let to_block: Element<'_, Message> = match recipient {
            Some(addr) => column![
                text("To").size(13).color(t.sub),
                Space::new().height(4),
                colored_address(t, addr),
            ]
            .into(),
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

        let review_box = column![
            sending_row,
            divider(t),
            to_block,
            divider(t),
            gas_row,
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

        let recipient_short = self
            .recipient
            .map(|a| short_address_str(&format!("{a:#x}")))
            .unwrap_or_else(|| self.to.clone());
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

/// Trim trailing zeros from an ether-formatted decimal string while
/// preserving readability ("0.000210000000000000" -> "0.00021").
fn trim_eth_display(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}
