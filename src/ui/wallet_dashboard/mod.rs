//! Kao Wallet dashboard — the main screen shown after unlock.
//!
//! Layout mirrors the HTML mock in `kao/project/Kao Wallet.html`:
//! a thin sidebar (wordmark · Home/Activity/Settings · theme dots), a header
//! with a mood kaomoji, and one of three content panes. Send and Receive are
//! modal overlays rendered via `stack`.

use std::mem;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, Bytes, TxHash};
use iced::border::Radius;
use iced::widget::operation::focus as focus_widget;
use iced::widget::{Space, column, container, row, stack, text};
use iced::{Alignment, Background, Border, Element, Length, Padding, Subscription, Task};
use tracing::{debug, info, warn};

mod account_dropdown;
mod activity;
mod appearance;
mod contacts_settings;
mod function_panel;
mod header;
mod home;
mod modal_chrome;
mod nav;
mod networks;
mod receive;
mod safe_send;
mod safe_tx_detail;
mod send;
mod settings_root;
mod sidebar;
mod swap;
mod tx_details;

use account_dropdown::AccountDropdown;
use contacts_settings::ContactsPane;
use networks::NetworksPane;
use receive::ReceivePane;
use safe_send::{SafeSendPane, SafeSendRequest};
use safe_tx_detail::SafeTxDetailPane;
use send::SendPane;
use swap::SwapPane;
use tx_details::TxDetailsPane;

/// User mood emoji shown in the header and balance hero. Currently constant;
/// future iterations might derive it from portfolio P&L or recent activity.
pub(super) const MOOD: &str = "(´｡• ᵕ •｡`)";

/// How many transactions to ask the indexer for. Generous enough that the
/// Activity scroll view is rarely empty after a single round-trip, small
/// enough that providers without server-side paging stay responsive.
const HISTORY_LIMIT: usize = 50;

/// How long after a copy we wait before nuking the clipboard. Doubles as
/// the lifetime of the bottom-right "autoclear in N…" chip — the chip's
/// progress bar fills this whole duration.
const CLIPBOARD_CLEAR_SECS: u64 = 10;

use modal_chrome::ModalChrome;
pub use nav::Nav;

use crate::chain::PerChain;
use crate::indexer::IndexedTx;
use crate::net::{BalanceFetcher, VerificationStatus};
use crate::portfolio::{LiveToken, PortfolioCache};
use crate::settings::{self, IndexerProvider};
use crate::ui::kao_theme::with_alpha;
use crate::ui::kao_theme::{KaoTheme, ThemeKind};
use crate::ui::kao_widgets::{fill_style, mono};
use crate::wallet::tx::SendPlan;
use crate::wallet::{
    AccountDescriptor, Contact, ContactsBook, KaoSigner, SafeDescriptor, SignerHandoff,
    account_address, handoff_with, short_address,
};

// ── Messages ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Message {
    /// Side-effect ack from the Mainnet Helios verification refresh.
    /// The handler just samples `network.last_status(Mainnet)` to drive
    /// the header badge — no per-address state changes here, so a stale
    /// fetch landing after an account switch is harmless.
    VerificationRefreshed,
    /// The App refreshed the wallet's Safes (typically on
    /// app-open). Carries the new full safes vec — the dashboard
    /// replaces its stale clone wholesale so the account dropdown
    /// stops rendering old labels / owner sets / watch-vs-signer
    /// classifications. Sent at most once per unlock.
    SafesUpdated(Vec<SafeDescriptor>),
    /// Per-chain portfolio result. The dashboard issues one fetch per
    /// configured chain in parallel and merges by `chain` as each lands,
    /// so a slow Optimism RPC never blocks the Mainnet rows from
    /// rendering. `address` is checked on arrival so a late response
    /// can't pollute the wrong account's portfolio (or its cache slot).
    PortfolioFetched {
        address: Address,
        chain: crate::chain::Chain,
        result: Result<Vec<LiveToken>, String>,
    },
    /// Result of a per-chain history fetch. `address` is the address it
    /// was issued against (dropped on mismatch); `chain` is the network
    /// it was fetched from. Each configured chain emits its own message
    /// so a slow L2 RPC can't stall Mainnet rows from rendering.
    HistoryFetched {
        address: Address,
        chain: crate::chain::Chain,
        result: Result<Vec<IndexedTx>, String>,
    },
    /// User clicked the "couldn't load activity" retry button.
    RetryHistory,
    /// Result of fetching the active Safe's pending multisig queue from
    /// the Safe Transaction Service. `safe` is the Safe address the fetch
    /// was issued against — dropped on arrival if the user has switched
    /// identity since (same staleness guard as `PortfolioFetched`).
    SafePendingFetched {
        safe: Address,
        chain: crate::chain::Chain,
        result: Result<Vec<crate::safe::service::PendingSafeTx>, String>,
    },
    SelectNav(Nav),
    SelectTheme(ThemeKind),
    OpenSend,
    OpenReceive,
    OpenSwap,
    OpenAccountDropdown,
    /// User clicked the refresh button next to the assets list.
    /// Re-issues the portfolio fetch (and verification refresh) for
    /// the current identity. Tokens stay visible during the refresh;
    /// only the indicator on the button reflects the in-flight state.
    RefreshPortfolio,
    /// User clicked an activity row. The argument is the index into
    /// `self.history` at the moment of the click; bounds-checked when
    /// handling because the history can refresh between view and event.
    OpenTxDetails(usize),
    AccountDropdown(account_dropdown::Message),
    Receive(receive::Message),
    Send(send::Message),
    Swap(swap::Message),
    TxDetails(tx_details::Message),
    Tick,
    OpenNetworksSettings,
    Networks(networks::Message),
    OpenAppearanceSettings,
    CloseAppearanceSettings,
    OpenContactsSettings,
    /// Open the Contacts pane in Add mode pre-filled with the given
    /// address (and optional ENS string when known). Wired from the Send
    /// pane's "Save as contact" CTA so users go straight from a fresh
    /// recipient to a contact draft.
    OpenContactsPaneWith {
        address: Address,
        ens: Option<String>,
    },
    Contacts(contacts_settings::Message),
    /// Child messages from the Safe-send modal.
    SafeSend(safe_send::Message),
    /// Result of a Safe-send sign+broadcast task. No signer handoff
    /// needed: the executor was derived inside the task from a
    /// linked owner's key, so nothing was moved out of the
    /// dashboard.
    SafeSendBroadcastReturn(Result<TxHash, String>),
    /// Result of a Safe-send *propose-to-service* task (vs. the direct
    /// broadcast above).
    SafeSendProposeReturn(Result<(), String>),
    /// User tapped a pending Safe queue row. Index into `safe_pending`
    /// at click time; bounds-checked on handling.
    OpenSafeTxDetails(usize),
    /// Child messages from the Safe-tx detail modal.
    SafeTxDetail(safe_tx_detail::Message),
    /// Full detail (owners + signatures) for the open queued tx landed.
    SafeTxDetailLoaded(Result<crate::safe::service::SafeTxDetail, String>),
    /// A confirm / execute / reject action finished. `Ok` carries a short
    /// success label for the modal's notice line.
    SafeTxActionDone(Result<String, String>),
    /// Header pencil clicked — switch the title slot to an editable text
    /// input pre-filled with the active account's current display name.
    BeginRenameAccount,
    /// Each keystroke while the rename input is open.
    RenameInput(String),
    /// User pressed Enter (or clicked ✓) — commit the draft to the account.
    CommitRename,
    /// User pressed Escape (or clicked ✗) — discard the draft.
    CancelRename,
    /// Result of a sign-and-broadcast task spawned from `Message::Send(Confirm)`.
    /// Carries the signer back via `SignerHandoff` so the dashboard can put it
    /// back into `self.signer` (it had to be moved out by value to be sent to
    /// the async task — `KaoSigner` is non-`Clone`).
    SendBroadcastReturn {
        result: Result<TxHash, String>,
        signer: SignerHandoff,
    },
    /// Result of the dashboard's startup reverse-ENS lookup. Carries the
    /// address it was issued against so a quick account switch can't apply
    /// a name to the wrong account.
    EnsAutoNameResolved {
        address: Address,
        result: Result<Option<String>, String>,
    },
    /// Side-effect ack from a clipboard write. No-op handler.
    ClipboardWritten,
    /// Auto-clear timer fired. Carries the generation it was armed
    /// against; if a fresher copy bumped the counter, this dispatches
    /// nothing and returns. Otherwise it kicks the ownership-check
    /// `clipboard::read` so we don't clobber unrelated content the user
    /// copied after the wallet's copy.
    ClipboardClearArmed {
        generation: u64,
    },
    /// Result of the ownership check. Clears the system clipboard only
    /// when its current contents still match what the wallet wrote
    /// (guards against the user copying a phone number five seconds
    /// later and having it nuked at second ten).
    ClipboardClearProbe {
        generation: u64,
        actual: Option<String>,
    },
}

/// Outcomes bubbled up to the parent app.
#[derive(Debug, Clone)]
pub enum Outcome {
    Switch(usize),
    Add,
    /// User edited the active account's display name. Carries the new
    /// value (or `None` to clear back to the indexed default).
    RenameActive(Option<String>),
    /// User saved a new/edited contacts list. The App writes the vec
    /// into the shared in-memory book and dispatches a disk save.
    SaveContacts(Vec<Contact>),
    /// User clicked Send on a hardware account whose signer is the
    /// view-only placeholder (the device wasn't connected at unlock).
    /// The App pushes the matching reconnect screen and, on success,
    /// re-enters the dashboard with the live signer and the Send modal
    /// pre-opened.
    NeedsHardwareReconnect,
}

// ── State ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum Modal {
    None,
    Send(SendPane),
    Receive(ReceivePane),
    Swap(SwapPane),
    AccountDropdown(AccountDropdown),
    TxDetails(TxDetailsPane),
    SafeSend(SafeSendPane),
    SafeTxDetail(SafeTxDetailPane),
}

/// Which settings pane is currently rendered. The Settings nav slot can show
/// either the root list of categories or one of the deeper category screens.
#[derive(Debug)]
enum SettingsPane {
    Root,
    Networks(NetworksPane),
    Appearance,
    Contacts(ContactsPane),
}

#[derive(Debug)]
pub struct WalletScreen {
    /// Live signer kept alive for the dashboard's session. Hardware variants
    /// own a USB transport so the device must remain plugged in. The Send
    /// quick action is gated on `signer.can_sign()` to lock view-only
    /// accounts out of broadcasting transactions.
    signer: KaoSigner,
    address: Address,
    /// All accounts in the unlocked wallet, used to render the account
    /// dropdown. `accounts[active_index]` corresponds to `signer`.
    accounts: Vec<AccountDescriptor>,
    /// All Safes onboarded into this wallet. Surfaced in the account
    /// dropdown alongside accounts, both for visibility and so the
    /// `Safe signer` cross-badge can be computed for accounts that are
    /// linked owners. Updated whenever the App pushes a new
    /// `WalletDescriptor` (e.g. after Safe onboarding completes).
    safes: Vec<SafeDescriptor>,
    active_index: usize,
    theme_kind: ThemeKind,
    nav: Nav,
    modal: Modal,
    /// Open/close animation state for the Send/Receive/Swap modal slot. The
    /// account dropdown bypasses chrome (instant open/close).
    chrome: ModalChrome,
    /// Shared Helios-backed RPC client; cloned into each balance fetch task.
    network: Arc<dyn BalanceFetcher>,
    /// Verification state of the most recent Mainnet Helios call. Sampled
    /// from `network.last_status(Mainnet)` after `VerificationRefreshed`
    /// lands; rendered in the header as a small "Verified by Helios /
    /// Unverified RPC" badge.
    verification: VerificationStatus,
    /// Which Settings sub-screen is currently rendered.
    settings_pane: SettingsPane,
    /// Live portfolio entries fetched from on-chain balances + CoinGecko.
    portfolio: Vec<LiveToken>,
    /// True while a portfolio fetch is in flight.
    portfolio_loading: bool,
    /// True while a *user-initiated* refresh is in flight. The home
    /// view shows tokens normally and renders a small busy hint on
    /// the refresh button — distinct from `portfolio_loading`, which
    /// is the cold-start path that hides the asset list entirely.
    portfolio_refreshing: bool,
    /// Process-lifetime cache shared with `App` so switching back to a
    /// previously-loaded account renders its tokens immediately while a
    /// fresh fetch refreshes them in the background.
    portfolio_cache: PortfolioCache,
    /// Inline rename draft for the active account. `Some(s)` means the
    /// header is showing the rename text input; `None` means it's showing
    /// the static name + pencil affordance.
    rename_draft: Option<String>,
    /// Merged transactions across every configured chain, newest first
    /// (sorted by timestamp). Empty while every chain is still in flight
    /// or when no source returned rows.
    history: Vec<IndexedTx>,
    /// `true` while at least one per-chain history fetch is still in
    /// flight. Cleared when *every* chain has reported back.
    history_loading: bool,
    /// `true` once *any* per-chain history fetch has completed (Ok or
    /// Err) for this dashboard. Drives lazy-load: the first switch to
    /// Activity kicks off the fetch; subsequent switches re-render the
    /// cached rows. Reset to `false` only by building a fresh screen.
    history_loaded: bool,
    /// Per-chain error captured when both the indexer and the on-chain
    /// fallback failed. The activity pane shows the retry affordance
    /// only when the merged feed is empty *and* at least one chain
    /// errored — partial successes still render rows.
    history_errors: PerChain<Option<String>>,
    /// Set of chains we're still waiting on. As each chain lands the
    /// chain is removed; `history_loading` flips to `false` once empty.
    history_pending: Vec<crate::chain::Chain>,
    /// Shared contacts book. Read by the Send picker, the Send review
    /// step, the Activity feed (named counterparties), and the tx
    /// details modal; written by the Contacts settings pane on save.
    /// `Arc<RwLock<…>>` so a contact edit is visible everywhere on the
    /// next view tick without rebuilding the dashboard.
    contacts: Arc<RwLock<ContactsBook>>,
    /// When `Some(idx)`, the dashboard is in **Safe mode**: header,
    /// hero, portfolio, and quick actions key off `safes[idx]`'s
    /// address rather than the EOA `self.address`. The EOA signer
    /// stays alive for Safe-TX execution (it pays gas as the
    /// executor); only what the user *sees* re-keys. Clicking any
    /// account in the dropdown clears this back to `None`.
    active_safe: Option<usize>,
    /// The active Safe's pending multisig queue, fetched from the Safe
    /// Transaction Service and tagged with a lifecycle FSM state. Only
    /// populated in Safe mode; cleared on switch back to an EOA. Empty in
    /// EOA mode and when the active Safe has no queued transactions.
    safe_pending: Vec<crate::safe::service::PendingSafeTx>,
    /// True while the pending-queue fetch for the active Safe is in
    /// flight. Drives a muted "loading" line in the Home pane without
    /// blocking the asset list.
    safe_pending_loading: bool,
    /// Last pending-queue fetch error (service unreachable, decode
    /// failure). Rendered as a muted one-liner; never blocks the rest of
    /// the Home pane.
    safe_pending_error: Option<String>,
    /// Active clipboard auto-clear, set when the user copies anything
    /// from a child modal (Send, TxDetails). Drives the bottom-right
    /// countdown chip and the deferred ownership-checked clear.
    clipboard_clear: Option<ClipboardClearState>,
    /// Monotonic counter bumped on every fresh copy so a stale
    /// `ClipboardClearArmed` / `ClipboardClearProbe` from an older arm
    /// can't clobber the chip or the clipboard.
    clipboard_clear_gen: u64,
}

/// Tracks an in-flight clipboard auto-clear: when it lands, what we
/// wrote so the ownership check can decide whether to clear, and the
/// generation the timer was armed against for stale-task dispatch.
#[derive(Debug, Clone)]
struct ClipboardClearState {
    /// What the wallet wrote into the system clipboard. Compared
    /// against the live clipboard contents at probe time — if they
    /// differ, the user copied something else after us and we leave
    /// their content alone.
    expected: String,
    /// Wall-clock instant the clear is scheduled to fire. The corner
    /// chip subtracts `Instant::now()` from this each frame to draw the
    /// countdown text and the progress bar.
    deadline: Instant,
    /// Generation tag matched by the ack messages. Bumped on every
    /// fresh copy so a stale arm can't fire after a newer one.
    generation: u64,
}

