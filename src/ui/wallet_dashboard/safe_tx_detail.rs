//! Safe transaction detail modal — full view of one queued multisig tx
//! plus the collaborative actions: **Confirm** (add my signature),
//! **Execute** (broadcast once threshold is met), and **Reject** (queue a
//! same-nonce cancellation).
//!
//! The pane is a viewer + intent emitter, like `safe_send`: it carries no
//! signer or RPC access. On open the dashboard loads the full
//! `SafeTxDetail` (owners + their signatures) and pushes it in via
//! [`SafeTxDetailPane::set_detail`]; the action buttons bubble
//! [`Outcome`]s the dashboard turns into signed service calls, with the
//! result fed back through [`SafeTxDetailPane::set_action_result`].

use alloy::primitives::{Address, B256};
use iced::border::Radius;
use iced::keyboard;
use iced::widget::{Space, column, container, row, scrollable, text};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Subscription, Task};

use crate::chain::Chain;
use crate::safe::service::{PendingSafeTx, SafeTxDetail, SafeTxState};
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{
    bold, colored_address, kao_fit, kao_scrollable_style, modal_wrapper, mono, mono_bold,
    mono_black, primary_button, secondary_button,
};
use crate::wallet::short_address;

#[derive(Debug, Clone)]
pub enum Message {
    Close,
    BoxClickIgnored,
    Confirm,
    Execute,
    Reject,
    Key(keyboard::Event),
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Closed,
    /// Sign this tx with one of my linked owners and POST the
    /// confirmation.
    Confirm,
    /// Assemble the collected signatures and broadcast `execTransaction`.
    Execute,
    /// Propose a same-nonce rejection.
    Reject,
}

#[derive(Debug)]
pub struct SafeTxDetailPane {
    safe: Address,
    chain: Chain,
    /// Lean record the row was opened from — renders immediately while
    /// the full detail loads.
    pending: PendingSafeTx,
    /// Every owner of the Safe (for the signed/pending checklist).
    owners: Vec<Address>,
    /// Owner addresses this wallet can actually sign with (Local or
    /// hardware; not view-only).
    signable: Vec<Address>,
    /// Whether the wallet holds a Local account that can pay gas to
    /// execute. Gates the Execute button.
    has_local_executor: bool,
    /// Full detail (owners' signatures + reconstructed tx), loaded async.
    detail: Option<SafeTxDetail>,
    loading: bool,
    error: Option<String>,
    /// An action (confirm/execute/reject) is in flight.
    busy: bool,
    /// Last action result: `(message, is_error)`.
    notice: Option<(String, bool)>,
}

