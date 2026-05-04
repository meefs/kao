//! Transaction details modal — full info for a single row in the activity
//! pane plus an explorer URL the user can copy.
//!
//! The pane is a passive viewer: it owns a cloned `IndexedTx` and the
//! owner address (so it can colour from/to consistently with the
//! activity row), and it bubbles `CopyText`/`Closed` outcomes upward to
//! the dashboard's existing clipboard plumbing.

use std::time::{SystemTime, UNIX_EPOCH};

use iced::keyboard;
use iced::widget::{Space, column, container, row, scrollable, text};
use iced::{Alignment, Element, Length, Padding, Subscription, Task};

use crate::indexer::{IndexedTx, TokenTransfer, TxDirection, TxStatus};
use crate::portfolio::format_token_balance;
use crate::settings::{self, IndexerProvider};
use crate::ui::kao_theme::KaoTheme;
use crate::ui::kao_widgets::{
    black, bold, colored_address, kao_fit, mono, mono_black, mono_bold, modal_wrapper,
    secondary_button, small_secondary_button,
};

#[derive(Debug, Clone)]
pub enum Message {
    CopyHash,
    CopyExplorerUrl,
    CopyFrom,
    CopyTo,
    Close,
    BoxClickIgnored,
    Key(keyboard::Event),
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Closed,
    CopyText(String),
}

#[derive(Debug)]
pub struct TxDetailsPane {
    tx: IndexedTx,
    /// Resolved at construction time so `view` doesn't reread settings on
    /// every redraw. The provider's hosted explorer (Blockscout web UI)
    /// is preferred when the user is using Blockscout; otherwise we fall
    /// back to Etherscan.
    explorer_url: String,
    /// Pre-formatted button label ("Copy Etherscan link" / "Copy
    /// Blockscout link"). Stored on the pane because `secondary_button`
    /// borrows the label by reference; building it inside `view` would
    /// produce a value that doesn't outlive the function.
    copy_url_label: String,
}

impl TxDetailsPane {
    pub fn new(tx: IndexedTx) -> Self {
        let (explorer_url, explorer_label) = explorer_for(&tx);
        let copy_url_label = format!("Copy {explorer_label} link");
        Self {
            tx,
            explorer_url,
            copy_url_label,
        }
    }