impl WalletScreen {
    /// Build a fresh dashboard.
    ///
    /// `initial_nav` selects the tab the dashboard lands on. `None`
    /// defaults to Home (first-unlock and add-account flows);
    /// `Some(_)` is passed by `App::switch_account` so switching
    /// accounts while reading the Activity feed doesn't yank the user
    /// back to Home.
    // The constructor was already at clippy's 7-arg threshold before
    // safes were added; bundling into a config struct would be a wider
    // refactor of every WalletScreen::new caller for no real
    // readability gain (the arg names are already self-documenting).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        signer: KaoSigner,
        accounts: Vec<AccountDescriptor>,
        safes: Vec<SafeDescriptor>,
        active_index: usize,
        network: Arc<dyn BalanceFetcher>,
        portfolio_cache: PortfolioCache,
        contacts: Arc<RwLock<ContactsBook>>,
        initial_nav: Option<Nav>,
    ) -> Self {
        let address = signer.address();
        // Seed from the cache when this address has been viewed before:
        // the user sees their tokens immediately on switch and the
        // background fetch (still kicked off by the App) refreshes
        // values silently when it lands.
        // Pull every chain's cached rows so an account switch renders
        // the full multi-chain portfolio immediately; the per-chain
        // background fetches refresh values silently as they land.
        let cached: Option<Vec<LiveToken>> = portfolio_cache.lock().ok().map(|c| {
            crate::chain::Chain::ALL
                .iter()
                .filter_map(|chain| c.get(&(address, *chain)).cloned())
                .flatten()
                .collect()
        });
        let cached = cached.filter(|v: &Vec<LiveToken>| !v.is_empty());
        let (portfolio, portfolio_loading) = match cached {
            Some(p) => (p, false),
            None => (Vec::new(), true),
        };
        Self {
            signer,
            address,
            accounts,
            safes,
            active_index,
            theme_kind: settings::theme(),
            nav: initial_nav.unwrap_or(Nav::Home),
            modal: Modal::None,
            chrome: ModalChrome::new(),
            network,
            verification: VerificationStatus::Connecting,
            settings_pane: SettingsPane::Root,
            portfolio,
            portfolio_loading,
            portfolio_refreshing: false,
            portfolio_cache,
            active_safe: None,
            safe_pending: Vec::new(),
            safe_pending_loading: false,
            safe_pending_error: None,
            rename_draft: None,
            history: Vec::new(),
            history_loading: false,
            history_loaded: false,
            history_errors: PerChain::default(),
            history_pending: Vec::new(),
            contacts,
            clipboard_clear: None,
            clipboard_clear_gen: 0,
        }
    }

    /// Move the live signer out of the dashboard. Used by the App when it
    /// transitions away from the dashboard (e.g. into the add-account flow)
    /// and wants to park the signer to return cheaply later.
    pub fn into_signer(self) -> KaoSigner {
        self.signer
    }

    /// Address the dashboard currently *displays* — the active Safe's
    /// address in Safe mode, otherwise the EOA. All portfolio /
    /// balance / activity / verification fetches key off this. The
    /// EOA signer (`self.signer` / `self.address`) stays in place for
    /// transaction signing; only the user-visible identity moves.
    fn display_address(&self) -> Address {
        match self.active_safe {
            Some(idx) => self
                .safes
                .get(idx)
                .map(|s| s.address())
                .unwrap_or(self.address),
            None => self.address,
        }
    }

    /// The active Safe descriptor when in Safe mode. `None` in EOA
    /// mode or if `active_safe` points at a stale index (defensive —
    /// shouldn't happen but cheaper than panicking).
    fn active_safe_descriptor(&self) -> Option<&SafeDescriptor> {
        self.active_safe.and_then(|i| self.safes.get(i))
    }

    /// Chains whose balances/history are valid for the current
    /// display identity.
    ///
    /// In EOA mode: every chain Kao supports. The same hex EOA
    /// address is a self-custodial wallet across chains, so its
    /// per-chain balances are independently meaningful.
    ///
    /// In Safe mode: just the Safe's own chain. A Safe is a contract
    /// pinned to a specific deployment; the same address on another
    /// chain is either an unrelated contract or empty space, and
    /// rendering its balance there would mislead the user.
    fn allowed_chains(&self) -> Vec<crate::chain::Chain> {
        match self.active_safe_descriptor() {
            Some(safe) => crate::chain::Chain::ALL
                .iter()
                .copied()
                .filter(|c| c.chain_id() == safe.chain_id)
                .collect(),
            None => crate::chain::Chain::ALL.to_vec(),
        }
    }

    /// Reset `history_pending` to the set of chains the dashboard
    /// will actually fetch — `allowed_chains()` intersected with
    /// "has at least one configured RPC". Single source of truth
    /// for the three callsites that kick a history refresh.
    fn reset_history_pending(&mut self) {
        self.history_pending = self
            .allowed_chains()
            .into_iter()
            .filter(|c| !settings::rpcs(*c).is_empty())
            .collect();
    }

    /// Seed `self.portfolio` from the cache for the currently
    /// displayed address. Used on Safe-mode entry so the user sees
    /// previously-fetched token rows immediately while the live
    /// fetch refreshes them. Leaves the existing portfolio alone if
    /// the cache misses entirely on this address.
    fn seed_portfolio_from_cache(&mut self) {
        let addr = self.display_address();
        let allowed = self.allowed_chains();
        let cached: Vec<LiveToken> = match self.portfolio_cache.lock() {
            Ok(c) => allowed
                .iter()
                .filter_map(|chain| c.get(&(addr, *chain)).cloned())
                .flatten()
                .collect(),
            Err(_) => Vec::new(),
        };
        self.portfolio = cached;
    }

    /// Write `text` to the system clipboard and arm the deferred clear.
    /// Bumps the generation counter so any older arm fires stale and
    /// no-ops. The returned task batches the write itself with a
    /// `tokio::time::sleep` that emits `ClipboardClearArmed` once the
    /// deadline passes — the ownership check happens at that point, not
    /// here.
    fn arm_clipboard_clear(&mut self, text: String) -> Task<Message> {
        self.clipboard_clear_gen = self.clipboard_clear_gen.wrapping_add(1);
        let generation = self.clipboard_clear_gen;
        self.clipboard_clear = Some(ClipboardClearState {
            expected: text.clone(),
            deadline: Instant::now() + Duration::from_secs(CLIPBOARD_CLEAR_SECS),
            generation,
        });
        let timer = Task::perform(
            async move {
                tokio::time::sleep(Duration::from_secs(CLIPBOARD_CLEAR_SECS)).await;
                generation
            },
            |generation| Message::ClipboardClearArmed { generation },
        );
        Task::batch([
            iced::clipboard::write(text).map(|_: ()| Message::ClipboardWritten),
            timer,
        ])
    }

    /// True while a send is in flight (the signer has been moved into a
    /// broadcast task and not yet reclaimed). Used by the App to refuse
    /// disruptive transitions like begin-add-account during a send — those
    /// would race with the in-flight broadcast on the signer cell.
    pub fn is_send_busy(&self) -> bool {
        match &self.modal {
            Modal::Send(p) => p.busy(),
            Modal::SafeSend(p) => p.busy(),
            Modal::SafeTxDetail(p) => p.busy(),
            _ => false,
        }
    }

    /// The active address in short `0xabcd…ef01` form. For diagnostic logs.
    pub fn address_for_log(&self) -> String {
        short_address(self.address)
    }

    /// Currently-selected sidebar tab. The App threads this through
    /// `enter_dashboard` on account switch so the user stays on the tab
    /// they were reading instead of being yanked back to Home.
    pub fn current_nav(&self) -> Nav {
        self.nav
    }

    /// Whether the Send button should be live. In EOA mode: true when
    /// the signer can sign directly, or when the active descriptor is
    /// a hardware account (click escalates to reconnect). View-only
    /// accounts return false. In Safe mode: true when at least one
    /// linked owner is a Local account in this wallet — that owner
    /// serves as both signer and gas-paying executor, so the active
    /// EOA's signing capability is irrelevant.
    fn can_send(&self) -> bool {
        if let Some(safe) = self.active_safe_descriptor() {
            return self.has_local_linked_owner(safe);
        }
        self.signer.can_sign()
            || matches!(
                self.accounts.get(self.active_index),
                Some(AccountDescriptor::Ledger { .. } | AccountDescriptor::Trezor { .. })
            )
    }

    /// Whether `safe` has at least one linked owner that's a `Local`
    /// account in this wallet. Determines whether Safe-mode Send is
    /// reachable.
    fn has_local_linked_owner(&self, safe: &SafeDescriptor) -> bool {
        safe.linked_signer_indices.iter().any(|idx| {
            matches!(
                self.accounts.get(*idx as usize),
                Some(AccountDescriptor::Local { .. })
            )
        })
    }

    /// `(address, descriptor)` for every linked owner this wallet can
    /// sign with — Local or hardware, excluding view-only. The Safe-tx
    /// detail modal uses these to drive Confirm/Reject.
    fn safe_signable_owners(&self, safe: &SafeDescriptor) -> Vec<(Address, AccountDescriptor)> {
        signable_owners_of(safe, &self.accounts)
    }

    /// The account descriptor whose address matches `addr`, if this wallet
    /// holds it. Used to rebuild a signer for a chosen owner.
    fn owner_desc_for(&self, addr: Address) -> Option<AccountDescriptor> {
        owner_desc_by_address(addr, &self.accounts)
    }

    /// First Local account key in the wallet — the gas-paying executor for
    /// execute-from-queue. `None` when the wallet is hardware/view-only
    /// only (execution then needs an external relayer).
    fn first_local_key(&self) -> Option<B256> {
        first_local_key_of(&self.accounts)
    }

    /// Mark the open Safe-tx detail modal busy (no-op if a different modal
    /// is showing). Wrapped so the action handlers can mutate the pane
    /// without holding its borrow across `self` helper calls.
    fn mark_safe_detail_busy(&mut self) {
        if let Modal::SafeTxDetail(p) = &mut self.modal {
            p.mark_busy();
        }
    }

    /// Push an action result into the open Safe-tx detail modal.
    fn set_safe_detail_result(&mut self, result: Result<String, String>) {
        if let Modal::SafeTxDetail(p) = &mut self.modal {
            p.set_action_result(result);
        }
    }

    /// Issue a Mainnet `network.balance` call purely to refresh the
    /// Helios verification badge — the result is discarded; only the
    /// side-effect on `last_status(Mainnet)` matters. Without this the
    /// badge would never leave "Connecting…": `get_code`,
    /// `get_storage_at`, and `call` deliberately don't touch
    /// `last_status` so the proxy walker can't flicker the badge on
    /// every clear-signing probe, and the portfolio fetch goes through
    /// the raw fallback provider rather than helios.
    pub fn refresh_verification_task(&self) -> Task<Message> {
        // Helios verification probes the displayed address — in Safe
        // mode that's the Safe's address, which is the right thing
        // to probe because that's what the user is looking at.
        let address = self.display_address();
        let network = self.network.clone();
        Task::perform(
            async move {
                debug!(addr = %short_address(address), "dashboard: refresh helios verification");
                let started = std::time::Instant::now();
                let ok = network
                    .balance(address, crate::chain::Chain::Mainnet)
                    .await
                    .is_ok();
                debug!(
                    elapsed = ?started.elapsed(),
                    ok,
                    "dashboard: helios verification refresh completed",
                );
            },
            |_| Message::VerificationRefreshed,
        )
    }

    /// Issue one history fetch per configured chain in parallel. Mirrors
    /// the portfolio fan-out: each chain emits its own
    /// `HistoryFetched { address, chain, result }`, and the handler
    /// merges the result so a slow L2 RPC can't block Mainnet rows from
    /// rendering. Each chain tries the configured indexer first; on
    /// `IndexerProvider::None` or any error, falls back to the on-chain
    /// `eth_getLogs` walk.
    pub fn fetch_history_task(&self) -> Task<Message> {
        let address = self.display_address();
        let provider_kind = settings::indexer_provider();
        let mut tasks: Vec<Task<Message>> = Vec::new();
        for chain in self.allowed_chains() {
            if settings::rpcs(chain).is_empty() {
                // L2 chain that the user never configured — skip silently.
                continue;
            }
            let network = self.network.clone();
            tasks.push(Task::perform(
                async move {
                    debug!(
                        addr = %short_address(address),
                        chain = %chain.label(),
                        indexer = ?provider_kind,
                        "fetching history",
                    );
                    let started = std::time::Instant::now();
                    // NoopIndexer always returns Ok([]), which would never
                    // trigger the fallback. Skip it explicitly so the on-chain
                    // walk runs.
                    let primary = if matches!(provider_kind, IndexerProvider::None) {
                        Err("indexer disabled".to_string())
                    } else {
                        crate::indexer::build_indexer_for(chain)
                            .transactions(address, HISTORY_LIMIT)
                            .await
                    };
                    let result = match primary {
                        Ok(v) => Ok(v),
                        Err(primary_err) => {
                            debug!(
                                chain = %chain.label(),
                                error = %primary_err,
                                "indexer failed; falling back to on-chain",
                            );
                            match network.provider(chain).await {
                                Some(p) => crate::indexer::onchain::fetch_onchain_history(
                                    &p,
                                    address,
                                    chain,
                                    HISTORY_LIMIT,
                                )
                                .await
                                .map_err(|e| format!("indexer: {primary_err}; on-chain: {e}")),
                                None => Err(primary_err),
                            }
                        }
                    };
                    // Stamp the chain on every row regardless of source —
                    // indexer impls default to Mainnet, the on-chain walk
                    // already sets it, but stamping unconditionally keeps
                    // the merged feed honest if any source forgets.
                    let result = result.map(|mut v| {
                        for r in v.iter_mut() {
                            r.chain = chain;
                        }
                        v
                    });
                    debug!(
                        elapsed = ?started.elapsed(),
                        chain = %chain.label(),
                        ok = result.is_ok(),
                        count = result.as_ref().map(|v| v.len()).unwrap_or(0),
                        "history fetch completed",
                    );
                    (address, chain, result)
                },
                |(address, chain, result)| Message::HistoryFetched {
                    address,
                    chain,
                    result,
                },
            ));
        }
        Task::batch(tasks)
    }

    /// Issue one portfolio fetch per configured chain in parallel.
    ///
    /// A chain is "configured" when its execution RPC list in settings
    /// is non-empty — Mainnet is always configured (the built-in default
    /// seeds it); L2 chains are only configured if the user opted in
    /// via the setup flow's Alchemy / Custom RPCs path or the Networks
    /// pane. Each chain emits its own `PortfolioFetched { address, chain, result }`
    /// message; the handler merges by chain so a slow Optimism RPC
    /// can't stall the Mainnet rows from rendering.
    pub fn fetch_portfolio_task(&self) -> Task<Message> {
        let address = self.display_address();
        let provider_kind = settings::indexer_provider();
        let mut tasks: Vec<Task<Message>> = Vec::new();
        for chain in self.allowed_chains() {
            if settings::rpcs(chain).is_empty() {
                // L2 chain that the user never configured — skip silently.
                continue;
            }
            let network = self.network.clone();
            tasks.push(Task::perform(
                async move {
                    debug!(
                        addr = %short_address(address),
                        chain = %chain.label(),
                        indexer = ?provider_kind,
                        "fetching portfolio",
                    );
                    let started = std::time::Instant::now();
                    // Routing:
                    //
                    // * Mainnet uses the indexer for *discovery only* — it
                    //   enumerates the ERC-20 contracts this address holds,
                    //   then balances and Uniswap prices are read on-chain
                    //   via Multicall3. The indexer's own balance/price
                    //   fields are dropped, so a malicious or stale indexer
                    //   can't fake what the user sees. When the indexer is
                    //   `NoopIndexer` or fails, we fall back to the curated
                    //   `MAINNET_OVERLAY` (still over Multicall3).
                    // * L2 chains keep the current indexer-as-source flow:
                    //   one HTTP round-trip vs. two Multicall3 batches, and
                    //   the on-chain walk's L2 token list (overlay + bundled
                    //   Superchain tokenlist) is already its fallback.
                    let indexer = crate::indexer::build_indexer_for(chain);
                    let result = if chain == crate::chain::Chain::Mainnet {
                        let discovered = indexer
                            .balances(address)
                            .await
                            .ok()
                            .map(crate::indexer::into_discovered_tokens)
                            .unwrap_or_default();
                        match network.provider(chain).await {
                            Some(p) => {
                                crate::portfolio::fetch_portfolio_with_discovery(
                                    address,
                                    chain,
                                    &p,
                                    &discovered,
                                )
                                .await
                            }
                            None => Err(format!(
                                "no execution RPCs configured for {}",
                                chain.label()
                            )),
                        }
                    } else {
                        let from_indexer = indexer
                            .balances(address)
                            .await
                            .ok()
                            .filter(|v| !v.is_empty());
                        if let Some(tokens) = from_indexer {
                            Ok(crate::indexer::into_live_tokens(chain, tokens))
                        } else {
                            match network.provider(chain).await {
                                Some(p) => {
                                    crate::portfolio::fetch_portfolio(address, chain, &p).await
                                }
                                None => Err(format!(
                                    "no execution RPCs configured for {}",
                                    chain.label()
                                )),
                            }
                        }
                    };
                    debug!(
                        elapsed = ?started.elapsed(),
                        chain = %chain.label(),
                        ok = result.is_ok(),
                        count = result.as_ref().map(|v: &Vec<LiveToken>| v.len()).unwrap_or(0),
                        "portfolio fetch completed",
                    );
                    (address, chain, result)
                },
                |(address, chain, result)| Message::PortfolioFetched {
                    address,
                    chain,
                    result,
                },
            ));
        }
        Task::batch(tasks)
    }

    /// Fetch the active Safe's pending multisig queue from the Safe
    /// Transaction Service. Returns `None` in EOA mode, when the Safe
    /// sits on a chain Kao doesn't recognize, or when that chain has no
    /// configured RPC (the queue's FSM needs the Safe's on-chain nonce,
    /// read through `network`). `Option` rather than `Task::none()` so
    /// callers can tie `safe_pending_loading` to whether a
    /// `SafePendingFetched` will actually arrive to clear it — setting
    /// the flag for a fetch that never launches leaves the queue spinner
    /// stuck forever. Mirrors `fetch_portfolio_task`'s shape otherwise:
    /// clone the shared fetcher into the task, log timing, tag the result
    /// with the `(safe, chain)` it was issued against so a late landing
    /// can't pollute another identity.
    pub fn fetch_safe_pending_task(&self) -> Option<Task<Message>> {
        let safe = self.active_safe_descriptor()?;
        let chain = crate::chain::Chain::from_chain_id(safe.chain_id)?;
        if settings::rpcs(chain).is_empty() {
            return None;
        }
        let address = safe.address();
        let threshold = safe.threshold;
        let network = self.network.clone();
        Some(Task::perform(
            async move {
                debug!(
                    safe = %short_address(address),
                    chain = %chain.label(),
                    "fetching safe pending queue",
                );
                let started = std::time::Instant::now();
                let result =
                    crate::safe::service::fetch_pending(&*network, address, chain, threshold).await;
                debug!(
                    elapsed = ?started.elapsed(),
                    ok = result.is_ok(),
                    count = result.as_ref().map(Vec::len).unwrap_or(0),
                    "safe pending fetch completed",
                );
                (address, chain, result)
            },
            |(safe, chain, result)| Message::SafePendingFetched {
                safe,
                chain,
                result,
            },
        ))
    }

    /// Reverse-resolve the active address against ENS (forward-verified)
    /// and use the result as a default account name when none is set yet.
    /// Skipped when the user has already named the account: we never
    /// overwrite an explicit choice with an inferred one. Also skipped for
    /// inputs that look like they were just imported with an ENS name —
    /// the import flow already set the name, so a second lookup is wasted
    /// work (and would noisily reapply it).
    pub fn fetch_ens_name_task(&self) -> Task<Message> {
        let address = self.address;
        let network = self.network.clone();
        let already_named = self
            .accounts
            .get(self.active_index)
            .and_then(|a| a.name())
            .is_some();
        if already_named {
            return Task::none();
        }
        Task::perform(
            async move {
                debug!(addr = %short_address(address), "ens reverse lookup");
                let started = std::time::Instant::now();
                let result = match network.provider(crate::chain::Chain::Mainnet).await {
                    Some(provider) => crate::ens::lookup_address(&provider, address).await,
                    None => Err("no execution RPCs configured".to_string()),
                };
                debug!(
                    elapsed = ?started.elapsed(),
                    found = matches!(&result, Ok(Some(_))),
                    "ens reverse lookup completed",
                );
                (address, result)
            },
            |(address, result)| Message::EnsAutoNameResolved { address, result },
        )
    }

    pub fn update(&mut self, message: Message) -> (Task<Message>, Option<Outcome>) {
        match message {
            Message::VerificationRefreshed => {
                self.verification = self.network.last_status(crate::chain::Chain::Mainnet);
            }
            Message::RefreshPortfolio => {
                // Don't reset `portfolio_loading` if the portfolio is
                // already populated — the rendered tokens stay
                // visible during refresh, and the home view's
                // refresh button carries its own busy hint. Setting
                // `loading` only when empty preserves the existing
                // first-render skeleton path.
                self.portfolio_loading = self.portfolio.is_empty();
                self.portfolio_refreshing = true;
                // In Safe mode the refresh chip also reloads the pending
                // queue so freshly-signed/executed txs surface promptly.
                // The loading flag tracks whether a fetch was actually
                // issued — `None` (EOA mode, unknown chain, no RPCs)
                // means no `SafePendingFetched` will arrive to clear it.
                let pending = self.fetch_safe_pending_task();
                self.safe_pending_loading = pending.is_some();
                return (
                    Task::batch([
                        self.refresh_verification_task(),
                        self.fetch_portfolio_task(),
                        pending.unwrap_or_else(Task::none),
                    ]),
                    None,
                );
            }
            Message::SafesUpdated(safes) => {
                // Wholesale replace — refresh-on-open runs in batch
                // and returns the complete updated list, so there's
                // nothing to merge.
                self.safes = safes;
            }
            Message::PortfolioFetched {
                address,
                chain,
                result,
            } => {
                // Always write the (address, chain) we issued the fetch
                // for into the cache — it's still the correct slot for
                // that address's data even if the user has since
                // switched away. Only the live portfolio merge is
                // gated on `address == display_address` (which in
                // Safe mode is the Safe's address).
                if let Ok(tokens) = &result
                    && let Ok(mut cache) = self.portfolio_cache.lock()
                {
                    cache.insert((address, chain), tokens.clone());
                }
                if address != self.display_address() {
                    return (Task::none(), None);
                }
                // Cross-chain Safe rows: a stale fetch that was
                // issued before SelectSafe but landed after can carry
                // a chain that's no longer in scope for the active
                // Safe. Drop it before merging so the user never
                // sees Optimism balances on a Mainnet Safe.
                if !self.allowed_chains().contains(&chain) {
                    return (Task::none(), None);
                }
                // Loading flag clears once *any* chain lands; the user
                // sees results stream in rather than wait for the
                // slowest chain. Refresh flag follows the same
                // first-wins rule so the button's busy hint clears
                // as soon as anything new arrives.
                self.portfolio_loading = false;
                self.portfolio_refreshing = false;
                match result {
                    Ok(tokens) => {
                        // Merge by chain: replace the rows belonging to
                        // `chain` with the new ones, leave other chains'
                        // rows untouched. Re-sort ETH-first then by USD
                        // value descending so a late-landing chain
                        // doesn't shuffle a stable row order — the
                        // original portfolio sort already maintained this.
                        self.portfolio.retain(|t| t.chain != chain);
                        self.portfolio.extend(tokens);
                        self.portfolio.sort_by(|a, b| {
                            // Native ETH bubbles up first per chain.
                            let a_native = a.contract.is_none();
                            let b_native = b.contract.is_none();
                            match b_native.cmp(&a_native) {
                                std::cmp::Ordering::Equal => b
                                    .usd_value
                                    .partial_cmp(&a.usd_value)
                                    .unwrap_or(std::cmp::Ordering::Equal),
                                other => other,
                            }
                        });
                    }
                    Err(e) => warn!(
                        chain = %chain.label(),
                        error = %e,
                        "portfolio fetch failed",
                    ),
                }
            }
            Message::SafePendingFetched {
                safe,
                chain,
                result,
            } => {
                // Staleness guard: drop if the user has left this Safe
                // (switched to an EOA or a different Safe) or if the
                // active Safe has since moved off this chain. Mirrors the
                // `PortfolioFetched` address/chain guards.
                let still_active = self
                    .active_safe_descriptor()
                    .is_some_and(|s| s.address() == safe && s.chain_id == chain.chain_id());
                if !still_active {
                    return (Task::none(), None);
                }
                self.safe_pending_loading = false;
                match result {
                    Ok(pending) => {
                        self.safe_pending = pending;
                        self.safe_pending_error = None;
                    }
                    Err(e) => {
                        warn!(
                            safe = %short_address(safe),
                            chain = %chain.label(),
                            error = %e,
                            "safe pending fetch failed",
                        );
                        self.safe_pending_error = Some(e);
                    }
                }
            }
            Message::HistoryFetched {
                address,
                chain,
                result,
            } => {
                if address != self.display_address() {
                    return (Task::none(), None);
                }
                if !self.allowed_chains().contains(&chain) {
                    // Same staleness guard as PortfolioFetched — a
                    // pre-Safe-mode history fetch on a different chain
                    // shouldn't pollute the Safe's activity feed.
                    return (Task::none(), None);
                }
                self.history_loaded = true;
                // Drop this chain from the pending set; once empty,
                // history_loading flips to false. The user sees the
                // fastest chain's rows appear immediately while slower
                // chains keep loading in the background.
                self.history_pending.retain(|c| *c != chain);
                if self.history_pending.is_empty() {
                    self.history_loading = false;
                }
                match result {
                    Ok(txs) => {
                        self.history_errors.set(chain, None);
                        // Replace this chain's rows in the merged feed
                        // and re-sort newest-first by timestamp (block
                        // numbers aren't comparable across chains).
                        self.history.retain(|r| r.chain != chain);
                        self.history.extend(txs);
                        self.history.sort_by(|a, b| {
                            b.timestamp
                                .cmp(&a.timestamp)
                                .then_with(|| b.hash.cmp(&a.hash))
                        });
                        // Cap to HISTORY_LIMIT after merging so a quiet
                        // chain doesn't get squeezed out by a busy one
                        // (each chain fetched up to HISTORY_LIMIT; the
                        // merged view shows the top N by recency).
                        self.history.truncate(HISTORY_LIMIT);
                    }
                    Err(e) => {
                        warn!(
                            chain = %chain.label(),
                            error = %e,
                            "history fetch failed",
                        );
                        self.history_errors.set(chain, Some(e));
                    }
                }
            }
            Message::RetryHistory => {
                if self.history_loading {
                    return (Task::none(), None);
                }
                self.history_loading = true;
                self.history_errors = PerChain::default();
                self.reset_history_pending();
                return (self.fetch_history_task(), None);
            }
            Message::SelectNav(nav) => {
                if self.nav != nav {
                    self.settings_pane = SettingsPane::Root;
                }
                self.nav = nav;
                // Lazy-load the activity feed on the first switch into
                // the Activity tab so the dashboard doesn't pay the
                // indexer (and on-chain fallback) round-trips on every
                // account switch.
                if matches!(nav, Nav::Activity) && !self.history_loaded && !self.history_loading {
                    self.history_loading = true;
                    self.reset_history_pending();
                    return (self.fetch_history_task(), None);
                }
            }
            Message::SelectTheme(k) => {
                self.theme_kind = k;
                settings::set_theme(k);
            }
            Message::OpenSend => {
                // Safe mode: route Send to the SafeSend modal. The
                // EOA signer is still alive in `self.signer` and gets
                // moved into the broadcast task at Confirm time — it
                // pays gas as the executor.
                if let Some(safe) = self.active_safe_descriptor() {
                    self.modal = Modal::SafeSend(SafeSendPane::new(safe, &self.accounts));
                    self.chrome.open();
                    return (Task::none(), None);
                }
                if !self.signer.can_sign() {
                    // Hardware accounts can sign — they just need the device
                    // reconnected. Escalate so the App can push the matching
                    // connect screen and come back with a live signer +
                    // Send modal opened. True view-only accounts have no
                    // signing material at all, so they stay no-op here.
                    if matches!(
                        self.accounts.get(self.active_index),
                        Some(AccountDescriptor::Ledger { .. } | AccountDescriptor::Trezor { .. })
                    ) {
                        return (Task::none(), Some(Outcome::NeedsHardwareReconnect));
                    }
                    info!("send disabled: active account is view-only");
                    return (Task::none(), None);
                }
                self.modal = Modal::Send(SendPane::new(self.address));
                self.chrome.open();
                // Refresh portfolio + hero balance as the modal opens so
                // the token tabs and Max button work off fresh numbers
                // instead of whatever was last cached.
                return (
                    Task::batch([
                        self.refresh_verification_task(),
                        self.fetch_portfolio_task(),
                    ]),
                    None,
                );
            }
            Message::Send(child_msg) => {
                let Modal::Send(p) = &mut self.modal else {
                    return (Task::none(), None);
                };
                // ── Coordinator-side intercepts ────────────────────────
                //
                // Some pane messages need data the pane doesn't carry
                // (portfolio entries, the live signer, the network
                // provider). Handle those here, then either short-circuit
                // or forward to the pane.

                // Max: compute the largest sendable amount for the active
                // token. For native ETH, subtract the (loaded) gas cost so
                // the user isn't left dust-stuck at broadcast time.
                if let send::Message::Max = &child_msg {
                    if let Some(tk) = self.portfolio.get(p.token_idx()) {
                        let max_str = compute_max_amount(tk, p);
                        p.apply_max(max_str);
                    }
                    return (Task::none(), None);
                }

                // Step(2): user clicked "Review →". Spawn a quote task
                // (gas + 1559 fees + nonce) AND a clear-signing decode
                // task — both against the same plan the pane will
                // eventually broadcast.
                if let send::Message::Step(2) = &child_msg {
                    let plan = p.build_plan(&self.portfolio);
                    let pre_task = match plan {
                        Some(pl) => {
                            p.quote_started();
                            let decode_seq = p.decode_started();
                            let quote_task = spawn_quote_task(self.network.clone(), pl.clone());
                            let decode_task =
                                spawn_decode_task(self.network.clone(), decode_seq, pl);
                            Task::batch([quote_task, decode_task])
                        }
                        None => Task::none(),
                    };
                    let (task, _outcome) = p.update(child_msg);
                    let task = task.map(Message::Send);
                    return (Task::batch([pre_task, task]), None);
                }

                // Confirm: user clicked "Confirm Send ✓". Need to move
                // the signer out of the dashboard, run sign+broadcast in
                // a task, and route the signer back via `SignerHandoff`.
                if let send::Message::Confirm = &child_msg {
                    let plan = p.build_plan(&self.portfolio);
                    // Clone the quote — TxQuote stopped being Copy when it
                    // grew the SimulationResult field; spawn_broadcast_task
                    // still takes ownership.
                    let quote = p.quote().cloned();
                    info!(
                        has_plan = plan.is_some(),
                        has_quote = quote.is_some(),
                        "send: confirm clicked",
                    );
                    if let (Some(plan), Some(quote)) = (plan, quote) {
                        info!(
                            chain = %plan.chain.label(),
                            chain_id = plan.chain.chain_id(),
                            from = %plan.from,
                            recipient = %plan.recipient,
                            amount_units = %plan.amount_units,
                            erc20 = matches!(plan.token, crate::wallet::tx::SendToken::Erc20 { .. }),
                            gas_limit = quote.gas_limit,
                            nonce = quote.nonce,
                            "send: spawning broadcast task",
                        );
                        // Move the signer out — only ViewOnly stays
                        // behind so the dashboard view doesn't crash if
                        // it dereferences the signer's address while the
                        // task is running. self.address stays correct.
                        let signer =
                            mem::replace(&mut self.signer, KaoSigner::ViewOnly(self.address));
                        let handoff = handoff_with(signer);
                        let pre_task =
                            spawn_broadcast_task(self.network.clone(), handoff, plan, quote);
                        let (task, _outcome) = p.update(child_msg);
                        let task = task.map(Message::Send);
                        return (Task::batch([pre_task, task]), None);
                    }
                    // Missing plan or quote — let the pane no-op the
                    // confirm. Surface this loudly: it's the most common
                    // "send button does nothing" cause (button enabled,
                    // user clicks, no broadcast spawned).
                    warn!("send: confirm dropped — no plan or no quote");
                    let (task, _outcome) = p.update(child_msg);
                    return (task.map(Message::Send), None);
                }

                let (task, outcome) = p.update(child_msg);
                // After pumping the pane, check whether the recipient
                // input now points at an ENS-shaped value that hasn't been
                // dispatched yet. The pane bumps a sequence on each
                // change; `take_pending_ens` returns Some exactly once
                // per sequence so a no-op repaint won't refire the lookup.
                let ens_task = match p.take_pending_ens() {
                    Some((seq, name)) => spawn_ens_resolve_task(self.network.clone(), seq, name),
                    None => Task::none(),
                };
                let task = task.map(Message::Send);
                match outcome {
                    Some(send::Outcome::Closed) => {
                        self.chrome.start_close();
                        return (Task::batch([task, ens_task]), None);
                    }
                    Some(send::Outcome::CopyText(s)) => {
                        let copy_task = self.arm_clipboard_clear(s);
                        return (Task::batch([task, copy_task, ens_task]), None);
                    }
                    Some(send::Outcome::SaveAsContact { address, ens }) => {
                        // Reuse the existing OpenContactsPaneWith path —
                        // switches nav, opens contacts pane in Add mode
                        // pre-filled, and starts the modal close
                        // animation. We synthesize the same Message
                        // here and forward it to ourselves on the next
                        // tick rather than inlining the body to keep
                        // the two entry points behaviourally identical.
                        let open_task = Task::done(Message::OpenContactsPaneWith { address, ens });
                        return (Task::batch([task, ens_task, open_task]), None);
                    }
                    None => return (Task::batch([task, ens_task]), None),
                }
            }
            Message::SendBroadcastReturn { result, signer } => {
                // Reclaim the signer regardless of pane state — the
                // dashboard must always end up holding it again.
                if let Ok(mut g) = signer.lock() {
                    if let Some(s) = g.take() {
                        self.signer = s;
                    } else {
                        warn!("broadcast return: signer cell was empty");
                    }
                }
                // Pump the result into the pane if it's still open. If
                // the user closed the modal mid-broadcast we silently
                // drop the result — the tx was still sent and a future
                // balance refresh will surface it.
                if let Modal::Send(p) = &mut self.modal {
                    let success = result.is_ok();
                    match &result {
                        Ok(hash) => info!(hash = %format!("{hash:#x}"), "broadcast ok"),
                        Err(e) => warn!(error = %e, "broadcast failed"),
                    }
                    let (task, _outcome) = p.update(send::Message::BroadcastDone(result));
                    // Refresh balance + portfolio + history on success so
                    // the dashboard reflects the new state (the hero
                    // balance, held-token list, and activity feed all
                    // shift). History is gated on `history_loaded`: if
                    // the user never opened the Activity tab there's no
                    // point paying for a fetch they won't see.
                    let refresh = if success {
                        let mut tasks = vec![
                            self.refresh_verification_task(),
                            self.fetch_portfolio_task(),
                        ];
                        if self.history_loaded {
                            self.history_loading = true;
                            self.reset_history_pending();
                            tasks.push(self.fetch_history_task());
                        }
                        Task::batch(tasks)
                    } else {
                        Task::none()
                    };
                    return (Task::batch([task.map(Message::Send), refresh]), None);
                }
            }
            Message::EnsAutoNameResolved { address, result } => {
                // The lookup was issued against `self.address` at the time
                // it was kicked off; if the user switched accounts before
                // it landed, drop it.
                if address != self.address {
                    return (Task::none(), None);
                }
                let name = match result {
                    Ok(Some(n)) => n,
                    Ok(None) => return (Task::none(), None),
                    Err(e) => {
                        warn!(error = %e, "ens auto-name lookup failed");
                        return (Task::none(), None);
                    }
                };
                // Re-check the name slot — the user could have renamed
                // since we kicked off the lookup, and we never overwrite
                // an explicit choice with an inferred one.
                let acc = match self.accounts.get_mut(self.active_index) {
                    Some(a) if a.name().is_none() => a,
                    _ => return (Task::none(), None),
                };
                acc.set_name(Some(name.clone()));
                // Bubble up so the App persists the rename to disk via
                // its existing rename pipeline.
                return (Task::none(), Some(Outcome::RenameActive(Some(name))));
            }
            Message::ClipboardWritten => {}
            Message::ClipboardClearArmed { generation } => {
                // Stale arm — a fresher copy bumped the counter — drop.
                if self
                    .clipboard_clear
                    .as_ref()
                    .is_none_or(|s| s.generation != generation)
                {
                    return (Task::none(), None);
                }
                // Read the live clipboard so the next handler can decide
                // whether the value is still ours. Doing the check at
                // probe time (instead of unconditionally clearing) keeps
                // us from clobbering content the user copied in the
                // meantime from outside the wallet.
                let task = iced::clipboard::read()
                    .map(move |actual| Message::ClipboardClearProbe { generation, actual });
                return (task, None);
            }
            Message::ClipboardClearProbe { generation, actual } => {
                let Some(state) = &self.clipboard_clear else {
                    return (Task::none(), None);
                };
                if state.generation != generation {
                    return (Task::none(), None);
                }
                let still_ours = actual.as_deref() == Some(state.expected.as_str());
                self.clipboard_clear = None;
                if still_ours {
                    let clear = iced::clipboard::write(String::new())
                        .map(|_: ()| Message::ClipboardWritten);
                    return (clear, None);
                }
                return (Task::none(), None);
            }
            Message::OpenReceive => {
                // In Safe mode, hand the Safe's address to the
                // existing Receive pane — it's address-agnostic.
                self.modal = Modal::Receive(ReceivePane::new(self.display_address()));
                self.chrome.open();
            }
            Message::Receive(child_msg) => {
                let Modal::Receive(p) = &mut self.modal else {
                    return (Task::none(), None);
                };
                let (task, outcome) = p.update(child_msg);
                let task = task.map(Message::Receive);
                match outcome {
                    Some(receive::Outcome::Closed) => {
                        self.chrome.start_close();
                        return (task, None);
                    }
                    None => return (task, None),
                }
            }
            Message::OpenSwap => {
                // Swap is meaningless from a Safe in v1 (no Safe-TX
                // calldata composer for swap routers yet). Silently
                // no-op when in Safe mode; the quick-action button
                // gates itself on `can_swap()` so users don't see a
                // disabled affordance.
                if self.active_safe.is_some() {
                    return (Task::none(), None);
                }
                self.modal = Modal::Swap(SwapPane::new());
                self.chrome.open();
            }
            Message::Swap(child_msg) => {
                let Modal::Swap(p) = &mut self.modal else {
                    return (Task::none(), None);
                };
                let (task, outcome) = p.update(child_msg);
                let task = task.map(Message::Swap);
                match outcome {
                    Some(swap::Outcome::Closed) => {
                        self.chrome.start_close();
                        return (task, None);
                    }
                    None => return (task, None),
                }
            }
            Message::OpenAccountDropdown => {
                self.modal = Modal::AccountDropdown(AccountDropdown::new());
            }
            Message::OpenTxDetails(idx) => {
                let Some(tx) = self.history.get(idx).cloned() else {
                    // History refreshed in the gap between view and click;
                    // silently ignore — the user will retry against the
                    // newer list.
                    return (Task::none(), None);
                };
                self.modal = Modal::TxDetails(TxDetailsPane::new(tx));
                self.chrome.open();
            }
            Message::TxDetails(child_msg) => {
                let Modal::TxDetails(p) = &mut self.modal else {
                    return (Task::none(), None);
                };
                let (task, outcome) = p.update(child_msg);
                let task = task.map(Message::TxDetails);
                match outcome {
                    Some(tx_details::Outcome::Closed) => {
                        self.chrome.start_close();
                        return (task, None);
                    }
                    Some(tx_details::Outcome::CopyText(s)) => {
                        let copy = self.arm_clipboard_clear(s);
                        return (Task::batch([task, copy]), None);
                    }
                    None => return (task, None),
                }
            }
            Message::AccountDropdown(child_msg) => {
                let Modal::AccountDropdown(d) = &mut self.modal else {
                    return (Task::none(), None);
                };
                let (task, outcome) = d.update(child_msg);
                let task = task.map(Message::AccountDropdown);
                match outcome {
                    Some(account_dropdown::Outcome::Switch(idx)) => {
                        self.modal = Modal::None;
                        // Switching to an EOA exits Safe mode in the
                        // same gesture.
                        let was_safe = self.active_safe.is_some();
                        self.active_safe = None;
                        if was_safe {
                            // Drop the Safe's history so the EOA's
                            // history lazy-loads on the next Activity
                            // visit instead of showing stale Safe
                            // txs. The pending queue is Safe-only, so
                            // clear it outright.
                            self.history.clear();
                            self.history_loaded = false;
                            self.history_errors = PerChain::default();
                            self.safe_pending.clear();
                            self.safe_pending_error = None;
                            self.safe_pending_loading = false;
                        }
                        if idx != self.active_index && idx < self.accounts.len() {
                            // Different account: the App rebuilds the
                            // dashboard and kicks the multi-chain
                            // portfolio fetch for the new EOA there.
                            return (task, Some(Outcome::Switch(idx)));
                        }
                        // Same underlying EOA — the App does NOT rebuild,
                        // so nothing else will refresh the portfolio. If
                        // we just left Safe mode, `self.portfolio` still
                        // holds the Safe's single-chain rows (a Base Safe
                        // fetches Base only via `allowed_chains`). Re-seed
                        // from the EOA's cache (now `display_address` is
                        // the EOA and every chain is back in scope) and
                        // refetch so all the EOA's chains repopulate.
                        if was_safe {
                            self.seed_portfolio_from_cache();
                            self.portfolio_loading = self.portfolio.is_empty();
                            let refresh = Task::batch([
                                self.refresh_verification_task(),
                                self.fetch_portfolio_task(),
                            ]);
                            return (Task::batch([task, refresh]), None);
                        }
                        return (task, None);
                    }
                    Some(account_dropdown::Outcome::SelectSafe(idx)) => {
                        self.modal = Modal::None;
                        if idx < self.safes.len() {
                            self.active_safe = Some(idx);
                            // Seed the Safe portfolio from cache if
                            // we've viewed it before; kick a fresh
                            // fetch either way so on-chain state
                            // catches up. History resets so the
                            // Activity tab lazy-loads for the Safe.
                            self.seed_portfolio_from_cache();
                            self.portfolio_loading = self.portfolio.is_empty();
                            self.history.clear();
                            self.history_loaded = false;
                            self.history_errors = PerChain::default();
                            // Reset the pending queue for the newly-active
                            // Safe — no cache, so show the loading line
                            // until the service answers. Loading only
                            // when the fetch actually launches, else the
                            // spinner would never clear.
                            self.safe_pending.clear();
                            self.safe_pending_error = None;
                            let pending = self.fetch_safe_pending_task();
                            self.safe_pending_loading = pending.is_some();
                            let refresh = Task::batch([
                                self.refresh_verification_task(),
                                self.fetch_portfolio_task(),
                                pending.unwrap_or_else(Task::none),
                            ]);
                            return (Task::batch([task, refresh]), None);
                        }
                        return (task, None);
                    }
                    Some(account_dropdown::Outcome::Add) => {
                        self.modal = Modal::None;
                        return (task, Some(Outcome::Add));
                    }
                    Some(account_dropdown::Outcome::Closed) => {
                        self.modal = Modal::None;
                        return (task, None);
                    }
                    None => return (task, None),
                }
            }
            Message::SafeSend(child_msg) => {
                let Modal::SafeSend(p) = &mut self.modal else {
                    return (Task::none(), None);
                };
                if let safe_send::Message::Confirm = &child_msg {
                    let Some(req) = p.outgoing_request() else {
                        warn!("safe-send: confirm dropped — form not ready");
                        return (Task::none(), None);
                    };
                    let owner_keys =
                        match collect_owner_keys(&req.linked_local_indices, &self.accounts) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!(error = %e, "safe-send: owner key lookup failed");
                                let _ = p.update(safe_send::Message::BroadcastDone(Err(e)));
                                return (Task::none(), None);
                            }
                        };
                    if owner_keys.is_empty() {
                        let msg = "No local owners available to sign — link a Local account to this Safe before sending.".to_string();
                        let _ = p.update(safe_send::Message::BroadcastDone(Err(msg)));
                        return (Task::none(), None);
                    }
                    info!(
                        chain = %req.chain.label(),
                        chain_id = req.chain.chain_id(),
                        safe = %req.safe_address,
                        to = %req.to,
                        value_wei = %req.value,
                        threshold = req.threshold,
                        owners_local = owner_keys.len(),
                        "safe-send: spawning broadcast task",
                    );
                    p.mark_busy();
                    let pre_task =
                        spawn_safe_broadcast_task(self.network.clone(), req, owner_keys);
                    return (pre_task, None);
                }
                if let safe_send::Message::Propose = &child_msg {
                    let Some(req) = p.outgoing_request() else {
                        warn!("safe-send: propose dropped — form not ready");
                        return (Task::none(), None);
                    };
                    // Sign once with the first signable owner (Local or
                    // hardware) and POST to the service for co-signers.
                    let Some(owner_desc) = req
                        .signable_indices
                        .first()
                        .and_then(|&idx| self.accounts.get(idx as usize).cloned())
                    else {
                        let _ = p.update(safe_send::Message::ProposeDone(Err(
                            "no signable owner linked to this Safe".to_string(),
                        )));
                        return (Task::none(), None);
                    };
                    info!(
                        chain = %req.chain.label(),
                        safe = %req.safe_address,
                        to = %req.to,
                        value_wei = %req.value,
                        "safe-send: spawning propose task",
                    );
                    p.mark_busy();
                    return (
                        spawn_safe_propose_task(self.network.clone(), owner_desc, req),
                        None,
                    );
                }
                let (task, outcome) = p.update(child_msg);
                // After pumping the pane, check whether the recipient
                // input now points at an ENS-shaped value that hasn't
                // been dispatched yet. Same sequence-guarded pattern as
                // the EOA Send pane.
                let ens_task = match p.take_pending_ens() {
                    Some((seq, name)) => {
                        spawn_safe_send_ens_resolve_task(self.network.clone(), seq, name)
                    }
                    None => Task::none(),
                };
                let task = task.map(Message::SafeSend);
                match outcome {
                    Some(safe_send::Outcome::Closed) => {
                        self.modal = Modal::None;
                        return (Task::batch([task, ens_task]), None);
                    }
                    Some(safe_send::Outcome::CopyText(s)) => {
                        let arm = self.arm_clipboard_clear(s);
                        return (Task::batch([task, arm, ens_task]), None);
                    }
                    Some(safe_send::Outcome::SaveAsContact { address, ens }) => {
                        // Same path as the EOA Send pane's CTA: switch
                        // nav to Settings → Contacts in Add mode
                        // pre-filled. `OpenContactsPaneWith` also runs
                        // the modal close animation, so the SafeSend
                        // modal tears down on the next chrome tick.
                        let open_task =
                            Task::done(Message::OpenContactsPaneWith { address, ens });
                        return (Task::batch([task, ens_task, open_task]), None);
                    }
                    None => return (Task::batch([task, ens_task]), None),
                }
            }
            Message::SafeSendBroadcastReturn(result) => {
                if let Modal::SafeSend(p) = &mut self.modal {
                    let success = result.is_ok();
                    match &result {
                        Ok(hash) => info!(hash = %format!("{hash:#x}"), "safe-send broadcast ok"),
                        Err(e) => warn!(error = %e, "safe-send broadcast failed"),
                    }
                    let (task, _outcome) =
                        p.update(safe_send::Message::BroadcastDone(result));
                    let refresh = if success {
                        Task::batch([
                            self.refresh_verification_task(),
                            self.fetch_portfolio_task(),
                        ])
                    } else {
                        Task::none()
                    };
                    return (Task::batch([task.map(Message::SafeSend), refresh]), None);
                }
            }
            Message::SafeSendProposeReturn(result) => {
                if let Modal::SafeSend(p) = &mut self.modal {
                    let success = result.is_ok();
                    match &result {
                        Ok(()) => info!("safe-send propose ok"),
                        Err(e) => warn!(error = %e, "safe-send propose failed"),
                    }
                    let (task, _outcome) =
                        p.update(safe_send::Message::ProposeDone(result));
                    // On success refresh the pending queue so the new
                    // proposal shows up the moment the user closes.
                    let refresh = if success {
                        self.fetch_safe_pending_task().unwrap_or_else(Task::none)
                    } else {
                        Task::none()
                    };
                    return (Task::batch([task.map(Message::SafeSend), refresh]), None);
                }
            }
            Message::OpenSafeTxDetails(idx) => {
                let Some(pending) = self.safe_pending.get(idx).cloned() else {
                    // Queue refreshed between view and click — ignore.
                    return (Task::none(), None);
                };
                let Some(safe) = self.active_safe_descriptor() else {
                    return (Task::none(), None);
                };
                let Some(chain) = crate::chain::Chain::from_chain_id(safe.chain_id) else {
                    return (Task::none(), None);
                };
                let safe_addr = safe.address();
                let threshold = safe.threshold;
                let owners: Vec<Address> =
                    safe.owners.iter().map(|o| Address::from(*o)).collect();
                let signable: Vec<Address> = self
                    .safe_signable_owners(safe)
                    .into_iter()
                    .map(|(a, _)| a)
                    .collect();
                let has_local_executor = self.first_local_key().is_some();
                let hash = pending.safe_tx_hash;
                self.modal = Modal::SafeTxDetail(SafeTxDetailPane::new(
                    safe_addr,
                    chain,
                    pending,
                    owners,
                    signable,
                    has_local_executor,
                ));
                self.chrome.open();
                return (
                    spawn_safe_detail_load_task(
                        self.network.clone(),
                        safe_addr,
                        chain,
                        hash,
                        threshold,
                    ),
                    None,
                );
            }
            Message::SafeTxDetailLoaded(result) => {
                if let Modal::SafeTxDetail(p) = &mut self.modal {
                    p.set_detail(result);
                }
            }
            Message::SafeTxActionDone(result) => {
                let Modal::SafeTxDetail(p) = &mut self.modal else {
                    return (Task::none(), None);
                };
                let ok = result.is_ok();
                match &result {
                    Ok(msg) => info!(msg, "safe-tx action ok"),
                    Err(e) => warn!(error = %e, "safe-tx action failed"),
                }
                let (safe, chain, hash) = (p.safe(), p.chain(), p.safe_tx_hash());
                p.set_action_result(result);
                if !ok {
                    return (Task::none(), None);
                }
                // Reload the queue and this tx's detail so the FSM badge
                // and owner checklist reflect the new state.
                let threshold = self
                    .active_safe_descriptor()
                    .map(|s| s.threshold)
                    .unwrap_or(0);
                let reload = spawn_safe_detail_load_task(
                    self.network.clone(),
                    safe,
                    chain,
                    hash,
                    threshold,
                );
                let refresh = self.fetch_safe_pending_task().unwrap_or_else(Task::none);
                return (Task::batch([reload, refresh]), None);
            }
            Message::SafeTxDetail(child) => {
                // Pump the child with a scoped &mut borrow so the rest of
                // the arm can call `self` helpers (owner lookup) without
                // overlapping the pane's borrow.
                let (task, outcome) = {
                    let Modal::SafeTxDetail(p) = &mut self.modal else {
                        return (Task::none(), None);
                    };
                    let (t, o) = p.update(child);
                    (t.map(Message::SafeTxDetail), o)
                };
                match outcome {
                    Some(safe_tx_detail::Outcome::Closed) => {
                        self.chrome.start_close();
                        return (task, None);
                    }
                    Some(safe_tx_detail::Outcome::Confirm) => {
                        // Sign with the first linked owner that hasn't
                        // signed yet, then POST the confirmation.
                        let prep = if let Modal::SafeTxDetail(p) = &self.modal {
                            p.loaded_detail().map(|d| {
                                (
                                    d.tx.clone(),
                                    d.safe_tx_hash,
                                    p.unsigned_signable().first().copied(),
                                    p.safe(),
                                    p.chain(),
                                )
                            })
                        } else {
                            None
                        };
                        let Some((tx, hash, owner, safe, chain)) = prep else {
                            return (task, None);
                        };
                        let owner_desc = owner.and_then(|a| self.owner_desc_for(a));
                        self.mark_safe_detail_busy();
                        let Some(owner_desc) = owner_desc else {
                            self.set_safe_detail_result(Err(
                                "no linked owner left to sign with".to_string(),
                            ));
                            return (task, None);
                        };
                        let confirm = spawn_safe_confirm_task(
                            self.network.clone(),
                            owner_desc,
                            safe,
                            chain,
                            tx,
                            hash,
                        );
                        return (Task::batch([task, confirm]), None);
                    }
                    Some(safe_tx_detail::Outcome::Execute) => {
                        let prep = if let Modal::SafeTxDetail(p) = &self.modal {
                            p.loaded_detail().map(|d| {
                                let confs: Vec<(Address, Bytes)> = d
                                    .confirmations
                                    .iter()
                                    .map(|c| (c.owner, c.signature.clone()))
                                    .collect();
                                (d.tx.clone(), confs, p.safe(), p.chain())
                            })
                        } else {
                            None
                        };
                        let Some((tx, confirmations, safe, chain)) = prep else {
                            return (task, None);
                        };
                        let executor_key = self.first_local_key();
                        self.mark_safe_detail_busy();
                        let Some(executor_key) = executor_key else {
                            self.set_safe_detail_result(Err(
                                "need a Local account to pay gas for execution".to_string(),
                            ));
                            return (task, None);
                        };
                        let exec = spawn_safe_execute_task(
                            self.network.clone(),
                            executor_key,
                            safe,
                            chain,
                            tx,
                            confirmations,
                        );
                        return (Task::batch([task, exec]), None);
                    }
                    Some(safe_tx_detail::Outcome::Reject) => {
                        let prep = if let Modal::SafeTxDetail(p) = &self.modal {
                            Some((p.safe(), p.chain(), p.nonce(), p.signable_owner()))
                        } else {
                            None
                        };
                        let Some((safe, chain, nonce, owner)) = prep else {
                            return (task, None);
                        };
                        let owner_desc = owner.and_then(|a| self.owner_desc_for(a));
                        self.mark_safe_detail_busy();
                        let Some(owner_desc) = owner_desc else {
                            self.set_safe_detail_result(Err(
                                "no linked owner available to reject".to_string(),
                            ));
                            return (task, None);
                        };
                        let reject = spawn_safe_reject_task(
                            self.network.clone(),
                            owner_desc,
                            safe,
                            chain,
                            nonce,
                        );
                        return (Task::batch([task, reject]), None);
                    }
                    None => return (task, None),
                }
            }
            Message::Tick => {
                if self.chrome.tick_settled() {
                    self.modal = Modal::None;
                }
            }
            Message::OpenNetworksSettings => {
                self.settings_pane =
                    SettingsPane::Networks(NetworksPane::new(self.network.clone()));
            }
            Message::OpenAppearanceSettings => {
                self.settings_pane = SettingsPane::Appearance;
            }
            Message::CloseAppearanceSettings => {
                self.settings_pane = SettingsPane::Root;
            }
            Message::OpenContactsSettings => {
                self.settings_pane =
                    SettingsPane::Contacts(ContactsPane::new(self.contacts.clone()));
            }
            Message::OpenContactsPaneWith { address, ens } => {
                // From the Send pane's "Save as contact" CTA. Switch
                // the nav to Settings, close the Send modal (the chrome
                // animation drives the swap), and open the contacts
                // pane in Add mode pre-filled.
                self.nav = Nav::Settings;
                self.settings_pane = SettingsPane::Contacts(ContactsPane::new_with_prefill(
                    self.contacts.clone(),
                    address,
                    ens,
                ));
                self.chrome.start_close();
                // Drop the user straight onto the NAME input — they
                // came here to type one.
                return (
                    ContactsPane::focus_initial_task().map(Message::Contacts),
                    None,
                );
            }
            Message::Contacts(child_msg) => {
                let SettingsPane::Contacts(p) = &mut self.settings_pane else {
                    return (Task::none(), None);
                };
                let (task, outcome) = p.update(child_msg);
                // Same dispatch pattern as the Send pane: after pumping
                // the pane we ask if it has a fresh ENS-shaped input
                // that hasn't been resolved yet, and spawn a task tagged
                // with the pane's seq so stale results are dropped.
                let ens_task = match p.take_pending_ens() {
                    Some((seq, name)) => {
                        spawn_contacts_ens_resolve_task(self.network.clone(), seq, name)
                    }
                    None => Task::none(),
                };
                let task = task.map(Message::Contacts);
                match outcome {
                    Some(contacts_settings::Outcome::Closed) => {
                        self.settings_pane = SettingsPane::Root;
                        return (Task::batch([task, ens_task]), None);
                    }
                    Some(contacts_settings::Outcome::SaveRequested(vec)) => {
                        return (
                            Task::batch([task, ens_task]),
                            Some(Outcome::SaveContacts(vec)),
                        );
                    }
                    None => return (Task::batch([task, ens_task]), None),
                }
            }
            Message::BeginRenameAccount => {
                let current = self
                    .accounts
                    .get(self.active_index)
                    .map(|a| a.name().unwrap_or("").to_string())
                    .unwrap_or_default();
                self.rename_draft = Some(current);
                return (focus_widget(header::RENAME_INPUT_ID), None);
            }
            Message::RenameInput(s) => {
                if self.rename_draft.is_some() {
                    self.rename_draft = Some(s);
                }
            }
            Message::CommitRename => {
                let Some(draft) = self.rename_draft.take() else {
                    return (Task::none(), None);
                };
                // Match `AccountDescriptor::set_name`'s trim-and-collapse rule
                // so the in-memory copy and the persisted copy agree on what
                // counts as "no name set".
                let trimmed = draft.trim();
                let cleaned = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
                if let Some(acc) = self.accounts.get_mut(self.active_index) {
                    acc.set_name(cleaned.clone());
                }
                return (Task::none(), Some(Outcome::RenameActive(cleaned)));
            }
            Message::CancelRename => {
                self.rename_draft = None;
            }
            Message::Networks(child_msg) => {
                let SettingsPane::Networks(p) = &mut self.settings_pane else {
                    return (Task::none(), None);
                };
                let (task, outcome) = p.update(child_msg);
                let task = task.map(Message::Networks);
                match outcome {
                    Some(networks::Outcome::Closed) => {
                        self.settings_pane = SettingsPane::Root;
                        return (task, None);
                    }
                    None => return (task, None),
                }
            }
        }
        (Task::none(), None)
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs: Vec<Subscription<Message>> = Vec::new();
        match &self.modal {
            Modal::AccountDropdown(d) => {
                subs.push(d.subscription().map(Message::AccountDropdown));
            }
            Modal::Swap(p) => {
                subs.push(p.subscription().map(Message::Swap));
            }
            Modal::Receive(p) => {
                subs.push(p.subscription().map(Message::Receive));
            }
            Modal::Send(p) => {
                subs.push(p.subscription().map(Message::Send));
            }
            Modal::TxDetails(p) => {
                subs.push(p.subscription().map(Message::TxDetails));
            }
            Modal::SafeSend(p) => {
                subs.push(p.subscription().map(Message::SafeSend));
            }
            Modal::SafeTxDetail(p) => {
                subs.push(p.subscription().map(Message::SafeTxDetail));
            }
            Modal::None => {}
        }
        match &self.settings_pane {
            SettingsPane::Networks(p) => subs.push(p.subscription().map(Message::Networks)),
            SettingsPane::Contacts(p) => subs.push(p.subscription().map(Message::Contacts)),
            _ => {}
        }
        if self.chrome.is_animating() || self.clipboard_clear.is_some() {
            // `time::every` actively drives ticks (and therefore redraws)
            // on a timer; `window::frames()` only observes redraws the
            // runtime already decided to do, which left the animation idle
            // between unrelated events. 16 ms (~60 Hz) is plenty for the
            // 220 ms ease — going faster just burns CPU during the modal
            // open/close transition. The clipboard countdown chip rides
            // the same subscription so its progress bar animates smoothly
            // for the 10-second auto-clear window.
            subs.push(iced::time::every(Duration::from_millis(16)).map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn theme(&self) -> KaoTheme {
        KaoTheme::for_kind(self.theme_kind)
    }

    // ── View ────────────────────────────────────────────────────────────────

    pub fn view(&self) -> Element<'_, Message> {
        let t = self.theme();

        let app = row![sidebar::view(t, self.nav), self.main_pane(t)]
            .width(Length::Fill)
            .height(Length::Fill);

        let background: Element<'_, Message> = container(app)
            .style(move |_| fill_style(t.bg))
            .width(Length::Fill)
            .height(Length::Fill)
            .into();

        // Snapshot the contacts book to an owned `ContactsView` for
        // any modal/pane that wants borrow-free contact data. iced
        // views are synchronous so the lock is uncontested; this
        // sidesteps the lifetime issue of holding a read guard across
        // Always render the same `stack![background, modal_layer]` tree
        // shape so iced doesn't reset child widget state when a modal
        // opens or closes. Without this, opening a modal changes the
        // tree from `Container` to `Stack(Container, Modal)`, and the
        // activity feed's scroll position (and any other internal
        // widget state below) snaps back to its default.
        let modal_layer: Element<'_, Message> = match &self.modal {
            Modal::None => Space::new().width(0).height(0).into(),
            Modal::Send(p) => {
                // EOA Send: exclude the active EOA from the picker
                // so the user doesn't see themselves as a recipient.
                // All Safes are valid destinations (deposit-to-Safe
                // is a common flow).
                let picker = self.recipient_picker(Some(self.active_index), None);
                p.view(t, &self.portfolio, picker, self.chrome.progress())
                    .map(Message::Send)
            }
            Modal::Receive(p) => p.view(t, self.chrome.progress()).map(Message::Receive),
            Modal::Swap(p) => p.view(t, self.chrome.progress()).map(Message::Swap),
            Modal::AccountDropdown(d) => d
                .view(t, &self.accounts, &self.safes, self.active_index, self.active_safe)
                .map(Message::AccountDropdown),
            Modal::TxDetails(p) => {
                let tx_book = match self.contacts.read() {
                    Ok(g) => g.clone(),
                    Err(_) => ContactsBook::new(),
                };
                p.view(t, self.chrome.progress(), &tx_book)
                    .map(Message::TxDetails)
            }
            Modal::SafeSend(p) => {
                // Safe Send: exclude the active Safe from the picker.
                // Own EOAs stay visible — withdrawing from a Safe back
                // to a signing key is a normal flow.
                let picker = self.recipient_picker(None, self.active_safe);
                p.view(t, picker, self.chrome.progress())
                    .map(Message::SafeSend)
            }
            Modal::SafeTxDetail(p) => {
                p.view(t, self.chrome.progress()).map(Message::SafeTxDetail)
            }
        };
        let composed: Element<'_, Message> = stack![background, modal_layer].into();

        // Bottom-right clipboard auto-clear chip rides on top of
        // whatever modal layer is currently visible. The chip is a
        // pointer-event sink only over its own card area; the rest of
        // the overlay is `Space`, so clicks on the screen below pass
        // through to the active modal/dashboard.
        match &self.clipboard_clear {
            None => composed,
            Some(state) => stack![composed, clipboard_clear_chip(t, state)].into(),
        }
    }

    // ── Send-flow helpers used by the broadcast Tasks ──────────────────────

    /// Build the merged recipient picker view for the Send /
    /// SafeSend modals. Combines saved contacts with the user's own
    /// accounts and Safes, dropping the active sender so the user
    /// doesn't see themselves as a destination. Lock-poisoned books
    /// degrade to the merged-without-contacts variant so own-account
    /// picking still works.
    fn recipient_picker(
        &self,
        exclude_account: Option<usize>,
        exclude_safe: Option<usize>,
    ) -> send::ContactsView {
        let empty = ContactsBook::new();
        match self.contacts.read() {
            Ok(g) => send::ContactsView::merged(
                &g,
                &self.accounts,
                &self.safes,
                exclude_account,
                exclude_safe,
            ),
            Err(_) => send::ContactsView::merged(
                &empty,
                &self.accounts,
                &self.safes,
                exclude_account,
                exclude_safe,
            ),
        }
    }

    // ── Main pane (header + body) ──────────────────────────────────────────

    fn main_pane<'a>(&'a self, t: KaoTheme) -> Element<'a, Message> {
        // Hold the read guard for the duration of `main_pane`. iced
        // views are synchronous and the lock is uncontested, so the
        // guard's lifetime safely outlives the returned Element.
        let contacts_guard = self.contacts.read().ok();
        let empty_book = ContactsBook::new();
        let contacts: &ContactsBook = match &contacts_guard {
            Some(g) => g,
            None => &empty_book,
        };
        let body: Element<'_, Message> = match self.nav {
            Nav::Home => home::view(
                t,
                self.can_send(),
                &self.portfolio,
                self.portfolio_loading,
                self.portfolio_refreshing,
                &self.safe_pending,
                self.safe_pending_loading,
                self.safe_pending_error.as_deref(),
            ),
            Nav::Activity => {
                // Show the error placeholder only when every configured
                // chain failed *and* nothing rendered. Otherwise partial
                // successes carry the feed even with one source down.
                let any_err = crate::chain::Chain::ALL
                    .iter()
                    .any(|c| self.history_errors.get(*c).is_some());
                let merged_error: Option<&str> = if any_err && self.history.is_empty() {
                    crate::chain::Chain::ALL
                        .iter()
                        .find_map(|c| self.history_errors.get(*c).as_deref())
                } else {
                    None
                };
                activity::view(
                    t,
                    self.display_address(),
                    &self.history,
                    self.history_loading,
                    merged_error,
                    contacts,
                )
            }
            Nav::Settings => match &self.settings_pane {
                SettingsPane::Root => settings_root::view(t),
                SettingsPane::Networks(p) => p.view(t).map(Message::Networks),
                SettingsPane::Appearance => appearance::view(t, self.theme_kind),
                SettingsPane::Contacts(p) => p.view(t).map(Message::Contacts),
            },
        };

        let display_addr = self.display_address();
        let display_name: String = match self.active_safe_descriptor() {
            Some(safe) => {
                let idx = self.active_safe.unwrap_or(0);
                let base = safe.display_name(idx);
                let total_owners = safe.owners.len().max(safe.linked_signer_indices.len());
                format!("{base} — Safe {} of {}", safe.threshold, total_owners)
            }
            None => self
                .accounts
                .get(self.active_index)
                .map(|a| a.display_name(self.active_index))
                .unwrap_or_else(|| format!("Account {}", self.active_index + 1)),
        };
        let network_label: &str = match self.active_safe_descriptor() {
            Some(safe) => crate::chain::Chain::ALL
                .iter()
                .find(|c| c.chain_id() == safe.chain_id)
                .map(|c| c.display_name())
                .unwrap_or("Unknown chain"),
            None => "Ethereum Mainnet",
        };
        let is_safe = self.active_safe.is_some();

        column![
            header::view(
                t,
                display_addr,
                self.verification,
                display_name,
                self.rename_draft.as_deref(),
                network_label,
                is_safe,
            ),
            body
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }
}

