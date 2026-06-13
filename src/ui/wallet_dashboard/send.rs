//! Send modal — multi-step wizard (recipient → amount → review → success).
//!
//! The pane carries no signer or RPC access. It bubbles `QuoteRequested` /
//! `BroadcastRequested` outcomes upward to the dashboard, which holds the
//! `KaoSigner` and `BalanceFetcher::provider()` and runs the actual
//! `wallet::tx::build_quote` / `wallet::tx::sign_and_send`. Results flow back
//! through `QuoteFetched` / `BroadcastDone` messages.

use std::str::FromStr;

use alloy::primitives::utils::format_units;
use alloy::primitives::{Address, TxHash};
use iced::border::Radius;
use iced::keyboard;
use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
use iced::{Alignment, Background, Border, Color, Element, Length, Padding, Subscription, Task};

use super::home::format_symbol;
use crate::decode::clear_sign::DecodeResult;
use crate::ens;
use crate::portfolio::LiveToken;
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{
    avatar, black, bold, colored_address, ghost_button, hint_pill, hover_tint, kao_fit,
    kao_scrollable_style, kao_text, kaomoji_for_account, kaomoji_for_index, modal_wrapper, mono,
    mono_black, mono_bold, primary_button, secondary_button, text_input_style, token_avatar,
};
use crate::ui::wallet_dashboard::function_panel;
use crate::ui::wallet_dashboard::sim_view::simulation_block;
use crate::wallet::sim::SimOutcome;
use crate::wallet::tx::{SendPlan, SendToken, TxQuote, parse_amount_units};
use crate::wallet::{AccountDescriptor, ContactsBook, SafeDescriptor, account_address};

/// Source of a picker row. Drives the kind chip rendered next to the
/// row's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// Saved contact from the contacts book.
    Contact,
    /// One of the user's own accounts (Local/Ledger/Trezor/View-only).
    OwnAccount,
    /// One of the user's own Safes (signing or watch-only).
    OwnSafe,
}

/// One entry in the merged recipient picker. Built once per view at
/// the dashboard level so the iced widget tree owns its strings.
#[derive(Debug, Clone)]
pub struct PickerEntry {
    pub name: String,
    pub address: Address,
    pub kaomoji: String,
    pub kind: PickerKind,
    /// Pinned ENS name when this entry came from a contact with an
    /// ENS record. Own-account / own-Safe entries don't carry ENS
    /// (no reverse-resolve at view time). Drives the
    /// `AddressVerifying` resolution state on pick.
    pub ens: Option<String>,
    /// Short tag rendered next to the name. `None` for plain
    /// contacts (the most common type — the chip would be noise);
    /// otherwise the account/Safe kind ("Local", "Ledger", "Trezor",
    /// "View only", "Safe", "Safe watch").
    pub chip: Option<&'static str>,
}

/// View-time snapshot of the recipient picker. Despite the name,
/// this carries the **merged** book: saved contacts + the user's own
/// accounts + the user's own Safes, with the active sender excluded.
///
/// Sending to yourself is a real and frequent need — rebalancing
/// between hot wallet and Safe, withdrawing from a Safe back to the
/// signing EOA — so the picker has to surface own-addresses without
/// forcing the user to save them as contacts first.
#[derive(Debug, Clone, Default)]
pub struct ContactsView {
    pub entries: Vec<PickerEntry>,
}

impl ContactsView {
    /// Merged picker: contacts (first), then the user's own accounts
    /// and Safes. Dedupe by address — if an own-account is also a
    /// saved contact, the contact entry wins (user's chosen name +
    /// kaomoji + any ENS pin).
    ///
    /// `exclude_account` / `exclude_safe` drop the active sender so
    /// the user doesn't see themselves as a destination. In EOA
    /// mode pass `Some(active_index), None`; in Safe mode pass
    /// `None, Some(active_safe_idx)` (own EOAs are valid Safe-send
    /// destinations — withdrawing back to a signing key is a normal
    /// flow).
    pub fn merged(
        book: &ContactsBook,
        accounts: &[AccountDescriptor],
        safes: &[SafeDescriptor],
        exclude_account: Option<usize>,
        exclude_safe: Option<usize>,
    ) -> Self {
        let mut entries: Vec<PickerEntry> = book.iter().map(picker_entry_from_contact).collect();
        let mut seen: std::collections::HashSet<Address> =
            entries.iter().map(|e| e.address).collect();

        for (idx, acc) in accounts.iter().enumerate() {
            if exclude_account == Some(idx) {
                continue;
            }
            let Some(addr) = account_address(acc) else {
                continue;
            };
            if !seen.insert(addr) {
                continue;
            }
            let chip: &'static str = match acc {
                AccountDescriptor::Local { .. } => "Local",
                AccountDescriptor::Ledger { .. } => "Ledger",
                AccountDescriptor::Trezor { .. } => "Trezor",
                AccountDescriptor::ViewOnly { .. } => "View only",
            };
            entries.push(PickerEntry {
                name: acc.display_name(idx),
                address: addr,
                kaomoji: kaomoji_for_account(idx).to_string(),
                kind: PickerKind::OwnAccount,
                ens: None,
                chip: Some(chip),
            });
        }

        for (idx, safe) in safes.iter().enumerate() {
            if exclude_safe == Some(idx) {
                continue;
            }
            let addr = safe.address();
            if !seen.insert(addr) {
                continue;
            }
            let watch_only = safe.linked_signer_indices.is_empty();
            let kao = if watch_only {
                "(◐_◐)"
            } else {
                "(◐‿◐)"
            };
            let chip = if watch_only { "Safe watch" } else { "Safe" };
            entries.push(PickerEntry {
                name: safe.display_name(idx),
                address: addr,
                kaomoji: kao.to_string(),
                kind: PickerKind::OwnSafe,
                ens: None,
                chip: Some(chip),
            });
        }

        Self { entries }
    }

    pub fn name_for(&self, addr: Address) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.address == addr)
            .map(|e| e.name.as_str())
    }
}

