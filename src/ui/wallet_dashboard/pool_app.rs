//! Privacy Pools app — an Apps-tab pane (the `NamesApp` pattern): self-contained
//! UI state that bubbles [`Outcome`]s to the dashboard coordinator, which owns
//! all async I/O (discover/sync/quote/prove/submit) and routes every EOA
//! signature through the shared clear-sign review gate.
//!
//! Views: identity **Setup** (create / restore) → masked **Backup** (reveal on
//! demand) → **Overview** (per-chain pools + pool accounts + anonymity set) →
//! **Deposit** (clear-signed on-chain) → **Withdraw** (relayer or self-relay) →
//! **Review** (fee breakdown) → **Proving** ("Generating the ZK Proof").

// The pane is instantiated + driven by the Apps-tab coordinator wiring, which
// lands next; until that call site exists it reads as dead code. Removed then.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use alloy::primitives::utils::format_units;
use alloy::primitives::{Address, U256};
use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
use iced::{Alignment, Background, Border, Element, Length, Padding, Subscription, keyboard};
use secrecy::{ExposeSecret, SecretString};
use tracing::{debug, warn};

use crate::chain::Chain;
use crate::pool::PoolInfo;
use crate::pool::relayer::{QuoteResponse, Relayer};
use crate::pool::sync::PoolState;
use crate::portfolio::{LiveToken, format_token_balance};
use crate::ui::kao_theme::{KaoTheme, with_alpha};
use crate::ui::kao_widgets::{
    black, bold, bullet_wave, ghost_button, hint_pill, kao_scrollable_style, kao_toggle, mono,
    mono_bold, primary_button, progress_bar, screen_subtitle, screen_title, secondary_button,
    text_input_style, thin_divider, token_avatar, vspace,
};

/// The chains Privacy Pools runs on, in display order.
const CHAINS: &[Chain] = &[Chain::Mainnet, Chain::Optimism];

#[derive(Debug, Clone)]
pub enum Message {
    // setup / identity
    CreateIdentity,
    OpenRestore,
    RestoreInput(String),
    RestoreSubmit,
    // backup
    OpenBackup,
    ToggleReveal,
    CopyPhrase,
    BackupDone,
    // navigation
    SelectChain(Chain),
    Refresh,
    Back,
    /// Leave the Privacy Pools app, back to the Apps launcher.
    ExitApp,
    // asp setting
    ToggleAsp(bool),
    AspUrlInput(String),
    // deposit
    OpenDeposit(usize),
    DepositAmount(String),
    /// Set the deposit amount to a precomputed value (the Min / Max chips).
    DepositSetAmount(String),
    DepositReview,
    // withdraw
    OpenWithdraw(usize, usize),
    WithdrawTarget(String),
    WithdrawAmount(String),
    WithdrawPercent(u8),
    SelectRelayer(usize),
    WithdrawGetQuote,
    WithdrawConfirm,
    // ragequit
    Ragequit(usize, usize),
    // success screen (after a broadcast lands)
    CopyTxHash,
    CopyExplorer,
    SubmittedDone,
    Key(keyboard::Event),
    /// Animation frame while the proving screen is up (drives the spinner /
    /// progress / stage-line without any real progress signal from the prover).
    Tick,
}

/// Full parameters for a withdrawal, bubbled on quote + confirm so the
/// coordinator re-syncs and rebuilds the proof inputs from fresh state.
#[derive(Debug, Clone)]
pub struct WithdrawRequest {
    /// The pool to withdraw from (carries chain, scope, entrypoint, decimals).
    pub info: PoolInfo,
    /// Index into that pool's recovered accounts.
    pub account: usize,
    pub target: Address,
    pub amount: U256,
    /// Relayer base URL, or `None` for a self-relayed (direct) withdrawal.
    pub relayer: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// Generate + save a fresh 24-word pool mnemonic.
    CreateIdentity,
    /// Restore the pool identity from a phrase.
    RestoreIdentity(SecretString),
    /// User wants to see the recovery phrase — coordinator calls `show_backup`.
    RevealBackup,
    /// Discover pools + sync the active account's notes for `chain`.
    Sync(Chain),
    /// Deposit `amount` into `info`'s pool (clear-signed on-chain tx).
    Deposit { info: PoolInfo, amount: U256 },
    /// Fetch a relayer fee quote for a withdrawal.
    Quote(WithdrawRequest),
    /// Prove + submit the withdrawal via the relayer (the only withdrawal path).
    Submit {
        req: WithdrawRequest,
        quote: QuoteResponse,
    },
    /// Ragequit (original-depositor exit) of a pool account.
    Ragequit { info: PoolInfo, account: usize },
    /// Copy text to the clipboard (with the coordinator's auto-clear).
    CopyText(String),
    /// Leave the Privacy Pools app — the Apps coordinator returns to the
    /// launcher (never bubbles to the dashboard).
    Close,
}

/// Which pool op just broadcast — carried on [`Message::PoolSubmitted`] so the
/// success screen can word itself correctly (all three ops share one result
/// message and one success view).
#[derive(Debug, Clone, Copy)]
pub enum PoolTxKind {
    Deposit,
    Withdrawal,
    Ragequit,
}

