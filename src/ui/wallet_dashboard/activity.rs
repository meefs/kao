//! Activity pane — transaction list backed by the configured indexer.
//!
//! Counterparty addresses surface as short `0x….` strings; reverse-ENS is
//! deliberately *not* applied here without forward verification, since
//! unverified reverse records are owner-controlled and impersonate
//! arbitrary names. If we later want ENS labels on this pane, the lookup
//! must go through `crate::ens::lookup_address`.

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::Address;
use iced::border::Radius;
use iced::widget::{Space, button, column, container, row, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding};

use crate::chain::Chain;
use crate::indexer::{IndexedTx, TokenTransfer, TxDirection};
use crate::portfolio::format_token_balance;
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{avatar, bold, hover_tint, kao_scrollable_style, mono, mono_black};
use crate::wallet::{ContactsBook, short_address};

use super::Message;

const RECV_KAO: &str = "(っ◕‿◕)っ";
const SEND_KAO: &str = "ᕕ( ᐛ )ᕗ";
const SELF_KAO: &str = "(･ω･)ﾉ";

pub fn view<'a>(
    t: KaoTheme,
    owner: Address,
    txs: &'a [IndexedTx],
    loading: bool,
    error: Option<&'a str>,
    contacts: &ContactsBook,
) -> Element<'a, Message> {
    let body: Element<'_, Message> = if loading {
        container(text("Loading activity…").size(13).color(t.sub))
            .padding(Padding::from([20, 0]))
            .width(Length::Fill)
            .center_x(Length::Fill)
            .into()
    } else if txs.is_empty() && error.is_some() {
        // Both the indexer and the on-chain fallback failed — give the
        // user a way out instead of a misleading "No transactions yet".
        let msg = text("Couldn't load activity.").size(13).color(t.sub);
        let retry = button(text("Retry").size(13).color(t.text).font(bold()))
            .padding(Padding::from([6, 14]))
            .on_press(Message::RetryHistory)
            .style(move |_theme, status| button::Style {
                background: Some(Background::Color(match status {
                    button::Status::Hovered | button::Status::Pressed => hover_tint(t.card, t.text),
                    _ => t.card,
                })),
                text_color: t.text,
                border: Border {
                    color: t.border,
                    width: 1.0,
                    radius: Radius::from(10),
                },
                ..button::Style::default()
            });
        column![msg, Space::new().height(10), retry]
            .align_x(Alignment::Center)
            .padding(Padding::from([20, 0]))
            .width(Length::Fill)
            .into()
    } else if txs.is_empty() {
        container(text("No transactions yet.").size(13).color(t.sub))
            .padding(Padding::from([20, 0]))
            .width(Length::Fill)
            .center_x(Length::Fill)
            .into()
    } else {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut col = column![].spacing(5);
        for (idx, tx) in txs.iter().enumerate() {
            col = col.push(tx_row(t, owner, tx, now, idx, contacts));
        }
        col.into()
    };

    iced::widget::scrollable(
        container(body)
            .padding(Padding::from([22, 24]))
            .width(Length::Fill),
    )
    .height(Length::Fill)
    .width(Length::Fill)
    .style(move |_, s| kao_scrollable_style(t, s))
    .into()
}

fn tx_row<'a>(
    t: KaoTheme,
    owner: Address,
    tx: &IndexedTx,
    now: u64,
    idx: usize,
    contacts: &ContactsBook,
) -> Element<'a, Message> {
    let recv = matches!(tx.direction, TxDirection::In | TxDirection::SelfTransfer);
    let (ab, kao) = match tx.direction {
        TxDirection::In => (t.ab3, RECV_KAO),
        TxDirection::Out => (t.ab1, SEND_KAO),
        TxDirection::SelfTransfer => (t.ab2, SELF_KAO),
    };

    let counterparty_addr = match tx.direction {
        TxDirection::In => tx.from,
        TxDirection::Out => tx.to.unwrap_or(owner),
        TxDirection::SelfTransfer => owner,
    };
    let counterparty = if tx.to.is_none() && matches!(tx.direction, TxDirection::Out) {
        "contract creation".to_string()
    } else {
        // Prefer the saved contact name when one matches; fall back to
        // the short hex form for unknown counterparties.
        contacts
            .name_for(counterparty_addr)
            .map(|n| n.to_string())
            .unwrap_or_else(|| short_address(counterparty_addr))
    };

    let label = match tx.direction {
        TxDirection::In => format!("From {counterparty}"),
        TxDirection::Out => format!("To {counterparty}"),
        TxDirection::SelfTransfer => format!("Self {counterparty}"),
    };

    let amount = format_amount(tx, recv);
    let nonzero = match &tx.token {
        Some(tok) => !tok.amount_raw.is_zero(),
        None => !tx.value.is_zero(),
    };
    let amount_color = if recv && nonzero { t.up } else { t.text };

    let meta_row = row![
        chain_chip(t, tx.chain),
        Space::new().width(6),
        text(format_relative(now, tx.timestamp))
            .size(11)
            .color(t.sub),
    ]
    .align_y(Alignment::Center);
    let left = column![
        text(label).size(14).color(t.text).font(bold()),
        Space::new().height(2),
        meta_row,
    ]
    .spacing(0);

    let right = column![
        text(amount).size(14).color(amount_color).font(mono_black()),
        text(tx.method.clone().unwrap_or_else(|| "transfer".into()))
            .size(11)
            .color(t.sub)
            .font(mono()),
    ]
    .align_x(Alignment::End);

    let row = row![
        avatar(t, kao, 40.0, ab),
        Space::new().width(13),
        column![left].width(Length::Fill),
        right,
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);

    // Card-shaped click target: same idle look as `card_style` but the
    // whole row is now a button so hover paints the canonical tint and
    // press → opens the details modal for this row's index.
    button(row)
        .padding(Padding::from([13, 15]))
        .width(Length::Fill)
        .on_press(Message::OpenTxDetails(idx))
        .style(move |_theme, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Hovered | button::Status::Pressed => hover_tint(t.card, t.text),
                _ => t.card,
            })),
            text_color: t.text,
            border: Border {
                color: t.border,
                width: 1.0,
                radius: Radius::from(14),
            },
            ..button::Style::default()
        })
        .into()
}