fn picker_entry_from_contact(c: &crate::wallet::Contact) -> PickerEntry {
    let kao = if c.kaomoji.is_empty() {
        "(◕‿◕)".to_string()
    } else {
        c.kaomoji.clone()
    };
    PickerEntry {
        name: c.name.clone(),
        address: c.address(),
        kaomoji: kao,
        kind: PickerKind::Contact,
        ens: c.ens.as_ref().map(|e| e.name.clone()),
        chip: None,
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    SetTo(String),
    /// User picked a recipient from the merged picker (contact /
    /// own-account / own-Safe). The chosen address is carried
    /// inline; `ens` is `Some` only for contact entries with a
    /// pinned ENS record, which triggers the AddressVerifying
    /// re-resolve flow.
    PickRecipient {
        address: Address,
        ens: Option<String>,
    },
    SetAmount(String),
    SetToken(usize),
    Max,
    Step(u8),
    Confirm,
    QuoteFetched(Result<TxQuote, String>),
    BroadcastDone(Result<TxHash, String>),
    /// Result of a clear-signing decode spawned by the dashboard.
    /// `seq` is the decode-generation counter; stale results dropped.
    DecodedReady {
        seq: u64,
        decoded: Box<DecodeResult>,
    },
    /// Result of an ENS forward-resolution task spawned by the dashboard.
    /// `seq` is the input-generation counter that was current when the task
    /// was spawned; results carrying a stale seq are dropped so the user's
    /// most recent typing always wins.
    EnsResolved {
        seq: u64,
        name: String,
        result: Result<Option<Address>, String>,
    },
    ToggleCalldata,
    AdvanceToDone,
    SendAnother,
    CopyHash,
    CopyEtherscan,
    Close,
    BoxClickIgnored,
    /// User clicked the inline "Save as contact" CTA on the recipient
    /// step. The pane bubbles up an `Outcome::SaveAsContact` carrying
    /// the resolved address (and ENS string when one was typed); the
    /// dashboard switches nav to Settings and opens the Contacts pane
    /// in Add mode.
    SaveAsContactClicked,
    /// User explicitly accepted an ENS divergence — the contact was
    /// pinned to address X but the live ENS now resolves to Y, and the
    /// user clicked "Use new address" to swap to Y.
    AcceptEnsDivergence,
    Key(keyboard::Event),
}

/// Resolution state of the recipient input. Tracks both the literal user
/// input and any ENS lookup that resulted from it.
#[derive(Debug, Clone)]
enum Resolution {
    /// Empty input.
    Empty,
    /// User typed something that's not a valid address and not ENS-shaped
    /// (no dot). Continue is disabled.
    Invalid,
    /// User pasted a valid hex address — no network round-trip needed.
    Address(Address),
    /// User typed an ENS-shaped name and a lookup is in flight.
    Resolving { name: String },
    /// User picked an ENS contact (or typed an ENS string with a known
    /// pinned address). The pinned address is usable immediately —
    /// Continue is enabled — but a fresh forward-resolve is in flight
    /// to verify the pin is still current. On match → silent acceptance
    /// (collapse to `Address`). On divergence → switch to
    /// `EnsDivergence`. On lookup error → fall through to `Address`
    /// with a soft warning hint (consistent with the typed-ENS Error
    /// path; we don't block sends on RPC flakes).
    AddressVerifying { pinned: Address, name: String },
    /// ENS lookup succeeded.
    Resolved { name: String, addr: Address },
    /// ENS lookup returned no address record.
    NotFound { name: String },
    /// ENS lookup errored (network, RPC, decoding).
    Error { name: String, msg: String },
    /// A contact's pinned address differs from the current live ENS
    /// resolution. Continue is disabled; the user must click "Use new
    /// address" (which collapses to `Address(fresh)`) or back out and
    /// pick again. Mirrors the security stance from the plan: we don't
    /// silently follow ENS owner changes for saved contacts.
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

#[derive(Debug, Clone)]
pub enum Outcome {
    Closed,
    /// User clicked one of the success-step copy buttons. Coordinator
    /// runs `iced::clipboard::write`.
    CopyText(String),
    /// User clicked "Save as contact" on the recipient step. Carries the
    /// resolved hex address and the ENS string when one was typed (so
    /// the contacts pane can pre-fill both the pinned address and the
    /// ENS slot). Dashboard switches nav to Settings → Contacts in Add
    /// mode and closes the Send modal.
    SaveAsContact {
        address: Address,
        ens: Option<String>,
    },
}

#[derive(Debug)]
pub struct SendPane {
    /// Sender's address — held so we can build a `SendPlan` without
    /// passing the signer around.
    from: Address,
    step: u8,
    to: String,
    /// Parsed/resolved recipient state. Inputs that are valid hex
    /// addresses skip the network; ENS-shaped inputs go through a
    /// dashboard-coordinated resolver. The `recipient()` accessor pulls a
    /// concrete `Address` out only when the state is settled.
    resolution: Resolution,
    /// Bumped on every recipient-input change. ENS lookups tag their
    /// results with the seq they were spawned at; stale results are dropped.
    resolution_seq: u64,
    /// Highest seq for which the dashboard has already spawned a task. Lets
    /// `take_pending_ens` return `Some` once per fresh input change without
    /// the dashboard having to track per-pane state.
    last_dispatched_seq: Option<u64>,
    amount: String,
    token_idx: usize,
    busy: bool,
    quote: Option<TxQuote>,
    quote_loading: bool,
    /// Latest broadcast/quote error. Cleared on user action.
    error: Option<String>,
    /// Set by `BroadcastDone(Ok(_))`; rendered on the success step.
    last_tx_hash: Option<TxHash>,
    /// Clear-signing result for the current SendPlan. `None` while a
    /// decode is in flight (with `decoded_loading = true`) or when the
    /// plan has empty calldata (native send — no decode needed).
    decoded: Option<Box<DecodeResult>>,
    decoded_loading: bool,
    /// Bumped each time the dashboard kicks a fresh decode. Stale
    /// results (slow decoder finishing after the plan changed) are
    /// dropped via this same sequence-number pattern as ENS resolves.
    decoded_seq: u64,
    /// Collapsible toggle for the decoded calldata section on the
    /// review step. Default `false` (collapsed).
    show_calldata: bool,
    /// Tracks whether the broadcast completed successfully. Used by
    /// the broadcasting step (step 3) to show the checklist progress
    /// and gate the delayed `AdvanceToDone` timer.
    broadcast_done: bool,
}

impl SendPane {
    pub fn new(from: Address) -> Self {
        Self {
            from,
            step: 0,
            to: String::new(),
            resolution: Resolution::Empty,
            resolution_seq: 0,
            last_dispatched_seq: None,
            amount: String::new(),
            token_idx: 0,
            busy: false,
            quote: None,
            quote_loading: false,
            error: None,
            last_tx_hash: None,
            decoded: None,
            decoded_loading: false,
            decoded_seq: 0,
            show_calldata: false,
            broadcast_done: false,
        }
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn token_idx(&self) -> usize {
        self.token_idx
    }

    pub fn quote(&self) -> Option<&TxQuote> {
        self.quote.as_ref()
    }

    /// Coordinator hook: returns `Some((seq, name))` exactly once per
    /// recipient-input change that landed on an ENS-shaped value. The
    /// dashboard spawns a forward-resolution task tagged with `seq`, and a
    /// later `EnsResolved` carries the result back. After the first
    /// dispatch this returns `None` until the user types something that
    /// bumps the seq again.
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

    /// Coordinator-driven Max: the dashboard knows the active token's raw
    /// balance and (when a quote is loaded) the expected ETH gas cost, so it
    /// computes the max sendable amount and pumps it back as a formatted
    /// string. We just slot it in.
    pub fn apply_max(&mut self, amount_str: String) {
        self.amount = amount_str;
    }

    pub fn update(&mut self, msg: Message) -> (Task<Message>, Option<Outcome>) {
        match msg {
            Message::SetTo(s) => {
                self.set_to(s);
                (Task::none(), None)
            }
            Message::PickRecipient { address, ens } => {
                // Bump the seq so any in-flight prior resolution
                // result is dropped (matches the typed-input
                // contract).
                self.resolution_seq = self.resolution_seq.wrapping_add(1);
                // Render the canonical hex of the picked address in
                // the input box so the user sees what they're
                // sending to even when picking by name. EIP-55
                // checksum keeps the value copy-paste safe.
                self.to = address.to_checksum(None);
                self.resolution = match ens {
                    Some(name) => {
                        // Contact carries a pinned ENS — kick a
                        // background verify against the same name.
                        // The dashboard's `take_pending_ens` will
                        // dispatch the lookup; `EnsResolved` lands
                        // back here and either silently accepts or
                        // surfaces a divergence banner.
                        Resolution::AddressVerifying {
                            pinned: address,
                            name,
                        }
                    }
                    None => Resolution::Address(address),
                };
                (Task::none(), None)
            }
            Message::EnsResolved { seq, name, result } => {
                if seq != self.resolution_seq {
                    return (Task::none(), None);
                }
                // Branch on which kind of resolve this is for: a
                // typed-ENS Resolving slot, or an AddressVerifying slot
                // (a contact pin). Mismatched names are dropped on
                // either path — same wraparound guard the previous
                // implementation used for `Resolving`.
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
                            Ok(Some(fresh)) if fresh == pinned => {
                                // Live ENS still resolves to the pinned
                                // address — silent acceptance, drop the
                                // verifying state so the UI no longer
                                // shows a "verifying…" hint.
                                Resolution::Address(pinned)
                            }
                            Ok(Some(fresh)) => Resolution::EnsDivergence {
                                name,
                                pinned,
                                fresh,
                            },
                            // RPC down or ENS record missing — fall
                            // through to the pinned address with no
                            // banner. Consistent with the typed-ENS
                            // Error path: don't block a send on
                            // network flakes; the user can still cancel.
                            Ok(None) | Err(_) => Resolution::Address(pinned),
                        };
                    }
                    _ => {}
                }
                (Task::none(), None)
            }
            Message::AcceptEnsDivergence => {
                if let Resolution::EnsDivergence { fresh, .. } = self.resolution.clone() {
                    self.resolution = Resolution::Address(fresh);
                }
                (Task::none(), None)
            }
            Message::SaveAsContactClicked => {
                // Capture the current resolved address (and the ENS
                // string when one was typed) so the contacts pane can
                // pre-fill both. Closing the modal is the dashboard's
                // job.
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
            Message::SetAmount(s) => {
                self.amount = s;
                self.error = None;
                (Task::none(), None)
            }
            Message::SetToken(i) => {
                self.token_idx = i;
                // A different token invalidates any existing quote — gas
                // cost is the same call but the calldata differs, and the
                // user shouldn't see a stale 21k gas line for an ERC-20.
                self.quote = None;
                (Task::none(), None)
            }
            Message::Max => (Task::none(), None),
            Message::Step(s) => {
                if s <= 4 {
                    self.step = s;
                }
                (Task::none(), None)
            }
            Message::Confirm => {
                // The dashboard intercepts this message *before* forwarding
                // to us so it can move the signer into a broadcast task.
                // Our role is just to flip into the busy state and enter
                // the broadcasting step. Refuse to mark busy if no quote
                // is loaded — the dashboard would also refuse to spawn the
                // task in that case, so we'd wedge the UI.
                if !self.busy && self.quote.is_some() {
                    self.busy = true;
                    self.error = None;
                    self.step = 3;
                }
                (Task::none(), None)
            }
            Message::QuoteFetched(result) => {
                self.quote_loading = false;
                match result {
                    Ok(q) => {
                        self.quote = Some(q);
                        self.error = None;
                    }
                    Err(e) => {
                        self.error = Some(e);
                    }
                }
                (Task::none(), None)
            }
            Message::BroadcastDone(result) => {
                self.busy = false;
                match result {
                    Ok(hash) => {
                        self.last_tx_hash = Some(hash);
                        self.broadcast_done = true;
                        self.error = None;
                    }
                    Err(e) => {
                        // Return to review so the user sees the error and
                        // can retry.
                        self.step = 2;
                        self.error = Some(e);
                    }
                }
                (Task::none(), None)
            }
            Message::DecodedReady { seq, decoded } => {
                // Drop stale results — the user might have backed out
                // of the review step and built a different plan before
                // this future resolved.
                if seq == self.decoded_seq {
                    self.decoded_loading = false;
                    self.decoded = Some(decoded);
                }
                (Task::none(), None)
            }
            Message::ToggleCalldata => {
                self.show_calldata = !self.show_calldata;
                (Task::none(), None)
            }
            Message::AdvanceToDone => {
                // Guard: only advance if the broadcast actually finished.
                // A stale timer (user closed and reopened the modal) is a
                // no-op.
                if self.broadcast_done {
                    self.step = 4;
                }
                (Task::none(), None)
            }
            Message::SendAnother => {
                // Reset the wizard to step 0 with all inputs cleared,
                // keeping the sender address.
                let from = self.from;
                *self = Self::new(from);
                (Task::none(), None)
            }
            Message::CopyHash => match self.last_tx_hash {
                Some(h) => (Task::none(), Some(Outcome::CopyText(format!("{h:#x}")))),
                None => (Task::none(), None),
            },
            Message::CopyEtherscan => match self.last_tx_hash {
                Some(h) => (
                    Task::none(),
                    Some(Outcome::CopyText(format!("https://etherscan.io/tx/{h:#x}"))),
                ),
                None => (Task::none(), None),
            },
            Message::Close => (Task::none(), Some(Outcome::Closed)),
            Message::BoxClickIgnored => (Task::none(), None),
            Message::Key(keyboard::Event::KeyPressed { key, .. }) => {
                if let keyboard::Key::Named(keyboard::key::Named::Escape) = key {
                    if matches!(self.step, 1 | 2) && !self.busy {
                        self.step -= 1;
                        (Task::none(), None)
                    } else {
                        // Steps 0, 3, 4: close the modal outright.
                        (Task::none(), Some(Outcome::Closed))
                    }
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

    /// Coordinator hook: called when the user presses "Review →" so the
    /// dashboard can fetch a quote against the same plan the pane will
    /// later broadcast. Returns `None` if the current state can't be
    /// turned into a valid plan (input parses missing).
    pub fn build_plan(&self, portfolio: &[LiveToken]) -> Option<SendPlan> {
        let recipient = self.resolution.recipient()?;
        let token = portfolio.get(self.token_idx)?;
        let amount_units = parse_amount_units(&self.amount, token.decimals).ok()?;
        if amount_units.is_zero() {
            return None;
        }
        if amount_units > token.balance_raw {
            return None;
        }
        let send_token = match token.contract {
            None => SendToken::Native,
            Some(addr) => SendToken::Erc20 { contract: addr },
        };
        Some(SendPlan {
            from: self.from,
            recipient,
            token: send_token,
            amount_units,
            chain: token.chain,
        })
    }

    /// Mark a quote fetch in flight. Called by the dashboard right after
    /// it spawns the quote task so the review step renders a "loading"
    /// indicator rather than a missing-quote state.
    pub fn quote_started(&mut self) {
        self.quote_loading = true;
        self.error = None;
    }

    /// Bump the decode seq, mark in flight, and return the new seq.
    /// The dashboard tags its `decode_call` task with this value; the
    /// matching `DecodedReady` message carries it back, and we drop
    /// any result whose seq doesn't match the latest.
    pub fn decode_started(&mut self) -> u64 {
        self.decoded_seq = self.decoded_seq.wrapping_add(1);
        self.decoded_loading = true;
        self.decoded = None;
        self.decoded_seq
    }

    pub fn view<'a>(
        &'a self,
        t: KaoTheme,
        portfolio: &'a [LiveToken],
        contacts: ContactsView,
        progress: f32,
    ) -> Element<'a, Message> {
        // Snapshot the contact data the steps need into owned values.
        // Lifetime hygiene: the dashboard's `view()` can't keep a
        // `&ContactsBook` alive past the function body, so we move
        // owned strings/vecs into the panes instead.
        let recipient_name: Option<String> = self
            .resolution
            .recipient()
            .and_then(|a| contacts.name_for(a).map(|s| s.to_string()));
        let recipient_in_book = self
            .resolution
            .recipient()
            .map(|a| contacts.name_for(a).is_some())
            .unwrap_or(false);

        // Look up recipient contact metadata for the review step.
        let recipient_kao: Option<String> = self.resolution.recipient().and_then(|a| {
            contacts
                .entries
                .iter()
                .find(|e| e.address == a)
                .map(|e| e.kaomoji.clone())
        });
        let recipient_chip: Option<&'static str> = self.resolution.recipient().and_then(|a| {
            contacts
                .entries
                .iter()
                .find(|e| e.address == a)
                .and_then(|e| e.chip)
        });

        let inner: Element<'_, Message> = match self.step {
            0 => self.step_recipient(
                t,
                contacts.entries,
                recipient_name.clone(),
                recipient_in_book,
            ),
            1 => self.step_amount(t, portfolio, recipient_name.clone()),
            2 => self.step_review(
                t,
                portfolio,
                recipient_name.clone(),
                recipient_in_book,
                recipient_kao,
                recipient_chip,
            ),
            3 => self.step_broadcast(t, portfolio, recipient_name.clone()),
            _ => self.step_success(t, portfolio, recipient_name),
        };

        let step_kao = match self.step {
            0 => "(・・;)ゞ",
            1 => "( •̀ω•́ )✧",
            2 => "(・_・ヾ",
            3 => "( ˙▿˙ )",
            _ => "ヽ(・∀・)ﾉ",
        };

        let head_title = match self.step {
            3 => "Broadcasting",
            4 => "Complete",
            _ => "Send",
        };
        let mut head_col = column![].spacing(2);
        head_col = head_col.push(text(head_title).size(22).color(t.text).font(black()));
        if self.step < 3 {
            head_col = head_col.push(
                text(format!("Step {} of 3", self.step + 1))
                    .size(12)
                    .color(t.sub)
                    .font(mono()),
            );
        }

        let mut head = row![head_col, Space::new().width(Length::Fill),]
            .align_y(Alignment::Start)
            .width(Length::Fill);
        // The review step has its own kaomoji inside the intent banner,
        // so skip the header kaomoji to avoid doubling up.
        if self.step != 2 {
            head = head.push(kao_text(t, step_kao, 30.0));
        }

        let mut content = column![head].spacing(0);
        content = content.push(Space::new().height(20));
        if self.step < 3 {
            content = content.push(self.progress_bar(t));
            content = content.push(Space::new().height(16));
        }
        content = content.push(inner);

        modal_wrapper(
            t,
            440.0,
            progress,
            Message::Close,
            Message::BoxClickIgnored,
            content.into(),
        )
    }

    fn progress_bar<'a>(&self, t: KaoTheme) -> Element<'a, Message> {
        let mut r = row![].spacing(5).width(Length::Fill);
        for i in 0..3u8 {
            let col = if i <= self.step.min(2) {
                t.a1
            } else {
                t.border
            };
            r = r.push(
                container(Space::new().width(Length::Fill).height(4))
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(Background::Color(col)),
                        border: Border {
                            color: col,
                            width: 0.0,
                            radius: Radius::from(2),
                        },
                        ..container::Style::default()
                    }),
            );
        }
        r.into()
    }

    fn step_recipient<'a>(
        &'a self,
        t: KaoTheme,
        snapshot: Vec<PickerEntry>,
        recipient_name: Option<String>,
        recipient_in_book: bool,
    ) -> Element<'a, Message> {
        let label = text("TO").size(11).color(t.sub).font(bold());

        let input = text_input("0x… address or name.eth", &self.to)
            .on_input(Message::SetTo)
            .padding(Padding::from([12, 14]))
            .size(15)
            .font(mono())
            .style(move |_theme, status| text_input_style(t, status));

        let parse_hint: Element<'_, Message> = match &self.resolution {
            Resolution::Empty => Space::new().height(0).into(),
            Resolution::Address(addr) => {
                // If the resolved address belongs to a saved contact,
                // show its name above the "valid address" tick.
                match &recipient_name {
                    Some(name) => container(
                        row![
                            text(format!("✓ {name}  ·  "))
                                .size(11)
                                .color(t.up)
                                .font(bold()),
                            text(short_address_str(&format!("{addr:#x}")))
                                .size(11)
                                .color(t.sub)
                                .font(mono()),
                        ]
                        .align_y(Alignment::Center),
                    )
                    .padding(Padding::from([4, 0]))
                    .into(),
                    None => container(text("✓ valid address").size(11).color(t.up).font(bold()))
                        .padding(Padding::from([4, 0]))
                        .into(),
                }
            }
            Resolution::AddressVerifying { pinned, name } => container(
                column![
                    row![
                        text(format!("✓ {name}  ·  "))
                            .size(11)
                            .color(t.up)
                            .font(bold()),
                        text(short_address_str(&format!("{pinned:#x}")))
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
                    text(short_address_str(&format!("{addr:#x}")))
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
                let banner = column![
                    text(format!(
                        "⚠ ENS “{name}” now resolves to a different address"
                    ))
                    .size(12)
                    .color(t.down)
                    .font(bold()),
                    Space::new().height(4),
                    row![
                        text("pinned: ").size(11).color(t.sub),
                        text(short_address_str(&format!("{pinned:#x}")))
                            .size(11)
                            .color(t.sub)
                            .font(mono()),
                    ],
                    row![
                        text("now:    ").size(11).color(t.sub),
                        text(short_address_str(&format!("{fresh:#x}")))
                            .size(11)
                            .color(t.text)
                            .font(mono()),
                    ],
                    Space::new().height(6),
                    secondary_button(t, "Use new address").on_press(Message::AcceptEnsDivergence),
                ]
                .spacing(2);
                container(banner)
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
        // settled, sendable address that's not already in the book. We
        // also surface it for resolved-ENS rows so the user can pin
        // both the ENS and its address in one step.
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

        // Picker covers contacts + own accounts + own Safes (active
        // sender excluded). "RECIPIENTS" reads more accurately than
        // "RECENT" now that the list isn't ordered by recency.
        let recent_label = text("RECIPIENTS").size(11).color(t.sub).font(bold());

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
            // Cap the picker's vertical footprint at ~3 rows. Without
            // this, a wallet with a long list would push the
            // Continue button off the modal — and the modal sits in
            // a fixed-width container with no outer scroll, so
            // there's no recovery.
            scrollable(col)
                .height(Length::Fixed(168.0))
                .width(Length::Fill)
                .style(move |_, status| kao_scrollable_style(t, status))
                .into()
        };

        let can_continue = self.resolution.recipient().is_some()
            && !matches!(self.resolution, Resolution::EnsDivergence { .. });
        let continue_btn =
            primary_button(t, "Continue →", can_continue).on_press_maybe(if can_continue {
                Some(Message::Step(1))
            } else {
                None
            });

        column![
            label,
            Space::new().height(6),
            input,
            parse_hint,
            save_cta,
            Space::new().height(12),
            recent_label,
            Space::new().height(4),
            contacts_block,
            Space::new().height(16),
            continue_btn,
        ]
        .width(Length::Fill)
        .into()
    }

    fn step_amount<'a>(
        &'a self,
        t: KaoTheme,
        portfolio: &'a [LiveToken],
        recipient_name: Option<String>,
    ) -> Element<'a, Message> {
        let recipient = self.resolution.recipient();
        let recipient_kao = "(￣ω￣)";

        let recipient_summary: Element<'_, Message> = match recipient {
            Some(addr) => {
                let mut col = column![
                    container(avatar(t, recipient_kao, 52.0, t.ab2))
                        .width(Length::Fill)
                        .center_x(Length::Fill),
                    Space::new().height(8),
                ]
                .align_x(Alignment::Center);
                // Header line above the chunked address. Priority:
                //   contact name > resolved ENS > nothing.
                // The chunked address is always the load-bearing
                // identifier the user is signing for; the name above
                // is supporting context.
                let header_label: Option<String> = recipient_name.clone().or_else(|| {
                    if let Resolution::Resolved { name, .. } = &self.resolution {
                        Some(name.clone())
                    } else {
                        None
                    }
                });
                if let Some(name) = header_label {
                    col = col.push(
                        container(text(name).size(13).color(t.text).font(bold()))
                            .width(Length::Fill)
                            .center_x(Length::Fill),
                    );
                    col = col.push(Space::new().height(4));
                }
                col = col.push(colored_address(t, addr));
                col.into()
            }
            None => column![
                container(
                    text("Recipient parse failed")
                        .size(13)
                        .color(t.down)
                        .font(bold())
                )
                .width(Length::Fill)
                .center_x(Length::Fill),
            ]
            .into(),
        };

        let mut tabs = row![].spacing(7).width(Length::Fill);
        for (i, tk) in portfolio.iter().take(4).enumerate() {
            tabs = tabs.push(self.token_tab(t, i, tk));
        }

        let token = portfolio.get(self.token_idx);
        let token_bal = token.map(|t| t.balance.as_str()).unwrap_or("0");
        let token_sym_with_chain = token
            .map(|t| format_symbol(&t.symbol, t.chain))
            .unwrap_or_else(|| "ETH".into());
        let amount_input = text_input("0.00", &self.amount)
            .on_input(Message::SetAmount)
            .padding(14)
            .size(34)
            .font(mono_black())
            .align_x(Alignment::Center)
            .style(move |_theme, status| text_input_style(t, status));

        // Live amount validation. Rejects unparseable input, zero, and
        // amounts above balance.
        let parsed_amount = token.and_then(|tk| parse_amount_units(&self.amount, tk.decimals).ok());
        let amount_valid = match (parsed_amount, token) {
            (Some(amt), Some(tk)) => !amt.is_zero() && amt <= tk.balance_raw,
            _ => false,
        };
        let amount_hint: Element<'_, Message> = if self.amount.trim().is_empty() {
            Space::new().height(0).into()
        } else if !amount_valid {
            container(
                text(match (parsed_amount, token) {
                    (None, _) => "Not a valid amount".to_string(),
                    (Some(amt), Some(tk)) if amt > tk.balance_raw => {
                        format!("Exceeds balance ({} {})", tk.balance, tk.symbol)
                    }
                    _ => "Amount must be > 0".to_string(),
                })
                .size(11)
                .color(t.down)
                .font(bold()),
            )
            .padding(Padding::from([4, 0]))
            .into()
        } else {
            Space::new().height(0).into()
        };

        let bal_line = row![
            text(format!("Balance: {} {}", token_bal, token_sym_with_chain))
                .size(12)
                .color(t.sub),
            Space::new().width(Length::Fill),
            ghost_button(t, text("Max").size(12).color(t.a1).font(bold()))
                .padding(Padding::from([2, 6]))
                .on_press(Message::Max),
        ]
        .width(Length::Fill);

        let back_btn = secondary_button(t, "← Back").on_press(Message::Step(0));
        let review_btn =
            primary_button(t, "Review →", amount_valid).on_press_maybe(if amount_valid {
                Some(Message::Step(2))
            } else {
                None
            });

        let action_row = row![
            container(back_btn).width(Length::FillPortion(1)),
            Space::new().width(9),
            container(review_btn).width(Length::FillPortion(2)),
        ]
        .width(Length::Fill);

        column![
            recipient_summary,
            Space::new().height(20),
            tabs,
            Space::new().height(14),
            amount_input,
            amount_hint,
            Space::new().height(7),
            bal_line,
            Space::new().height(16),
            action_row,
        ]
        .width(Length::Fill)
        .into()
    }

    fn token_tab<'a>(&self, t: KaoTheme, i: usize, tk: &'a LiveToken) -> Element<'a, Message> {
        let active = i == self.token_idx;
        let border_col = if active { t.a1 } else { t.border };
        let bg = if active { t.ab1 } else { t.card };
        // Always render the chain label below the symbol. The portfolio
        // can carry the same symbol on multiple chains (USDC on Mainnet
        // and Base, ETH on Mainnet and Optimism); without a chain line
        // those tabs are visually identical and clicking the wrong one
        // sends on the wrong network.
        let inner = column![
            kao_text(t, kaomoji_for_index(i), 11.0),
            Space::new().height(1),
            text(&tk.symbol).size(12).color(t.text).font(bold()),
            text(tk.chain.label()).size(9).color(t.sub).font(mono()),
        ]
        .align_x(Alignment::Center)
        .spacing(0);

        button(inner)
            .width(Length::Fill)
            .padding(Padding::from([8, 4]))
            .on_press(Message::SetToken(i))
            .style(move |_theme, status| button::Style {
                background: Some(Background::Color(match status {
                    button::Status::Hovered | button::Status::Pressed => hover_tint(bg, t.text),
                    _ => bg,
                })),
                text_color: t.text,
                border: Border {
                    color: border_col,
                    width: 1.5,
                    radius: Radius::from(10),
                },
                ..button::Style::default()
            })
            .into()
    }

    fn step_review<'a>(
        &'a self,
        t: KaoTheme,
        portfolio: &'a [LiveToken],
        recipient_name: Option<String>,
        recipient_in_book: bool,
        recipient_kao: Option<String>,
        recipient_chip: Option<&'static str>,
    ) -> Element<'a, Message> {
        let token = portfolio.get(self.token_idx);
        let token_sym = token.map(|t| t.symbol.as_str()).unwrap_or("ETH");
        let recipient = self.resolution.recipient();
        let chain = token.map(|t| t.chain).unwrap_or_default();

        // Precompute gas-insufficiency flag — used by intent banner,
        // gas warning card, and button label.
        let has_insufficient_eth = match (token, self.quote.as_ref()) {
            (Some(tk), Some(q)) => {
                let eth_balance = portfolio
                    .iter()
                    .find(|p| p.chain == tk.chain && p.contract.is_none())
                    .map(|p| p.balance_raw);
                let needed = if tk.contract.is_none() {
                    parse_amount_units(&self.amount, tk.decimals)
                        .ok()
                        .map(|amt| amt.saturating_add(q.eth_cost_wei))
                } else {
                    Some(q.eth_cost_wei)
                };
                matches!((eth_balance, needed), (Some(bal), Some(need)) if need > bal)
            }
            _ => false,
        };
        let sim_reverted = self
            .quote
            .as_ref()
            .map(|q| q.sim.is_revert())
            .unwrap_or(false);

        let recipient_short = recipient_name.clone().unwrap_or_else(|| {
            recipient
                .map(|a| short_address_str(&format!("{a:#x}")))
                .unwrap_or_else(|| self.to.clone())
        });
        let usd_price = token.map(|tk| tk.usd_price).unwrap_or(0.0);
        let amount_f = self.amount.parse::<f64>().unwrap_or(0.0);
        let usd_value = amount_f * usd_price;

        // ── (a) Plain-English intent banner ─────────────────────────
        let intent_kao = if has_insufficient_eth {
            "(・_・;)"
        } else {
            "( ◜◡◝ )"
        };

        let intent_text = row![
            text("You're sending ").size(15).color(t.text).font(bold()),
            text(format!("{} {}", self.amount, token_sym))
                .size(15)
                .color(t.a1)
                .font(bold()),
            text(" to ").size(15).color(t.text).font(bold()),
            text(format!("{}.", recipient_short.clone()))
                .size(15)
                .color(t.a1)
                .font(bold()),
        ]
        .align_y(Alignment::Center);

        let usd_sub = if usd_price > 0.0 {
            format!("on {} · ≈ ${usd_value:.2}", chain.display_name(),)
        } else {
            format!("on {}", chain.display_name())
        };

        let intent_banner = container(
            row![
                kao_text(t, intent_kao, 22.0),
                Space::new().width(12),
                column![
                    intent_text,
                    text(usd_sub).size(12).color(t.sub).font(bold()),
                ]
                .spacing(3),
            ]
            .align_y(Alignment::Center)
            .width(Length::Fill),
        )
        .padding(Padding::from([15, 17]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(t.ab1)),
            border: Border {
                color: with_alpha(t.a1, 0.27),
                width: 1.0,
                radius: Radius::from(15),
            },
            text_color: Some(t.text),
            ..container::Style::default()
        });

        // ── (b) Simulated balance changes ───────────────────────────
        let sim_unavailable = self
            .quote
            .as_ref()
            .map(|q| matches!(q.sim.outcome, SimOutcome::Unavailable))
            .unwrap_or(true);

        let balance_changes_card: Element<'_, Message> = if sim_reverted || sim_unavailable {
            match self.quote.as_ref().map(|q| &q.sim) {
                Some(sim) => simulation_block(t, sim, chain, portfolio),
                None => Space::new().height(0).into(),
            }
        } else {
            let mut changes_col = column![].spacing(0).width(Length::Fill);

            // Your token debit
            let balance_before = token.map(|tk| tk.balance_f64).unwrap_or(0.0);
            let balance_after = balance_before - amount_f;
            changes_col = changes_col.push(sim_row(
                t,
                chain,
                token.and_then(|tk| tk.contract),
                format!("Your {token_sym}"),
                Some(format!("{:.2} → {:.2}", balance_before, balance_after)),
                format!("− {} {token_sym}", self.amount),
                t.down,
            ));
            changes_col = changes_col.push(divider_line(t));

            // Gas fee debit
            if let Some(q) = &self.quote {
                let eth_str = format_units(q.eth_cost_wei, 18u8).unwrap_or_else(|_| "0".into());
                let eth_short = trim_eth_display(&eth_str);
                let eth_usd = portfolio.first().map(|p| p.usd_price).unwrap_or(0.0);
                let gas_usd = eth_str.parse::<f64>().unwrap_or(0.0) * eth_usd;
                let gas_sub = if eth_usd > 0.0 {
                    format!("≈ ${gas_usd:.2} · paid in ETH")
                } else {
                    "paid in ETH".into()
                };
                changes_col = changes_col.push(sim_row(
                    t,
                    chain,
                    None,
                    "Network fee".into(),
                    Some(gas_sub),
                    format!("− {eth_short} ETH"),
                    t.down,
                ));
                changes_col = changes_col.push(divider_line(t));
            }

            // Recipient credit
            changes_col = changes_col.push(sim_row(
                t,
                chain,
                token.and_then(|tk| tk.contract),
                format!("{recipient_short} receives"),
                None,
                format!("+ {} {token_sym}", self.amount),
                t.up,
            ));

            // Header row with label + badge
            let header_row = row![
                text("AFTER THIS TRANSACTION")
                    .size(10)
                    .color(t.sub)
                    .font(mono_bold()),
                Space::new().width(Length::Fill),
                hint_pill(t, "⟁ simulated · revm"),
            ]
            .align_y(Alignment::Center)
            .width(Length::Fill);

            container(column![header_row, Space::new().height(2), changes_col,].width(Length::Fill))
                .padding(
                    Padding::new(4.0)
                        .top(12.0)
                        .left(16.0)
                        .right(16.0)
                        .bottom(13.0),
                )
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(t.card_alt)),
                    border: Border {
                        color: t.border,
                        width: 1.0,
                        radius: Radius::from(15),
                    },
                    text_color: Some(t.text),
                    ..container::Style::default()
                })
                .into()
        };

        // ── (c) Recipient card ──────────────────────────────────────
        let recipient_card: Element<'_, Message> = match recipient {
            Some(addr) => {
                let kao = recipient_kao.unwrap_or_else(|| "(◕‿◕)".to_string());

                // Header: RECIPIENT label + optional badge
                let mut header_row =
                    row![text("RECIPIENT").size(10).color(t.sub).font(mono_bold()),]
                        .align_y(Alignment::Center);
                if recipient_in_book {
                    header_row = header_row.push(Space::new().width(Length::Fill));
                    header_row = header_row.push(
                        // Green badge — uses t.up color for "matches saved contact"
                        container(
                            text("✓ matches saved contact")
                                .size(10)
                                .color(t.up)
                                .font(mono_bold()),
                        )
                        .padding(Padding::from([3, 8]))
                        .style(move |_| container::Style {
                            background: Some(Background::Color(with_alpha(t.up, 0.08))),
                            border: Border {
                                color: with_alpha(t.up, 0.22),
                                width: 1.0,
                                radius: Radius::from(6),
                            },
                            ..container::Style::default()
                        }),
                    );
                }

                // Avatar + name + chip
                let display_name = recipient_name.clone().unwrap_or_else(|| {
                    if let Resolution::Resolved { name, .. } = &self.resolution {
                        name.clone()
                    } else {
                        short_address_str(&format!("{addr:#x}"))
                    }
                });
                let mut name_col =
                    column![text(display_name).size(14).color(t.text).font(bold()),].spacing(1);
                if let Some(chip) = recipient_chip {
                    name_col = name_col.push(text(chip).size(11).color(t.sub).font(bold()));
                }
                let name_row = row![avatar_owned(t, kao, 30.0), Space::new().width(10), name_col,]
                    .align_y(Alignment::Center);

                // Address sub-container with darker background.
                // Use the compact colored address (size 12) to fit
                // inside the card without overflow.
                let addr_container = container(colored_address_compact(t, addr))
                    .padding(Padding::from([10, 12]))
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(Background::Color(with_alpha(t.bg, 0.5))),
                        border: Border {
                            color: t.border,
                            width: 1.0,
                            radius: Radius::from(10),
                        },
                        ..container::Style::default()
                    });

                container(
                    column![
                        header_row,
                        Space::new().height(8),
                        name_row,
                        Space::new().height(10),
                        addr_container,
                    ]
                    .width(Length::Fill),
                )
                .padding(Padding::from([13, 16]))
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(t.card_alt)),
                    border: Border {
                        color: t.border,
                        width: 1.0,
                        radius: Radius::from(15),
                    },
                    text_color: Some(t.text),
                    ..container::Style::default()
                })
                .into()
            }
            None => container(
                text("(invalid recipient)")
                    .size(13)
                    .color(t.down)
                    .font(bold()),
            )
            .into(),
        };

        // ── (d) Collapsible decoded calldata ────────────────────────
        let calldata_block: Element<'_, Message> =
            if function_panel::view::<Message>(t, self.decoded.as_deref(), self.decoded_loading)
                .is_some()
                || self.decoded_loading
            {
                let fn_name: Option<String> = self.decoded.as_deref().and_then(|d| match d {
                    DecodeResult::ClearSigned { model, .. }
                    | DecodeResult::Fallback { model, .. } => Some(model.intent.clone()),
                    DecodeResult::Heuristic(decoded) => decoded.function_name.clone(),
                    DecodeResult::Empty => None,
                });
                let pill_label: Option<String> = fn_name.map(|name| {
                    if name.len() > 30 {
                        format!("{}…", &name[..28])
                    } else {
                        name
                    }
                });

                // The toggle button row
                let caret = if self.show_calldata { "▾" } else { "▸" };
                let mut toggle_row = row![
                    text(caret).size(12).color(t.sub).font(mono()),
                    Space::new().width(6),
                    text("Decoded call data")
                        .size(13)
                        .color(t.text)
                        .font(bold()),
                ]
                .align_y(Alignment::Center)
                .spacing(0);
                if let Some(label) = pill_label {
                    toggle_row = toggle_row.push(Space::new().width(8));
                    // Inline badge with owned string
                    toggle_row = toggle_row.push(
                        container(text(format!("{label}()")).size(10).color(t.a1).font(mono()))
                            .padding(Padding::from([2, 7]))
                            .style(move |_| container::Style {
                                border: Border {
                                    color: with_alpha(t.a1, 0.22),
                                    width: 1.0,
                                    radius: Radius::from(6),
                                },
                                ..container::Style::default()
                            }),
                    );
                }
                toggle_row = toggle_row.push(Space::new().width(Length::Fill));
                toggle_row = toggle_row.push(
                    text(if self.show_calldata {
                        "hide"
                    } else {
                        "for the paranoid"
                    })
                    .size(11)
                    .color(t.sub)
                    .font(mono()),
                );

                let toggle_btn: Element<'_, Message> = button(toggle_row.width(Length::Fill))
                    .width(Length::Fill)
                    .padding(Padding::from([13, 16]))
                    .on_press(Message::ToggleCalldata)
                    .style(move |_theme, _status| button::Style {
                        background: Some(Background::Color(Color::TRANSPARENT)),
                        text_color: t.text,
                        ..button::Style::default()
                    })
                    .into();

                let expanded: Element<'_, Message> = if self.show_calldata {
                    match function_panel::view::<Message>(
                        t,
                        self.decoded.as_deref(),
                        self.decoded_loading,
                    ) {
                        Some(panel) => container(panel)
                            .padding(Padding::from([0, 13]).bottom(13.0))
                            .width(Length::Fill)
                            .style(move |_| container::Style {
                                background: Some(Background::Color(t.bg)),
                                border: Border {
                                    color: t.border,
                                    width: 1.0,
                                    radius: Radius::from(11),
                                },
                                ..container::Style::default()
                            })
                            .into(),
                        None => Space::new().height(0).into(),
                    }
                } else {
                    Space::new().height(0).into()
                };

                // Wrap the whole thing in a card
                container(column![toggle_btn, expanded].spacing(0).width(Length::Fill))
                    .width(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(Background::Color(t.card_alt)),
                        border: Border {
                            color: t.border,
                            width: 1.0,
                            radius: Radius::from(15),
                        },
                        ..container::Style::default()
                    })
                    .into()
            } else {
                Space::new().height(0).into()
            };

        // ── (e) Verification badges ─────────────────────────────────
        let good_badge = |label: &'a str| -> Element<'a, Message> {
            container(text(label).size(10).color(t.up).font(mono_bold()))
                .padding(Padding::from([3, 7]))
                .style(move |_| container::Style {
                    background: Some(Background::Color(with_alpha(t.up, 0.06))),
                    border: Border {
                        color: with_alpha(t.up, 0.22),
                        width: 1.0,
                        radius: Radius::from(6),
                    },
                    ..container::Style::default()
                })
                .into()
        };
        let badges_row = row![
            good_badge("✓ Simulated locally · revm"),
            Space::new().width(7),
            good_badge("✓ Verified by Helios"),
        ]
        .align_y(Alignment::Center);

        // ── (f) Blocking gas warning ────────────────────────────────
        let gas_warning: Element<'_, Message> = if has_insufficient_eth {
            let gas_eth_str = self
                .quote
                .as_ref()
                .map(|q| {
                    let s = format_units(q.eth_cost_wei, 18u8).unwrap_or_else(|_| "0".into());
                    trim_eth_display(&s).to_string()
                })
                .unwrap_or_else(|| "—".into());
            container(
                row![
                    kao_text(t, "(；・_・)", 20.0),
                    Space::new().width(11),
                    column![
                        text("Can't sign yet — not enough ETH for gas")
                            .size(13)
                            .color(t.down)
                            .font(bold()),
                        Space::new().height(3),
                        text(format!(
                            "This network fee is paid in ETH. You need ≈ {} ETH on {}, but your ETH balance on this chain is 0.",
                            gas_eth_str,
                            chain.display_name(),
                        ))
                        .size(12)
                        .color(t.sub),
                    ]
                    .width(Length::Fill),
                ]
                .align_y(Alignment::Center)
                .width(Length::Fill),
            )
            .padding(Padding::from([13, 15]))
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(Background::Color(with_alpha(t.down, 0.08))),
                border: Border {
                    color: with_alpha(t.down, 0.35),
                    width: 1.0,
                    radius: Radius::from(14),
                },
                ..container::Style::default()
            })
            .into()
        } else {
            Space::new().height(0).into()
        };

        // ── Error block ─────────────────────────────────────────────
        let error_block: Element<'_, Message> = match &self.error {
            Some(msg) => container(
                text(format!("(╥﹏╥) {msg}"))
                    .size(12)
                    .color(t.down)
                    .font(bold()),
            )
            .padding(Padding::from([10, 4]))
            .into(),
            None => Space::new().height(0).into(),
        };

        // ── (g) Action buttons ──────────────────────────────────────
        let back_btn = secondary_button(t, "← Back").on_press(Message::Step(1));
        let confirm_enabled = !self.busy && self.quote.is_some() && !has_insufficient_eth;
        let confirm_label = if has_insufficient_eth {
            "Need ETH for gas"
        } else if sim_reverted {
            "Sign anyway ⚠"
        } else {
            "Sign & Send"
        };
        let confirm_btn =
            primary_button(t, confirm_label, confirm_enabled).on_press_maybe(if confirm_enabled {
                Some(Message::Confirm)
            } else {
                None
            });

        let action_row = row![
            container(back_btn).width(Length::FillPortion(1)),
            Space::new().width(9),
            container(confirm_btn).width(Length::FillPortion(2)),
        ]
        .width(Length::Fill);

        // ── Assemble + scrollable ───────────────────────────────────
        let content = column![
            intent_banner,
            Space::new().height(13),
            balance_changes_card,
            Space::new().height(13),
            recipient_card,
            Space::new().height(13),
            calldata_block,
            Space::new().height(10),
            badges_row,
            Space::new().height(13),
            gas_warning,
            error_block,
            Space::new().height(14),
            action_row,
        ]
        .width(Length::Fill)
        .padding(Padding::ZERO.right(12));

        scrollable(content)
            .width(Length::Fill)
            .style(move |_, status| kao_scrollable_style(t, status))
            .into()
    }

    fn step_broadcast<'a>(
        &'a self,
        t: KaoTheme,
        portfolio: &'a [LiveToken],
        recipient_name: Option<String>,
    ) -> Element<'a, Message> {
        let token_sym = portfolio
            .get(self.token_idx)
            .map(|t| t.symbol.as_str())
            .unwrap_or("ETH");
        let recipient_short = recipient_name.unwrap_or_else(|| {
            self.resolution
                .recipient()
                .map(|a| short_address_str(&format!("{a:#x}")))
                .unwrap_or_else(|| self.to.clone())
        });

        let big_kao = container(kao_fit(t, "( ˙▿˙ )", 280.0, 64.0))
            .width(Length::Fill)
            .center_x(Length::Fill);

        let title_str = if self.broadcast_done {
            "On its way!"
        } else if self.busy {
            "Broadcasting…"
        } else {
            "Signing…"
        };
        let title = container(text(title_str).size(22).color(t.text).font(black()))
            .width(Length::Fill)
            .center_x(Length::Fill);

        let summary = container(
            text(format!(
                "{} {} → {}",
                self.amount, token_sym, recipient_short
            ))
            .size(14)
            .color(t.sub),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        // 3-step progress checklist
        let checklist = container(
            column![
                progress_check_row(t, "Signed locally", true),
                progress_check_row(t, "Broadcast to network", self.broadcast_done),
                progress_check_row(t, "Waiting for confirmation", false),
            ]
            .spacing(6)
            .width(Length::Fill),
        )
        .padding(Padding::from([14, 16]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(t.card_alt)),
            border: Border {
                color: t.border,
                width: 1.0,
                radius: Radius::from(12),
            },
            text_color: Some(t.text),
            ..container::Style::default()
        });

        // TX hash display when available
        let hash_block: Element<'_, Message> = match self.last_tx_hash {
            Some(h) => container(
                text(format!("tx: {}", short_address_str(&format!("{h:#x}"))))
                    .size(11)
                    .color(t.sub)
                    .font(mono()),
            )
            .width(Length::Fill)
            .center_x(Length::Fill)
            .padding(Padding::from([6, 0]))
            .into(),
            None => Space::new().height(0).into(),
        };

        column![
            Space::new().height(16),
            big_kao,
            Space::new().height(16),
            title,
            Space::new().height(6),
            summary,
            Space::new().height(16),
            checklist,
            Space::new().height(8),
            hash_block,
        ]
        .width(Length::Fill)
        .into()
    }

    fn step_success<'a>(
        &'a self,
        t: KaoTheme,
        portfolio: &'a [LiveToken],
        recipient_name: Option<String>,
    ) -> Element<'a, Message> {
        let token_sym = portfolio
            .get(self.token_idx)
            .map(|t| t.symbol.as_str())
            .unwrap_or("ETH");
        let big_kao = container(kao_fit(t, "ヽ(・∀・)ﾉ", 320.0, 76.0))
            .width(Length::Fill)
            .center_x(Length::Fill);

        let title = container(text("Sent!").size(26).color(t.text).font(black()))
            .width(Length::Fill)
            .center_x(Length::Fill);

        let recipient_short = recipient_name.unwrap_or_else(|| match &self.resolution {
            Resolution::Resolved { name, .. } => name.clone(),
            _ => self
                .resolution
                .recipient()
                .map(|a| short_address_str(&format!("{a:#x}")))
                .unwrap_or_else(|| self.to.clone()),
        });
        let detail = container(
            text(format!(
                "{} {} → {}",
                self.amount, token_sym, recipient_short,
            ))
            .size(15)
            .color(t.sub),
        )
        .width(Length::Fill)
        .center_x(Length::Fill);

        // ✓ Confirmed badge
        let confirmed_badge = container(text("✓ Confirmed").size(12).color(t.up).font(bold()))
            .padding(Padding::from([5, 12]))
            .style(move |_| container::Style {
                background: Some(Background::Color(with_alpha(t.up, 0.12))),
                border: Border {
                    color: t.up,
                    width: 1.0,
                    radius: Radius::from(8),
                },
                text_color: Some(t.up),
                ..container::Style::default()
            });
        let badge_wrap = container(confirmed_badge)
            .width(Length::Fill)
            .center_x(Length::Fill);

        // TX hash card
        let hash_card: Element<'_, Message> = match self.last_tx_hash {
            Some(h) => {
                let hash_str = format!("{h:#x}");
                let hash_display = short_address_str(&hash_str);
                let copy_btn = secondary_button(t, "Copy hash").on_press(Message::CopyHash);
                let etherscan_btn =
                    secondary_button(t, "Etherscan").on_press(Message::CopyEtherscan);
                container(
                    column![
                        text(format!("TX: {hash_display}"))
                            .size(12)
                            .color(t.sub)
                            .font(mono()),
                        Space::new().height(8),
                        row![
                            container(copy_btn).width(Length::FillPortion(1)),
                            Space::new().width(8),
                            container(etherscan_btn).width(Length::FillPortion(1)),
                        ]
                        .width(Length::Fill),
                    ]
                    .width(Length::Fill),
                )
                .padding(Padding::from([12, 14]))
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(t.card_alt)),
                    border: Border {
                        color: t.border,
                        width: 1.0,
                        radius: Radius::from(12),
                    },
                    text_color: Some(t.text),
                    ..container::Style::default()
                })
                .into()
            }
            None => Space::new().height(0).into(),
        };

        // Action row: Done + Send another
        let done_btn = secondary_button(t, "Done").on_press(Message::Close);
        let send_another_btn =
            primary_button(t, "Send another", true).on_press(Message::SendAnother);
        let action_row = row![
            container(done_btn).width(Length::FillPortion(1)),
            Space::new().width(9),
            container(send_another_btn).width(Length::FillPortion(2)),
        ]
        .width(Length::Fill);

        column![
            Space::new().height(8),
            big_kao,
            Space::new().height(16),
            title,
            Space::new().height(6),
            detail,
            Space::new().height(10),
            badge_wrap,
            Space::new().height(14),
            hash_card,
            Space::new().height(16),
            action_row,
        ]
        .width(Length::Fill)
        .into()
    }
}