impl PoolTxKind {
    fn title(self) -> &'static str {
        match self {
            PoolTxKind::Deposit => "Deposit submitted",
            PoolTxKind::Withdrawal => "Withdrawal submitted",
            PoolTxKind::Ragequit => "Ragequit submitted",
        }
    }

    fn blurb(self) -> &'static str {
        match self {
            PoolTxKind::Deposit => {
                "Your deposit is on its way into the pool. Your balance updates once it confirms."
            }
            PoolTxKind::Withdrawal => {
                "The relayer has submitted your withdrawal on-chain — it should confirm shortly."
            }
            PoolTxKind::Ragequit => {
                "Your exit is on its way on-chain. Your pool balance updates once it confirms."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Setup,
    Restore,
    Backup,
    Overview,
    Deposit,
    Withdraw,
    Review,
    Proving,
    /// Durable confirmation after a broadcast lands (tx hash + explorer link).
    Success,
}

pub struct PoolApp {
    view: View,
    has_identity: bool,
    chain: Chain,
    syncing: bool,
    error: Option<String>,
    /// Discovered pools for the active chain.
    pools: Vec<PoolInfo>,
    /// Per-chain discovery cache, so switching chain tabs doesn't re-hit the
    /// 0xbow API. `Refresh` bypasses it (re-discovers the active chain).
    pool_cache: HashMap<Chain, Vec<PoolInfo>>,
    /// Synced state keyed by pool address (20 bytes).
    states: BTreeMap<[u8; 20], PoolState>,
    // backup
    backup_phrase: Option<SecretString>,
    revealed: bool,
    did_copy: bool,
    // restore
    restore_input: String,
    // deposit draft
    deposit_pool: usize,
    deposit_amount: String,
    // withdraw draft
    withdraw_pool: usize,
    withdraw_account: usize,
    withdraw_target: String,
    withdraw_amount: String,
    relayers: Vec<Relayer>,
    relayer_sel: usize,
    quote: Option<QuoteResponse>,
    // ASP (0xbow association-set feed) — opt-in compliance data source.
    asp_enabled: bool,
    asp_url: String,
    // proving — the SDK's Groth16 prover emits no progress hooks, so the screen
    // shows an indeterminate-but-lively animation driven off `proving_started`
    // (see `proving_view`): a spinner, an eased asymptotic bar, and a cycling
    // stage line. `proving_label` is the op headline ("Proving the exit…").
    proving_started: Option<Instant>,
    proving_label: String,
    // success — set by `set_submitted` once a broadcast lands: the op kind and
    // its optional tx hash (relayed withdrawals may return none). Drives the
    // durable `View::Success` confirmation screen.
    submitted: Option<(PoolTxKind, Option<String>)>,
}

impl std::fmt::Debug for PoolApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never format the backup phrase.
        f.debug_struct("PoolApp")
            .field("view", &self.view)
            .field("chain", &self.chain)
            .field("pools", &self.pools.len())
            .finish()
    }
}

impl PoolApp {
    pub fn new() -> Self {
        Self {
            view: View::Setup,
            has_identity: false,
            chain: Chain::Mainnet,
            syncing: false,
            error: None,
            pools: Vec::new(),
            pool_cache: HashMap::new(),
            states: BTreeMap::new(),
            backup_phrase: None,
            revealed: false,
            did_copy: false,
            restore_input: String::new(),
            deposit_pool: 0,
            deposit_amount: String::new(),
            withdraw_pool: 0,
            withdraw_account: 0,
            withdraw_target: String::new(),
            withdraw_amount: String::new(),
            relayers: crate::pool::relayer::default_relayers(),
            relayer_sel: 0,
            asp_enabled: true,
            asp_url: crate::pool::asp::DEFAULT_ASP_URL.to_string(),
            quote: None,
            proving_started: None,
            proving_label: String::new(),
            submitted: None,
        }
    }

    // ── coordinator-driven setters ──────────────────────────────────────────

    /// First open: ask the coordinator to load the identity + sync.
    pub fn on_open(&mut self) -> Option<Outcome> {
        Some(Outcome::Sync(self.chain))
    }

    pub fn set_identity(&mut self, has: bool) {
        self.has_identity = has;
        if has && matches!(self.view, View::Setup | View::Restore) {
            self.view = View::Overview;
        } else if !has {
            self.view = View::Setup;
        }
    }

    pub fn set_syncing(&mut self, syncing: bool) {
        self.syncing = syncing;
    }

    pub fn set_error(&mut self, e: Option<String>) {
        self.error = e;
    }

    pub fn set_pools(&mut self, chain: Chain, pools: Vec<PoolInfo>, relayers: Vec<Relayer>) {
        self.pool_cache.insert(chain, pools.clone());
        if chain == self.chain {
            self.pools = pools;
            if !relayers.is_empty() {
                self.relayers = relayers;
                self.relayer_sel = self.relayer_sel.min(self.relayers.len().saturating_sub(1));
            }
        }
    }

    pub fn set_state(&mut self, pool: Address, state: PoolState) {
        self.states.insert(pool.into_array(), state);
    }

    /// The next unused deposit index for a pool from its **synced** state, or
    /// `None` when the pool hasn't been synced yet. The coordinator must not
    /// guess an index (defaulting to 0 over an existing on-chain deposit yields
    /// `PrecommitmentAlreadyUsed`); `None` means "history unknown — sync first".
    pub fn next_deposit_index(&self, pool: Address) -> Option<u64> {
        self.states
            .get(&pool.into_array())
            .map(|s| s.next_deposit_index())
    }

    /// The configured ASP feed URL when the opt-in feed is enabled, else `None`
    /// (a private withdrawal then errors, telling the user to enable it here).
    pub fn asp_url(&self) -> Option<String> {
        let url = self.asp_url.trim();
        (self.asp_enabled && !url.is_empty()).then(|| url.to_string())
    }

    /// The 0xbow API base for pool discovery — the configured endpoint (or the
    /// default). Independent of the ASP toggle: listing pools is public info,
    /// distinct from fetching the association set for a private withdrawal.
    pub fn asp_endpoint(&self) -> String {
        let url = self.asp_url.trim();
        if url.is_empty() {
            crate::pool::asp::DEFAULT_ASP_URL.to_string()
        } else {
            url.to_string()
        }
    }

    /// Drop back to the overview (e.g. after a ragequit proof lands and the
    /// review overlay opens over it, or on cancel), clearing the proving screen.
    pub fn reset_to_overview(&mut self) {
        self.proving_started = None;
        self.view = View::Overview;
    }

    /// Enter the Backup view with the recovery phrase (masked until revealed).
    pub fn show_backup(&mut self, phrase: SecretString) {
        self.backup_phrase = Some(phrase);
        self.revealed = false;
        self.did_copy = false;
        self.view = View::Backup;
    }

    pub fn set_quote(&mut self, quote: QuoteResponse) {
        self.quote = Some(quote);
        self.view = View::Review;
    }

    /// Enter the proving screen. `label` is the op headline; the animation is
    /// time-driven from now (the prover reports no real progress).
    pub fn set_proving(&mut self, label: impl Into<String>) {
        self.proving_started = Some(Instant::now());
        self.proving_label = label.into();
        self.view = View::Proving;
    }

