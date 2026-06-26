//! Swap modal — surface 1 of the CoW integration. Wraps the shared
//! [`SwapComposer`] for token/amount entry + quote, then **blocks** on the
//! placed order until it settles: after the user confirms, the modal stays open
//! showing live status (waiting → filled/expired/cancelled) rather than handing
//! off to a background list (that's the Apps pane's job, surface 2).
//!
//! All network + signing work lives in the coordinator; this pane only emits
//! [`Outcome`]s (quote/place requests) and renders the phase it's been put into.

use iced::border::Radius;
use iced::keyboard;
use iced::widget::{Space, button, column, container, row, scrollable, text};
use iced::{Alignment, Background, Border, Element, Length, Padding, Subscription, Task};

use crate::cow::api::QuoteResponse;
use crate::cow::composer::{self, SwapComposer, SwapDraft};
use crate::cow::tracked::{OrderStatus, TrackedOrder};
use crate::portfolio::{LiveToken, format_token_balance};
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{
    bold, ghost_button, kao_scrollable_style, modal_wrapper, mono, mono_bold, primary_button,
};

const MODAL_WIDTH: f32 = 560.0;

/// Max height of the scrolling form region inside the modal. Header (title) and
/// the action buttons stay pinned outside it, so they're always reachable even
/// when the token lists + quote make the form taller than the window.
const FORM_MAX_HEIGHT: f32 = 460.0;

#[derive(Debug, Clone)]
pub enum Message {
    Composer(composer::Message),
    /// Cancel the order being tracked (off-chain signed cancel; ERC-20 only).
    CancelOrder(String),
    Close,
    BoxClickIgnored,
    Key(keyboard::Event),
}

// One-shot outcome; the QuoteResponse size gap isn't worth boxing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Outcome {
    /// User asked to dismiss; coordinator runs the close transition.
    Closed,
    /// Fetch a quote for this draft (the first network call — explicit).
    RequestQuote(SwapDraft),
    /// Place the quoted order (approve+sign+post, or EthFlow createOrder).
    RequestPlace {
        draft: SwapDraft,
        quote: QuoteResponse,
    },
    /// Cancel the tracked order off-chain (the coordinator signs + DELETEs it).
    RequestCancel { uid: String },
}

#[derive(Debug)]
enum Phase {
    /// Picking tokens / amount / quote.
    Compose,
    /// Placement in flight (approval + signing + submission).
    Placing,
    /// Order placed; blocking on settlement. Holds the order UID; the
    /// coordinator threads the live [`TrackedOrder`] into `view`.
    Tracking(String),
}

#[derive(Debug)]
pub struct SwapPane {
    composer: SwapComposer,
    phase: Phase,
}

impl SwapPane {
    pub fn new() -> Self {
        Self {
            composer: SwapComposer::new(),
            phase: Phase::Compose,
        }
    }

    /// The UID this modal is blocking on, if any.
    pub fn tracking_uid(&self) -> Option<&str> {
        match &self.phase {
            Phase::Tracking(uid) => Some(uid.as_str()),
            _ => None,
        }
    }

    /// Coordinator feeds a quote (or its error) back into the composer.
    pub fn on_quote(&mut self, result: Result<QuoteResponse, String>) {
        self.composer.update(composer::Message::QuoteResult(result));
    }

    /// Placement succeeded — switch to blocking status tracking.
    pub fn begin_tracking(&mut self, uid: String) {
        self.phase = Phase::Tracking(uid);
    }

    /// Placement failed — return to the composer with the error shown.
    pub fn placement_failed(&mut self, e: String) {
        self.composer.set_error(e);
        self.phase = Phase::Compose;
    }