// ── Review-step helper widgets ──────────────────────────────────────────────

/// Compact colored address (font size 12) for use inside the narrow
/// recipient card. Same chunk-colouring as `colored_address` but sized
/// to fit within the card without overflow.
fn colored_address_compact<'a>(t: KaoTheme, addr: Address) -> Element<'a, Message> {
    use crate::ui::kao_widgets::chunk_palette;
    let checksum = addr.to_checksum(None);
    let body = &checksum[2..];
    let chunk_colors = chunk_palette(t);
    let mut spans = row![text("0x").size(12).color(t.sub).font(mono_bold())].spacing(0);
    for (i, color) in chunk_colors.iter().enumerate() {
        let start = i * 4;
        let chunk = body[start..start + 4].to_string();
        spans = spans.push(text(chunk).size(12).color(*color).font(mono_bold()));
    }
    container(spans)
        .width(Length::Fill)
        .center_x(Length::Fill)
        .padding(Padding::from([2, 0]))
        .into()
}

/// Simulation row matching the design's SimRow: avatar + name column
/// (name + optional after-balance subtext) + delta amount.
fn sim_row<'a>(
    t: KaoTheme,
    chain: crate::chain::Chain,
    contract: Option<Address>,
    name: String,
    after: Option<String>,
    delta: String,
    delta_color: Color,
) -> Element<'a, Message> {
    let mut name_col = column![text(name).size(13).color(t.text).font(bold()),].spacing(1);
    if let Some(after_text) = after {
        name_col = name_col.push(text(after_text).size(10).color(t.sub).font(mono()));
    }

    row![
        token_avatar(t, chain, contract, "(•◡•)", 30.0, t.ab2),
        Space::new().width(11),
        name_col,
        Space::new().width(Length::Fill),
        text(delta).size(14).color(delta_color).font(mono_bold()),
    ]
    .align_y(Alignment::Center)
    .padding(Padding::from([10, 0]))
    .width(Length::Fill)
    .into()
}