// ── Clipboard auto-clear chip ──────────────────────────────────────────────

/// Bottom-right chip rendered while the clipboard auto-clear is armed.
/// Two stacked rows: the countdown text on top, a thin progress bar
/// below. The bar drains as time elapses; when the bar reaches zero
/// the `ClipboardClearArmed` task fires, the ownership check runs, and
/// the clipboard is cleared if its contents still match what we wrote.
fn clipboard_clear_chip<'a>(t: KaoTheme, state: &'a ClipboardClearState) -> Element<'a, Message> {
    let now = Instant::now();
    let total = Duration::from_secs(CLIPBOARD_CLEAR_SECS);
    let remaining = state.deadline.saturating_duration_since(now);
    // `as_secs()` truncates; the +1 unless exactly on a second boundary
    // means the user reads "10…" the moment the chip lands and
    // counts down through "1…" for the final second instead of seeing
    // a flash of "0…" before the chip disappears.
    let secs_label = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
    let fraction = (remaining.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0);

    // Custom progress bar built from two fixed-width inner containers
    // in a row so we can theme it without leaning on the iced default.
    // 160 px is wide enough to read at-a-glance and narrow enough that
    // the chip stays compact on small windows.
    const BAR_WIDTH: f32 = 160.0;
    const BAR_HEIGHT: f32 = 4.0;
    let filled_px = BAR_WIDTH * fraction;
    let rest_px = BAR_WIDTH - filled_px;
    let filled = container(Space::new())
        .width(Length::Fixed(filled_px))
        .height(Length::Fixed(BAR_HEIGHT))
        .style(move |_| container::Style {
            background: Some(Background::Color(t.a1)),
            border: Border {
                color: iced::Color::TRANSPARENT,
                width: 0.0,
                radius: Radius::from(2),
            },
            ..container::Style::default()
        });
    let rest = container(Space::new())
        .width(Length::Fixed(rest_px))
        .height(Length::Fixed(BAR_HEIGHT))
        .style(move |_| container::Style {
            background: Some(Background::Color(with_alpha(t.border, 0.6))),
            border: Border {
                color: iced::Color::TRANSPARENT,
                width: 0.0,
                radius: Radius::from(2),
            },
            ..container::Style::default()
        });
    let bar = row![filled, rest]
        .width(Length::Fixed(BAR_WIDTH))
        .align_y(Alignment::Center);

    let label = text(format!("autoclear in {secs_label}…"))
        .size(11)
        .color(t.sub)
        .font(mono());

    let card = container(
        column![
            row![text("📋").size(11), Space::new().width(6), label].align_y(Alignment::Center),
            Space::new().height(6),
            bar,
        ]
        .width(Length::Shrink),
    )
    .padding(Padding::from([8, 12]))
    .style(move |_| container::Style {
        background: Some(Background::Color(t.card_alt)),
        border: Border {
            color: with_alpha(t.border, 0.7),
            width: 1.0,
            radius: Radius::from(10),
        },
        text_color: Some(t.text),
        ..container::Style::default()
    });

    // Pin to bottom-right: column[Space::Fill, row[Space::Fill, card]]
    // with 16 px of breathing room on the right and bottom edges.
    let bottom_row = row![
        Space::new().width(Length::Fill),
        card,
        Space::new().width(16),
    ]
    .width(Length::Fill);
    column![
        Space::new().height(Length::Fill),
        bottom_row,
        Space::new().height(16),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

// ── Send-flow helpers ──────────────────────────────────────────────────────

/// Format the largest amount the user can send for `tk`. For native ETH, if
/// the pane already has a quote loaded we subtract the gas cost so the
/// transaction won't bounce on insufficient-funds at broadcast time.
fn compute_max_amount(tk: &LiveToken, p: &SendPane) -> String {
    use alloy::primitives::utils::format_units;
    let raw = tk.balance_raw;
    let max_raw = if tk.contract.is_none() {
        // Native ETH: leave room for gas if we already have a quote.
        match p.quote() {
            Some(q) if raw > q.eth_cost_wei => raw - q.eth_cost_wei,
            _ => raw,
        }
    } else {
        raw
    };
    let raw_str =
        format_units(max_raw, tk.decimals).unwrap_or_else(|_| tk.balance.replace(',', ""));
    trim_trailing_decimal_zeros(&raw_str)
}

/// Strip trailing zeros (and the dangling decimal point) from a decimal
/// string. `format_units` always pads to `decimals` fractional digits, so
/// "1.000000000000000000" comes back from a 1 ETH balance — pumping that
/// into the amount input shows the user a wall of zeros after the value
/// they actually care about.
fn trim_trailing_decimal_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".into()
    } else {
        trimmed.to_string()
    }
}

