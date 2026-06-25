//! Apps pane — placeholder for the future on-chain apps browser. The nav
//! slot and this pane exist so the sidebar's "Apps" row has a real
//! destination; the catalogue itself lands in a later stage.

use iced::widget::{Space, column, container};
use iced::{Alignment, Element, Length};

use crate::ui::kao_theme::KaoTheme;
use crate::ui::kao_widgets::{kao_hero, screen_subtitle, screen_title};

use super::Message;

pub fn view<'a>(t: KaoTheme) -> Element<'a, Message> {
    let body = column![
        kao_hero(t, "( ◞･㉨･)", 64.0),
        Space::new().height(18),
        screen_title(t, "On-chain apps"),
        Space::new().height(8),
        screen_subtitle(
            t,
            "Curated dapps land here soon — coming in a future update."
        ),
    ]
    .align_x(Alignment::Center)
    .width(Length::Fill);

    container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}
