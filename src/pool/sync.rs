//! Chain sync: replay a pool's events over Kao's provider, rebuild the state
//! tree, recover this account's notes, and check the rebuilt root against
//! on-chain state.
//!
//! Logs (`eth_getLogs`) can't be Helios-verified, so — exactly as the SDK
//! intends — trust is anchored in `eth_call`: the rebuilt state root is checked
//! against the pool's on-chain root. We do that check through Kao's *verified*
//! `call()` (Helios) so a tampered log set can't pass. Discovery of *which*
//! notes are ours is deterministic from the mnemonic, so a lying RPC can at
//! worst hide deposits (surfaced as a stale balance), never forge one.

use std::collections::HashSet;

use alloy::network::Ethereum;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::sol_types::SolCall;
use serde::{Deserialize, Serialize};

use privacy_pools::{
    Account, DepositLog, Field, IEntrypoint, IPrivacyPool, LeanImt, PoolAccount, PoolLogs,
    RagequitLog, Syncer, WithdrawLog, field_to_u256, poseidon, recover_accounts, u256_to_field,
};

use crate::chain::Chain;
use crate::net::BalanceFetcher;

use super::{PoolError, PoolInfo};

/// Consecutive-miss gap limit for note recovery — matches the reference SDKs.
const GAP_LIMIT: usize = 10;

/// Blocks re-scanned below a cached cursor to absorb shallow reorgs — anything
/// dropped/replaced within this depth is re-fetched fresh and dedup-merged.
const REORG_BUFFER: u64 = 64;

/// Blocks per `eth_getLogs` window (matches the SDK's default chunk size).
const SCAN_CHUNK: u64 = 5_000;
/// Retries per window on a transient `get_logs` failure (502 / timeout / reset)
/// before the whole scan gives up. Public-RPC `get_logs` hiccups are
/// overwhelmingly transient, and one bad window shouldn't abort a scan that runs
/// from the Entrypoint deploy block.
const CHUNK_MAX_RETRIES: u32 = 4;
/// Base backoff between window retries, doubled each attempt (300/600/1200/2400 ms).
const CHUNK_RETRY_BASE_MS: u64 = 300;

/// A synced snapshot of one pool for the active account. The state tree lives
/// off this struct now — it's sourced from the 0xbow API + Helios-verified only
/// when a withdrawal needs it (see [`verified_state_tree`]); display only needs
/// the recovered notes.
#[derive(Debug, Clone)]
pub struct PoolState {
    /// Recovered pool accounts (the "PA-1 / PA-2 …" list), in deposit order.
    pub accounts: Vec<PoolAccount>,
    /// Last block scanned (the incremental-resync cursor).
    pub to_block: u64,
}

impl PoolState {
    /// The next unused deposit index for this scope (recovery finds indices
    /// `0..n`; the next deposit uses `n`).
    pub fn next_deposit_index(&self) -> u64 {
        self.accounts
            .iter()
            .map(|a| a.deposit_index)
            .max()
            .map_or(0, |m| m + 1)
    }

    /// Total spendable value across all (non-ragequit, non-empty) pool accounts.
    pub fn total_spendable(&self) -> U256 {
        self.accounts
            .iter()
            .filter_map(|a| a.spendable())
            .map(|c| field_to_u256(c.value))
            .fold(U256::ZERO, |acc, v| acc + v)
    }
}

// ── persisted note cache ─────────────────────────────────────────────────────
//
// The event logs (deposits / withdrawals / ragequits) that pertain to *this*
// account's notes, plus the block scanned to. Persisted encrypted so a re-open
// only scans `[to_block+1 - REORG_BUFFER, head]` instead of re-scanning from the
// Entrypoint deploy block. `Field`/`U256`/`Address` are stored as fixed byte
// arrays (postcard is not self-describing); the SDK types aren't serde-derived.

