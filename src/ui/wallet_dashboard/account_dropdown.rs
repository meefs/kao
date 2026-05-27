//! Account picker overlay anchored under the header address pill. Lists all
//! accounts in the unlocked wallet and an "Add new address" action.
//!
//! TEA component: owns no internal state today (it's a pure view of the
//! coordinator's accounts + active_index), but wrapping it in the standard
//! shape keeps the coordinator's `Message` enum decoupled from the dropdown's
//! internal events and gives the dropdown its own keyboard subscription.

use iced::border::Radius;
use iced::keyboard;
use iced::widget::{Space, button, column, container, mouse_area, row, scrollable, stack, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Subscription, Task};

use crate::chain::Chain;
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{
    avatar, black, bold, ghost_button, hover_tint, kao_scrollable_style, kaomoji_for_account, mono,
    thin_divider,
};
use crate::wallet::{
    AccountDescriptor, SafeDescriptor, account_is_safe_signer, account_short_address, short_address,
};

#[derive(Debug, Clone)]
pub enum Message {
    Select(usize),
    Add,
    Close,
    BoxClickIgnored,
    Key(keyboard::Event),
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Switch(usize),
    Add,
    Closed,
}

#[derive(Debug, Default)]
pub struct AccountDropdown;

impl AccountDropdown {
    pub fn new() -> Self {
        Self
    }

    pub fn update(&mut self, msg: Message) -> (Task<Message>, Option<Outcome>) {
        match msg {
            Message::Select(idx) => (Task::none(), Some(Outcome::Switch(idx))),
            Message::Add => (Task::none(), Some(Outcome::Add)),
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

    pub fn view<'a>(
        &self,
        t: KaoTheme,
        accounts: &'a [AccountDescriptor],
        safes: &'a [SafeDescriptor],
        active_index: usize,
    ) -> Element<'a, Message> {
        // Backdrop: full-window mouse_area to catch outside clicks.
        let backdrop = mouse_area(
            container(Space::new().width(Length::Fill).height(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_press(Message::Close);

        // Build the panel: one row per account, then any Safes inline
        // (matching the user's flow language "The Safe then appears in
        // the account list"), then "Add new address". Safes carry no
        // signing handle yet in 3b, so they render as non-clickable
        // info rows — the affordance comes back when the Safe-TX flow
        // lands in a future stage.
        let mut list = column![].spacing(2).width(Length::Fill);
        for (idx, account) in accounts.iter().enumerate() {
            let is_signer = account_is_safe_signer(idx as u32, safes);
            list = list.push(account_row(t, idx, account, active_index, is_signer));
        }
        for (idx, safe) in safes.iter().enumerate() {
            list = list.push(safe_row(t, idx, safe));
        }
        list = list.push(thin_divider(t));
        list = list.push(add_account_row(t));

        let panel = container(
            scrollable(list)
                .height(Length::Shrink)
                .style(move |_, s| kao_scrollable_style(t, s)),
        )
        .padding(Padding::from([10, 8]))
        .width(Length::Fixed(296.0))
        .max_height(360.0)
        .style(move |_| container::Style {
            background: Some(Background::Color(t.card)),
            border: Border {
                color: t.border,
                width: 1.0,
                radius: Radius::from(14),
            },
            text_color: Some(t.text),
            shadow: iced::Shadow {
                color: Color::from_rgba(0.0, 0.0, 0.0, if t.dark { 0.6 } else { 0.16 }),
                offset: iced::Vector::new(0.0, 12.0),
                blur_radius: 32.0,
            },
            ..container::Style::default()
        });

        // Anchor the panel under the header address. Sidebar width is 100,
        // header padding is [14, 24] and the address pill sits below the
        // title. ~78 from window top, ~124 from window left lands under the
        // trigger.
        let layer = container(column![
            Space::new().height(78.0),
            row![
                Space::new().width(124.0),
                mouse_area(panel).on_press(Message::BoxClickIgnored),
                Space::new().width(Length::Fill),
            ],
            Space::new().height(Length::Fill),
        ])
        .width(Length::Fill)
        .height(Length::Fill);

        stack![backdrop, layer].into()
    }
}

fn account_row<'a>(
    t: KaoTheme,
    idx: usize,
    account: &'a AccountDescriptor,
    active_index: usize,
    is_safe_signer: bool,
) -> Element<'a, Message> {
    let active = idx == active_index;
    let kao = kaomoji_for_account(idx);
    let label = account.display_name(idx);
    let addr_text = account_short_address(account);
    let kind = match account {
        AccountDescriptor::Local { .. } => "Local",
        AccountDescriptor::Ledger { .. } => "Ledger",
        AccountDescriptor::Trezor { .. } => "Trezor",
        AccountDescriptor::ViewOnly { .. } => "View Only",
    };

    let mut kind_row = row![
        text(label).size(13).color(t.text).font(bold()),
        Space::new().width(8),
        text(kind).size(10).color(t.sub).font(mono()),
    ]
    .align_y(Alignment::Center);
    if is_safe_signer {
        // Tiny accent-tinted chip telling the user this account is
        // load-bearing for at least one Safe in the wallet.
        kind_row = kind_row
            .push(Space::new().width(6))
            .push(text("Safe signer").size(10).color(t.a1).font(mono()));
    }
    let info = column![kind_row, text(addr_text).size(11).color(t.sub).font(mono())].spacing(1);

    let check = if active {
        text("◉").size(14).color(t.a1)
    } else {
        text("○").size(14).color(t.sub)
    };

    let inner = row![
        avatar(t, kao, 32.0, t.ab1),
        Space::new().width(10),
        container(info).width(Length::Fill),
        check,
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);

    let bg = if active { t.ab1 } else { Color::TRANSPARENT };
    button(inner)
        .padding(Padding::from([6, 8]))
        .width(Length::Fill)
        .on_press(Message::Select(idx))
        .style(move |_theme, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Hovered | button::Status::Pressed => hover_tint(bg, t.text),
                _ => bg,
            })),
            text_color: t.text,
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: Radius::from(10),
            },
            ..button::Style::default()
        })
        .into()
}

