//! Activity pane — transaction list. Demo data today; the eventual fetch
//! pipeline lands here.

use iced::widget::{Space, column, container, row, text};
use iced::{Alignment, Element, Length, Padding};

use crate::ui::kao_theme::KaoTheme;
use crate::ui::kao_widgets::{avatar, bold, card_style, mono, mono_black};

use super::Message;

#[derive(Debug, Clone, Copy)]
struct Tx {
    recv: bool,
    counterparty: &'static str,
    amount: &'static str,
    usd: &'static str,
    ago: &'static str,
    kao: &'static str,
}

const TRANSACTIONS: &[Tx] = &[
    Tx {
        recv: true,
        counterparty: "vitalik.eth",
        amount: "+0.5 ETH",
        usd: "$1,658.12",
        ago: "2 min ago",
        kao: "(っ◕‿◕)っ",
    },
    Tx {
        recv: false,
        counterparty: "0x742d…35Cc",
        amount: "−100 USDC",
        usd: "$100.00",
        ago: "1 hr ago",
        kao: "ᕕ( ᐛ )ᕗ",
    },
    Tx {
        recv: true,
        counterparty: "opensea.io",
        amount: "+0.08 ETH",
        usd: "$265.30",
        ago: "3 hrs ago",
        kao: "(っ◕‿◕)っ",
    },
    Tx {
        recv: false,
        counterparty: "friend.eth",
        amount: "−0.1 ETH",
        usd: "$331.62",
        ago: "Yesterday",
        kao: "ᕕ( ᐛ )ᕗ",
    },
    Tx {
        recv: true,
        counterparty: "uniswap",
        amount: "+42 LINK",
        usd: "$632.82",
        ago: "2 days ago",
        kao: "(っ◕‿◕)っ",
    },
    Tx {
        recv: false,
        counterparty: "0xDEad…b00b",
        amount: "−250 USDC",
        usd: "$250.00",
        ago: "3 days ago",
        kao: "ᕕ( ᐛ )ᕗ",
    },
];

pub fn view<'a>(t: KaoTheme) -> Element<'a, Message> {
    let mut col = column![].spacing(5);
    for tx in TRANSACTIONS {
        col = col.push(tx_row(t, *tx));
    }
    iced::widget::scrollable(
        container(col)
            .padding(Padding::from([22, 24]))
            .width(Length::Fill),
    )
    .height(Length::Fill)
    .width(Length::Fill)
    .into()
}

fn tx_row<'a>(t: KaoTheme, tx: Tx) -> Element<'a, Message> {
    let ab = if tx.recv { t.ab3 } else { t.ab1 };
    let label = if tx.recv {
        format!("From {}", tx.counterparty)
    } else {
        format!("To {}", tx.counterparty)
    };
    let left = column![
        text(label).size(14).color(t.text).font(bold()),
        text(tx.ago).size(11).color(t.sub),
    ]
    .spacing(0);

    let amount_color = if tx.recv { t.up } else { t.text };
    let right = column![
        text(tx.amount)
            .size(14)
            .color(amount_color)
            .font(mono_black()),
        text(tx.usd).size(11).color(t.sub).font(mono()),
    ]
    .align_x(Alignment::End);

    let row = row![
        avatar(t, tx.kao, 40.0, ab),
        Space::new().width(13),
        column![left].width(Length::Fill),
        right,
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);

    container(row)
        .padding(Padding::from([13, 15]))
        .width(Length::Fill)
        .style(move |_| card_style(t))
        .into()
}