    pub fn update(&mut self, msg: Message) -> (Task<Message>, Option<Outcome>) {
        match msg {
            Message::Composer(cm) => {
                if !matches!(self.phase, Phase::Compose) {
                    return (Task::none(), None);
                }
                match self.composer.update(cm) {
                    Some(composer::Outcome::RequestQuote(draft)) => {
                        (Task::none(), Some(Outcome::RequestQuote(draft)))
                    }
                    Some(composer::Outcome::RequestPlace { draft, quote }) => {
                        self.phase = Phase::Placing;
                        (Task::none(), Some(Outcome::RequestPlace { draft, quote }))
                    }
                    None => (Task::none(), None),
                }
            }
            Message::CancelOrder(uid) => (Task::none(), Some(Outcome::RequestCancel { uid })),
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
        &'a self,
        t: KaoTheme,
        portfolio: &'a [LiveToken],
        tracked: Option<&'a TrackedOrder>,
        progress: f32,
    ) -> Element<'a, Message> {
        let (content, dismissable): (Element<'a, Message>, bool) = match &self.phase {
            Phase::Compose => (self.compose_view(t, portfolio), true),
            Phase::Placing => (placing_view(t), false),
            Phase::Tracking(_) => tracking_view(t, tracked),
        };

        // While placement or non-terminal settlement is in flight, the backdrop
        // is inert (surface 1 "blocks until settled"); Compose and a terminal
        // result dismiss normally.
        let on_backdrop = if dismissable {
            Message::Close
        } else {
            Message::BoxClickIgnored
        };

        modal_wrapper(
            t,
            MODAL_WIDTH,
            progress,
            on_backdrop,
            Message::BoxClickIgnored,
            content,
        )
    }

    fn compose_view<'a>(&'a self, t: KaoTheme, portfolio: &'a [LiveToken]) -> Element<'a, Message> {
        let header = column![
            row![
                text("(⇌ω⇌)").size(22).color(t.a3),
                Space::new().width(8),
                text("Swap").size(20).color(t.text).font(bold()),
                Space::new().width(Length::Fill),
                ghost_button(t, text("✕").size(15).color(t.sub)).on_press(Message::Close),
            ]
            .align_y(Alignment::Center),
            Space::new().height(6),
            text("Powered by CoW Protocol · MEV-protected · ERC-20 orders are gasless")
                .size(11)
                .color(t.sub)
                .font(mono()),
        ]
        .width(Length::Fill);

        // The form scrolls within a bounded height; the header and the action
        // buttons stay pinned so "Get quote" / "Place order" are always visible.
        let form = self.composer.view_body(t, portfolio).map(Message::Composer);
        let scroll_body = container(
            scrollable(container(form).padding(Padding {
                top: 0.0,
                right: 12.0,
                bottom: 0.0,
                left: 0.0,
            }))
            .height(Length::Shrink)
            .style(move |_, s| kao_scrollable_style(t, s)),
        )
        .max_height(FORM_MAX_HEIGHT);

        let actions = self.composer.view_actions(t).map(Message::Composer);

        column![
            header,
            Space::new().height(14),
            scroll_body,
            Space::new().height(14),
            actions,
        ]
        .width(Length::Fill)
        .into()
    }
}

fn placing_view<'a>(t: KaoTheme) -> Element<'a, Message> {
    let step = |s: &'static str| text(s).size(12).color(t.sub).center();
    column![
        text("(づ｡◕‿‿◕｡)づ").size(34).color(t.a1).center(),
        Space::new().height(12),
        text("Placing your order…")
            .size(16)
            .color(t.text)
            .font(bold())
            .center(),
        Space::new().height(10),
        // Spell out the on-chain/off-chain steps so it's clear where approval
        // and signing happen — they run automatically here (your device will
        // prompt for hardware wallets).
        step("1. Approve token for the CoW vault relayer (first time only)"),
        step("2. Sign your order (EIP-712)"),
        step("3. Submit to CoW — solvers settle it on-chain"),
        Space::new().height(8),
        text("Confirm on your device if prompted.")
            .size(11)
            .color(t.sub)
            .center(),
    ]
    .spacing(3)
    .width(Length::Fill)
    .align_x(Alignment::Center)
    .into()
}

