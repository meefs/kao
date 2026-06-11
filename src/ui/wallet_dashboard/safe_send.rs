//! Send a transaction from a Safe — three-step modal wizard
//! (compose → review → success).
//!
//! Mirrors the visual language of the EOA Send pane (`send.rs`):
//! kaomoji + progress bar header, colored-address chunks on review,
//! review-row layout for key facts, kao-hero success state. Reuses
//! all common widgets from `kao_widgets` so the styling stays one
//! upgrade away from the EOA flow.
//!
//! Recipient input mirrors the EOA Send pane: it accepts `0x…`
//! addresses, ENS names (forward-resolved through the dashboard),
//! and saved contacts via the inline picker. Pinned ENS contacts
//! are re-resolved live; a divergence between the pinned address
//! and the fresh ENS record is surfaced as a banner the user must
//! explicitly accept before continuing.
//!
//! Carries no signer or RPC access; the dashboard intercepts
//! `Confirm` and spawns the Safe broadcast task with the live signer
//! handoff (see `mod.rs::spawn_safe_broadcast_task`). It also owns
//! the ENS-resolve task — the pane signals readiness through
//! `take_pending_ens` and consumes the result via `EnsResolved`.
//!
//! Pre-flight (at construction): the Safe's threshold must be ≤ the
//! count of linked owners in this wallet that are `Local`. Hardware
//! variants currently return `UnsupportedOperation(SignHash)` from
//! `KaoSigner::sign_hash`, so they can't contribute. View-only
//! linked owners likewise can't sign. The pre-flight failure state
//! replaces the compose step with a banner explaining what's
//! missing.

use std::str::FromStr;

use alloy::primitives::utils::{format_units, parse_units};
use alloy::primitives::{Address, B256, TxHash, U256};
use iced::border::Radius;
use iced::keyboard;
use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Subscription, Task};

use crate::chain::Chain;
use crate::ens;
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{
    avatar, black, bold, colored_address, colored_hash, hover_tint, kao_fit, kao_fit_size,
    kao_scrollable_style, kao_text, modal_wrapper, mono, mono_bold, primary_button, review_row,
    secondary_button, text_input_style, vspace,
};
use crate::ui::wallet_dashboard::send::{ContactsView, PickerEntry, PickerKind};
use crate::wallet::{AccountDescriptor, SafeDescriptor, account_address, short_address};

/// Number of steps in the wizard. Hard-coded so the progress bar and
/// "Step X of N" header agree without threading the constant through
/// every render call.
const TOTAL_STEPS: u8 = 2;

#[derive(Debug, Clone)]
pub enum Message {
    SetTo(String),
    /// User picked a recipient from the merged picker (contact /
    /// own-account / own-Safe). Address carried inline; `ens` is
    /// `Some` only for contact entries with a pinned ENS record.
    PickRecipient {
        address: Address,
        ens: Option<String>,
    },
    /// User clicked the inline "Save as contact" CTA. The dashboard
    /// switches nav to Settings → Contacts in Add mode pre-filled.
    SaveAsContactClicked,
    /// User explicitly accepted an ENS divergence (pinned address
    /// differs from fresh resolve). Collapses to the fresh address.
    AcceptEnsDivergence,
    /// Forward-ENS resolve result from the dashboard. `seq` is the
    /// recipient-generation counter; stale results are dropped.
    EnsResolved {
        seq: u64,
        name: String,
        result: Result<Option<Address>, String>,
    },
    SetAmount(String),
    Step(u8),
    /// Result of the dashboard's review-prep task: the SafeTx was built
    /// at the live nonce and its hash verified against the contract
    /// (domain separator + `getTransactionHash`). Carries the pinned
    /// `(nonce, safeTxHash)` the review screen displays and the signing
    /// tasks will refuse to deviate from. `seq` drops stale results
    /// after a back-and-forth.
    HashReady {
        seq: u64,
        result: Result<(u64, B256), String>,
    },
    /// Re-run the review-prep after a failure (RPC hiccup etc.).
    RetryPrepare,
    /// "Sign & execute now" — sign locally and broadcast in one shot.
    /// Intercepted by the dashboard (needs signer + RPC).
    Confirm,
    /// "Propose to co-signers" — sign once and POST to the Transaction
    /// Service. Also dashboard-intercepted.
    Propose,
    BroadcastDone(Result<TxHash, String>),
    /// Result of a propose-to-service action.
    ProposeDone(Result<(), String>),
    CopyHash,
    CopyExplorerLink,
    Close,
    BoxClickIgnored,
    Key(keyboard::Event),
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Closed,
    CopyText(String),
    /// User clicked "Save as contact" on the recipient input. Carries
    /// the resolved address and the typed ENS string (when one was
    /// entered) so the contacts pane can pre-fill both.
    SaveAsContact {
        address: Address,
        ens: Option<String>,
    },
}

/// Resolution state of the recipient input. Tracks both the literal
/// user input and any ENS lookup that resulted from it. Same shape
/// as `send::Resolution`; the duplication is deliberate so the EOA
/// Send pane can evolve independently of the Safe Send pane (their
/// review/success surrounding context is meaningfully different).
#[derive(Debug, Clone)]
enum Resolution {
    Empty,
    Invalid,
    Address(Address),
    Resolving {
        name: String,
    },
    AddressVerifying {
        pinned: Address,
        name: String,
    },
    Resolved {
        name: String,
        addr: Address,
    },
    NotFound {
        name: String,
    },
    Error {
        name: String,
        msg: String,
    },
    EnsDivergence {
        name: String,
        pinned: Address,
        fresh: Address,
    },
}

