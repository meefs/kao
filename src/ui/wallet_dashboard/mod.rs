//! Kao Wallet dashboard — the main screen shown after unlock.
//!
//! Layout mirrors the HTML mock in `kao/project/Kao Wallet.html`:
//! a thin sidebar (wordmark · Home/Activity/Settings · theme dots), a header
//! with a mood kaomoji, and one of three content panes. Send and Receive are
//! modal overlays rendered via `stack`.

use std::mem;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, TxHash};
use iced::widget::operation::focus as focus_widget;
use iced::widget::{column, container, row, stack};
use iced::{Element, Length, Subscription, Task};
use tracing::{debug, info, warn};

mod account_dropdown;
mod activity;
mod appearance;
mod header;
mod home;
mod modal_chrome;
mod nav;
mod networks;
mod receive;
mod send;
mod settings_root;
mod sidebar;
mod swap;
mod tx_details;

use account_dropdown::AccountDropdown;
use networks::NetworksPane;
use receive::ReceivePane;
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

use modal_chrome::ModalChrome;
pub use nav::Nav;

use crate::indexer::IndexedTx;
use crate::net::{BalanceFetcher, VerificationStatus};
use crate::portfolio::{LiveToken, PortfolioCache};
use crate::settings;
use crate::ui::kao_theme::{KaoTheme, ThemeKind};
use crate::ui::kao_widgets::fill_style;
use crate::wallet::tx::SendPlan;
use crate::wallet::{
    AccountDescriptor, KaoSigner, SignerHandoff, handoff_with, short_address,
};

// ── Messages ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Message {
    /// Carries the address the fetch was issued against so a quick
    /// account switch (or a network/indexer change that kicks fresh
    /// fetches) can't apply a stale balance to a different account.
    BalanceFetched {
        address: Address,
        result: Result<String, String>,
    },
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
    /// Result of an indexer transaction-history fetch. `address` is
    /// the address it was issued against; dropped on mismatch.
    HistoryFetched {
        address: Address,
        result: Result<Vec<IndexedTx>, String>,
    },
    SelectNav(Nav),
    SelectTheme(ThemeKind),
    OpenSend,
    OpenReceive,
    OpenSwap,
    OpenAccountDropdown,
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
}

/// Outcomes bubbled up to the parent app.
#[derive(Debug, Clone)]
pub enum Outcome {
    Switch(usize),
    Add,
    /// User edited the active account's display name. Carries the new
    /// value (or `None` to clear back to the indexed default).
    RenameActive(Option<String>),
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
}

/// Which settings pane is currently rendered. The Settings nav slot can show
/// either the root list of categories or one of the deeper category screens.
#[derive(Debug)]
enum SettingsPane {
    Root,
    Networks(NetworksPane),
    Appearance,
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
    active_index: usize,
    balance: Result<String, String>,
    theme_kind: ThemeKind,
    nav: Nav,
    modal: Modal,
    /// Open/close animation state for the Send/Receive/Swap modal slot. The
    /// account dropdown bypasses chrome (instant open/close).
    chrome: ModalChrome,
    /// Shared Helios-backed RPC client; cloned into each balance fetch task.
    network: Arc<dyn BalanceFetcher>,
    /// Verification state of the most recent balance fetch. Sampled from
    /// `network.last_status()` whenever a `BalanceFetched` lands; rendered in
    /// the header as a small "Verified by Helios / Unverified RPC" badge.
    verification: VerificationStatus,
    /// Which Settings sub-screen is currently rendered.
    settings_pane: SettingsPane,
    /// Live portfolio entries fetched from on-chain balances + CoinGecko.
    portfolio: Vec<LiveToken>,
    /// True while a portfolio fetch is in flight.
    portfolio_loading: bool,
    /// Process-lifetime cache shared with `App` so switching back to a
    /// previously-loaded account renders its tokens immediately while a
    /// fresh fetch refreshes them in the background.
    portfolio_cache: PortfolioCache,
    /// Inline rename draft for the active account. `Some(s)` means the
    /// header is showing the rename text input; `None` means it's showing
    /// the static name + pencil affordance.
    rename_draft: Option<String>,
    /// Most recent transactions for the active address, newest first. Empty
    /// while a fetch is in flight or when the active provider returns
    /// nothing (e.g. `IndexerProvider::None`).
    history: Vec<IndexedTx>,
    /// True while an indexer history fetch is in flight.
    history_loading: bool,
}