impl SafeTxDetailPane {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        safe: Address,
        chain: Chain,
        pending: PendingSafeTx,
        owners: Vec<Address>,
        signable: Vec<Address>,
        has_local_executor: bool,
    ) -> Self {
        Self {
            safe,
            chain,
            pending,
            owners,
            signable,
            has_local_executor,
            detail: None,
            loading: true,
            error: None,
            busy: false,
            notice: None,
        }
    }

    pub fn safe(&self) -> Address {
        self.safe
    }
    pub fn chain(&self) -> Chain {
        self.chain
    }
    pub fn nonce(&self) -> u64 {
        self.pending.nonce
    }
    pub fn safe_tx_hash(&self) -> B256 {
        self.pending.safe_tx_hash
    }
    /// First owner this wallet can sign with (any state) — the signer for
    /// a rejection, which doesn't care whether the target was signed.
    pub fn signable_owner(&self) -> Option<Address> {
        self.signable.first().copied()
    }
    pub fn busy(&self) -> bool {
        self.busy
    }
    pub fn loaded_detail(&self) -> Option<&SafeTxDetail> {
        self.detail.as_ref()
    }

    /// State driving the badge + action gating. Prefers the freshly-loaded
    /// detail's state (which a post-action reload refreshes) over the
    /// list-time `pending.state` snapshot, so the badge advances after a
    /// confirm/execute without closing the modal.
    fn effective_state(&self) -> SafeTxState {
        self.detail
            .as_ref()
            .map(|d| d.state)
            .unwrap_or(self.pending.state)
    }

    /// Owner addresses that this wallet controls and that have **not** yet
    /// signed — the candidates for Confirm. Empty until detail loads.
    pub fn unsigned_signable(&self) -> Vec<Address> {
        let Some(detail) = &self.detail else {
            return Vec::new();
        };
        let signed = detail.owners_signed();
        self.signable
            .iter()
            .copied()
            .filter(|a| !signed.contains(a))
            .collect()
    }

    pub fn set_detail(&mut self, result: Result<SafeTxDetail, String>) {
        self.loading = false;
        match result {
            Ok(d) => {
                self.detail = Some(d);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    pub fn mark_busy(&mut self) {
        self.busy = true;
        self.notice = None;
    }

    pub fn set_action_result(&mut self, result: Result<String, String>) {
        self.busy = false;
        self.notice = Some(match result {
            Ok(msg) => (msg, false),
            Err(e) => (e, true),
        });
    }

    pub fn update(&mut self, msg: Message) -> (Task<Message>, Option<Outcome>) {
        match msg {
            Message::Close => (Task::none(), Some(Outcome::Closed)),
            Message::BoxClickIgnored => (Task::none(), None),
            // Swallow action presses while one is already in flight so a
            // double-tap can't fire two signatures / broadcasts.
            Message::Confirm if !self.busy => (Task::none(), Some(Outcome::Confirm)),
            Message::Execute if !self.busy => (Task::none(), Some(Outcome::Execute)),
            Message::Reject if !self.busy => (Task::none(), Some(Outcome::Reject)),
            Message::Confirm | Message::Execute | Message::Reject => (Task::none(), None),
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
        let is_rejection =
            self.pending.to == self.safe && self.pending.value.is_zero();

        let header_kao = container(kao_fit(t, "(￣ー￣)ゞ", 220.0, 48.0))
            .width(Length::Fill)
            .center_x(Length::Fill);
        let title_text = if is_rejection {
            "On-chain rejection"
        } else {
            "Safe transaction"
        };
        let title = container(text(title_text).size(14).color(t.sub).font(bold()))
            .width(Length::Fill)
            .center_x(Length::Fill);

        // Hero: value (or "Cancels nonce N" for a rejection).
        let hero: Element<'_, Message> = if is_rejection {
            text(format!("Cancels nonce {}", self.pending.nonce))
                .size(22)
                .color(t.text)
                .font(mono_black())
                .into()
        } else {
            text(format!("{} ETH", format_eth(self.pending.value)))
                .size(30)
                .color(t.text)
                .font(mono_black())
                .into()
        };
        let hero = container(hero).width(Length::Fill).center_x(Length::Fill);

        let badge = container(state_badge(t, self.effective_state()))
            .width(Length::Fill)
            .center_x(Length::Fill);

        // Field stack.
        let mut fields = column![].spacing(14).width(Length::Fill);
        if !is_rejection {
            fields = fields.push(field(
                t,
                "To",
                colored_address(t, self.pending.to),
            ));
        }
        fields = fields.push(simple_field(t, "Network", self.chain.display_name().to_string()));
        fields = fields.push(simple_field(t, "Nonce", self.pending.nonce.to_string()));

        // Owners checklist.
        fields = fields.push(self.owners_block(t));

        // Result / error notices.
        if let Some(err) = &self.error {
            fields = fields.push(notice_line(t, err, true));
        }
        if let Some((msg, is_err)) = &self.notice {
            fields = fields.push(notice_line(t, msg, *is_err));
        }

        let actions = self.actions(t);

        let body = column![
            header_kao,
            Space::new().height(8),
            title,
            Space::new().height(10),
            hero,
            Space::new().height(8),
            badge,
            Space::new().height(22),
            fields,
            Space::new().height(20),
            actions,
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        let scrollable_body = scrollable(container(body).width(Length::Fill))
            .height(Length::Shrink)
            .style(move |_, s| kao_scrollable_style(t, s));

        modal_wrapper(
            t,
            440.0,
            progress,
            Message::Close,
            Message::BoxClickIgnored,
            scrollable_body.into(),
        )
    }

    /// Owners signed/pending list. Shows a loading line until detail
    /// lands, then a ✓/○ row per owner with a "you" tag on owners this
    /// wallet controls.
    fn owners_block<'a>(&'a self, t: KaoTheme) -> Element<'a, Message> {
        let label = text(format!(
            "OWNERS ({} of {})",
            self.detail.as_ref().map(|d| d.owners_signed().len()).unwrap_or(0),
            self.detail
                .as_ref()
                .map(|d| d.confirmations_required)
                .unwrap_or(0),
        ))
        .size(13)
        .color(t.sub)
        .font(bold());

        let mut col = column![label, Space::new().height(8)].width(Length::Fill);

        if self.loading {
            col = col.push(text("Loading signatures…").size(12).color(t.sub).font(mono()));
            return col.into();
        }
        let signed = self
            .detail
            .as_ref()
            .map(|d| d.owners_signed())
            .unwrap_or_default();
        for owner in &self.owners {
            let has = signed.contains(owner);
            let (glyph, gcolor) = if has { ("✓", t.up) } else { ("○", t.sub) };
            let mine = self.signable.contains(owner);
            let name = if mine {
                format!("{} (you)", short_address(*owner))
            } else {
                short_address(*owner)
            };
            col = col.push(
                row![
                    text(glyph).size(13).color(gcolor).font(bold()),
                    Space::new().width(8),
                    text(name).size(12).color(t.text).font(mono()),
                ]
                .align_y(Alignment::Center)
                .padding(Padding::from([2, 0]))
                .width(Length::Fill),
            );
        }
        col.into()
    }

    /// `(confirm, execute, reject)` availability for the effective state
    /// and this wallet's capabilities. Pure — drives the button row and
    /// is unit-tested. Callers also gate on `detail` being loaded.
    fn action_availability(&self) -> (bool, bool, bool) {
        let state = self.effective_state();
        let can_confirm = matches!(state, SafeTxState::AwaitingConfirmations { .. })
            && !self.unsigned_signable().is_empty();
        let can_execute = matches!(state, SafeTxState::AwaitingExecution { is_next: true, .. })
            && self.has_local_executor;
        let can_reject = matches!(
            state,
            SafeTxState::AwaitingConfirmations { .. } | SafeTxState::AwaitingExecution { .. }
        ) && !self.signable.is_empty();
        (can_confirm, can_execute, can_reject)
    }

    /// Action buttons, gated by the loaded state + this wallet's
    /// capabilities. Hidden entirely for terminal states (Replaced /
    /// Executed) and while detail is still loading.
    fn actions<'a>(&'a self, t: KaoTheme) -> Element<'a, Message> {
        if self.detail.is_none() {
            return Space::new().height(0).into();
        }
        let busy = self.busy;
        let state = self.effective_state();
        let mut buttons: Vec<Element<'a, Message>> = Vec::new();

        let (can_confirm, can_execute, can_reject) = self.action_availability();

        if can_confirm {
            let mut b = primary_button(t, if busy { "Signing…" } else { "Confirm" }, !busy);
            if !busy {
                b = b.on_press(Message::Confirm);
            }
            buttons.push(container(b).width(Length::FillPortion(1)).into());
        }
        if can_execute {
            let mut b = primary_button(t, if busy { "Executing…" } else { "Execute" }, !busy);
            if !busy {
                b = b.on_press(Message::Execute);
            }
            buttons.push(container(b).width(Length::FillPortion(1)).into());
        }
        if can_reject {
            let mut b = secondary_button(t, if busy { "…" } else { "Reject" });
            if !busy {
                b = b.on_press(Message::Reject);
            }
            buttons.push(container(b).width(Length::FillPortion(1)).into());
        }

        if buttons.is_empty() {
            // Terminal or nothing actionable for this wallet — explain why
            // rather than showing dead buttons.
            let why = match state {
                SafeTxState::Executed { .. } => "Already executed.",
                SafeTxState::Replaced => "Replaced — this nonce was consumed.",
                SafeTxState::AwaitingExecution { is_next: false, .. } => {
                    "Waiting on an earlier-nonce transaction first."
                }
                _ => "No actions available for your linked owners.",
            };
            return text(why).size(12).color(t.sub).font(mono()).into();
        }

        let mut r = row![].width(Length::Fill);
        for (i, b) in buttons.into_iter().enumerate() {
            if i > 0 {
                r = r.push(Space::new().width(10));
            }
            r = r.push(b);
        }
        r.into()
    }
}