impl Resolution {
    fn recipient(&self) -> Option<Address> {
        match self {
            Resolution::Address(a)
            | Resolution::Resolved { addr: a, .. }
            | Resolution::AddressVerifying { pinned: a, .. } => Some(*a),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct SafeSendPane {
    pub safe_address: Address,
    /// Resolved chain for this Safe — `None` if `safe.chain_id` is not
    /// in `Chain::ALL`. We never silently fall back to Mainnet here,
    /// because EIP-712's chainId is bound into the SafeTx hash, and a
    /// wrong chainId would let a signature intended for chain X be
    /// replayed on Mainnet if a Safe with the same address exists
    /// there. `None` blocks signing via [`Self::outgoing_request`] and
    /// surfaces an "unsupported chain" preflight banner.
    pub safe_chain: Option<Chain>,
    /// Raw chain_id from the stored `SafeDescriptor`, retained for
    /// display in the unsupported-chain banner.
    pub safe_chain_id: u64,
    /// `VERSION()` snapshot from the descriptor — forwarded in the
    /// request so the signing tasks re-assert the version guard.
    safe_version: String,
    /// Transaction-service base for this Safe (custom mirror or the
    /// public default) — the propose path POSTs there.
    service_base: String,
    /// `Some(reason)` when `safe_version` fails
    /// `safe::tx::ensure_signable_version` — i.e. the Safe's EIP-712
    /// domain shape isn't the one Kao signs (pre-1.3, or newer than the
    /// reviewed range). Blocks the whole flow with a preflight banner,
    /// same severity as an unsupported chain.
    version_block: Option<String>,
    pub threshold: u32,
    /// Indices (into the wallet's `accounts`) of linked owners that
    /// are `Local`. Sorted in canonical wallet-vec order; the
    /// broadcast task picks the first `threshold` of these to sign.
    pub linked_local_indices: Vec<u32>,
    /// Indices of linked owners that can sign at all — `Local` **or**
    /// hardware (Ledger/Trezor), excluding view-only. A single one is
    /// enough to *propose* to the Transaction Service, even when the
    /// wallet can't reach the threshold itself.
    pub signable_indices: Vec<u32>,
    /// Addresses of the linked Local owners (same length and order
    /// as `linked_local_indices`). Pre-computed at construction so
    /// the review screen can render owner kaomojis + short addresses
    /// without re-deriving signers per repaint.
    linked_local_addresses: Vec<Address>,
    /// Kinds of linked owners that can't sign locally (hardware /
    /// view-only). Displayed for context in the pre-flight banner.
    unsupported_owner_kinds: Vec<&'static str>,
    /// 0 = compose, 1 = review, 2 = success.
    step: u8,
    to: String,
    /// Parsed/resolved recipient state. Inputs that are valid hex
    /// addresses skip the network; ENS-shaped inputs go through a
    /// dashboard-coordinated resolver.
    resolution: Resolution,
    /// Bumped on every recipient-input change. ENS lookups tag their
    /// results with the seq they were spawned at; stale results dropped.
    resolution_seq: u64,
    /// Highest seq for which the dashboard has already spawned a task.
    last_dispatched_seq: Option<u64>,
    amount: String,
    error: Option<String>,
    busy: bool,
    last_tx_hash: Option<TxHash>,
    /// Set once a propose-to-service action succeeds — drives the
    /// "Proposed" success state (which has no tx hash, unlike execute).
    proposed: bool,
    /// The `(nonce, safeTxHash)` pair pinned by the review-prep task —
    /// the exact hash shown to the user. `None` until the prep returns;
    /// both sign buttons stay disabled until then, so nothing can be
    /// signed that wasn't displayed.
    prepared: Option<(u64, B256)>,
    /// Review-prep failure (RPC down, verification mismatch). Shown on
    /// the review card with a retry affordance.
    prepare_error: Option<String>,
    /// Bumped on every entry to review (and on back-out) so an
    /// in-flight prep result for a stale form can't attach.
    prepare_seq: u64,
    /// Highest `prepare_seq` the dashboard has already spawned a prep
    /// task for. Same once-per-change pattern as ENS dispatch.
    prepare_dispatched: Option<u64>,
}

impl SafeSendPane {
    pub fn new(safe: &SafeDescriptor, accounts: &[AccountDescriptor]) -> Self {
        let mut linked_local_indices = Vec::new();
        let mut linked_local_addresses = Vec::new();
        let mut signable_indices = Vec::new();
        let mut unsupported_owner_kinds = Vec::new();
        for &idx in &safe.linked_signer_indices {
            match accounts.get(idx as usize) {
                Some(acc @ AccountDescriptor::Local { .. }) => {
                    linked_local_indices.push(idx);
                    signable_indices.push(idx);
                    if let Some(addr) = account_address(acc) {
                        linked_local_addresses.push(addr);
                    }
                }
                // Hardware owners can sign (EIP-712 on device) — enough to
                // propose — but the wallet holds no key for them, so they
                // don't count toward the local-execute threshold.
                Some(AccountDescriptor::Ledger { .. }) => {
                    signable_indices.push(idx);
                    unsupported_owner_kinds.push("Ledger");
                }
                Some(AccountDescriptor::Trezor { .. }) => {
                    signable_indices.push(idx);
                    unsupported_owner_kinds.push("Trezor");
                }
                Some(AccountDescriptor::ViewOnly { .. }) => {
                    unsupported_owner_kinds.push("View only")
                }
                None => {}
            }
        }
        let chain = Chain::ALL
            .iter()
            .find(|c| c.chain_id() == safe.chain_id)
            .copied();
        Self {
            safe_address: safe.address(),
            safe_chain: chain,
            safe_chain_id: safe.chain_id,
            safe_version: safe.version.clone(),
            service_base: safe.tx_service_base().to_string(),
            version_block: crate::safe::tx::ensure_signable_version(&safe.version).err(),
            threshold: safe.threshold,
            linked_local_indices,
            signable_indices,
            linked_local_addresses,
            unsupported_owner_kinds,
            step: 0,
            to: String::new(),
            resolution: Resolution::Empty,
            resolution_seq: 0,
            last_dispatched_seq: None,
            amount: String::new(),
            error: None,
            busy: false,
            last_tx_hash: None,
            proposed: false,
            prepared: None,
            prepare_error: None,
            prepare_seq: 0,
            prepare_dispatched: None,
        }
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    /// Threshold pre-flight: do we have at least `threshold` local
    /// signers to cover this Safe? Gates the "Sign & execute now" path,
    /// which signs locally and broadcasts in one shot.
    pub fn has_enough_local_signers(&self) -> bool {
        (self.linked_local_indices.len() as u32) >= self.threshold
    }

    /// Do we control **any** owner that can sign (Local or hardware)? A
    /// single signature is enough to *propose* to the Transaction Service
    /// for co-owners to finish.
    pub fn has_any_signable(&self) -> bool {
        !self.signable_indices.is_empty()
    }

    /// Parsed amount in wei — `Some` iff `self.amount` is a valid
    /// non-negative decimal ETH amount. The negative-rejection check
    /// matters because alloy's `parse_units("-1", 18)` succeeds and
    /// casts to a huge `U256` via two's-complement otherwise.
    fn parsed_amount(&self) -> Option<U256> {
        let trimmed = self.amount.trim();
        if trimmed.is_empty() || trimmed.starts_with('-') {
            return None;
        }
        parse_units(trimmed, 18u8).ok().map(Into::into)
    }

    fn can_continue_from_compose(&self) -> bool {
        self.safe_chain.is_some()
            && self.version_block.is_none()
            && self.has_any_signable()
            && self.resolution.recipient().is_some()
            && !matches!(self.resolution, Resolution::EnsDivergence { .. })
            && self.parsed_amount().is_some()
    }

    /// Settled = an action already completed (executed or proposed); no
    /// further action should fire from the review step.
    fn settled(&self) -> bool {
        self.last_tx_hash.is_some() || self.proposed
    }

    /// "Sign & execute now" is reachable only when this wallet alone can
    /// meet the threshold with Local keys — and only once the reviewed
    /// safeTxHash is on screen (`prepared`): nothing gets signed that
    /// the user couldn't verify.
    fn can_execute_now(&self) -> bool {
        !self.busy
            && !self.settled()
            && self.has_enough_local_signers()
            && self.can_continue_from_compose()
            && self.prepared.is_some()
    }

    /// "Propose to co-signers" is reachable with a single signable owner,
    /// once the reviewed safeTxHash is on screen.
    fn can_propose(&self) -> bool {
        !self.busy && !self.settled() && self.can_continue_from_compose() && self.prepared.is_some()
    }

    /// Reset the pinned hash and bump the seq so any in-flight prep
    /// result drops. Called on every review entry/exit — the displayed
    /// hash must always describe the *current* form.
    fn begin_prepare(&mut self) {
        self.prepared = None;
        self.prepare_error = None;
        self.prepare_seq = self.prepare_seq.wrapping_add(1);
    }

    /// Coordinator hook: returns `Some((seq, request))` exactly once per
    /// review entry (or retry) that still needs its safeTxHash computed.
    /// Mirrors [`Self::take_pending_ens`].
    pub fn take_pending_prepare(&mut self) -> Option<(u64, SafeSendRequest)> {
        if self.step != 1
            || self.prepared.is_some()
            || self.prepare_error.is_some()
            || self.prepare_dispatched == Some(self.prepare_seq)
        {
            return None;
        }
        let req = self.outgoing_request()?;
        self.prepare_dispatched = Some(self.prepare_seq);
        Some((self.prepare_seq, req))
    }

    /// Snapshot of the inputs the dashboard needs to spawn the
    /// broadcast task. `None` if the form isn't yet ready to send, or
    /// if the Safe's chain is not in `Chain::ALL` (signing is refused
    /// rather than falling back to Mainnet — see `safe_chain` doc).
    pub fn outgoing_request(&self) -> Option<SafeSendRequest> {
        if self.version_block.is_some() {
            return None;
        }
        Some(SafeSendRequest {
            safe_address: self.safe_address,
            chain: self.safe_chain?,
            version: self.safe_version.clone(),
            service_base: self.service_base.clone(),
            to: self.resolution.recipient()?,
            value: self.parsed_amount()?,
            threshold: self.threshold,
            linked_local_indices: self.linked_local_indices.clone(),
            signable_indices: self.signable_indices.clone(),
            prepared: self.prepared.map(|(nonce, safe_tx_hash)| PreparedSafeTx {
                nonce,
                safe_tx_hash,
            }),
        })
    }

    pub fn mark_busy(&mut self) {
        self.busy = true;
        self.error = None;
    }

    /// Coordinator hook: returns `Some((seq, name))` exactly once per
    /// recipient-input change that landed on an ENS-shaped value.
    /// Mirrors `send::SendPane::take_pending_ens`.
    pub fn take_pending_ens(&mut self) -> Option<(u64, String)> {
        match &self.resolution {
            Resolution::Resolving { name } | Resolution::AddressVerifying { name, .. }
                if self.last_dispatched_seq != Some(self.resolution_seq) =>
            {
                let seq = self.resolution_seq;
                self.last_dispatched_seq = Some(seq);
                Some((seq, name.clone()))
            }
            _ => None,
        }
    }

    fn set_to(&mut self, raw: String) {
        self.to = raw;
        self.resolution_seq = self.resolution_seq.wrapping_add(1);
        let trimmed = self.to.trim();
        self.resolution = if trimmed.is_empty() {
            Resolution::Empty
        } else if let Ok(addr) = Address::from_str(trimmed) {
            Resolution::Address(addr)
        } else if ens::looks_like_ens(trimmed) {
            Resolution::Resolving {
                name: trimmed.to_string(),
            }
        } else {
            Resolution::Invalid
        };
    }

    pub fn update(&mut self, msg: Message) -> (Task<Message>, Option<Outcome>) {
        match msg {
            Message::SetTo(s) => {
                self.set_to(s);
                self.error = None;
                (Task::none(), None)
            }
            Message::PickRecipient { address, ens } => {
                self.resolution_seq = self.resolution_seq.wrapping_add(1);
                self.to = address.to_checksum(None);
                self.resolution = match ens {
                    Some(name) => Resolution::AddressVerifying {
                        pinned: address,
                        name,
                    },
                    None => Resolution::Address(address),
                };
                (Task::none(), None)
            }
            Message::SaveAsContactClicked => {
                let (addr, ens) = match &self.resolution {
                    Resolution::Address(a) => (*a, None),
                    Resolution::Resolved { addr, name } => (*addr, Some(name.clone())),
                    _ => return (Task::none(), None),
                };
                (
                    Task::none(),
                    Some(Outcome::SaveAsContact { address: addr, ens }),
                )
            }
            Message::AcceptEnsDivergence => {
                if let Resolution::EnsDivergence { fresh, .. } = self.resolution.clone() {
                    self.resolution = Resolution::Address(fresh);
                }
                (Task::none(), None)
            }
            Message::EnsResolved { seq, name, result } => {
                if seq != self.resolution_seq {
                    return (Task::none(), None);
                }
                match &self.resolution {
                    Resolution::Resolving { name: pending } if pending == &name => {
                        self.resolution = match result {
                            Ok(Some(addr)) => Resolution::Resolved { name, addr },
                            Ok(None) => Resolution::NotFound { name },
                            Err(msg) => Resolution::Error { name, msg },
                        };
                    }
                    Resolution::AddressVerifying {
                        pinned,
                        name: pending,
                    } if pending == &name => {
                        let pinned = *pinned;
                        self.resolution = match result {
                            Ok(Some(fresh)) if fresh == pinned => Resolution::Address(pinned),
                            Ok(Some(fresh)) => Resolution::EnsDivergence {
                                name,
                                pinned,
                                fresh,
                            },
                            Ok(None) | Err(_) => Resolution::Address(pinned),
                        };
                    }
                    _ => {}
                }
                (Task::none(), None)
            }
            Message::SetAmount(s) => {
                self.amount = s;
                self.error = None;
                (Task::none(), None)
            }
            Message::Step(n) => {
                match n {
                    0 => {
                        self.step = 0;
                        // Invalidate the pinned hash — the form is
                        // editable again, so the next review must
                        // re-derive and re-display it.
                        self.begin_prepare();
                    }
                    1 if self.can_continue_from_compose() => {
                        self.step = 1;
                        self.begin_prepare();
                    }
                    _ => {}
                }
                (Task::none(), None)
            }
            Message::HashReady { seq, result } => {
                if seq == self.prepare_seq && self.step == 1 {
                    match result {
                        Ok((nonce, hash)) => {
                            self.prepared = Some((nonce, hash));
                            self.prepare_error = None;
                        }
                        Err(e) => {
                            self.prepared = None;
                            self.prepare_error = Some(e);
                        }
                    }
                }
                (Task::none(), None)
            }
            Message::RetryPrepare => {
                if self.step == 1 {
                    self.begin_prepare();
                }
                (Task::none(), None)
            }
            Message::Confirm | Message::Propose => {
                // The dashboard intercepts Confirm/Propose to spawn the
                // signing task; we only land here if it didn't.
                (Task::none(), None)
            }
            Message::ProposeDone(Ok(())) => {
                self.busy = false;
                self.proposed = true;
                self.error = None;
                self.step = 2;
                (Task::none(), None)
            }
            Message::ProposeDone(Err(e)) => {
                self.busy = false;
                self.error = Some(e);
                (Task::none(), None)
            }
            Message::BroadcastDone(Ok(hash)) => {
                self.busy = false;
                self.last_tx_hash = Some(hash);
                self.error = None;
                self.step = 2;
                (Task::none(), None)
            }
            Message::BroadcastDone(Err(e)) => {
                self.busy = false;
                self.error = Some(e);
                (Task::none(), None)
            }
            Message::CopyHash => match self.last_tx_hash {
                Some(h) => (Task::none(), Some(Outcome::CopyText(format!("{h:#x}")))),
                None => (Task::none(), None),
            },
            Message::CopyExplorerLink => match (self.last_tx_hash, self.safe_chain) {
                (Some(h), Some(chain)) => {
                    let url = format!("{}/tx/{h:#x}", chain.default_blockscout_url());
                    (Task::none(), Some(Outcome::CopyText(url)))
                }
                _ => (Task::none(), None),
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

    pub fn view<'a>(
        &'a self,
        t: KaoTheme,
        contacts: ContactsView,
        progress: f32,
    ) -> Element<'a, Message> {
        if self.safe_chain.is_none() {
            let body = column![
                step_header(t, 0),
                progress_bar(t, 0),
                vspace(20),
                unsupported_chain_banner(t, self.safe_chain_id),
                vspace(16),
                primary_button(t, "Close", true).on_press(Message::Close),
            ]
            .width(Length::Fill);
            return wrap_modal(t, progress, body.into());
        }
        if let Some(reason) = &self.version_block {
            let body = column![
                step_header(t, 0),
                progress_bar(t, 0),
                vspace(20),
                banner(t, "Unsupported Safe version", reason.clone()),
                vspace(16),
                primary_button(t, "Close", true).on_press(Message::Close),
            ]
            .width(Length::Fill);
            return wrap_modal(t, progress, body.into());
        }
        // Block only when this wallet controls NO owner that can sign.
        // With at least one signable owner — even a single hardware key
        // below the threshold — the user can still propose to co-signers.
        if !self.has_any_signable() {
            let body = column![
                step_header(t, 0),
                progress_bar(t, 0),
                vspace(20),
                preflight_banner(
                    t,
                    self.threshold,
                    self.linked_local_indices.len(),
                    &self.unsupported_owner_kinds,
                ),
                vspace(16),
                primary_button(t, "Close", true).on_press(Message::Close),
            ]
            .width(Length::Fill);
            return wrap_modal(t, progress, body.into());
        }

        // Pre-derive the contact / ENS label for the current
        // recipient so review + success steps can render identically
        // without re-walking the contacts book.
        let recipient_name: Option<String> = self
            .resolution
            .recipient()
            .and_then(|a| contacts.name_for(a).map(|s| s.to_string()));
        let recipient_in_book = self
            .resolution
            .recipient()
            .map(|a| contacts.name_for(a).is_some())
            .unwrap_or(false);

        let body: Element<'_, Message> = match self.step {
            0 => self.view_compose(t, contacts.entries, recipient_in_book),
            1 => self.view_review(t, recipient_name),
            _ => self.view_success(t, recipient_name),
        };
        wrap_modal(t, progress, body)
    }

    fn view_compose<'a>(
        &'a self,
        t: KaoTheme,
        snapshot: Vec<PickerEntry>,
        recipient_in_book: bool,
    ) -> Element<'a, Message> {
        // Safe by construction: `view` short-circuits to the
        // unsupported-chain banner when `safe_chain` is `None`.
        let chain = self.safe_chain.expect("safe_chain gated by view()");
        let safe_card = safe_summary(t, self.safe_address, chain);

        let to_label = text("TO").size(11).color(t.sub).font(mono_bold());
        let to_input = text_input("0x… address or name.eth", &self.to)
            .on_input(Message::SetTo)
            .padding(Padding::from([12, 14]))
            .size(15)
            .font(mono())
            .style(move |_, status| text_input_style(t, status));

        let parse_hint: Element<'_, Message> = match &self.resolution {
            Resolution::Empty => Space::new().height(0).into(),
            Resolution::Address(addr) => container(
                row![
                    text("✓ valid address").size(11).color(t.up).font(bold()),
                    Space::new().width(8),
                    text(short_address(*addr))
                        .size(11)
                        .color(t.sub)
                        .font(mono()),
                ]
                .align_y(Alignment::Center),
            )
            .padding(Padding::from([4, 0]))
            .into(),
            Resolution::AddressVerifying { pinned, name } => container(
                column![
                    row![
                        text(format!("✓ {name}  ·  "))
                            .size(11)
                            .color(t.up)
                            .font(bold()),
                        text(short_address(*pinned))
                            .size(11)
                            .color(t.sub)
                            .font(mono()),
                    ]
                    .align_y(Alignment::Center),
                    text("(verifying ENS…)").size(10).color(t.sub),
                ]
                .spacing(2),
            )
            .padding(Padding::from([4, 0]))
            .into(),
            Resolution::Resolved { name, addr } => container(
                row![
                    text(format!("✓ {name} →  "))
                        .size(11)
                        .color(t.up)
                        .font(bold()),
                    text(short_address(*addr))
                        .size(11)
                        .color(t.sub)
                        .font(mono()),
                ]
                .align_y(Alignment::Center),
            )
            .padding(Padding::from([4, 0]))
            .into(),
            Resolution::Resolving { name } => container(
                text(format!("(；・∀・) resolving {name}…"))
                    .size(11)
                    .color(t.sub)
                    .font(bold()),
            )
            .padding(Padding::from([4, 0]))
            .into(),
            Resolution::NotFound { name } => container(
                text(format!("ENS name “{name}” has no address record"))
                    .size(11)
                    .color(t.down)
                    .font(bold()),
            )
            .padding(Padding::from([4, 0]))
            .into(),
            Resolution::Error { name, msg } => container(
                text(format!("ENS lookup for “{name}” failed: {msg}"))
                    .size(11)
                    .color(t.down)
                    .font(bold()),
            )
            .padding(Padding::from([4, 0]))
            .into(),
            Resolution::EnsDivergence {
                name,
                pinned,
                fresh,
            } => {
                let inner = column![
                    text(format!(
                        "⚠ ENS “{name}” now resolves to a different address"
                    ))
                    .size(12)
                    .color(t.down)
                    .font(bold()),
                    vspace(4),
                    row![
                        text("pinned: ").size(11).color(t.sub),
                        text(short_address(*pinned))
                            .size(11)
                            .color(t.sub)
                            .font(mono()),
                    ],
                    row![
                        text("now:    ").size(11).color(t.sub),
                        text(short_address(*fresh))
                            .size(11)
                            .color(t.text)
                            .font(mono()),
                    ],
                    vspace(6),
                    secondary_button(t, "Use new address").on_press(Message::AcceptEnsDivergence),
                ]
                .spacing(2);
                container(inner)
                    .padding(Padding::from([8, 10]))
                    .style(move |_| container::Style {
                        background: Some(Background::Color(t.ab1)),
                        border: Border {
                            color: t.down,
                            width: 1.0,
                            radius: Radius::from(8),
                        },
                        text_color: Some(t.text),
                        ..container::Style::default()
                    })
                    .into()
            }
            Resolution::Invalid => container(
                text("Not a valid 0x… address or ENS name")
                    .size(11)
                    .color(t.down)
                    .font(bold()),
            )
            .padding(Padding::from([4, 0]))
            .into(),
        };

        // Inline "Save as contact" CTA: only when the recipient is a
        // settled, sendable address that's not already in the book.
        let save_cta: Element<'_, Message> = match &self.resolution {
            Resolution::Address(_) | Resolution::Resolved { .. } if !recipient_in_book => {
                container(
                    secondary_button(t, "+ Save as contact")
                        .on_press(Message::SaveAsContactClicked),
                )
                .padding(Padding::from([6, 0]))
                .into()
            }
            _ => Space::new().height(0).into(),
        };

        let amount_label = text("AMOUNT").size(11).color(t.sub).font(mono_bold());
        let amount_input = text_input("0.0", &self.amount)
            .on_input(Message::SetAmount)
            .padding(Padding::from([12, 14]))
            .size(15)
            .style(move |_, status| text_input_style(t, status));
        let amount_row = row![
            container(amount_input).width(Length::Fill),
            Space::new().width(10),
            text("ETH").size(13).color(t.sub).font(bold()),
        ]
        .align_y(Alignment::Center);

        let amount_preview: Element<'_, Message> = match self.parsed_amount() {
            Some(amt) => {
                let formatted = format_units(amt, 18).unwrap_or_else(|_| "?".into());
                text(format!("≈ {formatted} ETH"))
                    .size(11)
                    .color(t.sub)
                    .font(mono())
                    .into()
            }
            None => Space::new().height(0).into(),
        };