/// Returns the tracking content and whether the modal is dismissable (true only
/// once the order is in a terminal state).
fn tracking_view<'a>(
    t: KaoTheme,
    tracked: Option<&'a TrackedOrder>,
) -> (Element<'a, Message>, bool) {
    let Some(o) = tracked else {
        // Order vanished from the list (shouldn't happen) — let the user out.
        let body = column![
            text("Order placed")
                .size(16)
                .color(t.text)
                .font(bold())
                .center(),
            Space::new().height(14),
            primary_button(t, "Close", true).on_press(Message::Close),
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);
        return (body.into(), true);
    };

    let (sell_s, _) = format_token_balance(o.sell_amount, o.sell_decimals);
    let pair = text(format!("{sell_s} {} → {}", o.sell_symbol, o.buy_symbol))
        .size(13)
        .color(t.sub)
        .font(mono());

    let terminal = o.status.is_terminal();
    let (face, headline) = match o.status {
        OrderStatus::Fulfilled => ("(◕‿◕)♡", "Swapped!"),
        OrderStatus::Cancelled => ("(._.)", "Order cancelled"),
        OrderStatus::Expired => ("(；一_一)", "Order expired"),
        OrderStatus::Open | OrderStatus::PresignaturePending => ("(｡･ω･｡)", "Waiting for solvers…"),
    };

    let mut body = column![
        text(face).size(34).color(t.a1).center(),
        Space::new().height(10),
        text(headline).size(17).color(t.text).font(bold()).center(),
        Space::new().height(6),
        pair,
        Space::new().height(12),
        status_badge(t, o.status),
    ]
    .width(Length::Fill)
    .align_x(Alignment::Center);

    if let (OrderStatus::Fulfilled, Some((_, got))) = (o.status, o.executed) {
        let (got_s, _) = format_token_balance(got, o.buy_decimals);
        body = body.push(Space::new().height(8));
        body = body.push(
            text(format!("Received {got_s} {}", o.buy_symbol))
                .size(13)
                .color(t.up)
                .font(bold()),
        );
    }

    body = body.push(Space::new().height(18));
    if terminal {
        body = body.push(primary_button(t, "Done", true).on_press(Message::Close));
    } else {
        body = body.push(
            text("CoW solvers are matching your order. This can take a little while.")
                .size(11)
                .color(t.sub)
                .center(),
        );
        // Off-chain cancel is wired for ERC-20 orders only; EthFlow orders
        // cancel on-chain (deferred), so they show no cancel here.
        if !o.is_ethflow {
            body = body.push(Space::new().height(12));
            body = body.push(
                button(text("Cancel order").size(12).color(t.down).font(bold()))
                    .padding(Padding::from([6, 14]))
                    .on_press(Message::CancelOrder(o.uid.clone()))
                    .style(move |_, status| button::Style {
                        background: Some(Background::Color(match status {
                            button::Status::Hovered | button::Status::Pressed => {
                                with_alpha(t.down, 0.10)
                            }
                            _ => t.card_alt,
                        })),
                        text_color: t.down,
                        border: Border {
                            color: with_alpha(t.down, 0.3),
                            width: 1.0,
                            radius: Radius::from(9),
                        },
                        ..button::Style::default()
                    }),
            );
        }
        body = body.push(Space::new().height(10));
        body = body.push(
            ghost_button(
                t,
                text("Hide & track in Apps")
                    .size(12)
                    .color(t.sub)
                    .font(bold()),
            )
            .on_press(Message::Close),
        );
    }

    (body.into(), terminal)
}

fn status_badge<'a>(t: KaoTheme, status: OrderStatus) -> Element<'a, Message> {
    let (fg, bg) = match status {
        OrderStatus::Fulfilled => (t.up, with_alpha(t.up, 0.12)),
        OrderStatus::Cancelled | OrderStatus::Expired => (t.down, with_alpha(t.down, 0.12)),
        OrderStatus::Open | OrderStatus::PresignaturePending => (t.sub, with_alpha(t.sub, 0.12)),
    };
    container(text(status.label()).size(11).color(fg).font(mono_bold()))
        .padding(Padding::from([3, 9]))
        .style(move |_| container::Style {
            background: Some(Background::Color(bg)),
            border: Border {
                color: with_alpha(fg, 0.25),
                width: 1.0,
                radius: Radius::from(8),
            },
            text_color: Some(fg),
            ..container::Style::default()
        })
        .into()
}