// ── small view helpers ───────────────────────────────────────────────────────

fn field<'a>(t: KaoTheme, label: &'a str, value: Element<'a, Message>) -> Element<'a, Message> {
    column![
        text(label.to_string()).size(13).color(t.sub).font(bold()),
        Space::new().height(4),
        value,
    ]
    .width(Length::Fill)
    .spacing(0)
    .into()
}

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

fn notice_line<'a>(t: KaoTheme, msg: &str, is_err: bool) -> Element<'a, Message> {
    let color = if is_err { t.down } else { t.up };
    container(text(msg.to_string()).size(12).color(color).font(mono()))
        .width(Length::Fill)
        .into()
}

/// Filled status chip — same color language as the queue rows in `home`.
fn state_badge<'a>(t: KaoTheme, state: SafeTxState) -> Element<'a, Message> {
    let (label, accent): (String, Color) = match state {
        SafeTxState::AwaitingConfirmations { have, required } => {
            (format!("{have}/{required} signatures"), t.a1)
        }
        SafeTxState::AwaitingExecution { is_next: true, .. } => {
            ("Ready to execute".to_string(), t.up)
        }
        SafeTxState::AwaitingExecution { is_next: false, .. } => ("Queued".to_string(), t.sub),
        SafeTxState::Replaced => ("Replaced".to_string(), t.down),
        SafeTxState::Executed { success: true } => ("Executed".to_string(), t.up),
        SafeTxState::Executed { success: false } => ("Failed".to_string(), t.down),
    };
    container(text(label).size(11).color(accent).font(bold()))
        .padding(Padding::from([3, 8]))
        .style(move |_| container::Style {
            background: Some(Background::Color(with_alpha(accent, 0.12))),
            border: Border {
                color: with_alpha(accent, 0.3),
                width: 1.0,
                radius: Radius::from(7),
            },
            ..container::Style::default()
        })
        .into()
}

