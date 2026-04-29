//! Swap modal — placeholder for the swap flow. Renders a "not released yet"
//! card with a single Close button. Establishes the modal-with-chrome routing
//! pattern shared by `receive` and `send`.

use iced::keyboard;
use iced::widget::{Space, column, text};
use iced::{Alignment, Element, Length, Subscription, Task};

use crate::ui::kao_theme::KaoTheme;
use crate::ui::kao_widgets::{bold, modal_wrapper, primary_button};

#[derive(Debug, Clone)]
pub enum Message {
    Close,
    BoxClickIgnored,
    Key(keyboard::Event),
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// User asked to dismiss; coordinator should run the close transition.
    Closed,
}

#[derive(Debug, Default)]
pub struct SwapPane;

impl SwapPane {
    pub fn new() -> Self {
        Self
    }

    pub fn update(&mut self, msg: Message) -> (Task<Message>, Option<Outcome>) {
        match msg {
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

    pub fn view<'a>(&self, t: KaoTheme, progress: f32) -> Element<'a, Message> {
        let content = column![
            text("(⇌ω⇌)").size(42).color(t.a3).center(),
            Space::new().height(12),
            text("Swap").size(20).color(t.text).font(bold()).center(),
            Space::new().height(8),
            text("Not released yet — stay tuned!")
                .size(14)
                .color(t.sub)
                .center(),
            Space::new().height(20),
            primary_button(t, "Close ╮(︶▽︶)╭", true).on_press(Message::Close),
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        modal_wrapper(
            t,
            320.0,
            progress,
            Message::Close,
            Message::BoxClickIgnored,
            content.into(),
        )
    }
}