impl WalletScreen {
    pub fn new(
        signer: KaoSigner,
        accounts: Vec<AccountDescriptor>,
        active_index: usize,
        network: Arc<dyn BalanceFetcher>,
        portfolio_cache: PortfolioCache,
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
            active_index,
            balance: Ok("—".into()),
            theme_kind: settings::theme(),
            nav: Nav::Home,
            modal: Modal::None,
            chrome: ModalChrome::new(),
            network,
            verification: VerificationStatus::Connecting,
            settings_pane: SettingsPane::Root,
            portfolio,
            portfolio_loading,
            portfolio_cache,
            rename_draft: None,
            history: Vec::new(),
            history_loading: true,
        }
    }

    /// Move the live signer out of the dashboard. Used by the App when it
    /// transitions away from the dashboard (e.g. into the add-account flow)
    /// and wants to park the signer to return cheaply later.
    pub fn into_signer(self) -> KaoSigner {
        self.signer
    }

    /// True while a send is in flight (the signer has been moved into a
    /// broadcast task and not yet reclaimed). Used by the App to refuse
    /// disruptive transitions like begin-add-account during a send — those
    /// would race with the in-flight broadcast on the signer cell.
    pub fn is_send_busy(&self) -> bool {
        match &self.modal {
            Modal::Send(p) => p.busy(),
            _ => false,
        }
    }

    /// The active address in short `0xabcd…ef01` form. For diagnostic logs.
    pub fn address_for_log(&self) -> String {
        short_address(self.address)
    }

    pub fn fetch_balance_task(&self) -> Task<Message> {
        let address = self.address;
        let network = self.network.clone();
        Task::perform(
            async move {
                debug!(addr = %short_address(address), "dashboard: helios get_balance");
                let started = std::time::Instant::now();
                let result = network.balance(address, crate::chain::Chain::Mainnet).await;
                debug!(
                    elapsed = ?started.elapsed(),
                    ok = result.is_ok(),
                    "dashboard: helios get_balance completed",
                );
                (address, result)
            },
            |(address, result)| Message::BalanceFetched { address, result },
        )
    }

    /// Pull the most recent transactions for the active address from the
    /// configured indexer. `IndexerProvider::None` short-circuits to an empty
    /// list — there's no on-chain fallback the way `fetch_portfolio_task`
    /// has, since reconstructing history from logs/blocks is way out of
    /// scope for a wallet UI.
    pub fn fetch_history_task(&self) -> Task<Message> {
        let address = self.address;
        let provider = settings::indexer_provider();
        Task::perform(
            async move {
                debug!(
                    addr = %short_address(address),
                    indexer = ?provider,
                    "fetching history",
                );
                let started = std::time::Instant::now();
                // History stays mainnet-only for now — Etherscan and
                // Blockscout don't have per-chain plumbing yet, and
                // showing a half-baked L2 history would be worse than
                // none. Per-chain history is a follow-up.
                let indexer = crate::indexer::build_indexer_for(crate::chain::Chain::Mainnet);
                let result = indexer.transactions(address, HISTORY_LIMIT).await;
                debug!(
                    elapsed = ?started.elapsed(),
                    ok = result.is_ok(),
                    count = result.as_ref().map(|v| v.len()).unwrap_or(0),
                    "history fetch completed",
                );
                (address, result)
            },
            |(address, result)| Message::HistoryFetched { address, result },
        )
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
        let address = self.address;
        let provider_kind = settings::indexer_provider();
        let mut tasks: Vec<Task<Message>> = Vec::new();
        for chain in crate::chain::Chain::ALL {
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
                    // Prefer the indexer when one is wired up for this
                    // chain (one HTTP round-trip vs. several on-chain
                    // reads + per-token pool lookups). When it isn't
                    // (Blockscout/Etherscan on L2, or `None` anywhere),
                    // `build_indexer_for` returns `NoopIndexer` and we
                    // fall back to the on-chain walk.
                    let indexer = crate::indexer::build_indexer_for(chain);
                    let from_indexer = indexer
                        .balances(address)
                        .await
                        .ok()
                        .filter(|v| !v.is_empty());
                    let result = if let Some(tokens) = from_indexer {
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
                |(address, chain, result)| {
                    Message::PortfolioFetched { address, chain, result }
                },
            ));
        }
        Task::batch(tasks)
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
            Message::BalanceFetched { address, result } => {
                // The fetch was issued against `address`; if the user
                // switched accounts (or invalidated the network client)
                // before it landed, drop it so the previous account's
                // balance can't appear under the new one.
                if address != self.address {
                    return (Task::none(), None);
                }
                self.balance = result;
                self.verification = self.network.last_status(crate::chain::Chain::Mainnet);
            }
            Message::PortfolioFetched { address, chain, result } => {
                // Always write the (address, chain) we issued the fetch
                // for into the cache — it's still the correct slot for
                // that account's data even if the user has since
                // switched away. Only the live portfolio merge is gated
                // on `address == self.address`.
                if let Ok(tokens) = &result
                    && let Ok(mut cache) = self.portfolio_cache.lock() {
                        cache.insert((address, chain), tokens.clone());
                    }
                if address != self.address {
                    return (Task::none(), None);
                }
                // Loading flag clears once *any* chain lands; the user
                // sees results stream in rather than wait for the
                // slowest chain.
                self.portfolio_loading = false;
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
            Message::HistoryFetched { address, result } => {
                if address != self.address {
                    return (Task::none(), None);
                }
                self.history_loading = false;
                match result {
                    Ok(txs) => self.history = txs,
                    Err(e) => warn!(error = %e, "history fetch failed"),
                }
            }
            Message::SelectNav(nav) => {
                if self.nav != nav {
                    self.settings_pane = SettingsPane::Root;
                }
                self.nav = nav;
            }
            Message::SelectTheme(k) => {
                self.theme_kind = k;
                settings::set_theme(k);
            }
            Message::OpenSend => {
                if !self.signer.can_sign() {
                    // View-only accounts can't broadcast. Open the modal in
                    // a "this is read-only" state would be nicer; for now,
                    // refuse to open it at all.
                    info!("send disabled: active account is view-only");
                    return (Task::none(), None);
                }
                self.modal = Modal::Send(SendPane::new(self.address));
                self.chrome.open();
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
                // (gas + 1559 fees + nonce) using the same plan the
                // pane will eventually broadcast.
                if let send::Message::Step(2) = &child_msg {
                    let plan = p.build_plan(&self.portfolio);
                    let pre_task = match plan {
                        Some(pl) => {
                            p.quote_started();
                            spawn_quote_task(self.network.clone(), pl)
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
                    let quote = p.quote();
                    if let (Some(plan), Some(quote)) = (plan, quote) {
                        // Move the signer out — only ViewOnly stays
                        // behind so the dashboard view doesn't crash if
                        // it dereferences the signer's address while the
                        // task is running. self.address stays correct.
                        let signer = mem::replace(
                            &mut self.signer,
                            KaoSigner::ViewOnly(self.address),
                        );
                        let handoff = handoff_with(signer);
                        let pre_task =
                            spawn_broadcast_task(self.network.clone(), handoff, plan, quote);
                        let (task, _outcome) = p.update(child_msg);
                        let task = task.map(Message::Send);
                        return (Task::batch([pre_task, task]), None);
                    }
                    // Missing plan or quote — let the pane no-op the
                    // confirm.
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
                        let copy_task =
                            iced::clipboard::write(s).map(|_: ()| Message::ClipboardWritten);
                        return (Task::batch([task, copy_task, ens_task]), None);
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
                        Ok(hash) => debug!(hash = %format!("{hash:#x}"), "broadcast ok"),
                        Err(e) => warn!(error = %e, "broadcast failed"),
                    }
                    let (task, _outcome) = p.update(send::Message::BroadcastDone(result));
                    // Refresh balance + portfolio + history on success so
                    // the dashboard reflects the new state (the hero
                    // balance, held-token list, and activity feed all
                    // shift).
                    let refresh = if success {
                        self.history_loading = true;
                        Task::batch([
                            self.fetch_balance_task(),
                            self.fetch_portfolio_task(),
                            self.fetch_history_task(),
                        ])
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
            Message::OpenReceive => {
                self.modal = Modal::Receive(ReceivePane::new(self.address));
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
                        let copy = iced::clipboard::write(s)
                            .map(|_: ()| Message::ClipboardWritten);
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
                        if idx != self.active_index && idx < self.accounts.len() {
                            return (task, Some(Outcome::Switch(idx)));
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
            Modal::None => {}
        }
        if let SettingsPane::Networks(p) = &self.settings_pane {
            subs.push(p.subscription().map(Message::Networks));
        }
        if self.chrome.is_animating() {
            // `time::every` actively drives ticks (and therefore redraws)
            // on a timer; `window::frames()` only observes redraws the
            // runtime already decided to do, which left the animation idle
            // between unrelated events. 16 ms (~60 Hz) is plenty for the
            // 220 ms ease — going faster just burns CPU during the modal
            // open/close transition.
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

        match &self.modal {
            Modal::None => background,
            Modal::Send(p) => stack![
                background,
                p.view(t, &self.portfolio, self.chrome.progress())
                    .map(Message::Send),
            ]
            .into(),
            Modal::Receive(p) => stack![
                background,
                p.view(t, self.chrome.progress()).map(Message::Receive),
            ]
            .into(),
            Modal::Swap(p) => stack![
                background,
                p.view(t, self.chrome.progress()).map(Message::Swap),
            ]
            .into(),
            Modal::AccountDropdown(d) => stack![
                background,
                d.view(t, &self.accounts, self.active_index)
                    .map(Message::AccountDropdown),
            ]
            .into(),
            Modal::TxDetails(p) => stack![
                background,
                p.view(t, self.chrome.progress()).map(Message::TxDetails),
            ]
            .into(),
        }
    }


    // ── Send-flow helpers used by the broadcast Tasks ──────────────────────

    // ── Main pane (header + body) ──────────────────────────────────────────

    fn main_pane(&self, t: KaoTheme) -> Element<'_, Message> {
        let body: Element<'_, Message> = match self.nav {
            Nav::Home => home::view(t, &self.signer, &self.portfolio, self.portfolio_loading),
            Nav::Activity => activity::view(t, self.address, &self.history, self.history_loading),
            Nav::Settings => match &self.settings_pane {
                SettingsPane::Root => settings_root::view(t),
                SettingsPane::Networks(p) => p.view(t).map(Message::Networks),
                SettingsPane::Appearance => appearance::view(t, self.theme_kind),
            },
        };

        let display_name = self
            .accounts
            .get(self.active_index)
            .map(|a| a.display_name(self.active_index))
            .unwrap_or_else(|| format!("Account {}", self.active_index + 1));

        column![
            header::view(
                t,
                self.address,
                self.verification,
                display_name,
                self.rename_draft.as_deref(),
            ),
            body
        ]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }


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
    format_units(max_raw, tk.decimals).unwrap_or_else(|_| tk.balance.replace(',', ""))
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

/// Spawn a quote task using the network's shared provider. Returns a
/// `Task` that resolves to a `Send::QuoteFetched(...)` message.
fn spawn_quote_task(
    network: Arc<dyn BalanceFetcher>,
    plan: SendPlan,
) -> Task<Message> {
    Task::perform(
        async move {
            match network.provider(crate::chain::Chain::Mainnet).await {
                Some(provider) => crate::wallet::tx::build_quote(&provider, &plan).await,
                None => Err("no execution RPCs configured".into()),
            }
        },
        |result| Message::Send(send::Message::QuoteFetched(result)),
    )
}

/// Spawn the sign-and-broadcast task. The `handoff` cell carries the
/// signer in (the dashboard moved it out via `mem::replace`); the task
/// puts it back when finished. The result message round-trips both the
/// broadcast result and the handoff so the dashboard reclaims the signer.
fn spawn_broadcast_task(
    network: Arc<dyn BalanceFetcher>,
    handoff: SignerHandoff,
    plan: SendPlan,
    quote: crate::wallet::tx::TxQuote,
) -> Task<Message> {
    let inner = handoff.clone();
    Task::perform(
        async move {
            let provider = match network.provider(crate::chain::Chain::Mainnet).await {
                Some(p) => p,
                None => return Err::<TxHash, String>("no execution RPCs configured".into()),
            };
            let signer_taken = {
                let mut g = match inner.lock() {
                    Ok(g) => g,
                    Err(e) => return Err(format!("signer cell poisoned: {e}")),
                };
                g.take()
            };
            let signer = match signer_taken {
                Some(s) => s,
                None => return Err("signer not available".into()),
            };
            let result =
                crate::wallet::tx::sign_and_send(&provider, &signer, plan, quote).await;
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
            0,
            Arc::new(MockFetcher::new()),
            cache,
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
    fn balance_fetched_for_other_address_is_dropped() {
        let active = addr(0xAA);
        let other = addr(0xBB);
        let mut s = screen_for(active, new_cache());
        let before = s.balance.clone();
        s.update(Message::BalanceFetched {
            address: other,
            result: Ok("999".into()),
        });
        // Active screen's balance must be untouched — the response was
        // for a different account.
        assert_eq!(s.balance, before);
    }

    #[test]
    fn balance_fetched_for_active_address_lands() {
        let active = addr(0xAA);
        let mut s = screen_for(active, new_cache());
        s.update(Message::BalanceFetched {
            address: active,
            result: Ok("1.23".into()),
        });
        assert_eq!(s.balance, Ok("1.23".into()));
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
        // Pre-seed history so we can detect any clobber.
        s.update(Message::HistoryFetched {
            address: active,
            result: Ok(Vec::new()),
        });
        let baseline_loading = s.history_loading;
        s.history_loading = true; // simulate a fresh fetch in flight
        s.update(Message::HistoryFetched {
            address: other,
            result: Ok(Vec::new()),
        });
        // The dropped response must not flip `history_loading` off —
        // otherwise the spinner would disappear before the *real*
        // fetch for `active` lands.
        assert!(
            s.history_loading,
            "history_loading must stay true when a foreign-address response is dropped",
        );
        // Sanity: the initial active-address response did clear it.
        assert!(!baseline_loading);
    }
}