/// Spawn a forward ENS resolution task for the Send recipient input.
/// Returns a `Task` that resolves to a `Send::EnsResolved(...)` message
/// tagged with `seq` so the pane can drop stale lookups.
fn spawn_ens_resolve_task(
    network: Arc<dyn BalanceFetcher>,
    seq: u64,
    name: String,
) -> Task<Message> {
    Task::perform(
        async move {
            let result = match network.provider(crate::chain::Chain::Mainnet).await {
                Some(provider) => crate::ens::resolve_name(&provider, &name).await,
                None => Err("no execution RPCs configured".to_string()),
            };
            (seq, name, result)
        },
        |(seq, name, result)| Message::Send(send::Message::EnsResolved { seq, name, result }),
    )
}

/// Forward ENS resolve for the SafeSend modal's recipient input. Same
/// shape as `spawn_ens_resolve_task`; differs only in the message
/// constructor it routes the result through.
fn spawn_safe_send_ens_resolve_task(
    network: Arc<dyn BalanceFetcher>,
    seq: u64,
    name: String,
) -> Task<Message> {
    Task::perform(
        async move {
            let result = match network.provider(crate::chain::Chain::Mainnet).await {
                Some(provider) => crate::ens::resolve_name(&provider, &name).await,
                None => Err("no execution RPCs configured".to_string()),
            };
            (seq, name, result)
        },
        |(seq, name, result)| {
            Message::SafeSend(safe_send::Message::EnsResolved { seq, name, result })
        },
    )
}