        // Picker covers contacts + own accounts + other Safes (the
        // active Safe is excluded by the merge filter).
        let contacts_label = text("RECIPIENTS").size(11).color(t.sub).font(mono_bold());
        let contacts_block: Element<'_, Message> = if snapshot.is_empty() {
            container(
                text("No saved contacts yet — add some in Settings → Contacts")
                    .size(11)
                    .color(t.sub),
            )
            .padding(Padding::from([6, 4]))
            .into()
        } else {
            let mut col = column![].spacing(2);
            for entry in snapshot.into_iter() {
                col = col.push(picker_row(t, entry, &self.to));
            }
            // Same cap as the EOA Send pane — without it a long
            // list pushes Review off the fixed-width modal.
            scrollable(col)
                .height(Length::Fixed(168.0))
                .width(Length::Fill)
                .style(move |_, status| kao_scrollable_style(t, status))
                .into()
        };

        let can_continue = self.can_continue_from_compose();
        let mut continue_btn = primary_button(t, "Review →", can_continue);
        if can_continue {
            continue_btn = continue_btn.on_press(Message::Step(1));
        }

        let mut content = column![
            step_header(t, 0),
            progress_bar(t, 0),
            vspace(20),
            safe_card,
            vspace(14),
            to_label,
            vspace(6),
            to_input,
            parse_hint,
            save_cta,
            vspace(14),
            amount_label,
            vspace(6),
            amount_row,
            vspace(4),
            amount_preview,
            vspace(14),
            contacts_label,
            vspace(4),
            contacts_block,
        ]
        .width(Length::Fill);

        if let Some(e) = &self.error {
            content = content.push(vspace(10)).push(error_banner(t, e));
        }

        content = content.push(vspace(16)).push(continue_btn);
        content.into()
    }

    fn view_review<'a>(
        &'a self,
        t: KaoTheme,
        recipient_name: Option<String>,
    ) -> Element<'a, Message> {
        let recipient = self.resolution.recipient().unwrap_or(Address::ZERO);
        let amount_wei = self.parsed_amount().unwrap_or(U256::ZERO);
        let amount_str = format_units(amount_wei, 18).unwrap_or_else(|_| "?".into());

        let sending_value = format!("{amount_str} ETH");
        // Safe by construction: `view` short-circuits to the
        // unsupported-chain banner when `safe_chain` is `None`, and
        // `can_continue_from_compose` blocks reaching step 1 in that
        // case anyway.
        let chain = self.safe_chain.expect("safe_chain gated by view()");
        let chain_sub = format!("on {}", chain.display_name());

        let from_block = column![
            text("From Safe").size(11).color(t.sub).font(mono_bold()),
            vspace(4),
            colored_address(t, self.safe_address),
        ]
        .width(Length::Fill);

        // Header label above the chunked address. Priority:
        //   contact name > resolved ENS > nothing.
        // The chunked address is always the load-bearing identifier
        // the user is signing for; the name is supporting context.
        let header_label: Option<String> = recipient_name.clone().or_else(|| {
            if let Resolution::Resolved { name, .. } = &self.resolution {
                Some(name.clone())
            } else {
                None
            }
        });
        let mut to_col = column![
            text("To").size(11).color(t.sub).font(mono_bold()),
            vspace(4)
        ];
        if let Some(name) = header_label {
            to_col = to_col.push(text(name).size(13).color(t.text).font(bold()));
            to_col = to_col.push(vspace(4));
        }
        to_col = to_col.push(colored_address(t, recipient));
        let to_block = to_col.width(Length::Fill);

        let threshold_label = format!(
            "{} of {}",
            self.threshold,
            self.linked_local_addresses.len() + self.unsupported_owner_kinds.len()
        );

        // Owner list — one row per linked Local owner that will sign.
        let mut owners_col = column![].spacing(4);
        let signing = self
            .linked_local_addresses
            .iter()
            .take(self.threshold as usize);
        for (i, addr) in signing.enumerate() {
            let kao = crate::ui::kao_widgets::kaomoji_for_index(i);
            owners_col = owners_col.push(
                row![
                    avatar(t, kao, 26.0, t.ab1),
                    Space::new().width(8),
                    text(short_address(*addr))
                        .size(12)
                        .color(t.text)
                        .font(mono()),
                ]
                .align_y(Alignment::Center),
            );
        }

        // The safeTxHash the signer(s) are about to commit to — the
        // verification anchor a hardware wallet shows on-device and
        // co-signers see in their own clients. Computed by the prep
        // task at the live nonce and double-checked against the Safe's
        // own `getTransactionHash` before it's displayed; the sign
        // buttons stay disabled until it's on screen.
        let hash_label = text("Safe tx hash").size(11).color(t.sub).font(mono_bold());
        let hash_block: Element<'_, Message> = match (&self.prepared, &self.prepare_error) {
            (Some((_, hash)), _) => column![
                hash_label,
                vspace(4),
                colored_hash(t, *hash),
                vspace(4),
                text("Verify this exact hash on your signing device and with co-signers.")
                    .size(10)
                    .color(t.sub),
            ]
            .width(Length::Fill)
            .into(),
            (None, Some(e)) => column![
                hash_label,
                vspace(4),
                error_banner(t, e),
                vspace(6),
                secondary_button(t, "Retry").on_press(Message::RetryPrepare),
            ]
            .width(Length::Fill)
            .into(),
            (None, None) => column![
                hash_label,
                vspace(4),
                text("Computing — verifying against the Safe on-chain…")
                    .size(11)
                    .color(t.sub)
                    .font(mono()),
            ]
            .width(Length::Fill)
            .into(),
        };

        let review_card = container(
            column![
                review_row(t, "Sending", &sending_value, true, false),
                container(text(chain_sub).size(10).color(t.sub).font(mono()))
                    .width(Length::Fill)
                    .align_x(Alignment::End),
                vspace(14),
                from_block,
                vspace(14),
                to_block,
                vspace(14),
                hash_block,
                vspace(14),
                review_row(t, "Threshold", &threshold_label, false, false),
                vspace(8),
                text("Signing with").size(11).color(t.sub).font(mono_bold()),
                vspace(4),
                owners_col,
                vspace(14),
                review_row(t, "Gas fee", "estimated at broadcast (｡•́︿•̀｡)", false, true),
            ]
            .spacing(0),
        )
        .padding(Padding::from([18, 20]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(t.card_alt)),
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: Radius::from(16),
            },
            text_color: Some(t.text),
            ..container::Style::default()
        });

        let mut content = column![
            step_header(t, 1),
            progress_bar(t, 1),
            vspace(20),
            review_card,
        ]
        .width(Length::Fill);

        if let Some(e) = &self.error {
            content = content.push(vspace(10)).push(error_banner(t, e));
        }

        let back_btn = secondary_button(t, "← Back").on_press(Message::Step(0));

        // Primary action is "Propose to co-signers" — always available
        // with a signable owner. When the wallet can meet the threshold
        // with Local keys alone, an additional "Sign & execute now"
        // shortcut broadcasts in one step.
        let can_propose = self.can_propose();
        let mut propose_btn = primary_button(
            t,
            if self.busy && !self.has_enough_local_signers() {
                "Proposing…"
            } else {
                "Propose to co-signers"
            },
            can_propose,
        );
        if can_propose {
            propose_btn = propose_btn.on_press(Message::Propose);
        }

        let action_row = if self.has_enough_local_signers() {
            let can_exec = self.can_execute_now();
            let mut exec_btn = primary_button(
                t,
                if self.busy {
                    "Signing & sending…"
                } else {
                    "Sign & execute now"
                },
                can_exec,
            );
            if can_exec {
                exec_btn = exec_btn.on_press(Message::Confirm);
            }
            column![
                row![
                    container(back_btn).width(Length::FillPortion(1)),
                    Space::new().width(9),
                    container(exec_btn).width(Length::FillPortion(3)),
                ]
                .align_y(Alignment::Center),
                vspace(8),
                propose_btn,
            ]
            .width(Length::Fill)
            .into()
        } else {
            let r: Element<'_, Message> = row![
                container(back_btn).width(Length::FillPortion(1)),
                Space::new().width(9),
                container(propose_btn).width(Length::FillPortion(3)),
            ]
            .align_y(Alignment::Center)
            .into();
            r
        };

        content = content.push(vspace(14)).push(action_row);
        content.into()
    }

    fn view_success<'a>(
        &'a self,
        t: KaoTheme,
        recipient_name: Option<String>,
    ) -> Element<'a, Message> {
        let amount_wei = self.parsed_amount().unwrap_or(U256::ZERO);
        let amount_str = format_units(amount_wei, 18).unwrap_or_else(|_| "?".into());
        let recipient = self.resolution.recipient().unwrap_or(Address::ZERO);
        // Prefer contact name → resolved ENS → short address.
        let recipient_label = recipient_name.unwrap_or_else(|| match &self.resolution {
            Resolution::Resolved { name, .. } => name.clone(),
            _ => short_address(recipient),
        });
        let hash_display = match self.last_tx_hash {
            Some(h) => {
                let s = format!("{h:#x}");
                if s.len() > 14 {
                    format!("{}…{}", &s[..8], &s[s.len() - 6..])
                } else {
                    s
                }
            }
            None => "—".to_string(),
        };

        let close_btn = primary_button(t, "Close (ﾉ◕ヮ◕)ﾉ*:･ﾟ✧", true).on_press(Message::Close);

        // Proposed-to-service success differs from executed: there's no
        // tx hash yet (co-signers still need to sign), so we show a
        // "collect signatures" hint instead of the explorer affordances.
        if self.proposed {
            let need = self.threshold.saturating_sub(1).max(1);
            return column![
                vspace(8),
                kao_fit(t, "(\u{ff89}\u{2267}\u{25bd}\u{2266})\u{ff89}", 320.0, 76.0),
                vspace(16),
                container(text("Proposed!").size(26).color(t.text).font(black()))
                    .width(Length::Fill)
                    .center_x(Length::Fill),
                vspace(6),
                container(text(format!("{amount_str} ETH → {recipient_label}")).size(15).color(t.sub))
                    .width(Length::Fill)
                    .center_x(Length::Fill),
                vspace(8),
                container(
                    text(format!(
                        "Queued for co-signers — {need} more signature{} needed. Find it under Pending transactions.",
                        if need == 1 { "" } else { "s" }
                    ))
                    .size(13)
                    .color(t.sub)
                )
                .width(Length::Fill)
                .center_x(Length::Fill),
                vspace(18),
                close_btn,
            ]
            .width(Length::Fill)
            .into();
        }

        let copy_hash_btn = secondary_button(t, "Copy hash").on_press(Message::CopyHash);
        let copy_link_btn =
            secondary_button(t, "Copy explorer link").on_press(Message::CopyExplorerLink);
        let copy_row = row![
            container(copy_hash_btn).width(Length::FillPortion(1)),
            Space::new().width(9),
            container(copy_link_btn).width(Length::FillPortion(1)),
        ]
        .align_y(Alignment::Center);

        column![
            vspace(8),
            kao_fit(t, "ヽ(・∀・)ﾉ", 320.0, 76.0),
            vspace(16),
            container(text("Sent!").size(26).color(t.text).font(black()))
                .width(Length::Fill)
                .center_x(Length::Fill),
            vspace(6),
            container(
                text(format!("{amount_str} ETH → {recipient_label}"))
                    .size(15)
                    .color(t.sub)
            )
            .width(Length::Fill)
            .center_x(Length::Fill),
            vspace(8),
            container(text(hash_display).size(12).color(t.sub).font(mono()))
                .width(Length::Fill)
                .center_x(Length::Fill),
            vspace(14),
            copy_row,
            vspace(16),
            close_btn,
        ]
        .width(Length::Fill)
        .into()
    }
}