    /// A broadcast landed — show the durable success screen with the tx hash so
    /// the user has a clear confirmation (deposit/withdraw/ragequit all route
    /// here). `tx` is `None` only when the relayer returns no hash.
    ///
    /// Returns a background re-sync so balances are fresh no matter how the user
    /// leaves the screen. This is safe: sync only mutates state (set_state /
    /// set_pools / set_syncing), never `view`, so it can't disturb the success
    /// screen underneath it.
    pub fn set_submitted(&mut self, kind: PoolTxKind, tx: Option<String>) -> Option<Outcome> {
        self.quote = None;
        self.proving_started = None;
        self.error = None;
        self.submitted = Some((kind, tx));
        self.view = View::Success;
        Some(Outcome::Sync(self.chain))
    }

    // ── update ──────────────────────────────────────────────────────────────

    pub fn update(&mut self, msg: Message) -> Option<Outcome> {
        self.error = None;
        match msg {
            Message::CreateIdentity => Some(Outcome::CreateIdentity),
            Message::OpenRestore => {
                self.view = View::Restore;
                None
            }
            Message::RestoreInput(v) => {
                self.restore_input = v;
                None
            }
            Message::RestoreSubmit => {
                let phrase = self
                    .restore_input
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if let Err(e) = crate::wallet::validate_mnemonic(&phrase) {
                    // Never log the phrase (or a validation error that might echo
                    // a word) — just the reason class.
                    debug!("privacy pools: restore rejected — invalid recovery phrase");
                    self.error = Some(format!("Invalid recovery phrase: {e}"));
                    return None;
                }
                self.restore_input.clear();
                Some(Outcome::RestoreIdentity(SecretString::new(
                    phrase.into_boxed_str(),
                )))
            }
            Message::OpenBackup => Some(Outcome::RevealBackup),
            Message::ToggleReveal => {
                self.revealed = !self.revealed;
                None
            }
            Message::CopyPhrase => {
                self.did_copy = true;
                self.backup_phrase
                    .as_ref()
                    .map(|p| Outcome::CopyText(p.expose_secret().to_string()))
            }
            Message::BackupDone => {
                self.backup_phrase = None;
                self.revealed = false;
                self.view = View::Overview;
                None
            }
            Message::SelectChain(c) => {
                if c != self.chain {
                    self.chain = c;
                    self.view = View::Overview;
                    // Serve from cache when we've already discovered this chain
                    // (account balances live in `states`, keyed by pool address,
                    // and persist across the switch) — no network round-trip.
                    if let Some(cached) = self.pool_cache.get(&c) {
                        self.pools = cached.clone();
                        return None;
                    }
                    self.pools.clear();
                    return Some(Outcome::Sync(c));
                }
                None
            }
            // Force a fresh discovery for the active chain (bypass the cache).
            Message::Refresh => {
                self.pool_cache.remove(&self.chain);
                Some(Outcome::Sync(self.chain))
            }
            Message::ExitApp => Some(Outcome::Close),
            Message::ToggleAsp(on) => {
                self.asp_enabled = on;
                None
            }
            Message::AspUrlInput(v) => {
                self.asp_url = v;
                None
            }
            Message::Back => {
                // Esc on the success screen dismisses it like the Done button.
                if matches!(self.view, View::Success) {
                    return self.update(Message::SubmittedDone);
                }
                self.view = match self.view {
                    View::Deposit | View::Withdraw | View::Backup => View::Overview,
                    View::Review => View::Withdraw,
                    other => other,
                };
                None
            }
            Message::OpenDeposit(pool) => {
                self.deposit_pool = pool;
                self.deposit_amount.clear();
                self.view = View::Deposit;
                None
            }
            Message::DepositAmount(v) => {
                self.deposit_amount = v;
                None
            }
            Message::DepositSetAmount(v) => {
                self.deposit_amount = v;
                None
            }
            Message::DepositReview => {
                let info = self.pools.get(self.deposit_pool)?;
                match parse_amount(&self.deposit_amount, info.decimals) {
                    Some(amount) if amount > U256::ZERO => Some(Outcome::Deposit {
                        info: info.clone(),
                        amount,
                    }),
                    _ => {
                        debug!("privacy pools: deposit amount invalid");
                        self.error = Some("Enter a valid amount".into());
                        None
                    }
                }
            }
            Message::OpenWithdraw(pool, account) => {
                self.withdraw_pool = pool;
                self.withdraw_account = account;
                self.withdraw_target.clear();
                self.withdraw_amount.clear();
                self.quote = None;
                self.view = View::Withdraw;
                None
            }
            Message::WithdrawTarget(v) => {
                self.withdraw_target = v;
                None
            }
            Message::WithdrawAmount(v) => {
                self.withdraw_amount = v;
                None
            }
            Message::WithdrawPercent(pct) => {
                if let Some(bal) = self.selected_account_balance() {
                    let info = &self.pools[self.withdraw_pool];
                    let amount = bal * U256::from(pct) / U256::from(100u8);
                    self.withdraw_amount = format_token_balance(amount, info.decimals).0;
                }
                None
            }
            Message::SelectRelayer(i) => {
                self.relayer_sel = i;
                None
            }
            Message::WithdrawGetQuote => self.build_withdraw_request().map(Outcome::Quote),
            Message::WithdrawConfirm => {
                let quote = self.quote.clone()?;
                self.build_withdraw_request()
                    .map(|req| Outcome::Submit { req, quote })
            }
            Message::Ragequit(pool, account) => {
                let info = self.pools.get(pool)?.clone();
                Some(Outcome::Ragequit { info, account })
            }
            Message::CopyTxHash => self
                .submitted
                .as_ref()
                .and_then(|(_, tx)| tx.clone())
                .map(Outcome::CopyText),
            Message::CopyExplorer => self
                .submitted
                .as_ref()
                .and_then(|(_, tx)| tx.as_deref())
                .map(|h| {
                    Outcome::CopyText(format!("{}/tx/{}", self.chain.default_blockscout_url(), h))
                }),
            Message::SubmittedDone => {
                self.submitted = None;
                self.view = View::Overview;
                // Refresh now that the user is heading back to the overview, so
                // the spent note / new balance shows.
                Some(Outcome::Sync(self.chain))
            }
            Message::Key(event) => {
                if let keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                } = event
                {
                    return self.update(Message::Back);
                }
                None
            }
            // Re-runs `update` so the proving view re-renders each frame (the
            // subscription only advances the animation by delivering a message).
            Message::Tick => None,
        }
    }

    fn build_withdraw_request(&mut self) -> Option<WithdrawRequest> {
        let info = self.pools.get(self.withdraw_pool)?;
        let target = match self.withdraw_target.trim().parse::<Address>() {
            Ok(a) => a,
            Err(_) => {
                debug!("privacy pools: withdraw target address invalid");
                self.error = Some("Enter a valid target address".into());
                return None;
            }
        };
        let amount = match parse_amount(&self.withdraw_amount, info.decimals) {
            Some(a) if a > U256::ZERO => a,
            _ => {
                debug!("privacy pools: withdraw amount invalid");
                self.error = Some("Enter a valid amount".into());
                return None;
            }
        };
        let Some(relayer) = self.relayers.get(self.relayer_sel).map(|r| r.url.clone()) else {
            warn!("privacy pools: no relayer available for this chain");
            self.error = Some("no relayer available for this chain".into());
            return None;
        };
        Some(WithdrawRequest {
            info: info.clone(),
            account: self.withdraw_account,
            target,
            amount,
            relayer: Some(relayer),
        })
    }

    fn selected_account_balance(&self) -> Option<U256> {
        let info = self.pools.get(self.withdraw_pool)?;
        let state = self.states.get(&info.pool.into_array())?;
        let acct = state.accounts.get(self.withdraw_account)?;
        acct.spendable()
            .map(|c| privacy_pools::field_to_u256(c.value))
    }

    pub fn subscription(&self) -> Subscription<Message> {
        // The proving screen animates off a ~60 Hz timer tick — nothing else in
        // the app drives redraws, so without this the spinner/bar would freeze.
        if matches!(self.view, View::Proving) {
            return Subscription::batch([
                keyboard::listen().map(Message::Key),
                iced::time::every(Duration::from_millis(16)).map(|_| Message::Tick),
            ]);
        }
        match self.view {
            View::Setup | View::Overview => Subscription::none(),
            _ => keyboard::listen().map(Message::Key),
        }
    }

    // ── view ─────────────────────────────────────────────────────────────────

    pub fn view<'a>(&'a self, t: KaoTheme, portfolio: &'a [LiveToken]) -> Element<'a, Message> {
        let content = match self.view {
            View::Setup => self.setup_view(t),
            View::Restore => self.restore_view(t),
            View::Backup => self.backup_view(t),
            View::Overview => self.overview_view(t),
            View::Deposit => self.deposit_view(t, portfolio),
            View::Withdraw => self.withdraw_view(t),
            View::Review => self.review_view(t),
            View::Proving => self.proving_view(t),
            View::Success => self.success_view(t),
        };
        // Wider than the forms so the overview's 2-column pool grid has room.
        let max_w = if matches!(self.view, View::Overview) {
            920
        } else {
            560
        };
        let mut col = column![content].width(Length::Fill).max_width(max_w);
        if let Some(e) = &self.error {
            col = col
                .push(vspace(10))
                .push(text(e).size(12).color(t.down).font(mono()));
        }
        scrollable(
            container(col)
                .center_x(Length::Fill)
                .padding(Padding::from([28, 32])),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_, s| kao_scrollable_style(t, s))
        .into()
    }

    fn setup_view<'a>(&self, t: KaoTheme) -> Element<'a, Message> {
        column![
            exit_link(t),
            vspace(10),
            title_with_kaomoji(t),
            vspace(6),
            screen_subtitle(t, "Deposit and withdraw privately on Ethereum & Optimism, verified with zero-knowledge proofs."),
            vspace(22),
            card(t, column![
                text("Create a pool identity").size(15).color(t.text).font(bold()),
                vspace(4),
                text("A dedicated 24-word recovery phrase — separate from your wallet seed — powers all of your pool accounts. Back it up: it can't be exported later.")
                    .size(12).color(t.sub).font(mono()),
                vspace(14),
                primary_button(t, "Create identity", true).on_press(Message::CreateIdentity),
            ].into()),
            vspace(10),
            container(
                ghost_button(t, text("I already have a phrase — restore").size(13).color(t.a1).font(bold()))
                    .on_press(Message::OpenRestore),
            )
            .center_x(Length::Fill),
        ]
        .width(Length::Fill)
        .into()
    }

    fn restore_view<'a>(&self, t: KaoTheme) -> Element<'a, Message> {
        column![
            back_link(t),
            vspace(8),
            screen_title(t, "Restore pool identity"),
            vspace(6),
            screen_subtitle(t, "Enter your 24-word Privacy Pools recovery phrase."),
            vspace(18),
            text_input("word1 word2 word3 …", &self.restore_input)
                .on_input(Message::RestoreInput)
                .on_submit(Message::RestoreSubmit)
                .padding(12)
                .style(move |_theme, s| text_input_style(t, s)),
            vspace(14),
            primary_button(t, "Restore", !self.restore_input.trim().is_empty())
                .on_press(Message::RestoreSubmit),
        ]
        .width(Length::Fill)
        .into()
    }

    fn backup_view<'a>(&self, t: KaoTheme) -> Element<'a, Message> {
        let words: Vec<String> = self
            .backup_phrase
            .as_ref()
            .map(|p| {
                p.expose_secret()
                    .split_whitespace()
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        let mut grid = column![].spacing(8);
        for (chunk_idx, chunk) in words.chunks(4).enumerate() {
            let mut r = row![].spacing(8);
            for (off, w) in chunk.iter().enumerate() {
                let idx = chunk_idx * 4 + off + 1;
                let shown = if self.revealed {
                    w.as_str()
                } else {
                    "••••••"
                };
                r = r.push(word_cell(t, idx, shown));
            }
            grid = grid.push(r);
        }

        let reveal_label = if self.revealed {
            "Hide"
        } else {
            "Tap to reveal"
        };
        column![
            back_link(t),
            vspace(8),
            screen_title(t, "Recovery phrase"),
            vspace(6),
            screen_subtitle(t, "Write these 24 words down and keep them offline. Anyone with them controls your pool funds."),
            vspace(18),
            grid,
            vspace(14),
            row![
                secondary_button(t, reveal_label).on_press(Message::ToggleReveal),
                Space::new().width(10),
                secondary_button(t, if self.did_copy { "Copied ✓" } else { "Copy" })
                    .on_press(Message::CopyPhrase),
            ],
            vspace(14),
            primary_button(t, "I've saved it", true).on_press(Message::BackupDone),
        ]
        .width(Length::Fill)
        .into()
    }

    fn overview_view<'a>(&'a self, t: KaoTheme) -> Element<'a, Message> {
        let mut col = column![
            exit_link(t),
            vspace(10),
            row![
                text("Privacy Pools").size(22).color(t.text).font(black()),
                Space::new().width(8),
                text("ヾ(⌐■_■)ノ♪").size(15).color(t.a3).font(mono()),
                Space::new().width(Length::Fill),
                ghost_button(t, text("⟳ Refresh").size(12).color(t.a1).font(bold()))
                    .on_press(Message::Refresh),
            ]
            .align_y(Alignment::Center),
            vspace(6),
            container(self.chain_tabs(t)).center_x(Length::Fill),
            vspace(16),
        ]
        .width(Length::Fill);

        if self.syncing && self.pools.is_empty() {
            col = col.push(empty_hint(t, "Loading pools from 0xbow…"));
        } else if self.pools.is_empty() {
            col = col.push(empty_hint(t, "No pools found on this chain."));
        } else {
            // Two-column grid: two cards per row, an empty column filler on an
            // odd trailing pool so the last card keeps half-width.
            let mut i = 0;
            while i < self.pools.len() {
                let mut r = row![]
                    .spacing(12)
                    .width(Length::Fill)
                    .align_y(Alignment::Start);
                for j in 0..2 {
                    match self.pools.get(i + j) {
                        Some(info) => r = r.push(self.pool_card(t, i + j, info)),
                        None => r = r.push(Space::new().width(Length::Fill)),
                    }
                }
                col = col.push(r).push(vspace(12));
                i += 2;
            }
        }

        col = col
            .push(vspace(6))
            .push(self.asp_section(t))
            .push(vspace(10))
            .push(
                container(
                    ghost_button(
                        t,
                        text("Back up recovery phrase")
                            .size(12)
                            .color(t.sub)
                            .font(bold()),
                    )
                    .on_press(Message::OpenBackup),
                )
                .center_x(Length::Fill),
            );
        col.into()
    }

    /// The opt-in 0xbow association-set feed toggle + endpoint. This is the ASP
    /// setting, kept in the app itself rather than global settings.
    fn asp_section<'a>(&self, t: KaoTheme) -> Element<'a, Message> {
        let head = row![
            column![
                text("0xbow association-set feed").size(13).color(t.text).font(bold()),
                text("Off by default is fully private but blocks withdrawals; on fetches the approved-deposit set from 0xbow.")
                    .size(11).color(t.sub).font(mono()),
            ]
            .spacing(2)
            .width(Length::Fill),
            Space::new().width(10),
            kao_toggle(t, self.asp_enabled, Message::ToggleAsp(!self.asp_enabled)),
        ]
        .align_y(Alignment::Center)
        .width(Length::Fill);

        let mut inner = column![head].spacing(10).width(Length::Fill);
        if self.asp_enabled {
            inner = inner.push(
                text_input("https://api.0xbow.io", &self.asp_url)
                    .on_input(Message::AspUrlInput)
                    .padding(10)
                    .style(move |_theme, s| text_input_style(t, s)),
            );
        }
        card(t, inner.into())
    }

    fn chain_tabs<'a>(&self, t: KaoTheme) -> Element<'a, Message> {
        let mut r = row![].spacing(8);
        for &c in CHAINS {
            let active = c == self.chain;
            let label =
                text(c.label())
                    .size(13)
                    .font(bold())
                    .color(if active { t.text } else { t.sub });
            r = r.push(
                button(label)
                    .padding(Padding::from([6, 14]))
                    .on_press(Message::SelectChain(c))
                    .style(move |_, _| button::Style {
                        background: Some(Background::Color(if active {
                            with_alpha(t.a3, 0.14)
                        } else {
                            t.card
                        })),
                        text_color: t.text,
                        border: Border {
                            color: if active { t.a3 } else { t.border },
                            width: 1.0,
                            radius: 10.0.into(),
                        },
                        ..Default::default()
                    }),
            );
        }
        r.into()
    }

    fn pool_card<'a>(
        &self,
        t: KaoTheme,
        pool_index: usize,
        info: &'a PoolInfo,
    ) -> Element<'a, Message> {
        let state = self.states.get(&info.pool.into_array());
        let (min_s, _) = format_token_balance(info.min_deposit, info.decimals);

        let meta = row![
            text(format!("anon set {}", info.anonymity_set))
                .size(11)
                .color(t.sub)
                .font(mono()),
            text(" · ").size(11).color(t.sub).font(mono()),
            text(if info.verified {
                "verified"
            } else {
                "unverified"
            })
            .size(11)
            .color(if info.verified { t.up } else { t.down })
            .font(mono()),
        ]
        .align_y(Alignment::Center);

        let min_line = text(format!("min {} {}", trim_zeros(&min_s), info.symbol))
            .size(11)
            .color(t.sub)
            .font(mono());

        let info_col = column![
            text(&info.symbol).size(16).color(t.text).font(bold()),
            meta,
            min_line,
        ]
        .spacing(4)
        .width(Length::Fill);

        // Bundled token logo (SVG); tokens without one fall back to the kaomoji
        // avatar. Native ETH uses the `0xEeee…` sentinel, so map it to `None` for
        // the shared native icon.
        let logo_contract = if info.is_native {
            None
        } else {
            Some(info.asset)
        };
        let logo = token_avatar(t, info.chain, logo_contract, "(≖ᴗ≖)", 34.0, t.ab2);

        // Deposit shares the row with the info block, vertically centered against
        // it rather than pinned to the symbol line.
        let header = row![
            logo,
            Space::new().width(12),
            info_col,
            small_action(t, "Deposit", Message::OpenDeposit(pool_index)),
        ]
        .align_y(Alignment::Center)
        .width(Length::Fill);

        let mut body = column![header].spacing(6).width(Length::Fill);

        // The user's own pool accounts (from the background account sync).
        let spendable: Vec<(usize, U256)> = state
            .map(|s| {
                s.accounts
                    .iter()
                    .enumerate()
                    .filter_map(|(i, a)| {
                        a.spendable()
                            .map(|c| (i, privacy_pools::field_to_u256(c.value)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (i, bal) in spendable {
            let (bs, _) = format_token_balance(bal, info.decimals);
            body = body.push(thin_divider(t)).push(
                // Label on the left, actions pushed to the right by a fill
                // spacer — mirrors the header's Deposit button.
                row![
                    text(format!("PA-{}", i + 1))
                        .size(12)
                        .color(t.text)
                        .font(mono_bold()),
                    Space::new().width(8),
                    text(format!("{} {}", trim_zeros(&bs), info.symbol))
                        .size(12)
                        .color(t.text)
                        .font(bold()),
                    Space::new().width(Length::Fill),
                    small_action(t, "Withdraw", Message::OpenWithdraw(pool_index, i)),
                    Space::new().width(6),
                    small_muted(t, "Ragequit", Message::Ragequit(pool_index, i)),
                ]
                .align_y(Alignment::Center)
                .width(Length::Fill),
            );
        }

        card(t, body.into())
    }

    fn deposit_view<'a>(&'a self, t: KaoTheme, portfolio: &'a [LiveToken]) -> Element<'a, Message> {
        let Some(info) = self.pools.get(self.deposit_pool) else {
            return empty_hint(t, "Pool unavailable.");
        };

        // Min = the pool's minimum deposit; Max = the depositor's on-chain
        // balance of this asset (omitted when we don't hold it / the chain
        // isn't loaded). Each chip carries a precise, precomputed amount string.
        let min_str = format_units(info.min_deposit, info.decimals)
            .map(|s| trim_zeros(&s))
            .unwrap_or_default();
        let mut presets =
            row![chip(t, "Min", false, Message::DepositSetAmount(min_str))].spacing(8);
        if let Some(s) = self
            .deposit_balance(info, portfolio)
            .and_then(|bal| format_units(bal, info.decimals).ok())
        {
            presets = presets.push(chip(
                t,
                "Max",
                false,
                Message::DepositSetAmount(trim_zeros(&s)),
            ));
        }

        column![
            back_link(t),
            vspace(8),
            pool_title(t, format!("Deposit {}", info.symbol)),
            vspace(6),
            screen_subtitle(
                t,
                "Funds join the pool's anonymity set. You'll review and sign the on-chain transaction."
            ),
            vspace(18),
            amount_field(t, &self.deposit_amount, &info.symbol, Message::DepositAmount),
            vspace(8),
            presets,
            vspace(14),
            primary_button(t, "Review deposit", !self.deposit_amount.trim().is_empty())
                .on_press(Message::DepositReview),
        ]
        .width(Length::Fill)
        .into()
    }

    /// The depositor's on-chain balance of `info`'s asset, matched from the
    /// portfolio by (chain, contract). `None` when it isn't held or that chain's
    /// balances aren't loaded — the Max chip is then hidden.
    fn deposit_balance(&self, info: &PoolInfo, portfolio: &[LiveToken]) -> Option<U256> {
        portfolio
            .iter()
            .find(|tk| {
                tk.chain.chain_id() == info.chain.chain_id()
                    && if info.is_native {
                        tk.contract.is_none()
                    } else {
                        tk.contract == Some(info.asset)
                    }
            })
            .map(|tk| tk.balance_raw)
    }

    fn withdraw_view<'a>(&'a self, t: KaoTheme) -> Element<'a, Message> {
        let Some(info) = self.pools.get(self.withdraw_pool) else {
            return empty_hint(t, "Pool unavailable.");
        };
        let mut relayer_row = row![].spacing(8);
        for (i, r) in self.relayers.iter().enumerate() {
            let active = i == self.relayer_sel;
            relayer_row = relayer_row.push(chip(t, &r.name, active, Message::SelectRelayer(i)));
        }

        column![
            back_link(t),
            vspace(8),
            pool_title(t, format!("Withdraw {}", info.symbol)),
            vspace(6),
            screen_subtitle(t, "A relayer submits the withdrawal and pays gas, so the recipient is unlinkable to your funded accounts."),
            vspace(18),
            text("Target address").size(11).color(t.sub).font(mono_bold()),
            vspace(4),
            text_input("0x… recipient", &self.withdraw_target)
                .on_input(Message::WithdrawTarget)
                .padding(12)
                .style(move |_theme, s| text_input_style(t, s)),
            vspace(14),
            amount_field(t, &self.withdraw_amount, &info.symbol, Message::WithdrawAmount),
            vspace(8),
            percent_row(t),
            vspace(14),
            text("Relayer").size(11).color(t.sub).font(mono_bold()),
            vspace(6),
            relayer_row,
            vspace(16),
            primary_button(t, "Get quote", true).on_press(Message::WithdrawGetQuote),
        ]
        .width(Length::Fill)
        .into()
    }

    fn review_view<'a>(&'a self, t: KaoTheme) -> Element<'a, Message> {
        let Some(info) = self.pools.get(self.withdraw_pool) else {
            return empty_hint(t, "Pool unavailable.");
        };
        let (fee_bps, fee_line) = match &self.quote {
            Some(q) => (
                q.fee_bps().unwrap_or(0),
                format!("Relay fee: {} bps", q.fee_bps().unwrap_or(0)),
            ),
            None => (0, "Fetching quote…".to_string()),
        };
        let amount = parse_amount(&self.withdraw_amount, info.decimals).unwrap_or(U256::ZERO);
        let fee = amount * U256::from(fee_bps) / U256::from(10_000u64);
        let received = amount.saturating_sub(fee);

        card(
            t,
            column![
                text("Review the withdrawal")
                    .size(16)
                    .color(t.text)
                    .font(bold()),
                vspace(10),
                kv(t, "To", &shorten(&self.withdraw_target)),
                kv(
                    t,
                    "Amount",
                    &format!(
                        "{} {}",
                        format_token_balance(amount, info.decimals).0,
                        info.symbol
                    )
                ),
                kv(t, "Fee", &fee_line),
                kv(
                    t,
                    "You receive",
                    &format!(
                        "{} {}",
                        format_token_balance(received, info.decimals).0,
                        info.symbol
                    )
                ),
                vspace(14),
                row![
                    secondary_button(t, "Back").on_press(Message::Back),
                    Space::new().width(10),
                    primary_button(t, "Confirm & prove", true).on_press(Message::WithdrawConfirm),
                ],
            ]
            .into(),
        )
    }

    fn proving_view<'a>(&'a self, t: KaoTheme) -> Element<'a, Message> {
        let elapsed = self
            .proving_started
            .map(|s| s.elapsed().as_secs_f32())
            .unwrap_or(0.0);

        // Eased asymptotic fill: quick at first, forever approaching (never
        // reaching) 100%. Honest about the prover being one opaque blocking
        // call — the screen switches away the instant it returns.
        let frac = 0.97 * (1.0 - (-elapsed / 7.0).exp());

        // Verbose stage line advances ~every 1.6 s and holds on the final
        // "Verifying" step (which genuinely is the last thing before the tx).
        let stage_idx = ((elapsed / 1.6) as usize).min(PROVING_STAGES.len() - 1);
        let dots = ".".repeat(1 + (elapsed * 2.0) as usize % 3);
        let stage = format!("{}{}", PROVING_STAGES[stage_idx], dots);

        // A little bullet-wave that sweeps back and forth — a font-safe "alive"
        // indicator (generic Monospace has no guaranteed braille glyphs).
        let spinner = bullet_wave(elapsed);

        let headline = if self.proving_label.is_empty() {
            "Working"
        } else {
            &self.proving_label
        };

        column![
            vspace(20),
            container(text(spinner).size(16).color(t.a3).font(mono_bold())).center_x(Length::Fill),
            vspace(12),
            container(
                text("Generating the ZK proof")
                    .size(20)
                    .color(t.text)
                    .font(bold())
            )
            .center_x(Length::Fill),
            vspace(6),
            container(text(headline).size(13).color(t.sub).font(mono())).center_x(Length::Fill),
            vspace(18),
            progress_bar(t, frac, t.a3),
            vspace(8),
            container(
                text(format!("{}%", (frac * 100.0) as u32))
                    .size(12)
                    .color(t.sub)
                    .font(mono())
            )
            .center_x(Length::Fill),
            vspace(14),
            container(text(stage).size(12).color(t.text).font(mono())).center_x(Length::Fill),
            vspace(4),
            container(
                text(format!("{:.0}s elapsed", elapsed))
                    .size(11)
                    .color(t.sub)
                    .font(mono())
            )
            .center_x(Length::Fill),
        ]
        .width(Length::Fill)
        .into()
    }

    /// Durable confirmation after a broadcast lands — the pool analogue of the
    /// Send flow's "Sent!" screen. Shows the op, a broadcast badge, and (when a
    /// hash is available) the tx hash with copy + explorer-link actions.
    fn success_view<'a>(&self, t: KaoTheme) -> Element<'a, Message> {
        let Some((kind, tx)) = &self.submitted else {
            return empty_hint(t, "Nothing to show.");
        };

        let mut col = column![
            vspace(10),
            container(text("(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧").size(24).color(t.a3).font(mono_bold()))
                .center_x(Length::Fill),
            vspace(16),
            container(text(kind.title()).size(22).color(t.text).font(black()))
                .center_x(Length::Fill),
            vspace(10),
            container(hint_pill(t, "✓ Broadcast to the network")).center_x(Length::Fill),
            vspace(14),
            container(screen_subtitle(t, kind.blurb())).center_x(Length::Fill),
        ]
        .width(Length::Fill)
        .align_x(Alignment::Center);

        if let Some(hash) = tx {
            col = col
                .push(vspace(18))
                .push(
                    container(
                        text(format!("TX: {}", shorten(hash)))
                            .size(13)
                            .color(t.sub)
                            .font(mono()),
                    )
                    .center_x(Length::Fill),
                )
                .push(vspace(10))
                .push(
                    container(row![
                        secondary_button(t, "Copy hash").on_press(Message::CopyTxHash),
                        Space::new().width(10),
                        secondary_button(t, "Copy explorer link").on_press(Message::CopyExplorer),
                    ])
                    .center_x(Length::Fill),
                );
        }

        col.push(vspace(22))
            .push(
                container(primary_button(t, "Done", true).on_press(Message::SubmittedDone))
                    .center_x(Length::Fill),
            )
            .into()
    }
}

