//! Networks settings sub-screen — exec RPC list, consensus RPC list,
//! checkpoint override. Owned by the dashboard's Settings nav slot, not the
//! modal stack.
//!
//! Save invalidates the shared `BalanceFetcher` so the next balance/portfolio
//! fetch rebuilds Helios against the new endpoints.

use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::B256;
use iced::widget::{Space, button, column, container, mouse_area, row, text, text_input};
use iced::{Alignment, Element, Length, Padding, Subscription, Task};

use crate::net::BalanceFetcher;
use crate::settings;
use crate::ui::kao_theme::KaoTheme;
use crate::ui::kao_widgets::{
    black, bold, primary_button, secondary_button, section, text_input_style,
};

#[derive(Debug, Clone)]
pub enum Message {
    Back,
    RpcChanged(usize, String),
    RpcAdd,
    RpcRemove(usize),
    ConsensusChanged(usize, String),
    ConsensusAdd,
    ConsensusRemove(usize),
    CheckpointChanged(String),
    Save,
    Saved,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// User backed out (or save completed) — coordinator should return to the
    /// settings root pane.
    Closed,
}

#[derive(Debug, Clone, Default)]
struct Draft {
    rpcs: Vec<String>,
    consensus_rpcs: Vec<String>,
    /// Hex of the user's pasted checkpoint override, blank to use auto.
    checkpoint_override: String,
}

#[derive(Debug)]
pub struct NetworksPane {
    draft: Draft,
    network: Arc<dyn BalanceFetcher>,
    saving: bool,
}

impl NetworksPane {
    pub fn new(network: Arc<dyn BalanceFetcher>) -> Self {
        let draft = Draft {
            rpcs: settings::rpcs(),
            consensus_rpcs: settings::consensus_rpcs(),
            checkpoint_override: settings::checkpoint_override()
                .map(|b| format!("0x{}", alloy::hex::encode(b.as_slice())))
                .unwrap_or_default(),
        };
        Self {
            draft,
            network,
            saving: false,
        }
    }

    pub fn update(&mut self, msg: Message) -> (Task<Message>, Option<Outcome>) {
        match msg {
            Message::Back => (Task::none(), Some(Outcome::Closed)),
            Message::RpcChanged(i, s) => {
                if let Some(slot) = self.draft.rpcs.get_mut(i) {
                    *slot = s;
                }
                (Task::none(), None)
            }
            Message::RpcAdd => {
                self.draft.rpcs.push(String::new());
                (Task::none(), None)
            }
            Message::RpcRemove(i) => {
                if i < self.draft.rpcs.len() {
                    self.draft.rpcs.remove(i);
                }
                (Task::none(), None)
            }
            Message::ConsensusChanged(i, s) => {
                if let Some(slot) = self.draft.consensus_rpcs.get_mut(i) {
                    *slot = s;
                }
                (Task::none(), None)
            }
            Message::ConsensusAdd => {
                self.draft.consensus_rpcs.push(String::new());
                (Task::none(), None)
            }
            Message::ConsensusRemove(i) => {
                if i < self.draft.consensus_rpcs.len() {
                    self.draft.consensus_rpcs.remove(i);
                }
                (Task::none(), None)
            }
            Message::CheckpointChanged(s) => {
                self.draft.checkpoint_override = s;
                (Task::none(), None)
            }
            Message::Save => {
                let cleaned_exec: Vec<String> = self
                    .draft
                    .rpcs
                    .iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let cleaned_consensus: Vec<String> = self
                    .draft
                    .consensus_rpcs
                    .iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let checkpoint_override = parse_checkpoint(self.draft.checkpoint_override.trim());
                settings::set_rpcs(cleaned_exec);
                settings::set_consensus_rpcs(cleaned_consensus);
                settings::set_checkpoint_override(checkpoint_override);
                self.saving = true;
                let network = self.network.clone();
                let task = Task::perform(
                    async move {
                        network.invalidate().await;
                    },
                    |_| Message::Saved,
                );
                (task, None)
            }
            Message::Saved => {
                self.saving = false;
                (Task::none(), Some(Outcome::Closed))
            }
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::none()
    }

    pub fn view<'a>(&'a self, t: KaoTheme) -> Element<'a, Message> {
        let header = row![
            mouse_area(
                container(text("← Back").size(12).color(t.sub).font(bold()))
                    .padding(Padding::from([4, 0])),
            )
            .on_press(Message::Back),
            Space::new().width(Length::Fill),
            text("Networks").size(14).color(t.text).font(black()),
            Space::new().width(Length::Fill),
            // Symmetry placeholder so the title centers visually.
            text("    ").size(12),
        ]
        .align_y(Alignment::Center)
        .width(Length::Fill);