/// Snapshot the dashboard takes from the pane to spawn the broadcast
/// task. Held by-value so the task doesn't need a reference to the
/// pane after kickoff.
#[derive(Debug, Clone)]
pub struct SafeSendRequest {
    pub safe_address: Address,
    pub chain: Chain,
    /// Descriptor's `VERSION()` snapshot — the signing tasks re-assert
    /// `safe::tx::ensure_signable_version` on it as a structural
    /// backstop to the pane's preflight banner.
    pub version: String,
    /// Transaction-service base for this Safe — where the propose path
    /// POSTs. Custom mirror or `DEFAULT_TX_SERVICE_BASE`.
    pub service_base: String,
    pub to: Address,
    pub value: U256,
    pub threshold: u32,
    pub linked_local_indices: Vec<u32>,
    pub signable_indices: Vec<u32>,
    /// The reviewed `(nonce, safeTxHash)` pin. `None` while the prep
    /// task is still running (the prep task itself sends a request
    /// without it); the sign/propose tasks REQUIRE it and refuse to
    /// sign anything that deviates from it.
    pub prepared: Option<PreparedSafeTx>,
}

/// The `(nonce, safeTxHash)` pair the user verified on the review
/// screen. Signing tasks rebuild the SafeTx at exactly this nonce and
/// abort if the live nonce moved or the rebuilt hash differs — the
/// signature can only ever cover the hash that was displayed.
#[derive(Debug, Clone, Copy)]
pub struct PreparedSafeTx {
    pub nonce: u64,
    pub safe_tx_hash: B256,
}