/// Verbose stage lines cycled on the proving screen. They roughly trace a
/// Groth16 prove — witness synthesis → polynomial commitments → MSMs → pairing
/// → assembly → local verify — but the SDK prover is a single opaque call, so
/// these are paced on a timer, not driven by real progress.
const PROVING_STAGES: &[&str] = &[
    "Loading the proving key",
    "Gathering Merkle-tree witnesses",
    "Synthesizing the circuit witness",
    "Committing witness polynomials",
    "Running multi-scalar multiplications",
    "Evaluating pairing constraints",
    "Assembling the Groth16 proof",
    "Verifying the proof locally",
];

// ── small view helpers ──────────────────────────────────────────────────────

fn card<'a>(t: KaoTheme, inner: Element<'a, Message>) -> Element<'a, Message> {
    container(inner)
        .padding(Padding::from([16, 18]))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(Background::Color(t.card)),
            border: Border {
                color: t.border,
                width: 1.0,
                radius: 16.0.into(),
            },
            text_color: Some(t.text),
            ..Default::default()
        })
        .into()
}

fn back_link<'a>(t: KaoTheme) -> Element<'a, Message> {
    ghost_button(t, text("← Back").size(13).color(t.sub).font(bold()))
        .on_press(Message::Back)
        .into()
}