/// Render one Safe row. Non-clickable in 3b — no Safe-TX flow exists
/// yet, so clicking would be a false affordance. We surface presence,
/// the watch-only state, and which chain the deployment is on; the row
/// becomes interactive when the Safe-TX flow lands.
fn safe_row<'a>(t: KaoTheme, idx: usize, safe: &'a SafeDescriptor) -> Element<'a, Message> {
    let watch_only = safe.linked_signer_indices.is_empty();
    let label = safe.display_name(idx);
    let chain_label = chain_label_for_id(safe.chain_id);
    let badge_text = if watch_only { "Safe (watch)" } else { "Safe" };
    // Watch-only Safes get muted text + a paler tinted background to
    // make it visually clear they're observe-only — same logic as
    // ViewOnly accounts but stronger since a Safe also doesn't have
    // the "switch active" affordance an account has.
    let (text_color, sub_color, bg_color) = if watch_only {
        (t.sub, with_alpha(t.sub, 0.7), with_alpha(t.ab2, 0.3))
    } else {
        (t.text, t.sub, with_alpha(t.ab1, 0.5))
    };

    let kind_row = row![
        text(label).size(13).color(text_color).font(bold()),
        Space::new().width(8),
        text(badge_text).size(10).color(t.a2).font(mono()),
        Space::new().width(6),
        text(chain_label).size(10).color(sub_color).font(mono()),
    ]
    .align_y(Alignment::Center);
    let info = column![
        kind_row,
        text(short_address(safe.address()))
            .size(11)
            .color(sub_color)
            .font(mono()),
    ]
    .spacing(1);

    // (◐‿◐) for signing Safes (two-eyes hint at multi-sig); (◐_◐) for
    // watch-only (same shape but a flat mouth — no "joy of signing").
    let kao = if watch_only {
        "(◐_◐)"
    } else {
        "(◐‿◐)"
    };
    let inner = row![
        avatar(t, kao, 32.0, t.ab2),
        Space::new().width(10),
        container(info).width(Length::Fill),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);

    container(inner)
        .padding(Padding::from([6, 8]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(bg_color)),
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: Radius::from(10),
            },
            text_color: Some(text_color),
            ..container::Style::default()
        })
        .into()
}

fn chain_label_for_id(chain_id: u64) -> &'static str {
    // Map back to Chain so we render the display label the rest of the
    // app uses ("Ethereum Mainnet" / "OP Mainnet" / "Base"). An
    // unknown chain_id (e.g. a future addition saved by a newer Kao)
    // renders as "?" — the row is still readable and the staleness is
    // visible.
    Chain::ALL
        .iter()
        .find(|c| c.chain_id() == chain_id)
        .map(|c| c.display_name())
        .unwrap_or("?")
}

fn add_account_row<'a>(t: KaoTheme) -> Element<'a, Message> {
    let inner = row![
        text("＋").size(15).color(t.a1).font(black()),
        Space::new().width(10),
        text("Add new address").size(13).color(t.text).font(bold()),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);

    ghost_button(t, inner)
        .padding(Padding::from([8, 8]))
        .width(Length::Fill)
        .on_press(Message::Add)
        .into()
}