fn wrap_modal<'a>(
    t: KaoTheme,
    progress: f32,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    modal_wrapper(
        t,
        440.0,
        progress,
        Message::Close,
        Message::BoxClickIgnored,
        content,
    )
}

/// Step kaomoji + "Send from Safe" / "Step N of M" header. Mirrors
/// the EOA Send header (`send.rs:576`) so the two flows feel like
/// siblings.
fn step_header<'a>(t: KaoTheme, step: u8) -> Element<'a, Message> {
    let kao = match step {
        0 => "(づ ◕‿◕ )づ",
        _ => "( •̀ω•́ )✧",
    };
    let step_label = format!(
        "Step {} of {TOTAL_STEPS}",
        step.saturating_add(1).min(TOTAL_STEPS)
    );
    row![
        kao_text(t, kao, 30.0),
        Space::new().width(12),
        column![
            text("Send from Safe").size(22).color(t.text).font(black()),
            text(step_label).size(12).color(t.sub).font(mono()),
        ]
        .spacing(0),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill)
    .into()
}

/// Two-segment progress bar. Mirrors the EOA Send pane's bar
/// (`send.rs:620`); only the segment count differs.
fn progress_bar<'a>(t: KaoTheme, step: u8) -> Element<'a, Message> {
    let mut bar = row![].spacing(5).width(Length::Fill);
    for i in 0..TOTAL_STEPS {
        let active = i <= step;
        let seg = container(Space::new().width(Length::Fill).height(4.0))
            .width(Length::FillPortion(1))
            .style(move |_| container::Style {
                background: Some(Background::Color(if active { t.a1 } else { t.border })),
                border: Border {
                    color: Color::TRANSPARENT,
                    width: 0.0,
                    radius: Radius::from(2),
                },
                ..container::Style::default()
            });
        bar = bar.push(seg);
    }
    container(bar).padding(Padding::from([16, 0])).into()
}

/// Compact card showing which Safe this send originates from.
fn safe_summary<'a>(t: KaoTheme, safe: Address, chain: Chain) -> Element<'a, Message> {
    let head = row![
        avatar(t, "(◐‿◐)", 34.0, t.ab2),
        Space::new().width(10),
        column![
            text("From Safe").size(11).color(t.sub).font(mono_bold()),
            text(chain.display_name())
                .size(12)
                .color(t.text)
                .font(bold()),
        ]
        .spacing(2)
        .width(Length::Fill),
    ]
    .align_y(Alignment::Center);

    container(column![head, vspace(8), colored_address(t, safe)].spacing(0))
        .padding(Padding::from([12, 14]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(t.card_alt)),
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: Radius::from(14),
            },
            text_color: Some(t.text),
            ..container::Style::default()
        })
        .into()
}