/// Top-left "← Apps" link that leaves the whole Privacy Pools app for the Apps
/// launcher (distinct from `back_link`, which steps back one view within it).
fn exit_link<'a>(t: KaoTheme) -> Element<'a, Message> {
    ghost_button(t, text("← Apps").size(13).color(t.sub).font(bold()))
        .on_press(Message::ExitApp)
        .into()
}

/// The centered "Privacy Pools ヾ(⌐■_■)ノ♪" heading.
fn title_with_kaomoji<'a>(t: KaoTheme) -> Element<'a, Message> {
    container(
        row![
            text("Privacy Pools").size(26).color(t.text).font(black()),
            Space::new().width(10),
            text("ヾ(⌐■_■)ノ♪").size(18).color(t.a3).font(mono()),
        ]
        .align_y(Alignment::Center),
    )
    .center_x(Length::Fill)
    .into()
}

/// An owned-title heading, styled like `screen_title` but for dynamic strings.
fn pool_title<'a>(t: KaoTheme, s: String) -> Element<'a, Message> {
    container(text(s).size(26).color(t.text).font(black()))
        .width(Length::Fill)
        .center_x(Length::Fill)
        .into()
}

fn empty_hint<'a>(t: KaoTheme, msg: &str) -> Element<'a, Message> {
    container(text(msg.to_string()).size(12).color(t.sub).font(mono()))
        .padding(20)
        .width(Length::Fill)
        .into()
}