/// The symmetric key the persisted note cache is encrypted under, derived from
/// the pool account's master keys via a domain-separated Poseidon hash — cheap
/// (no Argon2) and identity-bound (a different mnemonic yields a different key,
/// so its rows simply won't decrypt). The domain constant `"KAONOTES"` and the
/// leading position make it disjoint from every on-chain secret derivation
/// (`deposit_secrets`/`withdrawal_secrets`/`precommitment`).
pub fn notes_cipher_key(account: &Account) -> Result<[u8; 32], PoolError> {
    let keys = account.master_keys();
    let domain = Field::from(0x4b41_4f4e_4f54_4553u64); // "KAONOTES"
    let k = poseidon(&[domain, keys.nullifier, keys.secret]).map_err(PoolError::from)?;
    Ok(k.to_bytes_be())
}

/// The user-relevant logs + cursor for one pool, safe to persist and re-hydrate.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedNotes {
    pub to_block: u64,
    pub deposits: Vec<StoredDeposit>,
    pub withdrawals: Vec<StoredWithdraw>,
    pub ragequits: Vec<StoredRagequit>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDeposit {
    depositor: [u8; 20],
    commitment: [u8; 32],
    label: [u8; 32],
    value: [u8; 32],
    precommitment: [u8; 32],
    block: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredWithdraw {
    processooor: [u8; 20],
    value: [u8; 32],
    spent_nullifier: [u8; 32],
    new_commitment: [u8; 32],
    block: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredRagequit {
    ragequitter: [u8; 20],
    commitment: [u8; 32],
    label: [u8; 32],
    value: [u8; 32],
    block: u64,
}

impl From<&DepositLog> for StoredDeposit {
    fn from(d: &DepositLog) -> Self {
        Self {
            depositor: d.depositor.into_array(),
            commitment: d.commitment.to_bytes_be(),
            label: d.label.to_bytes_be(),
            value: d.value.to_be_bytes::<32>(),
            precommitment: d.precommitment.to_bytes_be(),
            block: d.block,
        }
    }
}
impl From<&StoredDeposit> for DepositLog {
    fn from(s: &StoredDeposit) -> Self {
        DepositLog {
            depositor: Address::from(s.depositor),
            commitment: Field::from_bytes_be(&s.commitment),
            label: Field::from_bytes_be(&s.label),
            value: U256::from_be_bytes(s.value),
            precommitment: Field::from_bytes_be(&s.precommitment),
            block: s.block,
        }
    }
}
impl From<&WithdrawLog> for StoredWithdraw {
    fn from(w: &WithdrawLog) -> Self {
        Self {
            processooor: w.processooor.into_array(),
            value: w.value.to_be_bytes::<32>(),
            spent_nullifier: w.spent_nullifier.to_bytes_be(),
            new_commitment: w.new_commitment.to_bytes_be(),
            block: w.block,
        }
    }
}
impl From<&StoredWithdraw> for WithdrawLog {
    fn from(s: &StoredWithdraw) -> Self {
        WithdrawLog {
            processooor: Address::from(s.processooor),
            value: U256::from_be_bytes(s.value),
            spent_nullifier: Field::from_bytes_be(&s.spent_nullifier),
            new_commitment: Field::from_bytes_be(&s.new_commitment),
            block: s.block,
        }
    }
}
impl From<&RagequitLog> for StoredRagequit {
    fn from(r: &RagequitLog) -> Self {
        Self {
            ragequitter: r.ragequitter.into_array(),
            commitment: r.commitment.to_bytes_be(),
            label: r.label.to_bytes_be(),
            value: r.value.to_be_bytes::<32>(),
            block: r.block,
        }
    }
}
impl From<&StoredRagequit> for RagequitLog {
    fn from(s: &StoredRagequit) -> Self {
        RagequitLog {
            ragequitter: Address::from(s.ragequitter),
            commitment: Field::from_bytes_be(&s.commitment),
            label: Field::from_bytes_be(&s.label),
            value: U256::from_be_bytes(s.value),
            block: s.block,
        }
    }
}

impl CachedNotes {
    /// Re-hydrate the cached logs into a `PoolLogs` (leaves omitted — the tree
    /// comes from the API, not from cached leaves).
    fn to_pool_logs(&self) -> PoolLogs {
        PoolLogs {
            leaves: Vec::new(),
            deposits: self.deposits.iter().map(DepositLog::from).collect(),
            withdrawals: self.withdrawals.iter().map(WithdrawLog::from).collect(),
            ragequits: self.ragequits.iter().map(RagequitLog::from).collect(),
            to_block: self.to_block,
        }
    }
}

/// Merge a freshly-scanned window into cached logs, deduping by the event's
/// natural key. The scanned side wins on overlap (the `REORG_BUFFER` re-scan is
/// authoritative for recent blocks), so a shallow reorg is corrected, not
/// duplicated. Leaves are dropped (tree comes from the API).
fn merge_dedup(scanned: PoolLogs, cached: &PoolLogs) -> PoolLogs {
    let mut dep_seen: HashSet<[u8; 32]> = scanned
        .deposits
        .iter()
        .map(|d| d.precommitment.to_bytes_be())
        .collect();
    let mut deposits = scanned.deposits;
    for d in &cached.deposits {
        if dep_seen.insert(d.precommitment.to_bytes_be()) {
            deposits.push(*d);
        }
    }

    let mut wd_seen: HashSet<[u8; 32]> = scanned
        .withdrawals
        .iter()
        .map(|w| w.spent_nullifier.to_bytes_be())
        .collect();
    let mut withdrawals = scanned.withdrawals;
    for w in &cached.withdrawals {
        if wd_seen.insert(w.spent_nullifier.to_bytes_be()) {
            withdrawals.push(*w);
        }
    }

    let mut rq_seen: HashSet<[u8; 32]> = scanned
        .ragequits
        .iter()
        .map(|r| r.label.to_bytes_be())
        .collect();
    let mut ragequits = scanned.ragequits;
    for r in &cached.ragequits {
        if rq_seen.insert(r.label.to_bytes_be()) {
            ragequits.push(*r);
        }
    }

    PoolLogs {
        leaves: Vec::new(),
        deposits,
        withdrawals,
        ragequits,
        to_block: scanned.to_block,
    }
}

/// Distil the merged logs down to only what pertains to `accounts` — one deposit
/// per account, the withdrawals that spent any of their notes, and their
/// ragequits — so the persisted cache stays small and reveals only this
/// account's activity.
fn user_relevant(
    merged: &PoolLogs,
    accounts: &[PoolAccount],
    account: &Account,
    scope: Field,
    to_block: u64,
) -> Result<CachedNotes, PoolError> {
    let mut precommitments: HashSet<[u8; 32]> = HashSet::new();
    let mut nullifiers: HashSet<[u8; 32]> = HashSet::new();
    let mut labels: HashSet<[u8; 32]> = HashSet::new();
    for a in accounts {
        precommitments.insert(
            account
                .deposit_precommitment(scope, a.deposit_index)?
                .to_bytes_be(),
        );
        labels.insert(a.label.to_bytes_be());
        nullifiers.insert(a.deposit.nullifier_hash()?.to_bytes_be());
        for c in &a.children {
            nullifiers.insert(c.nullifier_hash()?.to_bytes_be());
        }
    }

    let deposits = merged
        .deposits
        .iter()
        .filter(|d| precommitments.contains(&d.precommitment.to_bytes_be()))
        .map(StoredDeposit::from)
        .collect();
    let withdrawals = merged
        .withdrawals
        .iter()
        .filter(|w| nullifiers.contains(&w.spent_nullifier.to_bytes_be()))
        .map(StoredWithdraw::from)
        .collect();
    let ragequits = merged
        .ragequits
        .iter()
        .filter(|r| labels.contains(&r.label.to_bytes_be()))
        .map(StoredRagequit::from)
        .collect();

    Ok(CachedNotes {
        to_block,
        deposits,
        withdrawals,
        ragequits,
    })
}

/// Scan a pool's events from `from_block` to chain head, in `SCAN_CHUNK`-block
/// windows, retrying transient `get_logs` failures per window. The SDK's own
/// `scan_pool` aborts the entire scan on the first chunk error, so we drive the
/// chunking here (it accepts an explicit `to_block`) and merge the windows —
/// one flaky 502 from the upstream RPC no longer poisons the whole sync.
pub async fn scan(
    provider: &RootProvider<Ethereum>,
    info: &PoolInfo,
    from_block: u64,
) -> Result<PoolLogs, PoolError> {
    let head = provider
        .get_block_number()
        .await
        .map_err(|e| PoolError::Chain(format!("get_block_number: {e}")))?;
    let syncer = Syncer::new(info.pool, info.entrypoint);

    let mut acc = PoolLogs {
        to_block: head,
        ..Default::default()
    };
    let mut start = from_block;
    while start <= head {
        let end = (start + SCAN_CHUNK - 1).min(head);
        let chunk = scan_window(&syncer, provider, start, end).await?;
        acc.leaves.extend(chunk.leaves);
        acc.deposits.extend(chunk.deposits);
        acc.withdrawals.extend(chunk.withdrawals);
        acc.ragequits.extend(chunk.ragequits);
        start = end + 1;
    }
    Ok(acc)
}

/// Scan a single `[start, end]` window via the SDK, retrying transient RPC
/// failures with exponential backoff before surfacing the error.
async fn scan_window(
    syncer: &Syncer,
    provider: &RootProvider<Ethereum>,
    start: u64,
    end: u64,
) -> Result<PoolLogs, PoolError> {
    let mut attempt = 0u32;
    loop {
        match syncer.scan_pool(provider, start, Some(end)).await {
            Ok(logs) => return Ok(logs),
            Err(e) if attempt < CHUNK_MAX_RETRIES => {
                let delay = CHUNK_RETRY_BASE_MS << attempt;
                // Transient (RPC timeouts / rate limits) and self-healing on the
                // next attempt — keep it at debug so a working sync isn't noisy.
                // The exhausted-retries case below is the one that warrants WARN.
                tracing::debug!(
                    start,
                    end,
                    attempt = attempt + 1,
                    error = %e,
                    "privacy pools: get_logs window failed — retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                attempt += 1;
            }
            Err(e) => {
                tracing::warn!(
                    start,
                    end,
                    attempts = attempt + 1,
                    error = %e,
                    "privacy pools: get_logs window failed after retries"
                );
                return Err(PoolError::from(e));
            }
        }
    }
}

/// Incrementally sync one pool's notes for `account`. With a `cached` snapshot
/// we only scan `[to_block+1 - REORG_BUFFER, head]` and merge; without one (fresh
/// install / mnemonic-only restore) we scan from the Entrypoint deploy block.
/// Returns the recovered [`PoolState`] plus the refreshed [`CachedNotes`] to
/// persist. No state tree is built here — display doesn't need it (see
/// [`verified_state_tree`] for the withdrawal path).
pub async fn sync_state(
    provider: &RootProvider<Ethereum>,
    info: &PoolInfo,
    account: &Account,
    cached: Option<&CachedNotes>,
) -> Result<(PoolState, CachedNotes), PoolError> {
    let deploy = super::scan_from_block(info.chain);
    let (from, cached_logs) = match cached {
        Some(c) => (
            (c.to_block + 1).saturating_sub(REORG_BUFFER).max(deploy),
            c.to_pool_logs(),
        ),
        None => (deploy, PoolLogs::default()),
    };

    let scanned = scan(provider, info, from).await?;
    let head = scanned.to_block;
    let merged = merge_dedup(scanned, &cached_logs);
    let accounts = recover_accounts(account, info.scope, &merged, GAP_LIMIT)?;
    let next = user_relevant(&merged, &accounts, account, info.scope, head)?;

    // Every recovered account has exactly one on-chain deposit; if the filtered
    // set doesn't cover them all, the cache we started from was inconsistent
    // (corruption / a bug). Self-heal with one full re-scan from the deploy
    // block — never trust a partial cache for a financial balance.
    if next.deposits.len() != accounts.len() {
        if cached.is_some() {
            tracing::warn!(
                pool = %info.pool,
                accounts = accounts.len(),
                cached_deposits = next.deposits.len(),
                "privacy pools: note cache inconsistent — re-scanning from deploy block"
            );
            return Box::pin(sync_state(provider, info, account, None)).await;
        }
        return Err(PoolError::Chain(
            "note recovery inconsistent with the scanned deposits".into(),
        ));
    }

    Ok((
        PoolState {
            accounts,
            to_block: head,
        },
        next,
    ))
}

/// Build the pool's state tree from API-supplied `state_leaves` and prove it
/// current by matching its root against the on-chain `currentRoot()` via Helios
/// — **fail closed** if it doesn't verify (stale API / forged leaves), letting
/// the withdrawal path fall back to a fresh scan. This is what removes the
/// `LeafInserted` rebuild scan from the withdrawal.
pub async fn verified_state_tree(
    net: &dyn BalanceFetcher,
    info: &PoolInfo,
    state_leaves: &[Field],
) -> Result<LeanImt, PoolError> {
    let tree = LeanImt::from_leaves(state_leaves).map_err(PoolError::from)?;
    let root = tree
        .root()
        .ok_or_else(|| PoolError::Chain("empty state tree from API".into()))?;
    if !verify_state_root(net, info.chain, info.pool, root).await? {
        return Err(PoolError::Chain(
            "API state tree didn't match the on-chain root".into(),
        ));
    }
    Ok(tree)
}

/// Verified check: is `root` the pool's current on-chain state root? Uses Kao's
/// Helios-backed `call()` so a forged log set can't produce a matching root.
///
/// Only compares against `currentRoot()` (not the full 64-slot ring), so a
/// deposit landing between our scan and this read yields `false` — a
/// conservative "not verified", never a false positive.
pub async fn verify_state_root(
    net: &dyn BalanceFetcher,
    chain: Chain,
    pool: alloy::primitives::Address,
    root: Field,
) -> Result<bool, PoolError> {
    let data = IPrivacyPool::currentRootCall {}.abi_encode();
    let read = net
        .call(pool, data.into(), chain)
        .await
        .map_err(PoolError::Chain)?;
    if !read.verified {
        return Ok(false);
    }
    let onchain = IPrivacyPool::currentRootCall::abi_decode_returns(&read.value)
        .map_err(|e| PoolError::Chain(format!("decode currentRoot: {e}")))?;
    Ok(onchain == field_to_u256(root))
}

/// The current ASP root the Entrypoint enforces, read via Helios — a withdrawal
/// proof's `asp_root` must equal this (or the on-chain relay/withdraw reverts).
/// Returns `(root, verified)`.
pub async fn verified_asp_root(
    net: &dyn BalanceFetcher,
    info: &PoolInfo,
) -> Result<(Field, bool), PoolError> {
    let data = IEntrypoint::latestRootCall {}.abi_encode();
    let read = net
        .call(info.entrypoint, data.into(), info.chain)
        .await
        .map_err(PoolError::Chain)?;
    let root = IEntrypoint::latestRootCall::abi_decode_returns(&read.value)
        .map_err(|e| PoolError::Chain(format!("decode latestRoot: {e}")))?;
    Ok((u256_to_field(root), read.verified))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The SDK's own vector — account derivation is byte-compatible.
    const MNEMONIC: &str = "test test test test test test test test test test test junk";
    const OTHER: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";

    fn dep(precommitment: Field, value: u64, label: Field, block: u64) -> DepositLog {
        DepositLog {
            depositor: Address::ZERO,
            commitment: Field::from(0u64),
            label,
            value: U256::from(value),
            precommitment,
            block,
        }
    }

    fn logs(deposits: Vec<DepositLog>, to_block: u64) -> PoolLogs {
        PoolLogs {
            leaves: Vec::new(),
            deposits,
            withdrawals: Vec::new(),
            ragequits: Vec::new(),
            to_block,
        }
    }

    #[test]
    fn stored_deposit_roundtrips_through_conversion() {
        let d = dep(Field::from(7u64), 1_000, Field::from(9u64), 42);
        let back = DepositLog::from(&StoredDeposit::from(&d));
        assert_eq!(back.precommitment, d.precommitment);
        assert_eq!(back.value, d.value);
        assert_eq!(back.label, d.label);
        assert_eq!(back.block, d.block);
        assert_eq!(back.depositor, d.depositor);
    }

    #[test]
    fn cached_notes_postcard_roundtrips() {
        let notes = CachedNotes {
            to_block: 999,
            deposits: vec![StoredDeposit::from(&dep(
                Field::from(3u64),
                55,
                Field::from(4u64),
                12,
            ))],
            withdrawals: Vec::new(),
            ragequits: Vec::new(),
        };
        let bytes = postcard::to_stdvec(&notes).unwrap();
        let back: CachedNotes = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, notes);
    }

    #[test]
    fn notes_cipher_key_is_deterministic_and_identity_bound() {
        let a = Account::from_mnemonic(MNEMONIC).unwrap();
        let b = Account::from_mnemonic(OTHER).unwrap();
        let ka = notes_cipher_key(&a).unwrap();
        assert_eq!(ka, notes_cipher_key(&a).unwrap(), "deterministic");
        assert_ne!(ka, notes_cipher_key(&b).unwrap(), "identity-bound");
    }

    #[test]
    fn merge_dedup_prefers_scanned_on_overlap() {
        // Same precommitment in both, different value → the scanned (recent,
        // reorg-authoritative) copy must win; the other unique cached row stays.
        let scanned = logs(
            vec![dep(Field::from(1u64), 999, Field::from(0u64), 200)],
            200,
        );
        let cached = logs(
            vec![
                dep(Field::from(1u64), 10, Field::from(0u64), 100),
                dep(Field::from(2u64), 20, Field::from(0u64), 90),
            ],
            100,
        );
        let m = merge_dedup(scanned, &cached);
        assert_eq!(m.deposits.len(), 2);
        let values: Vec<u64> = m.deposits.iter().map(|d| d.value.to::<u64>()).collect();
        assert!(values.contains(&999), "scanned copy kept");
        assert!(!values.contains(&10), "cached duplicate dropped");
        assert!(values.contains(&20), "unique cached kept");
        assert_eq!(m.to_block, 200);
    }

    // The core correctness property: recovering from the full log set must equal
    // recovering from {persisted user cache} ∪ {tail-only re-scan}. Here the
    // user's deposits all predate the tail floor, so they come *only* from the
    // cache — proving the incremental path doesn't lose old notes.
    #[test]
    fn incremental_recovery_matches_full_scan() {
        let account = Account::from_mnemonic(MNEMONIC).unwrap();
        let scope = Field::from(42u64);

        let mut deposits = Vec::new();
        for i in 0..3u64 {
            let pre = account.deposit_precommitment(scope, i).unwrap();
            deposits.push(dep(
                pre,
                1_000 * (i + 1),
                Field::from(100 + i),
                (i + 1) * 10,
            ));
        }
        // A non-user deposit landing in the tail window — noise recovery ignores.
        deposits.push(dep(Field::from(0xdead_u64), 5, Field::from(1u64), 120));
        let full = logs(deposits, 300);

        let accounts_full = recover_accounts(&account, scope, &full, GAP_LIMIT).unwrap();
        assert_eq!(accounts_full.len(), 3);

        // Persist the user cache at cursor 150, then tail-only re-scan (>86).
        let cached = user_relevant(&full, &accounts_full, &account, scope, 150).unwrap();
        assert_eq!(cached.deposits.len(), accounts_full.len());
        let floor = 150u64 - REORG_BUFFER;
        let tail = logs(
            full.deposits
                .iter()
                .filter(|d| d.block > floor)
                .copied()
                .collect(),
            300,
        );
        // Sanity: none of the user's deposits are in the tail — they must be
        // supplied by the cache alone.
        assert_eq!(tail.deposits.len(), 1);

        let merged = merge_dedup(tail, &cached.to_pool_logs());
        let accounts_inc = recover_accounts(&account, scope, &merged, GAP_LIMIT).unwrap();

        let idx_full: Vec<u64> = accounts_full.iter().map(|a| a.deposit_index).collect();
        let idx_inc: Vec<u64> = accounts_inc.iter().map(|a| a.deposit_index).collect();
        assert_eq!(idx_full, idx_inc);
        let val_full: Vec<Field> = accounts_full.iter().map(|a| a.deposit.value).collect();
        let val_inc: Vec<Field> = accounts_inc.iter().map(|a| a.deposit.value).collect();
        assert_eq!(val_full, val_inc);
    }

    // The higher-risk path: an account with a partial-withdrawal *change chain*.
    // Both the deposit and its withdrawal predate the tail, so the whole chain
    // must be rebuilt from the persisted cache alone — if `user_relevant` failed
    // to capture the change withdrawal, the spendable balance would be wrong.
    #[test]
    fn incremental_recovery_rebuilds_change_chain_from_cache() {
        use privacy_pools::{Commitment, nullifier_hash};

        let account = Account::from_mnemonic(MNEMONIC).unwrap();
        let scope = Field::from(42u64);
        let label = Field::from(777u64); // as it would arrive in the Deposited event

        // Deposit note (value 1000) at index 0.
        let (n0, s0) = account.deposit_secrets(scope, 0).unwrap();
        let pre0 = account.deposit_precommitment(scope, 0).unwrap();
        let deposit_note = Commitment::new(Field::from(1000u64), label, n0, s0);
        let d = DepositLog {
            depositor: Address::ZERO,
            commitment: deposit_note.hash().unwrap(),
            label,
            value: U256::from(1000u64),
            precommitment: pre0,
            block: 10,
        };

        // Partial withdrawal of 400 → change note (value 600) at child index 0.
        let (n1, s1) = account.withdrawal_secrets(label, 0).unwrap();
        let change = Commitment::new(Field::from(600u64), label, n1, s1);
        let w = WithdrawLog {
            processooor: Address::ZERO,
            value: U256::from(400u64),
            spent_nullifier: nullifier_hash(n0).unwrap(),
            new_commitment: change.hash().unwrap(),
            block: 15,
        };

        let full = PoolLogs {
            leaves: Vec::new(),
            deposits: vec![d],
            withdrawals: vec![w],
            ragequits: Vec::new(),
            to_block: 300,
        };
        let accounts_full = recover_accounts(&account, scope, &full, GAP_LIMIT).unwrap();
        assert_eq!(accounts_full.len(), 1);
        assert_eq!(accounts_full[0].children.len(), 1);
        let spendable_full = accounts_full[0].spendable().map(|c| c.value);

        // Persist at cursor 100; both events predate the tail floor (36), so the
        // tail is empty and recovery must lean entirely on the cache.
        let cached = user_relevant(&full, &accounts_full, &account, scope, 100).unwrap();
        assert_eq!(cached.deposits.len(), 1);
        assert_eq!(
            cached.withdrawals.len(),
            1,
            "change withdrawal must be cached"
        );
        let tail = logs(Vec::new(), 300);
        let merged = merge_dedup(tail, &cached.to_pool_logs());
        let accounts_inc = recover_accounts(&account, scope, &merged, GAP_LIMIT).unwrap();

        assert_eq!(accounts_inc.len(), 1);
        assert_eq!(accounts_inc[0].children.len(), 1);
        assert_eq!(accounts_inc[0].spendable().map(|c| c.value), spendable_full);
        assert_eq!(
            accounts_inc[0].spendable().map(|c| c.value),
            Some(Field::from(600u64))
        );
    }
}