/// Picker row for one entry in the merged recipient list (contact,
/// own-account, or own-Safe). Mirrors `send::picker_row` so the
/// picker looks identical in both panes.
fn picker_row<'a>(t: KaoTheme, entry: PickerEntry, current_input: &str) -> Element<'a, Message> {
    let addr = entry.address;
    let checksum = addr.to_checksum(None);
    let selected = current_input.eq_ignore_ascii_case(&checksum);
    let bg = if selected { t.ab2 } else { Color::TRANSPARENT };

    let short = short_address(addr);
    let check = if selected { "✓" } else { " " };

    let mut name_row = row![text(entry.name.clone()).size(14).color(t.text).font(bold())]
        .align_y(Alignment::Center);
    if let Some(chip) = entry.chip {
        let chip_color = match entry.kind {
            PickerKind::OwnSafe => t.a2,
            _ => t.sub,
        };
        name_row = name_row
            .push(Space::new().width(8))
            .push(text(chip).size(10).color(chip_color).font(mono()));
    }

    let row_content = row![
        avatar_owned(t, entry.kaomoji.clone(), 34.0),
        Space::new().width(12),
        column![name_row, text(short).size(11).color(t.sub).font(mono()),]
            .spacing(0)
            .width(Length::Fill),
        text(check).size(16).color(t.a2),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill);

    let pick_msg = Message::PickRecipient {
        address: addr,
        ens: entry.ens.clone(),
    };
    button(row_content)
        .padding(Padding::from([9, 10]))
        .width(Length::Fill)
        .on_press(pick_msg)
        .style(move |_theme, status| button::Style {
            background: Some(Background::Color(match status {
                button::Status::Hovered | button::Status::Pressed => hover_tint(bg, t.text),
                _ => bg,
            })),
            text_color: t.text,
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: Radius::from(11),
            },
            ..button::Style::default()
        })
        .into()
}

/// Owned-string sibling of `kao_widgets::avatar`. Auto-shrinks the
/// font size so wide kaomoji glyphs fit inside the circle instead of
/// overflowing into the surrounding row.
fn avatar_owned<'a>(t: KaoTheme, kao: String, size: f32) -> Element<'a, Message> {
    let inner_pad: f32 = 4.0;
    let budget = (size - 2.0 * inner_pad).max(8.0);
    let max_font = (size * 0.40).max(10.0);
    let font_size = kao_fit_size(&kao, budget, max_font);
    container(text(kao).size(font_size).color(t.text))
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .center_x(Length::Fixed(size))
        .center_y(Length::Fixed(size))
        .style(move |_| container::Style {
            background: Some(Background::Color(t.ab2)),
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: Radius::from(size / 2.0),
            },
            text_color: Some(t.text),
            ..container::Style::default()
        })
        .into()
}

fn preflight_banner<'a>(
    t: KaoTheme,
    threshold: u32,
    local_count: usize,
    unsupported: &[&'static str],
) -> Element<'a, Message> {
    let unsupported_summary = if unsupported.is_empty() {
        String::new()
    } else {
        format!(
            " ({} owner{} can't sign locally: {})",
            unsupported.len(),
            if unsupported.len() == 1 { "" } else { "s" },
            unsupported.join(", "),
        )
    };
    let msg = format!(
        "This Safe requires {} signature{}. Only {} of the linked owners in this wallet can sign locally{}.",
        threshold,
        if threshold == 1 { "" } else { "s" },
        local_count,
        unsupported_summary,
    );
    banner(t, "Not enough local signers", msg)
}

fn unsupported_chain_banner<'a>(t: KaoTheme, chain_id: u64) -> Element<'a, Message> {
    let msg = format!(
        "This Safe is on chain {chain_id}, which this wallet doesn't know how to sign for. \
         Signing is disabled to prevent a cross-chain replay onto a different network."
    );
    banner(t, "Unsupported chain", msg)
}

fn error_banner<'a>(t: KaoTheme, msg: &str) -> Element<'a, Message> {
    container(
        text(format!("(╥﹏╥) {msg}"))
            .size(12)
            .color(t.down)
            .font(bold()),
    )
    .padding(Padding::from([10, 4]))
    .width(Length::Fill)
    .center_x(Length::Fill)
    .into()
}