fn word_cell<'a>(t: KaoTheme, idx: usize, word: &str) -> Element<'a, Message> {
    container(
        row![
            text(format!("{idx:>2}")).size(11).color(t.sub).font(mono()),
            Space::new().width(6),
            text(word.to_string())
                .size(13)
                .color(t.text)
                .font(mono_bold()),
        ]
        .align_y(Alignment::Center),
    )
    .padding(Padding::from([8, 10]))
    .width(Length::Fixed(128.0))
    .style(move |_| container::Style {
        background: Some(Background::Color(t.card_alt)),
        border: Border {
            color: t.border,
            width: 1.0,
            radius: 10.0.into(),
        },
        ..Default::default()
    })
    .into()
}

fn amount_field<'a>(
    t: KaoTheme,
    value: &str,
    symbol: &'a str,
    on_input: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    row![
        text_input("0.0", value)
            .on_input(on_input)
            .padding(12)
            .style(move |_theme, s| text_input_style(t, s)),
        Space::new().width(10),
        text(symbol.to_string()).size(14).color(t.sub).font(bold()),
    ]
    .align_y(Alignment::Center)
    .into()
}

fn percent_row<'a>(t: KaoTheme) -> Element<'a, Message> {
    let mut r = row![].spacing(8);
    for pct in [25u8, 50, 75, 100] {
        r = r.push(chip(
            t,
            &format!("{pct}%"),
            false,
            Message::WithdrawPercent(pct),
        ));
    }
    r.into()
}