/// Forward ENS resolve for the Settings → Contacts add/edit form. Same
/// shape as `spawn_ens_resolve_task`; differs only in the message
/// constructor it routes the result through.
fn spawn_contacts_ens_resolve_task(
    network: Arc<dyn BalanceFetcher>,
    seq: u64,
    name: String,
) -> Task<Message> {
    Task::perform(
        async move {
            let result = match network.provider(crate::chain::Chain::Mainnet).await {
                Some(provider) => crate::ens::resolve_name(&provider, &name).await,
                None => Err("no execution RPCs configured".to_string()),
            };
            (seq, name, result)
        },
        |(seq, name, result)| {
            Message::Contacts(contacts_settings::Message::EnsResolved { seq, name, result })
        },
    )
}

/// Spawn a clear-signing decode task. Walks the proxy chain rooted at
/// the plan's target, fetches verified bytecode, runs evmole + 4byte +
/// matcher, and humanizes the resulting args. Result message carries
/// `seq` so the SendPane can drop stale completions if the user backed
/// out of review and built a different plan.
fn spawn_decode_task(network: Arc<dyn BalanceFetcher>, seq: u64, plan: SendPlan) -> Task<Message> {
    let (to, _value, calldata) = plan.tx_target();
    let chain = plan.chain;
    Task::perform(
        async move {
            let decoded =
                crate::decode::render::decode_call(network.as_ref(), chain, to, calldata).await;
            (seq, decoded)
        },
        |(seq, decoded)| {
            Message::Send(send::Message::DecodedReady {
                seq,
                decoded: Box::new(decoded),
            })
        },
    )
}