/// 1px horizontal divider line.
fn divider_line<'a>(t: KaoTheme) -> Element<'a, Message> {
    container(Space::new().width(Length::Fill).height(1))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(t.border)),
            ..container::Style::default()
        })
        .into()
}

/// Checklist row for the broadcasting step: ✓/– marker + label.
fn progress_check_row<'a>(t: KaoTheme, label: &'a str, done: bool) -> Element<'a, Message> {
    let marker = if done { "✓" } else { "–" };
    let marker_color = if done { t.up } else { t.sub };
    let label_color = if done { t.text } else { t.sub };
    row![
        text(marker).size(14).color(marker_color).font(bold()),
        Space::new().width(8),
        text(label).size(13).color(label_color),
    ]
    .align_y(Alignment::Center)
    .width(Length::Fill)
    .into()
}

/// Picker row for one entry in the merged recipient list (contact,
/// own-account, or own-Safe). Free function rather than method
/// because it owns the snapshot data — the live book lives behind a
/// shared `RwLock` and we don't want to hold a read guard across
/// iced's widget construction.
///
/// Own-account and own-Safe entries render a small chip ("Local",
/// "Ledger", "Safe", etc.) next to the name so the user can tell at
/// a glance whether they're sending to a contact or one of their
/// own addresses. Plain contacts skip the chip (the most common
/// type — the chip would be noise).
fn picker_row<'a>(t: KaoTheme, entry: PickerEntry, current_input: &str) -> Element<'a, Message> {
    let addr = entry.address;
    let checksum = addr.to_checksum(None);
    let selected = current_input.eq_ignore_ascii_case(&checksum);
    let bg = if selected { t.ab2 } else { Color::TRANSPARENT };

    let short = short_address_str(&format!("{addr:#x}"));
    let check = if selected { "✓" } else { " " };

    let mut name_row = row![text(entry.name.clone()).size(14).color(t.text).font(bold())]
        .align_y(Alignment::Center);
    if let Some(chip) = entry.chip {
        // Tinted chip text to match the dropdown's kind labels — t.a2
        // for Safe entries, t.sub for accounts. Cheaper than rendering
        // a true pill since the recipient picker is dense.
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
    use crate::ui::kao_widgets::kao_fit_size;
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

/// "0xabcd…ef01" condenser. Used for the success step's hash + recipient
/// display, where the full hash isn't actionable.
fn short_address_str(s: &str) -> String {
    if s.len() >= 12 {
        format!("{}…{}", &s[..6], &s[s.len() - 4..])
    } else {
        s.to_string()
    }
}

/// Compact an ether-formatted decimal string for display next to a USD
/// total. Used for gas — values are typically sub-millieth, where the
/// raw `format_units` output runs to 18 fractional digits and wraps to
/// two lines on the review card.
///
/// Strategy: keep up to 3 significant digits past the leading zeros in
/// the fractional part, then trim trailing zeros. So
/// `"0.000014239683110688"` becomes `"0.0000142"` and
/// `"0.000210000000000000"` stays `"0.00021"`.
fn trim_eth_display(s: &str) -> String {
    let Some(dot) = s.find('.') else {
        return s.to_string();
    };
    let (int_part, dot_frac) = s.split_at(dot);
    let frac = &dot_frac[1..];
    let leading_zeros = frac.bytes().take_while(|b| *b == b'0').count();
    let keep = leading_zeros + 3;
    let truncated: String = frac.chars().take(keep).collect();
    let final_frac = truncated.trim_end_matches('0');
    if final_frac.is_empty() {
        int_part.to_string()
    } else {
        format!("{int_part}.{final_frac}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::{Contact, ContactEns, SafeTrust};

    fn local_account(seed: u8, name: Option<&str>) -> AccountDescriptor {
        let mut bytes = [seed; 32];
        if bytes.iter().all(|b| *b == 0) {
            bytes[0] = 1;
        }
        AccountDescriptor::Local {
            name: name.map(str::to_string),
            key_bytes: bytes,
        }
    }

    fn view_only_account(byte: u8, name: Option<&str>) -> AccountDescriptor {
        AccountDescriptor::ViewOnly {
            name: name.map(str::to_string),
            address: [byte; 20],
        }
    }

    fn safe(addr_byte: u8, name: Option<&str>, linked: Vec<u32>) -> SafeDescriptor {
        SafeDescriptor {
            name: name.map(str::to_string),
            chain_id: 1,
            address: [addr_byte; 20],
            version: "1.4.1".into(),
            trust: SafeTrust::Canonical,
            threshold: 1,
            owners: vec![[0u8; 20]; linked.len().max(1)],
            modules: Vec::new(),
            guard: None,
            fallback_handler: None,
            linked_signer_indices: linked,
            sibling_chains: Vec::new(),
            cached_at: 0,
            tx_service_url: None,
        }
    }

    fn contact(addr_byte: u8, name: &str, ens: Option<&str>) -> Contact {
        Contact {
            name: name.to_string(),
            address: [addr_byte; 20],
            kaomoji: String::new(),
            notes: String::new(),
            ens: ens.map(|n| ContactEns {
                name: n.to_string(),
                last_resolved_addr: [addr_byte; 20],
            }),
        }
    }

    #[test]
    fn merged_view_orders_contacts_before_own_addresses() {
        let mut book = ContactsBook::new();
        book.upsert(contact(0xaa, "alice", None));
        let accounts = vec![local_account(1, Some("hot wallet"))];
        let safes = vec![safe(0xcc, Some("treasury"), vec![0])];
        let view = ContactsView::merged(&book, &accounts, &safes, None, None);
        assert_eq!(view.entries.len(), 3);
        assert_eq!(view.entries[0].name, "alice");
        assert_eq!(view.entries[0].kind, PickerKind::Contact);
        assert_eq!(view.entries[1].name, "hot wallet");
        assert_eq!(view.entries[1].kind, PickerKind::OwnAccount);
        assert_eq!(view.entries[1].chip, Some("Local"));
        assert_eq!(view.entries[2].name, "treasury");
        assert_eq!(view.entries[2].kind, PickerKind::OwnSafe);
        assert_eq!(view.entries[2].chip, Some("Safe"));
    }

    #[test]
    fn merged_view_excludes_active_account_in_eoa_mode() {
        let book = ContactsBook::new();
        let accounts = vec![
            local_account(1, Some("hot")),
            local_account(2, Some("cold")),
        ];
        let view = ContactsView::merged(&book, &accounts, &[], Some(0), None);
        let names: Vec<_> = view.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["cold"]);
    }

    #[test]
    fn merged_view_excludes_active_safe_in_safe_mode() {
        let book = ContactsBook::new();
        let safes = vec![
            safe(0xc0, Some("treasury"), vec![]),
            safe(0xc1, Some("ops"), vec![]),
        ];
        let view = ContactsView::merged(&book, &[], &safes, None, Some(0));
        let names: Vec<_> = view.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["ops"]);
    }

    #[test]
    fn merged_view_dedupes_when_own_account_is_also_a_contact() {
        // Contact pinned to the same address as the user's view-only
        // account — only the contact entry should appear (user's
        // chosen name wins).
        let mut book = ContactsBook::new();
        book.upsert(contact(0x42, "alice (pinned)", Some("alice.eth")));
        let accounts = vec![view_only_account(0x42, Some("watched"))];
        let view = ContactsView::merged(&book, &accounts, &[], None, None);
        assert_eq!(view.entries.len(), 1);
        assert_eq!(view.entries[0].name, "alice (pinned)");
        assert_eq!(view.entries[0].kind, PickerKind::Contact);
        assert_eq!(view.entries[0].ens.as_deref(), Some("alice.eth"));
    }

    #[test]
    fn merged_view_renders_watch_only_safe_chip() {
        let book = ContactsBook::new();
        // Empty `linked_signer_indices` → watch-only Safe.
        let safes = vec![safe(0xcd, Some("public dao"), vec![])];
        let view = ContactsView::merged(&book, &[], &safes, None, None);
        assert_eq!(view.entries.len(), 1);
        assert_eq!(view.entries[0].kind, PickerKind::OwnSafe);
        assert_eq!(view.entries[0].chip, Some("Safe watch"));
    }

    #[test]
    fn merged_view_carries_contact_ens_through_picker_entry() {
        let mut book = ContactsBook::new();
        book.upsert(contact(0xab, "vitalik", Some("vitalik.eth")));
        let view = ContactsView::merged(&book, &[], &[], None, None);
        assert_eq!(view.entries[0].ens.as_deref(), Some("vitalik.eth"));
        // Own-account entries never carry ENS.
        let accounts = vec![local_account(1, Some("hot"))];
        let view = ContactsView::merged(&ContactsBook::new(), &accounts, &[], None, None);
        assert!(view.entries[0].ens.is_none());
    }

    #[test]
    fn name_for_resolves_across_all_picker_kinds() {
        let mut book = ContactsBook::new();
        book.upsert(contact(0xaa, "alice", None));
        let accounts = vec![local_account(1, Some("hot"))];
        let safes = vec![safe(0xcc, Some("treasury"), vec![0])];
        let view = ContactsView::merged(&book, &accounts, &safes, None, None);

        let alice_addr = Address::from([0xaa; 20]);
        assert_eq!(view.name_for(alice_addr), Some("alice"));
        // The "hot" account's address derives from the key bytes;
        // pull it back from the entries vec by kind.
        let hot_addr = view
            .entries
            .iter()
            .find(|e| matches!(e.kind, PickerKind::OwnAccount))
            .unwrap()
            .address;
        assert_eq!(view.name_for(hot_addr), Some("hot"));
        let safe_addr = Address::from([0xcc; 20]);
        assert_eq!(view.name_for(safe_addr), Some("treasury"));
        // Unknown address → None.
        assert!(view.name_for(Address::ZERO).is_none());
    }
}