        let mut rpc_rows = column![].spacing(6);
        for (i, url) in self.draft.rpcs.iter().enumerate() {
            let input = text_input("https://…", url)
                .on_input(move |s| Message::RpcChanged(i, s))
                .padding(Padding::from([6, 10]))
                .style(move |_theme, status| text_input_style(t, status));
            let remove = button(text("−").size(14).color(t.text).font(bold()))
                .padding(Padding::from([2, 10]))
                .on_press(Message::RpcRemove(i));
            rpc_rows = rpc_rows
                .push(row![input, Space::new().width(8), remove].align_y(Alignment::Center));
        }
        let add_btn = secondary_button(t, "＋ Add RPC").on_press(Message::RpcAdd);
        let exec_section = section(
            t,
            "Execution RPCs",
            "(◕‿◕✿)",
            "Helios picks one at random per session and verifies its responses.",
            column![rpc_rows, Space::new().height(8), add_btn].into(),
        );

        let mut consensus_rows = column![].spacing(6);
        for (i, url) in self.draft.consensus_rpcs.iter().enumerate() {
            let input = text_input("https://…", url)
                .on_input(move |s| Message::ConsensusChanged(i, s))
                .padding(Padding::from([6, 10]))
                .style(move |_theme, status| text_input_style(t, status));
            let remove = button(text("−").size(14).color(t.text).font(bold()))
                .padding(Padding::from([2, 10]))
                .on_press(Message::ConsensusRemove(i));
            consensus_rows = consensus_rows
                .push(row![input, Space::new().width(8), remove].align_y(Alignment::Center));
        }
        let add_consensus_btn =
            secondary_button(t, "＋ Add Consensus RPC").on_press(Message::ConsensusAdd);
        let consensus_section = section(
            t,
            "Consensus RPCs",
            "( ´ ▽ ` )ﾉ",
            "Beacon-chain LC API endpoints. Tried in shuffled order; first one that bootstraps wins.",
            column![consensus_rows, Space::new().height(8), add_consensus_btn].into(),
        );

        let placeholder = format!(
            "auto: 0x{}",
            alloy::hex::encode(settings::auto_checkpoint().as_slice())
        );
        let checkpoint_input = text_input(&placeholder, &self.draft.checkpoint_override)
            .on_input(Message::CheckpointChanged)
            .padding(Padding::from([6, 10]))
            .style(move |_theme, status| text_input_style(t, status));
        let checkpoint_section = section(
            t,
            "Checkpoint",
            "(⌐■_■)",
            "Leave blank to use the bundled checkpoint (or a freshly fetched one if it's stale). Paste a 32-byte hex hash to override.",
            checkpoint_input.into(),
        );

        let valid = draft_valid(&self.draft) && !self.saving;
        let save_btn = primary_button(t, "Save", valid);
        let save_btn = if valid {
            save_btn.on_press(Message::Save)
        } else {
            save_btn
        };
        let save_row = container(save_btn).padding(Padding::from([4, 0]));

        let body = column![
            header,
            Space::new().height(16),
            exec_section,
            Space::new().height(14),
            consensus_section,
            Space::new().height(14),
            checkpoint_section,
            Space::new().height(20),
            save_row,
        ]
        .spacing(0)
        .width(Length::Fill);

        iced::widget::scrollable(
            container(body)
                .padding(Padding::from([22, 24]))
                .width(Length::Fill),
        )
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
    }
}

fn parse_checkpoint(s: &str) -> Option<B256> {
    if s.is_empty() {
        return None;
    }
    B256::from_str(s).ok()
}

fn draft_valid(draft: &Draft) -> bool {
    if !list_has_at_least_one_valid_url(&draft.rpcs) {
        return false;
    }
    if !list_has_at_least_one_valid_url(&draft.consensus_rpcs) {
        return false;
    }
    let cp = draft.checkpoint_override.trim();
    if !cp.is_empty() && B256::from_str(cp).is_err() {
        return false;
    }
    true
}

/// True iff `list` contains at least one parseable HTTPS URL and no entry
/// with non-empty content that fails to parse or uses a non-HTTPS scheme.
/// Empty entries are tolerated so the user can have a blank row mid-edit;
/// they're stripped on save.
///
/// HTTPS is required: a plain-HTTP consensus endpoint lets a network
/// attacker tamper with the light-client bootstrap before any consensus
/// signatures get verified, and a plain-HTTP exec endpoint exposes
/// `eth_getProof` responses to the same MITM.
fn list_has_at_least_one_valid_url(list: &[String]) -> bool {
    let mut any_ok = false;
    for s in list {
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        match url::Url::parse(s) {
            Ok(url) if url.scheme() == "https" => any_ok = true,
            _ => return false,
        }
    }
    any_ok
}