fn format_eth(wei: alloy::primitives::U256) -> String {
    if wei.is_zero() {
        return "0".to_string();
    }
    let s = alloy::primitives::utils::format_ether(wei);
    let f = s.parse::<f64>().unwrap_or(0.0);
    if f >= 1.0 {
        format!("{f:.4}")
    } else {
        let s = format!("{f:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safe::SafeTx;
    use crate::safe::service::ServiceConfirmation;
    use alloy::primitives::{Bytes, U256};

    fn owner(b: u8) -> Address {
        Address::repeat_byte(b)
    }

    fn zero_safe_tx() -> SafeTx {
        SafeTx {
            to: Address::ZERO,
            value: U256::ZERO,
            data: Bytes::new(),
            operation: 0,
            safeTxGas: U256::ZERO,
            baseGas: U256::ZERO,
            gasPrice: U256::ZERO,
            gasToken: Address::ZERO,
            refundReceiver: Address::ZERO,
            nonce: U256::from(5u64),
        }
    }

    fn pending(state: SafeTxState) -> PendingSafeTx {
        PendingSafeTx {
            safe_tx_hash: B256::ZERO,
            to: owner(0xaa),
            value: U256::ZERO,
            data: Bytes::new(),
            nonce: 5,
            state,
            submission_ts: 0,
        }
    }

    fn detail(state: SafeTxState, signed: Vec<Address>) -> SafeTxDetail {
        SafeTxDetail {
            safe_tx_hash: B256::ZERO,
            tx: zero_safe_tx(),
            state,
            confirmations: signed
                .into_iter()
                .map(|o| ServiceConfirmation {
                    owner: o,
                    signature: Bytes::new(),
                })
                .collect(),
            confirmations_required: 2,
        }
    }

    /// Pane with two Safe owners; `signable` are the ones this wallet
    /// controls. `has_exec` toggles the local gas payer.
    fn pane(signable: Vec<Address>, has_exec: bool, pending_state: SafeTxState) -> SafeTxDetailPane {
        SafeTxDetailPane::new(
            Address::ZERO,
            Chain::Mainnet,
            pending(pending_state),
            vec![owner(0x11), owner(0x22)],
            signable,
            has_exec,
        )
    }

    #[test]
    fn unsigned_signable_excludes_already_signed() {
        let mut p = pane(
            vec![owner(0x11), owner(0x22)],
            true,
            SafeTxState::AwaitingConfirmations { have: 1, required: 2 },
        );
        p.set_detail(Ok(detail(
            SafeTxState::AwaitingConfirmations { have: 1, required: 2 },
            vec![owner(0x11)],
        )));
        assert_eq!(p.unsigned_signable(), vec![owner(0x22)]);
    }

    #[test]
    fn unsigned_signable_empty_before_detail_loads() {
        let p = pane(
            vec![owner(0x11)],
            true,
            SafeTxState::AwaitingConfirmations { have: 0, required: 2 },
        );
        assert!(p.unsigned_signable().is_empty());
    }

    #[test]
    fn effective_state_prefers_loaded_detail() {
        // List-time snapshot said "awaiting confirmations"; the reload
        // says it's now executable. The fresher detail must win.
        let mut p = pane(
            vec![owner(0x11)],
            true,
            SafeTxState::AwaitingConfirmations { have: 1, required: 2 },
        );
        p.set_detail(Ok(detail(
            SafeTxState::AwaitingExecution { required: 2, is_next: true },
            vec![owner(0x11), owner(0x22)],
        )));
        assert_eq!(
            p.effective_state(),
            SafeTxState::AwaitingExecution { required: 2, is_next: true }
        );
    }

    #[test]
    fn actions_awaiting_confirmations_allow_confirm_and_reject() {
        let mut p = pane(
            vec![owner(0x22)],
            true,
            SafeTxState::AwaitingConfirmations { have: 1, required: 2 },
        );
        p.set_detail(Ok(detail(
            SafeTxState::AwaitingConfirmations { have: 1, required: 2 },
            vec![owner(0x11)],
        )));
        // (confirm, execute, reject): signable owner 0x22 hasn't signed →
        // confirm; state isn't AwaitingExecution → no execute; reject ok.
        assert_eq!(p.action_availability(), (true, false, true));
    }

    #[test]
    fn actions_ready_to_execute() {
        let mut p = pane(
            vec![owner(0x11)],
            true,
            SafeTxState::AwaitingExecution { required: 2, is_next: true },
        );
        p.set_detail(Ok(detail(
            SafeTxState::AwaitingExecution { required: 2, is_next: true },
            vec![owner(0x11), owner(0x22)],
        )));
        // threshold met & next → execute + reject; nothing to confirm.
        assert_eq!(p.action_availability(), (false, true, true));
    }

    #[test]
    fn actions_execute_needs_local_executor() {
        let mut p = pane(
            vec![owner(0x11)],
            false, // no local gas payer
            SafeTxState::AwaitingExecution { required: 2, is_next: true },
        );
        p.set_detail(Ok(detail(
            SafeTxState::AwaitingExecution { required: 2, is_next: true },
            vec![owner(0x11), owner(0x22)],
        )));
        assert!(!p.action_availability().1);
    }

    #[test]
    fn actions_blocked_when_not_next() {
        let mut p = pane(
            vec![owner(0x11)],
            true,
            SafeTxState::AwaitingExecution { required: 2, is_next: false },
        );
        p.set_detail(Ok(detail(
            SafeTxState::AwaitingExecution { required: 2, is_next: false },
            vec![owner(0x11), owner(0x22)],
        )));
        // Can't execute (earlier nonce blocks it) but can still reject.
        assert_eq!(p.action_availability(), (false, false, true));
    }

    #[test]
    fn actions_none_for_terminal_states() {
        for state in [
            SafeTxState::Executed { success: true },
            SafeTxState::Executed { success: false },
            SafeTxState::Replaced,
        ] {
            let mut p = pane(vec![owner(0x11)], true, state);
            p.set_detail(Ok(detail(state, vec![owner(0x11)])));
            assert_eq!(p.action_availability(), (false, false, false));
        }
    }

    #[test]
    fn actions_none_without_signable_owner() {
        // Watch-only on this Safe: no signable owners → no confirm/reject
        // even though the state would otherwise allow them.
        let mut p = pane(
            vec![],
            false,
            SafeTxState::AwaitingConfirmations { have: 1, required: 2 },
        );
        p.set_detail(Ok(detail(
            SafeTxState::AwaitingConfirmations { have: 1, required: 2 },
            vec![owner(0x11)],
        )));
        assert_eq!(p.action_availability(), (false, false, false));
    }

    #[test]
    fn set_action_result_clears_busy_and_records_notice() {
        let mut p = pane(
            vec![owner(0x11)],
            true,
            SafeTxState::AwaitingConfirmations { have: 0, required: 2 },
        );
        p.mark_busy();
        assert!(p.busy());
        p.set_action_result(Err("nope".to_string()));
        assert!(!p.busy());
        assert_eq!(p.notice, Some(("nope".to_string(), true)));
    }

    #[test]
    fn set_detail_error_stops_loading_without_detail() {
        let mut p = pane(
            vec![owner(0x11)],
            true,
            SafeTxState::AwaitingConfirmations { have: 0, required: 2 },
        );
        assert!(p.loading);
        p.set_detail(Err("offline".to_string()));
        assert!(!p.loading);
        assert!(p.loaded_detail().is_none());
        assert_eq!(p.error.as_deref(), Some("offline"));
    }
}