/// Spawn a quote task using the network's shared provider. Returns a
/// `Task` that resolves to a `Send::QuoteFetched(...)` message. The
/// provider is selected by `plan.chain` so an L2 send hits the L2 RPC,
/// not mainnet.
fn spawn_quote_task(network: Arc<dyn BalanceFetcher>, plan: SendPlan) -> Task<Message> {
    let chain = plan.chain;
    Task::perform(
        async move {
            match network.provider(chain).await {
                Some(provider) => {
                    crate::wallet::tx::build_quote(&provider, network.clone(), &plan).await
                }
                None => {
                    warn!(chain = %chain.label(), "quote: no execution RPC configured");
                    Err("no execution RPCs configured".into())
                }
            }
        },
        |result| Message::Send(send::Message::QuoteFetched(result)),
    )
}

/// Spawn the sign-and-broadcast task. The `handoff` cell carries the
/// signer in (the dashboard moved it out via `mem::replace`); the task
/// puts it back when finished. The result message round-trips both the
/// broadcast result and the handoff so the dashboard reclaims the signer.
/// Pull the Local key bytes for the given account indices. Bails on
/// the first non-Local entry rather than silently dropping — by the
/// time we're spawning a broadcast, the SafeSend pane has already
/// pre-flighted that all requested indices are Local, so a non-Local
/// here is a wallet-state mismatch worth surfacing.
fn collect_owner_keys(
    indices: &[u32],
    accounts: &[AccountDescriptor],
) -> Result<Vec<alloy::primitives::B256>, String> {
    let mut out = Vec::with_capacity(indices.len());
    for &idx in indices {
        match accounts.get(idx as usize) {
            Some(AccountDescriptor::Local { key_bytes, .. }) => {
                out.push(alloy::primitives::B256::from_slice(key_bytes));
            }
            Some(_) => {
                return Err(format!(
                    "linked owner #{idx} is not a Local account in this wallet",
                ));
            }
            None => return Err(format!("linked owner #{idx} not found in this wallet")),
        }
    }
    Ok(out)
}

/// `(address, descriptor)` for every linked owner that can sign — Local
/// or hardware, excluding view-only and dangling indices. Drives the
/// detail modal's Confirm/Reject signer selection. Free function so it's
/// testable without a full `WalletScreen`.
fn signable_owners_of(
    safe: &SafeDescriptor,
    accounts: &[AccountDescriptor],
) -> Vec<(Address, AccountDescriptor)> {
    safe.linked_signer_indices
        .iter()
        .filter_map(|&idx| {
            let desc = accounts.get(idx as usize)?;
            if matches!(desc, AccountDescriptor::ViewOnly { .. }) {
                return None;
            }
            Some((account_address(desc)?, desc.clone()))
        })
        .collect()
}

/// The descriptor whose address matches `addr`, if the wallet holds it.
fn owner_desc_by_address(addr: Address, accounts: &[AccountDescriptor]) -> Option<AccountDescriptor> {
    accounts
        .iter()
        .find(|a| account_address(a) == Some(addr))
        .cloned()
}

/// First Local account's private key — the gas-paying executor for
/// execute-from-queue. `None` if the wallet is hardware/view-only only.
fn first_local_key_of(accounts: &[AccountDescriptor]) -> Option<B256> {
    accounts.iter().find_map(|a| match a {
        AccountDescriptor::Local { key_bytes, .. } => Some(B256::from_slice(key_bytes)),
        _ => None,
    })
}

/// Spawn the Safe-TX sign-and-broadcast task.
///
/// 1. Build the SafeTx (reads on-chain nonce).
/// 2. Cross-check the local EIP-712 hash against the Safe's own
///    `getTransactionHash` — abort on divergence rather than sign
///    something the contract won't recover the same way.
/// 3. For the first `threshold` owner keys (in `owner_keys` order),
///    derive a Local signer and sign the hash.
/// 4. Pack signatures Safe-style (ascending by address, 65 B each).
/// 5. Call `execute_safe_tx` using the first owner as gas payer.
///
/// The first entry in `owner_keys` doubles as the gas-paying
/// executor — that way the dashboard's active EOA can be anything
/// (including view-only), and Safe-mode Send works as long as ≥1
/// linked owner is `Local` in this wallet. No handoff dance for the
/// executor needed.
fn spawn_safe_broadcast_task(
    network: Arc<dyn BalanceFetcher>,
    req: SafeSendRequest,
    owner_keys: Vec<alloy::primitives::B256>,
) -> Task<Message> {
    use crate::safe::tx::{
        Operation, SafeTxInput, build_safe_tx, execute_safe_tx, pack_owner_signatures,
        safe_domain, safe_tx_hash, verify_safe_tx_hash_on_chain,
    };
    let chain = req.chain;
    Task::perform(
        async move {
            let provider = match network.provider(chain).await {
                Some(p) => p,
                None => {
                    warn!(chain = %chain.label(), "safe-broadcast: no execution RPC configured");
                    return Err::<TxHash, String>("no execution RPCs configured".into());
                }
            };

            // Executor = first linked Local owner. Derived inside the
            // task so we never carry secret material across the
            // Task::perform boundary explicitly.
            let executor_key = match owner_keys.first().copied() {
                Some(k) => k,
                None => {
                    warn!("safe-broadcast: no owner keys supplied");
                    return Err("no local owners available".into());
                }
            };
            let executor_signer = match crate::wallet::signer_from_bytes(&executor_key) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "safe-broadcast: derive executor failed");
                    return Err(format!("derive executor: {e}"));
                }
            };
            let executor = KaoSigner::Local(executor_signer);

            let input = SafeTxInput {
                safe: req.safe_address,
                chain,
                to: req.to,
                value: req.value,
                data: alloy::primitives::Bytes::new(),
                operation: Operation::Call,
            };
            let safe_tx = match build_safe_tx(network.as_ref(), input).await {
                Ok(t) => t,
                Err(e) => return Err(e),
            };
            let domain = safe_domain(req.safe_address, chain);
            let local_hash = safe_tx_hash(&safe_tx, &domain);
            // Defense-in-depth — abort if the contract computes a
            // different hash than we did. Almost always means the
            // address isn't actually a Safe 1.3+ at this chain.
            let chain_hash = match verify_safe_tx_hash_on_chain(
                network.as_ref(),
                &safe_tx,
                req.safe_address,
                chain,
            )
            .await
            {
                Ok(h) => h,
                Err(e) => return Err(e),
            };
            if local_hash != chain_hash {
                return Err(format!(
                    "safe hash mismatch: local {local_hash:#x} vs on-chain {chain_hash:#x}",
                ));
            }
            let mut sigs = Vec::with_capacity(req.threshold as usize);
            for key in owner_keys.into_iter().take(req.threshold as usize) {
                let local = match crate::wallet::signer_from_bytes(&key) {
                    Ok(s) => s,
                    Err(e) => return Err(format!("derive signer: {e}")),
                };
                let kao = KaoSigner::Local(local);
                let sig = match kao.sign_hash(local_hash).await {
                    Ok(s) => s,
                    Err(e) => return Err(format!("sign hash: {e}")),
                };
                sigs.push((kao.address(), sig));
            }
            let packed = pack_owner_signatures(sigs);
            execute_safe_tx(
                &provider,
                &executor,
                req.safe_address,
                chain,
                safe_tx,
                packed,
            )
            .await
        },
        Message::SafeSendBroadcastReturn,
    )
}