    pub fn update(&mut self, msg: Message) -> (Task<Message>, Option<Outcome>) {
        match msg {
            Message::CopyHash => (
                Task::none(),
                Some(Outcome::CopyText(format!("{:#x}", self.tx.hash))),
            ),
            Message::CopyExplorerUrl => (
                Task::none(),
                Some(Outcome::CopyText(self.explorer_url.clone())),
            ),
            Message::CopyFrom => (
                Task::none(),
                Some(Outcome::CopyText(self.tx.from.to_checksum(None))),
            ),
            Message::CopyTo => match self.tx.to {
                Some(addr) => (
                    Task::none(),
                    Some(Outcome::CopyText(addr.to_checksum(None))),
                ),
                None => (Task::none(), None),
            },
            Message::Close => (Task::none(), Some(Outcome::Closed)),
            Message::BoxClickIgnored => (Task::none(), None),
            Message::Key(keyboard::Event::KeyPressed { key, .. }) => {
                if let keyboard::Key::Named(keyboard::key::Named::Escape) = key {
                    (Task::none(), Some(Outcome::Closed))
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

    pub fn view<'a>(&'a self, t: KaoTheme, progress: f32) -> Element<'a, Message> {
        let kao = match self.tx.direction {
            TxDirection::In => "(っ◕‿◕)っ",
            TxDirection::Out => "ᕕ( ᐛ )ᕗ",
            TxDirection::SelfTransfer => "(･ω･)ﾉ",
        };
        let header_kao = container(kao_fit(t, kao, 220.0, 52.0))
            .width(Length::Fill)
            .center_x(Length::Fill);

        let direction_label = match self.tx.direction {
            TxDirection::In => "Received",
            TxDirection::Out => "Sent",
            TxDirection::SelfTransfer => "Self transfer",
        };
        let title = container(
            text(direction_label)
                .size(21)
                .color(t.text)
                .font(black()),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        let amount_color = match self.tx.direction {
            TxDirection::In if self.has_movement() => t.up,
            _ => t.text,
        };
        let amount_text = format_amount(&self.tx, self.tx.direction);
        let amount = container(
            text(amount_text)
                .size(28)
                .color(amount_color)
                .font(mono_black()),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        let status_text = match self.tx.status {
            TxStatus::Success => "Success",
            TxStatus::Failure => "Failed",
            TxStatus::Pending => "Pending",
        };
        let status_color = match self.tx.status {
            TxStatus::Success => t.up,
            TxStatus::Failure => t.down,
            TxStatus::Pending => t.sub,
        };
        let status = container(
            text(status_text)
                .size(12)
                .color(status_color)
                .font(bold()),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        // ── Field stack ──────────────────────────────────────────────────
        let mut fields = column![].spacing(14).width(Length::Fill);

        fields = fields.push(field(t, "From", colored_address(t, self.tx.from), Some(Message::CopyFrom)));
        match self.tx.to {
            Some(addr) => {
                fields = fields.push(field(
                    t,
                    "To",
                    colored_address(t, addr),
                    Some(Message::CopyTo),
                ));
            }
            None => {
                fields = fields.push(field(
                    t,
                    "To",
                    text("Contract creation")
                        .size(13)
                        .color(t.sub)
                        .font(mono())
                        .into(),
                    None,
                ));
            }
        }

        fields = fields.push(field(
            t,
            "Hash",
            text(format!("{:#x}", self.tx.hash))
                .size(14)
                .color(t.text)
                .font(mono())
                .wrapping(text::Wrapping::WordOrGlyph)
                .into(),
            Some(Message::CopyHash),
        ));

        if self.tx.block_number > 0 {
            fields = fields.push(simple_field(
                t,
                "Block",
                self.tx.block_number.to_string(),
            ));
        }

        if self.tx.timestamp > 0 {
            fields = fields.push(simple_field(
                t,
                "When",
                format_when(self.tx.timestamp),
            ));
        }

        if let (Some(used), Some(price)) = (self.tx.gas_used, self.tx.gas_price) {
            let fee_wei = (used as u128).saturating_mul(price);
            let fee = alloy::primitives::utils::format_ether(
                alloy::primitives::U256::from(fee_wei),
            );
            let f = fee.parse::<f64>().unwrap_or(0.0);
            fields = fields.push(simple_field(
                t,
                "Fee",
                format!("{} ETH", trim_amount(f)),
            ));
        }

        if let Some(method) = &self.tx.method
            && !method.is_empty() {
                fields = fields.push(simple_field(t, "Method", method.clone()));
            }

        // ── Explorer URL block ───────────────────────────────────────────
        let url_label = text("Explorer")
            .size(13)
            .color(t.sub)
            .font(bold());
        let url_value = text(self.explorer_url.clone())
            .size(13)
            .color(t.a1)
            .font(mono())
            .wrapping(text::Wrapping::WordOrGlyph);
        let url_block = column![
            url_label,
            Space::new().height(4),
            url_value,
        ]
        .width(Length::Fill)
        .spacing(0);

        // ── Action buttons ───────────────────────────────────────────────
        let actions = row![
            container(
                secondary_button(t, &self.copy_url_label).on_press(Message::CopyExplorerUrl),
            )
            .width(Length::FillPortion(1)),
            Space::new().width(10),
            container(secondary_button(t, "Copy hash").on_press(Message::CopyHash))
                .width(Length::FillPortion(1)),
        ]
        .width(Length::Fill);

        let body = column![
            header_kao,
            Space::new().height(8),
            title,
            Space::new().height(2),
            status,
            Space::new().height(14),
            amount,
            Space::new().height(20),
            fields,
            Space::new().height(20),
            url_block,
            Space::new().height(14),
            actions,
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        // The full address rendering can push the modal taller than the
        // window in compact themes; wrap in a scrollable so the user can
        // still reach the action buttons.
        let scrollable_body =
            scrollable(container(body).width(Length::Fill)).height(Length::Shrink);

        modal_wrapper(
            t,
            420.0,
            progress,
            Message::Close,
            Message::BoxClickIgnored,
            scrollable_body.into(),
        )
    }

    fn has_movement(&self) -> bool {
        match &self.tx.token {
            Some(tok) => !tok.amount_raw.is_zero(),
            None => !self.tx.value.is_zero(),
        }
    }
}

// ── Field helpers ───────────────────────────────────────────────────────────

/// Label-on-top, value-below row. `copy` adds a small trailing copy
/// affordance; `None` renders plain.
fn field<'a>(
    t: KaoTheme,
    label: &'a str,
    value: Element<'a, Message>,
    copy: Option<Message>,
) -> Element<'a, Message> {
    let label = text(label.to_string()).size(13).color(t.sub).font(bold());
    let header = match copy {
        Some(msg) => row![
            label,
            Space::new().width(Length::Fill),
            small_secondary_button(t, "Copy").on_press(msg),
        ]
        .align_y(Alignment::Center)
        .width(Length::Fill),
        None => row![label].width(Length::Fill),
    };
    column![header, Space::new().height(4), value]
        .width(Length::Fill)
        .spacing(0)
        .into()
}

/// Single-line `label / value` field for plain strings. Compact form
/// used by Block / Fee / Method / When.
fn simple_field<'a>(t: KaoTheme, label: &'a str, value: String) -> Element<'a, Message> {
    row![
        text(label.to_string()).size(13).color(t.sub),
        Space::new().width(Length::Fill),
        text(value).size(13).color(t.text).font(mono_bold()),
    ]
    .align_y(Alignment::Center)
    .padding(Padding::from([2, 0]))
    .width(Length::Fill)
    .into()
}

// ── Formatting helpers ──────────────────────────────────────────────────────

fn format_amount(tx: &IndexedTx, direction: TxDirection) -> String {
    let recv = matches!(direction, TxDirection::In | TxDirection::SelfTransfer);
    if let Some(tok) = &tx.token {
        return format_token_amount(tok, recv);
    }
    if tx.value.is_zero() {
        return "0 ETH".into();
    }
    let raw = alloy::primitives::utils::format_ether(tx.value);
    let f = raw.parse::<f64>().unwrap_or(0.0);
    let sign = if recv { "+" } else { "−" };
    format!("{sign}{} ETH", trim_amount(f))
}

fn format_token_amount(tok: &TokenTransfer, recv: bool) -> String {
    let symbol = if tok.symbol.is_empty() {
        "tokens".to_string()
    } else {
        tok.symbol.clone()
    };
    if tok.amount_raw.is_zero() {
        return format!("0 {symbol}");
    }
    let (_, f) = format_token_balance(tok.amount_raw, tok.decimals);
    let sign = if recv { "+" } else { "−" };
    format!("{sign}{} {symbol}", trim_amount(f))
}

fn trim_amount(f: f64) -> String {
    if f >= 1.0 {
        format!("{f:.4}")
    } else {
        let s = format!("{f:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// "2026-05-01 10:49 UTC". The activity row already shows the relative
/// "3 hrs ago" form; this modal adds an absolute timestamp so the user
/// can correlate against external records (block explorer, exchange
/// statements, etc.).
fn format_when(unix_secs: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let abs = format_iso_utc(unix_secs);
    if now == 0 || unix_secs > now {
        return abs;
    }
    let diff = now.saturating_sub(unix_secs);
    let relative = if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{} min ago", diff / 60)
    } else if diff < 86_400 {
        let hrs = diff / 3600;
        format!("{hrs} hr{} ago", if hrs == 1 { "" } else { "s" })
    } else {
        let days = diff / 86_400;
        format!("{days} day{} ago", if days == 1 { "" } else { "s" })
    };
    format!("{abs} ({relative})")
}

/// `YYYY-MM-DD HH:MM UTC` from unix seconds. Reuses the same civil-day
/// algorithm the indexer already trusts for parsing — kept inline so we
/// don't drag a chrono dependency in for a single label.
fn format_iso_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

/// Inverse of `days_from_civil` from the indexer module — Howard
/// Hinnant's public-domain algorithm. Accepts proleptic Gregorian days
/// since 1970-01-01 and returns (year, month, day).
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Build a `(url, label)` pair for the active indexer's web explorer.
/// Blockscout users get the configured Blockscout instance; everyone
/// else falls back to Etherscan, which is the universal default the
/// existing Send pane already links to.
fn explorer_for(tx: &IndexedTx) -> (String, String) {
    let hash = format!("{:#x}", tx.hash);
    match settings::indexer_provider() {
        IndexerProvider::Blockscout => {
            let base = settings::blockscout_base_url()
                .unwrap_or_else(|| "https://eth.blockscout.com".to_string());
            let trimmed = base.trim_end_matches('/');
            (format!("{trimmed}/tx/{hash}"), "Blockscout".to_string())
        }
        _ => (
            format!("https://etherscan.io/tx/{hash}"),
            "Etherscan".to_string(),
        ),
    }
}
