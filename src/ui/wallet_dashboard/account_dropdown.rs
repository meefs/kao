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

use crate::ui::kao_theme::KaoTheme;
use crate::ui::kao_widgets::{
    avatar, black, bold, ghost_button, hover_tint, kao_scrollable_style, kaomoji_for_account, mono,
    thin_divider,
};
use crate::wallet::{AccountDescriptor, account_short_address};

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
        active_index: usize,
    ) -> Element<'a, Message> {
        // Backdrop: full-window mouse_area to catch outside clicks.
        let backdrop = mouse_area(
            container(Space::new().width(Length::Fill).height(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_press(Message::Close);

        // Build the panel: one row per account + an "Add new address" row.
        let mut list = column![].spacing(2).width(Length::Fill);
        for (idx, account) in accounts.iter().enumerate() {
            list = list.push(account_row(t, idx, account, active_index));
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

    let info = column![
        row![
            text(label).size(13).color(t.text).font(bold()),
            Space::new().width(8),
            text(kind).size(10).color(t.sub).font(mono()),
        ]
        .align_y(Alignment::Center),
        text(addr_text).size(11).color(t.sub).font(mono()),
    ]
    .spacing(1);

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