/// Propose a Safe-send tx to the Transaction Service: build the SafeTx at
/// the live nonce, verify its hash on-chain, sign once as `owner_desc`
/// (software or hardware), and POST. Co-signers finish it from their own
/// wallets. Mirrors `spawn_safe_broadcast_task`'s build→verify→sign
/// front-half, but stops at the service instead of broadcasting.
fn spawn_safe_propose_task(
    network: Arc<dyn BalanceFetcher>,
    owner_desc: AccountDescriptor,
    req: SafeSendRequest,
) -> Task<Message> {
    use crate::safe::tx::{
        Operation, SafeTxInput, build_safe_tx, safe_domain, safe_tx_hash,
        sign_owner, verify_safe_tx_hash_on_chain,
    };
    let chain = req.chain;
    Task::perform(
        async move {
            let domain = safe_domain(req.safe_address, chain);
            let input = SafeTxInput {
                safe: req.safe_address,
                chain,
                to: req.to,
                value: req.value,
                data: alloy::primitives::Bytes::new(),
                operation: Operation::Call,
            };
            let tx = build_safe_tx(network.as_ref(), input).await?;
            let local = safe_tx_hash(&tx, &domain);
            let chain_hash =
                verify_safe_tx_hash_on_chain(network.as_ref(), &tx, req.safe_address, chain)
                    .await?;
            if local != chain_hash {
                return Err(format!(
                    "safe hash mismatch: local {local:#x} vs on-chain {chain_hash:#x}"
                ));
            }
            let signer = crate::wallet::build_owner_signer(&owner_desc).await?;
            let (sender, sig) = sign_owner(&signer, &tx, &domain, local).await?;
            crate::safe::service::propose(req.safe_address, chain, &tx, local, sender, &sig, Some("Kao"))
                .await?;
            Ok(())
        },
        Message::SafeSendProposeReturn,
    )
}

/// Load full detail (reconstructed `SafeTx` + per-owner signatures) for
/// one queued tx, for the detail modal's owner checklist and the
/// execute-from-queue path.
fn spawn_safe_detail_load_task(
    network: Arc<dyn BalanceFetcher>,
    safe: Address,
    chain: crate::chain::Chain,
    safe_tx_hash: B256,
    threshold: u32,
) -> Task<Message> {
    Task::perform(
        async move {
            crate::safe::service::fetch_detail(network.as_ref(), safe, chain, safe_tx_hash, threshold)
                .await
        },
        Message::SafeTxDetailLoaded,
    )
}

/// Confirm a queued tx: re-verify the safeTxHash on-chain, sign it as
/// `owner_desc` (software or hardware), and POST the confirmation. The
/// hash checks are defense-in-depth — refuse to sign if the detail we
/// loaded disagrees with what the Safe computes.
fn spawn_safe_confirm_task(
    network: Arc<dyn BalanceFetcher>,
    owner_desc: AccountDescriptor,
    safe: Address,
    chain: crate::chain::Chain,
    tx: crate::safe::SafeTx,
    safe_tx_hash: B256,
) -> Task<Message> {
    use crate::safe::tx::{
        safe_domain, safe_tx_hash as compute_hash, sign_owner, verify_safe_tx_hash_on_chain,
    };
    Task::perform(
        async move {
            let domain = safe_domain(safe, chain);
            let local = compute_hash(&tx, &domain);
            if local != safe_tx_hash {
                return Err("safe-tx: detail hash drifted; refusing to sign".to_string());
            }
            let chain_hash =
                verify_safe_tx_hash_on_chain(network.as_ref(), &tx, safe, chain).await?;
            if local != chain_hash {
                return Err(format!(
                    "safe hash mismatch: local {local:#x} vs on-chain {chain_hash:#x}"
                ));
            }
            let signer = crate::wallet::build_owner_signer(&owner_desc).await?;
            let (_, sig) = sign_owner(&signer, &tx, &domain, safe_tx_hash).await?;
            crate::safe::service::confirm(safe_tx_hash, chain, &sig).await?;
            Ok("Signature added".to_string())
        },
        Message::SafeTxActionDone,
    )
}

/// Execute a queued tx that reached threshold: assemble the signatures
/// the service collected and broadcast `execTransaction`. `executor_key`
/// is a Local account that pays gas (it need not be an owner — the Safe
/// validates signatures against its owner set, not `msg.sender`).
fn spawn_safe_execute_task(
    network: Arc<dyn BalanceFetcher>,
    executor_key: B256,
    safe: Address,
    chain: crate::chain::Chain,
    tx: crate::safe::SafeTx,
    confirmations: Vec<(Address, Bytes)>,
) -> Task<Message> {
    use crate::safe::tx::{assemble_signatures, execute_safe_tx};
    Task::perform(
        async move {
            let provider = network
                .provider(chain)
                .await
                .ok_or_else(|| "no execution RPCs configured".to_string())?;
            let executor = KaoSigner::Local(
                crate::wallet::signer_from_bytes(&executor_key).map_err(|e| e.to_string())?,
            );
            let packed = assemble_signatures(confirmations);
            let hash = execute_safe_tx(&provider, &executor, safe, chain, tx, packed).await?;
            Ok(format!("Executed · {hash:#x}"))
        },
        Message::SafeTxActionDone,
    )
}

/// Propose a same-nonce rejection for a queued tx: build the canonical
/// zero-value self-call at `nonce`, verify its hash on-chain, sign as
/// `owner_desc`, and POST it. Co-owners then confirm/execute it to void
/// the original.
fn spawn_safe_reject_task(
    network: Arc<dyn BalanceFetcher>,
    owner_desc: AccountDescriptor,
    safe: Address,
    chain: crate::chain::Chain,
    nonce: u64,
) -> Task<Message> {
    use crate::safe::tx::{
        build_rejection_tx, safe_domain, safe_tx_hash as compute_hash, sign_owner,
        verify_safe_tx_hash_on_chain,
    };
    Task::perform(
        async move {
            let domain = safe_domain(safe, chain);
            let tx = build_rejection_tx(safe, chain, nonce);
            let local = compute_hash(&tx, &domain);
            let chain_hash =
                verify_safe_tx_hash_on_chain(network.as_ref(), &tx, safe, chain).await?;
            if local != chain_hash {
                return Err(format!(
                    "safe hash mismatch: local {local:#x} vs on-chain {chain_hash:#x}"
                ));
            }
            let signer = crate::wallet::build_owner_signer(&owner_desc).await?;
            let (sender, sig) = sign_owner(&signer, &tx, &domain, local).await?;
            crate::safe::service::propose(safe, chain, &tx, local, sender, &sig, Some("Kao:reject"))
                .await?;
            Ok("Rejection proposed".to_string())
        },
        Message::SafeTxActionDone,
    )
}