fn banner<'a>(
    t: KaoTheme,
    title: impl Into<String>,
    msg: impl Into<String>,
) -> Element<'a, Message> {
    container(
        column![
            text(title.into()).size(12).color(t.a2).font(bold()),
            vspace(2),
            text(msg.into()).size(11).color(t.text),
        ]
        .spacing(0),
    )
    .padding(Padding::from([10, 12]))
    .width(Length::Fill)
    .style(move |_| container::Style {
        background: Some(Background::Color(with_alpha(t.a2, 0.08))),
        border: Border {
            color: with_alpha(t.a2, 0.4),
            width: 1.0,
            radius: Radius::from(10),
        },
        text_color: Some(t.text),
        ..container::Style::default()
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::{LedgerHdPath, SafeTrust};

    fn ledger_account() -> AccountDescriptor {
        AccountDescriptor::Ledger {
            name: None,
            path: LedgerHdPath::LedgerLive(0),
            address: [0u8; 20],
        }
    }

    fn view_only_account() -> AccountDescriptor {
        AccountDescriptor::ViewOnly {
            name: None,
            address: [0u8; 20],
        }
    }

    fn local_account(seed: u8) -> AccountDescriptor {
        let mut bytes = [seed; 32];
        if bytes.iter().all(|b| *b == 0) {
            bytes[0] = 1;
        }
        AccountDescriptor::Local {
            name: None,
            key_bytes: bytes,
        }
    }

    fn safe(threshold: u32, linked: Vec<u32>) -> SafeDescriptor {
        SafeDescriptor {
            name: None,
            chain_id: 1,
            address: [0x11; 20],
            version: "1.4.1".into(),
            trust: SafeTrust::Canonical,
            threshold,
            owners: vec![[0u8; 20]; linked.len()],
            modules: Vec::new(),
            guard: None,
            fallback_handler: None,
            linked_signer_indices: linked,
            sibling_chains: Vec::new(),
            cached_at: 0,
            tx_service_url: None,
        }
    }

    fn ready_pane() -> SafeSendPane {
        let accounts = vec![local_account(1), local_account(2)];
        let mut pane = SafeSendPane::new(&safe(2, vec![0, 1]), &accounts);
        pane.set_to("0x000000000000000000000000000000000000dEaD".to_string());
        pane.amount = "0.25".to_string();
        pane
    }

    /// A Safe on a chain not in `Chain::ALL` (e.g. xDai/100) must
    /// neither produce a `SafeSendRequest` nor unlock the
    /// compose→review transition. The EIP-712 chainId is bound into
    /// the SafeTx hash, so a silent fallback to Mainnet would let the
    /// resulting signature replay onto Mainnet if a Safe with the
    /// same address exists there.
    #[test]
    fn unknown_chain_id_blocks_signing() {
        let accounts = vec![local_account(1)];
        let mut s = safe(1, vec![0]);
        s.chain_id = 100;
        let mut pane = SafeSendPane::new(&s, &accounts);
        pane.set_to("0x000000000000000000000000000000000000dEaD".to_string());
        pane.amount = "0.25".to_string();
        assert!(pane.safe_chain.is_none());
        assert_eq!(pane.safe_chain_id, 100);
        assert!(pane.outgoing_request().is_none());
        assert!(!pane.can_continue_from_compose());
    }

    #[test]
    fn pre_flight_passes_when_threshold_met_by_local_signers() {
        let accounts = vec![local_account(1), local_account(2), local_account(3)];
        let pane = SafeSendPane::new(&safe(2, vec![0, 1, 2]), &accounts);
        assert!(pane.has_enough_local_signers());
        assert_eq!(pane.linked_local_indices, vec![0, 1, 2]);
        assert_eq!(pane.linked_local_addresses.len(), 3);
        assert!(pane.unsupported_owner_kinds.is_empty());
    }

    #[test]
    fn pre_flight_fails_when_only_hardware_linked() {
        let accounts = vec![ledger_account(), local_account(2)];
        let pane = SafeSendPane::new(&safe(2, vec![0, 1]), &accounts);
        assert!(!pane.has_enough_local_signers());
        assert_eq!(pane.linked_local_indices, vec![1]);
        assert_eq!(pane.unsupported_owner_kinds, vec!["Ledger"]);
    }

    /// Step the pane into review and feed it a successful prep result,
    /// as the dashboard's prepare task would.
    fn enter_reviewed(pane: &mut SafeSendPane) -> (u64, B256) {
        let _ = pane.update(Message::Step(1));
        assert_eq!(pane.step, 1);
        let (seq, _req) = pane.take_pending_prepare().expect("prep should dispatch");
        let pinned = (7u64, B256::repeat_byte(0xaa));
        let _ = pane.update(Message::HashReady {
            seq,
            result: Ok(pinned),
        });
        pinned
    }

    #[test]
    fn propose_reachable_with_only_a_hardware_owner() {
        // A single linked Ledger below threshold can't execute locally,
        // but it can sign once to propose to the service.
        let accounts = vec![ledger_account(), view_only_account()];
        let mut pane = SafeSendPane::new(&safe(2, vec![0, 1]), &accounts);
        pane.set_to("0x000000000000000000000000000000000000dEaD".to_string());
        pane.amount = "0.25".to_string();
        assert!(pane.has_any_signable());
        assert!(!pane.has_enough_local_signers());
        enter_reviewed(&mut pane);
        assert!(pane.can_propose());
        assert!(!pane.can_execute_now());
        // The Ledger index is offered as a signer; the view-only is not.
        assert_eq!(pane.signable_indices, vec![0]);
        let req = pane.outgoing_request().unwrap();
        assert_eq!(req.signable_indices, vec![0]);
    }

    #[test]
    fn sign_buttons_gated_on_prepared_hash() {
        let mut pane = ready_pane();
        let _ = pane.update(Message::Step(1));
        // At review but before the prep returns: nothing signable.
        assert!(!pane.can_propose());
        assert!(!pane.can_execute_now());
        let (seq, req) = pane.take_pending_prepare().unwrap();
        // The prep request itself carries no pin yet.
        assert!(req.prepared.is_none());
        // Dispatch is once-per-entry.
        assert!(pane.take_pending_prepare().is_none());
        let _ = pane.update(Message::HashReady {
            seq,
            result: Ok((9, B256::repeat_byte(0xbb))),
        });
        assert!(pane.can_propose());
        assert!(pane.can_execute_now());
        // The sign request pins exactly what was displayed.
        let req = pane.outgoing_request().unwrap();
        let pin = req.prepared.unwrap();
        assert_eq!(pin.nonce, 9);
        assert_eq!(pin.safe_tx_hash, B256::repeat_byte(0xbb));
        assert_eq!(req.version, "1.4.1");
    }

    #[test]
    fn stale_hash_ready_dropped_after_back_out() {
        let mut pane = ready_pane();
        let _ = pane.update(Message::Step(1));
        let (old_seq, _) = pane.take_pending_prepare().unwrap();
        // User goes back (form editable again) and returns to review.
        let _ = pane.update(Message::Step(0));
        let _ = pane.update(Message::Step(1));
        // The stale result must not attach to the new review.
        let _ = pane.update(Message::HashReady {
            seq: old_seq,
            result: Ok((1, B256::repeat_byte(0xcc))),
        });
        assert!(pane.prepared.is_none());
        assert!(!pane.can_propose());
        // The new review entry dispatches its own prep.
        let (new_seq, _) = pane.take_pending_prepare().unwrap();
        assert_ne!(new_seq, old_seq);
    }

    #[test]
    fn prepare_error_offers_retry_and_redispatches() {
        let mut pane = ready_pane();
        let _ = pane.update(Message::Step(1));
        let (seq, _) = pane.take_pending_prepare().unwrap();
        let _ = pane.update(Message::HashReady {
            seq,
            result: Err("rpc down".to_string()),
        });
        assert_eq!(pane.prepare_error.as_deref(), Some("rpc down"));
        assert!(!pane.can_propose());
        // Errored prep doesn't loop on its own…
        assert!(pane.take_pending_prepare().is_none());
        // …but Retry re-arms the dispatch.
        let _ = pane.update(Message::RetryPrepare);
        assert!(pane.prepare_error.is_none());
        assert!(pane.take_pending_prepare().is_some());
    }

    #[test]
    fn request_carries_service_base_from_descriptor() {
        // Default Safe → public gateway.
        let pane = ready_pane();
        let req = pane.outgoing_request().unwrap();
        assert_eq!(
            req.service_base,
            crate::safe::service::DEFAULT_TX_SERVICE_BASE
        );

        // Custom mirror on the descriptor → the propose path POSTs there.
        let accounts = vec![local_account(1)];
        let mut s = safe(1, vec![0]);
        s.tx_service_url = Some("https://txs.example-dao.org".into());
        let mut pane = SafeSendPane::new(&s, &accounts);
        pane.set_to("0x000000000000000000000000000000000000dEaD".to_string());
        pane.amount = "0.25".to_string();
        let req = pane.outgoing_request().unwrap();
        assert_eq!(req.service_base, "https://txs.example-dao.org");
    }

    #[test]
    fn broadcast_error_keeps_reviewed_pin_for_retry() {
        // A failed broadcast (RPC hiccup) returns to review with the
        // pinned hash intact — the user can retry signing the same
        // reviewed tx without a re-prep. (A nonce-advanced failure also
        // lands here; its error text tells the user to go Back, which
        // re-preps.)
        let mut pane = ready_pane();
        let pinned = enter_reviewed(&mut pane);
        pane.mark_busy();
        let _ = pane.update(Message::BroadcastDone(Err("rpc lost".into())));
        assert_eq!(pane.step, 1);
        assert_eq!(pane.prepared, Some(pinned));
        assert!(pane.can_execute_now());
        assert!(pane.can_propose());
    }

    #[test]
    fn no_prepare_dispatch_outside_review() {
        // Compose step: nothing to prepare yet.
        let mut pane = ready_pane();
        assert!(pane.take_pending_prepare().is_none());
        // Backing out of review also stops the dispatch.
        let _ = pane.update(Message::Step(1));
        let _ = pane.update(Message::Step(0));
        assert!(pane.take_pending_prepare().is_none());
    }

    #[test]
    fn settled_pane_ignores_late_hash_ready() {
        // Proposal completed (step 2); a late prep result must not
        // resurrect the sign buttons.
        let mut pane = ready_pane();
        let _ = pane.update(Message::Step(1));
        let (seq, _) = pane.take_pending_prepare().unwrap();
        let _ = pane.update(Message::HashReady {
            seq,
            result: Ok((7, B256::repeat_byte(0xaa))),
        });
        let _ = pane.update(Message::ProposeDone(Ok(())));
        assert_eq!(pane.step, 2);
        let _ = pane.update(Message::HashReady {
            seq,
            result: Ok((8, B256::repeat_byte(0xbb))),
        });
        // Settled: no action is reachable regardless of the late result.
        assert!(!pane.can_propose());
        assert!(!pane.can_execute_now());
    }

    #[test]
    fn unsupported_version_blocks_flow() {
        // 1.1.1 predates the chainId EIP-712 domain — signing for it
        // with our 1.3+ domain would hash wrong, so the pane refuses
        // outright, same severity as an unsupported chain.
        let accounts = vec![local_account(1)];
        let mut s = safe(1, vec![0]);
        s.version = "1.1.1".into();
        let mut pane = SafeSendPane::new(&s, &accounts);
        pane.set_to("0x000000000000000000000000000000000000dEaD".to_string());
        pane.amount = "0.25".to_string();
        assert!(pane.version_block.is_some());
        assert!(!pane.can_continue_from_compose());
        assert!(pane.outgoing_request().is_none());
    }

    #[test]
    fn no_signable_owner_blocks_continue() {
        // Only a view-only owner → nothing can sign, not even to propose.
        let accounts = vec![view_only_account()];
        let mut pane = SafeSendPane::new(&safe(1, vec![0]), &accounts);
        pane.set_to("0x000000000000000000000000000000000000dEaD".to_string());
        pane.amount = "0.25".to_string();
        assert!(!pane.has_any_signable());
        assert!(!pane.can_continue_from_compose());
        assert!(!pane.can_propose());
    }

    #[test]
    fn propose_done_lands_on_proposed_success_state() {
        let mut pane = ready_pane();
        let _ = pane.update(Message::Step(1));
        let _ = pane.update(Message::ProposeDone(Ok(())));
        assert_eq!(pane.step, 2);
        assert!(pane.proposed);
        // A settled pane offers no further actions.
        assert!(!pane.can_propose());
        assert!(!pane.can_execute_now());
    }

    #[test]
    fn propose_error_keeps_review_open() {
        let mut pane = ready_pane();
        let _ = pane.update(Message::Step(1));
        let _ = pane.update(Message::ProposeDone(Err("boom".to_string())));
        assert_eq!(pane.step, 1);
        assert!(!pane.proposed);
        assert_eq!(pane.error.as_deref(), Some("boom"));
    }

    #[test]
    fn pre_flight_categorises_unsupported_owners() {
        let accounts = vec![ledger_account(), view_only_account(), local_account(3)];
        let pane = SafeSendPane::new(&safe(1, vec![0, 1, 2]), &accounts);
        assert!(pane.has_enough_local_signers());
        assert_eq!(pane.linked_local_indices, vec![2]);
        assert_eq!(pane.unsupported_owner_kinds, vec!["Ledger", "View only"]);
    }

    #[test]
    fn parsed_amount_parses_decimal_eth() {
        let mut pane = SafeSendPane::new(&safe(1, vec![]), &[]);
        pane.amount = "1.5".to_string();
        let wei = pane.parsed_amount().unwrap();
        assert_eq!(wei.to_string(), "1500000000000000000");
    }

    #[test]
    fn parsed_amount_rejects_empty_and_negative() {
        let mut pane = SafeSendPane::new(&safe(1, vec![]), &[]);
        pane.amount = String::new();
        assert!(pane.parsed_amount().is_none());
        pane.amount = "-1".into();
        assert!(pane.parsed_amount().is_none());
    }

    #[test]
    fn step_advances_to_review_when_form_ready() {
        let mut pane = ready_pane();
        let (_, outcome) = pane.update(Message::Step(1));
        assert!(outcome.is_none());
        assert_eq!(pane.step, 1);
    }

    #[test]
    fn step_to_review_refused_when_form_invalid() {
        let accounts = vec![local_account(1)];
        let mut pane = SafeSendPane::new(&safe(1, vec![0]), &accounts);
        let _ = pane.update(Message::Step(1));
        assert_eq!(pane.step, 0);
    }

    #[test]
    fn back_from_review_drops_to_compose() {
        let mut pane = ready_pane();
        let _ = pane.update(Message::Step(1));
        assert_eq!(pane.step, 1);
        let _ = pane.update(Message::Step(0));
        assert_eq!(pane.step, 0);
    }

    #[test]
    fn broadcast_success_lands_on_success_step() {
        let mut pane = ready_pane();
        let _ = pane.update(Message::Step(1));
        pane.mark_busy();
        let hash = TxHash::repeat_byte(0xee);
        let _ = pane.update(Message::BroadcastDone(Ok(hash)));
        assert_eq!(pane.step, 2);
        assert!(!pane.busy);
        assert_eq!(pane.last_tx_hash, Some(hash));
    }

    #[test]
    fn broadcast_error_keeps_review_open() {
        let mut pane = ready_pane();
        let _ = pane.update(Message::Step(1));
        pane.mark_busy();
        let _ = pane.update(Message::BroadcastDone(Err("rpc lost".into())));
        assert_eq!(pane.step, 1);
        assert!(!pane.busy);
        assert_eq!(pane.error.as_deref(), Some("rpc lost"));
    }

    #[test]
    fn copy_explorer_link_emits_chain_specific_url() {
        let mut pane = ready_pane();
        pane.last_tx_hash = Some(TxHash::repeat_byte(0xab));
        let (_, outcome) = pane.update(Message::CopyExplorerLink);
        let url = match outcome {
            Some(Outcome::CopyText(s)) => s,
            other => panic!("expected CopyText, got {other:?}"),
        };
        assert!(
            url.starts_with("https://eth.blockscout.com/tx/0x"),
            "got {url}"
        );
    }

    // ── ENS / contacts integration ────────────────────────────────────

    #[test]
    fn set_to_with_hex_address_skips_ens() {
        let mut pane = SafeSendPane::new(&safe(1, vec![0]), &[local_account(1)]);
        pane.set_to("0x000000000000000000000000000000000000dEaD".to_string());
        assert!(matches!(pane.resolution, Resolution::Address(_)));
        assert!(pane.take_pending_ens().is_none());
    }

    #[test]
    fn set_to_with_ens_shaped_input_kicks_lookup_exactly_once() {
        let mut pane = SafeSendPane::new(&safe(1, vec![0]), &[local_account(1)]);
        pane.set_to("vitalik.eth".to_string());
        assert!(matches!(pane.resolution, Resolution::Resolving { .. }));
        // First take returns Some; second take returns None until
        // the user changes the input.
        let first = pane.take_pending_ens();
        assert!(first.is_some());
        assert!(pane.take_pending_ens().is_none());
    }

    #[test]
    fn ens_resolved_ok_some_collapses_to_resolved() {
        let mut pane = SafeSendPane::new(&safe(1, vec![0]), &[local_account(1)]);
        pane.set_to("vitalik.eth".to_string());
        let (seq, name) = pane.take_pending_ens().unwrap();
        let addr = Address::repeat_byte(0xab);
        let _ = pane.update(Message::EnsResolved {
            seq,
            name,
            result: Ok(Some(addr)),
        });
        assert!(matches!(
            pane.resolution,
            Resolution::Resolved { addr: a, .. } if a == addr,
        ));
    }

    #[test]
    fn ens_resolved_with_stale_seq_is_dropped() {
        let mut pane = SafeSendPane::new(&safe(1, vec![0]), &[local_account(1)]);
        pane.set_to("vitalik.eth".to_string());
        let _ = pane.take_pending_ens().unwrap();
        // User changes input — bumps the seq.
        pane.set_to("kao.eth".to_string());
        let stale = Address::repeat_byte(0x42);
        // Carry-over result tagged with the *old* seq — must drop.
        let _ = pane.update(Message::EnsResolved {
            seq: 1,
            name: "vitalik.eth".to_string(),
            result: Ok(Some(stale)),
        });
        // Still Resolving — the in-flight new lookup hasn't returned.
        assert!(matches!(pane.resolution, Resolution::Resolving { .. }));
    }

    #[test]
    fn pick_recipient_with_ens_enters_address_verifying() {
        let mut pane = SafeSendPane::new(&safe(1, vec![0]), &[local_account(1)]);
        let addr = Address::repeat_byte(0x77);
        let _ = pane.update(Message::PickRecipient {
            address: addr,
            ens: Some("vitalik.eth".to_string()),
        });
        match &pane.resolution {
            Resolution::AddressVerifying { pinned, name } => {
                assert_eq!(*pinned, addr);
                assert_eq!(name, "vitalik.eth");
            }
            other => panic!("expected AddressVerifying, got {other:?}"),
        }
        // The verify step kicks an ENS lookup.
        assert!(pane.take_pending_ens().is_some());
    }

    #[test]
    fn pick_recipient_without_ens_settles_directly() {
        let mut pane = SafeSendPane::new(&safe(1, vec![0]), &[local_account(1)]);
        let addr = Address::repeat_byte(0x55);
        let _ = pane.update(Message::PickRecipient {
            address: addr,
            ens: None,
        });
        assert!(matches!(pane.resolution, Resolution::Address(_)));
        assert!(pane.take_pending_ens().is_none());
    }

    #[test]
    fn ens_divergence_blocks_continue_until_accepted() {
        let mut pane = SafeSendPane::new(&safe(1, vec![0]), &[local_account(1)]);
        pane.amount = "0.1".into();
        let pinned = Address::repeat_byte(0x11);
        let _ = pane.update(Message::PickRecipient {
            address: pinned,
            ens: Some("vitalik.eth".to_string()),
        });
        // Form is filled and pinned address is usable now — Continue
        // should be enabled before the verify lands.
        assert!(pane.can_continue_from_compose());
        let (seq, name) = pane.take_pending_ens().unwrap();
        // Live ENS now resolves to a different address.
        let fresh = Address::repeat_byte(0x99);
        let _ = pane.update(Message::EnsResolved {
            seq,
            name,
            result: Ok(Some(fresh)),
        });
        assert!(matches!(pane.resolution, Resolution::EnsDivergence { .. }));
        // Continue is now disabled — the user must accept first.
        assert!(!pane.can_continue_from_compose());
        let _ = pane.update(Message::AcceptEnsDivergence);
        assert!(matches!(
            pane.resolution,
            Resolution::Address(a) if a == fresh,
        ));
        assert!(pane.can_continue_from_compose());
    }

    #[test]
    fn save_as_contact_emits_outcome_for_address_input() {
        let mut pane = SafeSendPane::new(&safe(1, vec![0]), &[local_account(1)]);
        pane.set_to("0x000000000000000000000000000000000000dEaD".into());
        let (_, outcome) = pane.update(Message::SaveAsContactClicked);
        let (addr, ens) = match outcome {
            Some(Outcome::SaveAsContact { address, ens }) => (address, ens),
            other => panic!("expected SaveAsContact, got {other:?}"),
        };
        assert_eq!(
            addr,
            "0x000000000000000000000000000000000000dEaD"
                .parse::<Address>()
                .unwrap()
        );
        assert!(ens.is_none());
    }

    #[test]
    fn save_as_contact_carries_ens_name_when_resolved() {
        let mut pane = SafeSendPane::new(&safe(1, vec![0]), &[local_account(1)]);
        pane.set_to("vitalik.eth".to_string());
        let (seq, name) = pane.take_pending_ens().unwrap();
        let addr = Address::repeat_byte(0xab);
        let _ = pane.update(Message::EnsResolved {
            seq,
            name,
            result: Ok(Some(addr)),
        });
        let (_, outcome) = pane.update(Message::SaveAsContactClicked);
        let (got_addr, ens) = match outcome {
            Some(Outcome::SaveAsContact { address, ens }) => (address, ens),
            other => panic!("expected SaveAsContact, got {other:?}"),
        };
        assert_eq!(got_addr, addr);
        assert_eq!(ens.as_deref(), Some("vitalik.eth"));
    }

    #[test]
    fn outgoing_request_uses_resolved_recipient() {
        let mut pane = ready_pane();
        // ready_pane uses set_to with a hex address, so the resolved
        // recipient is the hex address itself.
        let req = pane.outgoing_request().unwrap();
        assert_eq!(
            req.to,
            "0x000000000000000000000000000000000000dEaD"
                .parse::<Address>()
                .unwrap()
        );
        // Bumping the seq via a fresh hex input keeps things in sync.
        pane.set_to("0x000000000000000000000000000000000000beef".into());
        let req = pane.outgoing_request().unwrap();
        assert_eq!(
            req.to,
            "0x000000000000000000000000000000000000bEEf"
                .parse::<Address>()
                .unwrap()
        );
    }
}