/// Render the amount column. ERC-20 rows use the token's symbol +
/// decimals; native rows fall back to ETH wei. Zero-value pure-ETH rows
/// collapse to `0 ETH` without a sign so they don't masquerade as a real
/// transfer (contract calls, approvals, etc.).
fn format_amount(tx: &IndexedTx, recv: bool) -> String {
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
        if tok.is_nft { "NFT".to_string() } else { "tokens".to_string() }
    } else {
        tok.symbol.clone()
    };
    if tok.is_nft {
        // ERC-721: render as `SYMBOL #N` (or just `#N` for unknowns).
        let id = tok
            .token_id
            .map(|id| format!("#{id}"))
            .unwrap_or_default();
        let sign = if recv { "+" } else { "−" };
        return format!("{sign}{symbol} {id}").trim_end().to_string();
    }
    if tok.amount_raw.is_zero() {
        return format!("0 {symbol}");
    }
    let (_, f) = format_token_balance(tok.amount_raw, tok.decimals);
    let sign = if recv { "+" } else { "−" };
    format!("{sign}{} {symbol}", trim_amount(f))
}

/// 4 decimals for >=1 amounts, up to 6 for sub-1 with trailing zeros
/// stripped. Used by both the native ETH and token branches so the
/// activity feed has one consistent number style.
fn trim_amount(f: f64) -> String {
    if f >= 1.0 {
        format!("{f:.4}")
    } else {
        let s = format!("{f:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Per-chain accent color for the badge tint. Mainnet stays in the
/// theme's neutral sub color (signals "this is the default"); L2s pull
/// from the kao accent palette so the merged feed reads at a glance.
fn chain_tint(t: KaoTheme, chain: Chain) -> Color {
    match chain {
        Chain::Mainnet => t.sub,
        Chain::Base => t.a1,
        Chain::Optimism => t.a2,
    }
}

/// Small filled chip showing the chain's short label. Sized to match
/// the secondary metadata row so it doesn't visually outweigh the
/// counterparty line.
fn chain_chip<'a>(t: KaoTheme, chain: Chain) -> Element<'a, Message> {
    let accent = chain_tint(t, chain);
    container(
        text(chain.label().to_string())
            .size(10)
            .color(accent)
            .font(bold()),
    )
    .padding(Padding::from([2, 6]))
    .style(move |_| iced::widget::container::Style {
        background: Some(Background::Color(with_alpha(accent, 0.12))),
        border: Border {
            color: with_alpha(accent, 0.3),
            width: 1.0,
            radius: Radius::from(6),
        },
        ..iced::widget::container::Style::default()
    })
    .into()
}

/// "2 min ago" / "3 hrs ago" / "Yesterday" / "5 days ago". Stays roughly
/// in sync with the demo strings; precision past a day isn't worth the
/// extra strings.
fn format_relative(now: u64, then: u64) -> String {
    if then == 0 || then > now {
        return "just now".into();
    }
    let diff = now - then;
    if diff < 60 {
        return "just now".into();
    }
    let mins = diff / 60;
    if mins < 60 {
        return format!("{mins} min ago");
    }
    let hrs = mins / 60;
    if hrs < 24 {
        return format!("{hrs} hr{} ago", if hrs == 1 { "" } else { "s" });
    }
    let days = hrs / 24;
    if days == 1 {
        return "Yesterday".into();
    }
    format!("{days} days ago")
}