fn spawn_broadcast_task(
    network: Arc<dyn BalanceFetcher>,
    handoff: SignerHandoff,
    plan: SendPlan,
    quote: crate::wallet::tx::TxQuote,
) -> Task<Message> {
    let inner = handoff.clone();
    let chain = plan.chain;
    Task::perform(
        async move {
            let provider = match network.provider(chain).await {
                Some(p) => p,
                None => {
                    warn!(chain = %chain.label(), "broadcast: no execution RPC configured");
                    return Err::<TxHash, String>("no execution RPCs configured".into());
                }
            };
            let signer_taken = {
                let mut g = match inner.lock() {
                    Ok(g) => g,
                    Err(e) => {
                        warn!(error = %e, "broadcast: signer cell poisoned");
                        return Err(format!("signer cell poisoned: {e}"));
                    }
                };
                g.take()
            };
            let signer = match signer_taken {
                Some(s) => s,
                None => {
                    warn!("broadcast: signer cell empty at task entry");
                    return Err("signer not available".into());
                }
            };
            let result = crate::wallet::tx::sign_and_send(&provider, &signer, plan, quote).await;
            // Put the signer back so the dashboard can reclaim it.
            if let Ok(mut g) = inner.lock() {
                *g = Some(signer);
            }
            result
        },
        move |result| Message::SendBroadcastReturn {
            result,
            signer: handoff,
        },
    )
}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    //! Race-condition coverage for the three address-tagged fetch
    //! messages: an in-flight fetch must never apply to the wrong
    //! account when the user switches between accounts (or invalidates
    //! the network/indexer client) before the response lands.
    use super::*;
    use crate::chain::Chain;
    use crate::net::MockFetcher;
    use crate::portfolio::{LiveToken, new_cache};
    use crate::wallet::{KaoSigner, view_only_account};
    use alloy::primitives::{Address, U256};

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    fn screen_for(addr: Address, cache: PortfolioCache) -> WalletScreen {
        WalletScreen::new(
            KaoSigner::ViewOnly(addr),
            vec![view_only_account(addr)],
            Vec::new(),
            0,
            Arc::new(MockFetcher::new()),
            cache,
            Arc::new(RwLock::new(ContactsBook::new())),
            None,
        )
    }

    fn token(symbol: &str, chain: Chain) -> LiveToken {
        LiveToken {
            symbol: symbol.into(),
            name: symbol.into(),
            balance: "1".into(),
            balance_f64: 1.0,
            balance_raw: U256::from(1u64),
            decimals: 18,
            contract: None,
            usd_price: 1.0,
            usd_value: 1.0,
            chain,
        }
    }

    #[test]
    fn portfolio_fetched_for_other_address_does_not_pollute_live_view() {
        let active = addr(0xAA);
        let other = addr(0xBB);
        let cache = new_cache();
        let mut s = screen_for(active, cache.clone());
        assert!(s.portfolio.is_empty());
        s.update(Message::PortfolioFetched {
            address: other,
            chain: Chain::Mainnet,
            result: Ok(vec![token("USDC", Chain::Mainnet)]),
        });
        // Active account's live portfolio must stay empty — the
        // response was for a different account.
        assert!(s.portfolio.is_empty());
    }

    #[test]
    fn portfolio_fetched_for_other_address_still_fills_that_address_cache_slot() {
        let active = addr(0xAA);
        let other = addr(0xBB);
        let cache = new_cache();
        let mut s = screen_for(active, cache.clone());
        s.update(Message::PortfolioFetched {
            address: other,
            chain: Chain::Mainnet,
            result: Ok(vec![token("USDC", Chain::Mainnet)]),
        });
        // The data is correct for `other`'s slot — we only suppressed
        // the live merge into the active screen. The active address's
        // slot must remain untouched.
        let g = cache.lock().expect("cache");
        assert_eq!(
            g.get(&(other, Chain::Mainnet)).map(|v| v.len()),
            Some(1),
            "other's cache slot should be populated",
        );
        assert!(
            g.get(&(active, Chain::Mainnet)).is_none(),
            "active's cache slot must not be touched by another address's fetch",
        );
    }

    #[test]
    fn portfolio_fetched_for_active_address_merges_and_caches() {
        let active = addr(0xAA);
        let cache = new_cache();
        let mut s = screen_for(active, cache.clone());
        s.update(Message::PortfolioFetched {
            address: active,
            chain: Chain::Mainnet,
            result: Ok(vec![token("USDC", Chain::Mainnet)]),
        });
        assert_eq!(s.portfolio.len(), 1);
        assert_eq!(s.portfolio[0].symbol, "USDC");
        assert!(!s.portfolio_loading);
        let g = cache.lock().expect("cache");
        assert_eq!(g.get(&(active, Chain::Mainnet)).map(|v| v.len()), Some(1));
    }

    #[test]
    fn history_fetched_for_other_address_is_dropped() {
        let active = addr(0xAA);
        let other = addr(0xBB);
        let mut s = screen_for(active, new_cache());
        // Pre-seed history so we can detect any clobber. The Mainnet
        // response empties the pending set, so loading flips to false.
        s.history_pending = vec![Chain::Mainnet];
        s.history_loading = true;
        s.update(Message::HistoryFetched {
            address: active,
            chain: Chain::Mainnet,
            result: Ok(Vec::new()),
        });
        let baseline_loading = s.history_loading;
        // Simulate a fresh fetch in flight for both chains.
        s.history_pending = vec![Chain::Mainnet, Chain::Base];
        s.history_loading = true;
        s.update(Message::HistoryFetched {
            address: other,
            chain: Chain::Mainnet,
            result: Ok(Vec::new()),
        });
        // The dropped response must not shrink the pending set or flip
        // `history_loading` off — otherwise the spinner would disappear
        // before the *real* fetch for `active` lands.
        assert!(
            s.history_loading,
            "history_loading must stay true when a foreign-address response is dropped",
        );
        assert_eq!(
            s.history_pending.len(),
            2,
            "pending set must be untouched by a foreign-address response",
        );
        // Sanity: the initial active-address response did clear loading.
        assert!(!baseline_loading);
    }

    fn safe_descriptor(byte: u8, chain_id: u64) -> SafeDescriptor {
        SafeDescriptor {
            name: None,
            chain_id,
            address: [byte; 20],
            version: "1.4.1".into(),
            trust: crate::wallet::SafeTrust::Canonical,
            threshold: 1,
            owners: Vec::new(),
            modules: Vec::new(),
            guard: None,
            fallback_handler: None,
            linked_signer_indices: vec![0],
            sibling_chains: Vec::new(),
            cached_at: 0,
        }
    }

    fn screen_with_safes(addr: Address, safes: Vec<SafeDescriptor>) -> WalletScreen {
        WalletScreen::new(
            KaoSigner::ViewOnly(addr),
            vec![view_only_account(addr)],
            safes,
            0,
            Arc::new(MockFetcher::new()),
            crate::portfolio::new_cache(),
            Arc::new(RwLock::new(ContactsBook::new())),
            None,
        )
    }

    #[test]
    fn display_address_returns_safe_addr_when_active_safe_set() {
        let mut screen =
            screen_with_safes(addr(1), vec![safe_descriptor(0x55, 1)]);
        assert_eq!(screen.display_address(), addr(1));
        screen.active_safe = Some(0);
        assert_eq!(screen.display_address(), Address::from([0x55u8; 20]));
    }

    #[test]
    fn select_safe_outcome_enters_safe_mode_and_resets_history() {
        let mut screen =
            screen_with_safes(addr(1), vec![safe_descriptor(0x77, 1)]);
        screen.history.push(IndexedTx {
            hash: alloy::primitives::B256::ZERO,
            block_number: 1,
            timestamp: 0,
            from: Address::ZERO,
            to: None,
            value: U256::ZERO,
            gas_used: None,
            gas_price: None,
            status: crate::indexer::TxStatus::Success,
            direction: crate::indexer::TxDirection::SelfTransfer,
            method: None,
            token: None,
            chain: Chain::Mainnet,
        });
        screen.history_loaded = true;
        // The handler bails unless the AccountDropdown modal is
        // open; mirror what happens after the user clicks the
        // address pill.
        screen.modal = Modal::AccountDropdown(AccountDropdown::new());
        let _ = screen.update(Message::AccountDropdown(
            account_dropdown::Message::SelectSafe(0),
        ));
        assert_eq!(screen.active_safe, Some(0));
        assert!(screen.history.is_empty(), "history should clear on Safe entry");
        assert!(!screen.history_loaded);
    }

    #[test]
    fn can_send_in_safe_mode_true_when_linked_local_exists() {
        // Wallet: active = ViewOnly. Account 1 = Local (linked to Safe).
        // EOA mode would gate Send off (active is view-only), but in
        // Safe mode the linked Local owner serves as both signer and
        // gas-paying executor, so Send must stay clickable.
        let accounts = vec![
            view_only_account(addr(1)),
            crate::wallet::AccountDescriptor::Local {
                name: None,
                key_bytes: [0x7e; 32],
            },
        ];
        let mut safe = safe_descriptor(0x99, 1);
        safe.linked_signer_indices = vec![1];
        let mut screen = WalletScreen::new(
            KaoSigner::ViewOnly(addr(1)),
            accounts,
            vec![safe],
            0,
            Arc::new(MockFetcher::new()),
            crate::portfolio::new_cache(),
            Arc::new(RwLock::new(ContactsBook::new())),
            None,
        );
        // EOA mode: ViewOnly active → can't send.
        assert!(!screen.can_send());
        // Safe mode: linked Local owner unlocks Send.
        screen.active_safe = Some(0);
        assert!(screen.can_send());
    }

    #[test]
    fn allowed_chains_in_eoa_mode_returns_all_chains() {
        let screen = screen_with_safes(addr(1), vec![safe_descriptor(0x55, 1)]);
        assert_eq!(screen.allowed_chains(), Chain::ALL.to_vec());
    }

    #[test]
    fn allowed_chains_in_safe_mode_returns_only_safe_chain() {
        // Mainnet Safe — only Mainnet shows.
        let mut screen =
            screen_with_safes(addr(1), vec![safe_descriptor(0x55, Chain::Mainnet.chain_id())]);
        screen.active_safe = Some(0);
        assert_eq!(screen.allowed_chains(), vec![Chain::Mainnet]);

        // Base Safe — only Base shows.
        let mut screen =
            screen_with_safes(addr(1), vec![safe_descriptor(0x66, Chain::Base.chain_id())]);
        screen.active_safe = Some(0);
        assert_eq!(screen.allowed_chains(), vec![Chain::Base]);

        // Optimism Safe — only Optimism shows.
        let mut screen = screen_with_safes(
            addr(1),
            vec![safe_descriptor(0x77, Chain::Optimism.chain_id())],
        );
        screen.active_safe = Some(0);
        assert_eq!(screen.allowed_chains(), vec![Chain::Optimism]);
    }

    #[test]
    fn portfolio_fetched_on_disallowed_chain_in_safe_mode_drops() {
        // Mainnet Safe receives a late Base portfolio fetch — must
        // not pollute the live view. The cache still gets written
        // (it's keyed by (addr, chain) and might be useful when the
        // user navigates back to that EOA), but `self.portfolio`
        // stays Safe-chain-only.
        let mut screen = screen_with_safes(
            addr(1),
            vec![safe_descriptor(0x55, Chain::Mainnet.chain_id())],
        );
        screen.active_safe = Some(0);
        let safe_addr = Address::from([0x55u8; 20]);
        // Stray Base fetch addressed to the Safe address.
        let stray = vec![LiveToken {
            chain: Chain::Base,
            contract: None,
            name: "Ether".into(),
            symbol: "ETH".into(),
            decimals: 18,
            balance: "0.1".into(),
            balance_f64: 0.1,
            balance_raw: U256::ZERO,
            usd_price: 1000.0,
            usd_value: 100.0,
        }];
        let _ = screen.update(Message::PortfolioFetched {
            address: safe_addr,
            chain: Chain::Base,
            result: Ok(stray),
        });
        assert!(
            screen.portfolio.is_empty(),
            "Base fetch must not surface on a Mainnet Safe",
        );
    }

    #[test]
    fn history_fetched_on_disallowed_chain_in_safe_mode_drops() {
        let mut screen = screen_with_safes(
            addr(1),
            vec![safe_descriptor(0x55, Chain::Mainnet.chain_id())],
        );
        screen.active_safe = Some(0);
        let safe_addr = Address::from([0x55u8; 20]);
        let stray = vec![IndexedTx {
            hash: alloy::primitives::B256::ZERO,
            block_number: 1,
            timestamp: 0,
            from: Address::ZERO,
            to: None,
            value: U256::ZERO,
            gas_used: None,
            gas_price: None,
            status: crate::indexer::TxStatus::Success,
            direction: crate::indexer::TxDirection::SelfTransfer,
            method: None,
            token: None,
            chain: Chain::Optimism,
        }];
        let _ = screen.update(Message::HistoryFetched {
            address: safe_addr,
            chain: Chain::Optimism,
            result: Ok(stray),
        });
        assert!(
            screen.history.is_empty(),
            "Optimism history must not surface on a Mainnet Safe",
        );
    }

    #[test]
    fn switch_account_outcome_exits_safe_mode_and_resets_history() {
        let mut screen =
            screen_with_safes(addr(1), vec![safe_descriptor(0x77, 1)]);
        screen.active_safe = Some(0);
        screen.history_loaded = true;
        screen.modal = Modal::AccountDropdown(AccountDropdown::new());
        // Outcome::Switch(0) — same account, but exit Safe mode.
        let (_, outcome) = screen.update(Message::AccountDropdown(
            account_dropdown::Message::Select(0),
        ));
        // Outcome only bubbles when idx != active_index, so for the
        // same account we get None back. The state flip is what we
        // assert.
        assert!(outcome.is_none());
        assert!(screen.active_safe.is_none());
        assert!(!screen.history_loaded);
    }

    #[test]
    fn collect_owner_keys_pulls_local_bytes_in_order() {
        // Mix Local with other variants — collect should only walk
        // the indices we hand it, in the order given, and return the
        // matching key bytes verbatim.
        let accounts = vec![
            crate::wallet::AccountDescriptor::Local {
                name: None,
                key_bytes: [0xaa; 32],
            },
            crate::wallet::AccountDescriptor::ViewOnly {
                name: None,
                address: [0u8; 20],
            },
            crate::wallet::AccountDescriptor::Local {
                name: None,
                key_bytes: [0xbb; 32],
            },
        ];
        let got = collect_owner_keys(&[2, 0], &accounts).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], alloy::primitives::B256::repeat_byte(0xbb));
        assert_eq!(got[1], alloy::primitives::B256::repeat_byte(0xaa));
    }

    #[test]
    fn collect_owner_keys_rejects_non_local_index() {
        let accounts = vec![
            crate::wallet::AccountDescriptor::Local {
                name: None,
                key_bytes: [0xaa; 32],
            },
            crate::wallet::AccountDescriptor::ViewOnly {
                name: None,
                address: [0u8; 20],
            },
        ];
        let err = collect_owner_keys(&[1], &accounts).unwrap_err();
        assert!(err.contains("not a Local account"), "got: {err}");
    }

    #[test]
    fn collect_owner_keys_rejects_out_of_bounds_index() {
        let accounts: Vec<crate::wallet::AccountDescriptor> = Vec::new();
        let err = collect_owner_keys(&[0], &accounts).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    fn local_with_key(seed: u8) -> AccountDescriptor {
        AccountDescriptor::Local {
            name: None,
            key_bytes: [seed; 32],
        }
    }

    fn ledger_acct(a: Address) -> AccountDescriptor {
        AccountDescriptor::Ledger {
            name: None,
            path: crate::wallet::LedgerHdPath::LedgerLive(0),
            address: a.into_array(),
        }
    }

    #[test]
    fn signable_owners_excludes_view_only_and_dangling_indices() {
        let accounts = vec![
            local_with_key(0xaa),          // 0: signable
            view_only_account(addr(0xbb)), // 1: excluded
            ledger_acct(addr(0xcc)),       // 2: signable (hardware)
        ];
        let mut safe = safe_descriptor(0x11, 1);
        safe.linked_signer_indices = vec![0, 1, 2, 9]; // 9 is dangling
        let got = signable_owners_of(&safe, &accounts);
        assert_eq!(got.len(), 2, "view-only + dangling dropped");
        assert!(matches!(got[0].1, AccountDescriptor::Local { .. }));
        assert!(matches!(got[1].1, AccountDescriptor::Ledger { .. }));
        // Hardware owner's address is carried through from the descriptor.
        assert_eq!(got[1].0, addr(0xcc));
    }

    #[test]
    fn first_local_key_finds_first_local_only() {
        let accounts = vec![
            view_only_account(addr(0xbb)),
            local_with_key(0x07),
            local_with_key(0x09),
        ];
        assert_eq!(first_local_key_of(&accounts), Some(B256::repeat_byte(0x07)));
        // Hardware/view-only-only wallet → no local executor.
        let hw_only = vec![view_only_account(addr(0xbb)), ledger_acct(addr(0xcc))];
        assert!(first_local_key_of(&hw_only).is_none());
    }

    #[test]
    fn owner_desc_by_address_matches_or_none() {
        let l = local_with_key(0x05);
        let la = account_address(&l).expect("local has address");
        let accounts = vec![view_only_account(addr(0xbb)), l];
        assert!(matches!(
            owner_desc_by_address(la, &accounts),
            Some(AccountDescriptor::Local { .. })
        ));
        assert!(owner_desc_by_address(addr(0xff), &accounts).is_none());
    }

    #[test]
    fn trim_trailing_decimal_zeros_strips_padding() {
        // `format_units` pads to `decimals`; "1 ETH" comes back with 18
        // trailing zeros. The amount-input must show "1", not the wall.
        assert_eq!(trim_trailing_decimal_zeros("1.000000000000000000"), "1");
        assert_eq!(trim_trailing_decimal_zeros("0.500000000000000000"), "0.5");
        assert_eq!(trim_trailing_decimal_zeros("0.000000000000000000"), "0");
        // Non-trailing zeros and integer-only inputs are preserved.
        assert_eq!(
            trim_trailing_decimal_zeros("0.123456789012"),
            "0.123456789012"
        );
        assert_eq!(trim_trailing_decimal_zeros("42"), "42");
    }

    // ── Safe pending queue: task gating + staleness ──────────────────

    fn safe_desc(chain_id: u64, address: Address) -> crate::wallet::SafeDescriptor {
        crate::wallet::SafeDescriptor {
            name: None,
            chain_id,
            address: address.into(),
            version: "1.4.1".into(),
            trust: crate::wallet::SafeTrust::Canonical,
            threshold: 2,
            owners: vec![addr(0x11).into(), addr(0x22).into()],
            modules: Vec::new(),
            guard: None,
            fallback_handler: None,
            linked_signer_indices: Vec::new(),
            sibling_chains: Vec::new(),
            cached_at: 0,
        }
    }

    fn safe_screen(chain_id: u64, safe_addr: Address) -> WalletScreen {
        let mut s = WalletScreen::new(
            KaoSigner::ViewOnly(addr(0xAA)),
            vec![view_only_account(addr(0xAA))],
            vec![safe_desc(chain_id, safe_addr)],
            0,
            Arc::new(MockFetcher::new()),
            new_cache(),
            Arc::new(RwLock::new(ContactsBook::new())),
            None,
        );
        s.active_safe = Some(0);
        s
    }

    fn pending_tx(nonce: u64) -> crate::safe::service::PendingSafeTx {
        crate::safe::service::PendingSafeTx {
            safe_tx_hash: B256::repeat_byte(0xab),
            to: addr(0xdd),
            value: U256::ZERO,
            data: alloy::primitives::Bytes::new(),
            nonce,
            state: crate::safe::service::SafeTxState::AwaitingConfirmations {
                have: 1,
                required: 2,
            },
            submission_ts: 0,
        }
    }

    #[test]
    fn fetch_safe_pending_task_is_none_in_eoa_mode() {
        let s = screen_for(addr(0xAA), new_cache());
        assert!(s.fetch_safe_pending_task().is_none());
    }

    #[test]
    fn fetch_safe_pending_task_launches_for_mainnet_safe() {
        // Mainnet always has the seeded default RPC, so an active
        // Mainnet Safe must produce a real task.
        let s = safe_screen(1, addr(0xCC));
        assert!(s.fetch_safe_pending_task().is_some());
    }

    #[test]
    fn fetch_safe_pending_task_is_none_for_unsupported_chain() {
        // chain_id 42161 (Arbitrum) isn't in Chain::ALL — the fetch
        // can't run, so no task may launch (and therefore no loading
        // flag may be set by callers).
        let s = safe_screen(42161, addr(0xCC));
        assert!(s.fetch_safe_pending_task().is_none());
    }

    #[test]
    fn refresh_portfolio_does_not_strand_pending_loading_when_fetch_cannot_launch() {
        // Regression: RefreshPortfolio used to set `safe_pending_loading`
        // for *any* active Safe, even when `fetch_safe_pending_task`
        // couldn't launch — no SafePendingFetched ever arrived and the
        // queue spinner hung forever.
        let mut s = safe_screen(42161, addr(0xCC));
        s.update(Message::RefreshPortfolio);
        assert!(!s.safe_pending_loading);
    }

    #[test]
    fn refresh_portfolio_marks_pending_loading_when_fetch_launches() {
        let mut s = safe_screen(1, addr(0xCC));
        s.update(Message::RefreshPortfolio);
        assert!(s.safe_pending_loading);
    }

    #[test]
    fn safe_pending_fetched_for_active_safe_applies_and_clears_loading() {
        let safe = addr(0xCC);
        let mut s = safe_screen(1, safe);
        s.safe_pending_loading = true;
        s.update(Message::SafePendingFetched {
            safe,
            chain: Chain::Mainnet,
            result: Ok(vec![pending_tx(5)]),
        });
        assert_eq!(s.safe_pending.len(), 1);
        assert_eq!(s.safe_pending[0].nonce, 5);
        assert!(s.safe_pending_error.is_none());
        assert!(!s.safe_pending_loading);
    }

    #[test]
    fn safe_pending_fetched_for_other_safe_is_dropped() {
        // A fetch issued before the user switched Safes must not land in
        // the new identity's queue — same staleness rule as
        // PortfolioFetched/HistoryFetched.
        let mut s = safe_screen(1, addr(0xCC));
        s.update(Message::SafePendingFetched {
            safe: addr(0xEE), // not the active Safe
            chain: Chain::Mainnet,
            result: Ok(vec![pending_tx(5)]),
        });
        assert!(s.safe_pending.is_empty());
    }

    #[test]
    fn safe_pending_fetched_for_wrong_chain_is_dropped() {
        // Same address, different chain — a different Safe. The cached
        // descriptor pins (address, chain_id); a stale fetch tagged with
        // another chain must not apply.
        let safe = addr(0xCC);
        let mut s = safe_screen(1, safe);
        s.update(Message::SafePendingFetched {
            safe,
            chain: Chain::Base,
            result: Ok(vec![pending_tx(5)]),
        });
        assert!(s.safe_pending.is_empty());
    }

    #[test]
    fn safe_pending_fetched_error_is_surfaced_not_swallowed() {
        let safe = addr(0xCC);
        let mut s = safe_screen(1, safe);
        s.safe_pending_loading = true;
        s.update(Message::SafePendingFetched {
            safe,
            chain: Chain::Mainnet,
            result: Err("safe-service queue: HTTP 503".into()),
        });
        assert!(!s.safe_pending_loading);
        assert_eq!(
            s.safe_pending_error.as_deref(),
            Some("safe-service queue: HTTP 503"),
        );
        // A later successful refresh clears the error.
        s.update(Message::SafePendingFetched {
            safe,
            chain: Chain::Mainnet,
            result: Ok(Vec::new()),
        });
        assert!(s.safe_pending_error.is_none());
    }
}