fn chip<'a>(t: KaoTheme, label: &str, active: bool, msg: Message) -> Element<'a, Message> {
    button(
        text(label.to_string())
            .size(12)
            .font(bold())
            .color(if active { t.text } else { t.sub }),
    )
    .padding(Padding::from([5, 12]))
    .on_press(msg)
    .style(move |_, _| button::Style {
        background: Some(Background::Color(if active {
            with_alpha(t.a3, 0.14)
        } else {
            t.card
        })),
        text_color: t.text,
        border: Border {
            color: if active { t.a3 } else { t.border },
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    })
    .into()
}

fn small_action<'a>(t: KaoTheme, label: &str, msg: Message) -> Element<'a, Message> {
    button(text(label.to_string()).size(12).color(t.a1).font(bold()))
        .padding(Padding::from([4, 10]))
        .on_press(msg)
        .style(move |_, _| button::Style {
            background: Some(Background::Color(with_alpha(t.a1, 0.10))),
            text_color: t.a1,
            border: Border {
                color: with_alpha(t.a1, 0.3),
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn small_muted<'a>(t: KaoTheme, label: &str, msg: Message) -> Element<'a, Message> {
    button(text(label.to_string()).size(12).color(t.sub).font(bold()))
        .padding(Padding::from([4, 10]))
        .on_press(msg)
        .style(move |_, _| button::Style {
            background: Some(Background::Color(t.card_alt)),
            text_color: t.sub,
            border: Border {
                color: t.border,
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        })
        .into()
}

fn kv<'a>(t: KaoTheme, k: &str, v: &str) -> Element<'a, Message> {
    row![
        text(k.to_string()).size(12).color(t.sub).font(mono()),
        Space::new().width(Length::Fill),
        text(v.to_string()).size(12).color(t.text).font(mono_bold()),
    ]
    .width(Length::Fill)
    .padding(Padding::from([3, 0]))
    .into()
}

/// Trim trailing fractional zeros (and a dangling decimal point) from a
/// fixed-decimal display string: "0.010000" → "0.01", "25.0000" → "25",
/// "1000.00" → "1000". Integers (no `.`) pass through untouched.
fn trim_zeros(s: &str) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s.to_string()
    }
}

fn shorten(addr: &str) -> String {
    let a = addr.trim();
    if a.len() > 12 {
        format!("{}…{}", &a[..6], &a[a.len() - 4..])
    } else {
        a.to_string()
    }
}

/// Parse a decimal amount string into base units for `decimals`.
fn parse_amount(s: &str, decimals: u8) -> Option<U256> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let dec = decimals as usize;
    if frac_part.len() > dec {
        return None;
    }
    let mut digits = String::with_capacity(int_part.len() + dec);
    digits.push_str(int_part);
    digits.push_str(frac_part);
    for _ in 0..(dec - frac_part.len()) {
        digits.push('0');
    }
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some(U256::ZERO);
    }
    U256::from_str_radix(digits, 10).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_amount_scales_by_decimals() {
        assert_eq!(
            parse_amount("1", 18),
            Some(U256::from(10u64).pow(U256::from(18u64)))
        );
        assert_eq!(parse_amount("1.5", 6), Some(U256::from(1_500_000u64)));
        assert_eq!(
            parse_amount("0.01", 18),
            Some(U256::from(10u64).pow(U256::from(16u64)))
        );
        assert_eq!(parse_amount("", 18), None);
        assert_eq!(parse_amount("1.2345678", 6), None); // too many decimals
        assert_eq!(parse_amount("abc", 18), None);
    }

    #[test]
    fn trim_zeros_drops_trailing_fraction() {
        assert_eq!(trim_zeros("0.010000"), "0.01");
        assert_eq!(trim_zeros("25.0000"), "25");
        assert_eq!(trim_zeros("1000.00"), "1000");
        assert_eq!(trim_zeros("1.2345"), "1.2345");
        assert_eq!(trim_zeros("0"), "0");
        assert_eq!(trim_zeros("42"), "42");
    }
}
