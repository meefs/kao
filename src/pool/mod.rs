//! Privacy Pools (0xbow) engine — the pure logic + network layer behind the
//! Privacy Pools app, mirroring the `src/cow` / `src/names` split: this module
//! holds no UI state, exposes plain functions + serde DTOs, and the dashboard
//! coordinator owns the async tasks and routes every signature through the
//! clear-sign review gate.
//!
//! The heavy lifting (account derivation, Groth16 proving, calldata) lives in
//! the `privacy-pools` crate; this module adapts it to Kao's provider
//! (`net::BalanceFetcher`), keyring, proxy, and verified-read trust model.
//!
//! Privacy Pools runs on **Ethereum L1 and Optimism L2** (the two 0xbow
//! deployments Kao's Helios stack can verify). The pool list per chain is
//! discovered **from the Entrypoint contract** (`PoolRegistered` events); only
//! the two Entrypoint addresses + their deploy blocks are hardcoded.
//!
//! Layering:
//! - [`account`]  — the dedicated pool mnemonic → `privacy_pools::Account`.
//! - [`prover`]   — process-wide lazily-built Groth16 provers (17 MB zkey).
//! - [`discover`] — enumerate a chain's pools + metadata from the Entrypoint.
//! - [`sync`]     — chain scan + note recovery + root check against chain state.
//! - [`asp`]      — the opt-in 0xbow Association-Set feed (compliance proof).
//! - [`relayer`]  — the 0xbow relayer HTTP API (fee quote + submission).
//! - [`flow`]     — deposit / withdraw / ragequit orchestration.

// The engine + its submodules are consumed by the Apps-tab wiring (apps.rs +
// dashboard coordinator), which lands next. Until that call site exists the
// public surface reads as dead code; this staging allow is removed when it does.
#![allow(dead_code)]

use alloy::primitives::{Address, U256, address};

use privacy_pools::Field;

pub mod account;
pub mod asp;
pub mod discover;
pub mod flow;
pub mod prover;
pub mod relayer;
pub mod sync;

use crate::chain::Chain;

/// The native-asset (ETH) sentinel the Entrypoint uses — `0xEeee…EEeE`, NOT
/// `address(0)`. Re-exported from the SDK so the two never drift.
pub const NATIVE_ASSET: Address = privacy_pools::NATIVE_ASSET;

/// The 0xbow Entrypoint for a chain — the deposit/relay hub every pool shares —
/// or `None` if Privacy Pools isn't deployed there. Mainnet has its own
/// deployment; the L2s share one CREATE2 address.
pub fn entrypoint(chain: Chain) -> Option<Address> {
    match chain {
        Chain::Mainnet => Some(address!("6818809EefCe719E480a7526D76bD3e561526b46")),
        Chain::Optimism => Some(address!("44192215FEd782896BE2CE24E0Bfbf0BF825d15E")),
        Chain::Base => None,
    }
}

/// The Entrypoint's deploy block — the tight floor for every event scan on the
/// chain (no pool predates its Entrypoint). Scanning from here is both correct
/// and fast; a lower floor would only waste requests.
pub fn scan_from_block(chain: Chain) -> u64 {
    match chain {
        Chain::Mainnet => 22_153_713,   // 2025-03-29
        Chain::Optimism => 144_288_142, // 2025-11-26
        Chain::Base => 0,
    }
}

/// Privacy Pools is available on the chains with a known 0xbow deployment.
pub fn supported(chain: Chain) -> bool {
    entrypoint(chain).is_some()
}

/// A discovered pool: its on-chain identity (asset, pool, scope, entrypoint) +
/// token metadata + the Entrypoint's fee bounds. `scope` binds every
/// deposit/withdrawal to this specific pool + chain + asset.
#[derive(Debug, Clone)]
pub struct PoolInfo {
    pub chain: Chain,
    pub entrypoint: Address,
    pub asset: Address,
    pub pool: Address,
    pub scope: Field,
    pub symbol: String,
    pub decimals: u8,
    pub is_native: bool,
    /// Minimum deposit the Entrypoint accepts for this asset.
    pub min_deposit: U256,
    /// Entrypoint vetting fee (basis points) skimmed on deposit.
    pub vetting_fee_bps: U256,
    /// Maximum relay fee (basis points) a relayer may charge on withdrawal.
    pub max_relay_fee_bps: U256,
    /// The pool's current commitment count (`currentTreeSize`) — the anonymity
    /// set you blend into. Read at discovery time.
    pub anonymity_set: u64,
    /// Whether the on-chain identity reads (assetConfig / SCOPE / size) went
    /// through Helios light-client verification (vs a raw-RPC fallback).
    pub verified: bool,
}

impl PoolInfo {
    /// The scope as the decimal string the relayer `/request` and proof inputs use.
    pub fn scope_decimal(&self) -> String {
        self.scope.to_decimal()
    }
}

/// Errors surfaced by the pool engine. `String` payloads are already sanitized
/// for display where they may carry RPC/relayer/ASP URLs (see the redaction in
/// [`relayer`] / [`asp`]).
#[derive(Debug, Clone)]
pub enum PoolError {
    /// No RPC provider available for the chain.
    NoProvider,
    /// Privacy Pools isn't deployed on the selected chain.
    Unsupported,
    /// On-chain read/scan failure.
    Chain(String),
    /// ASP feed error (fetch, parse, or disabled).
    Asp(String),
    /// Relayer API error.
    Relayer(String),
    /// Proving / verification failure.
    Proof(String),
    /// Bad input (amount, address, insufficient balance, etc.).
    Input(String),
    /// The user hasn't enabled the ASP feed but the withdrawal path needs it.
    AspDisabled,
}

impl std::fmt::Display for PoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolError::NoProvider => write!(f, "no RPC configured for this chain"),
            PoolError::Unsupported => write!(f, "Privacy Pools isn't available on this chain"),
            PoolError::Chain(e) => write!(f, "chain error: {e}"),
            PoolError::Asp(e) => write!(f, "association-set error: {e}"),
            PoolError::Relayer(e) => write!(f, "relayer error: {e}"),
            PoolError::Proof(e) => write!(f, "proof error: {e}"),
            PoolError::Input(e) => write!(f, "{e}"),
            PoolError::AspDisabled => write!(
                f,
                "the 0xbow association-set feed is off — enable it in Settings to withdraw privately"
            ),
        }
    }
}

impl std::error::Error for PoolError {}

impl From<privacy_pools::Error> for PoolError {
    fn from(e: privacy_pools::Error) -> Self {
        match e {
            privacy_pools::Error::Chain(m) => PoolError::Chain(m),
            privacy_pools::Error::Input(m) => PoolError::Input(m),
            privacy_pools::Error::VerificationFailed => {
                PoolError::Proof("proof verification failed".into())
            }
            privacy_pools::Error::Prove(m) | privacy_pools::Error::Witness(m) => {
                PoolError::Proof(m)
            }
            privacy_pools::Error::Artifact(m) => PoolError::Proof(format!("artifact: {m}")),
            privacy_pools::Error::Io(e) => PoolError::Proof(format!("io: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use privacy_pools::{Account, Commitment};

    // The 0xbow SDK's own test vector — account derivation is byte-compatible.
    const MNEMONIC: &str = "test test test test test test test test test test test junk";

    #[test]
    fn account_derivation_is_deterministic() {
        let a = Account::from_mnemonic(MNEMONIC).unwrap();
        let scope = Field::from(42u64);
        let p0 = a.deposit_precommitment(scope, 0).unwrap();
        assert_eq!(p0, a.deposit_precommitment(scope, 0).unwrap());
        assert_ne!(p0, a.deposit_precommitment(scope, 1).unwrap());
    }

    #[test]
    fn chains_map_to_entrypoints() {
        assert!(supported(Chain::Mainnet));
        assert!(supported(Chain::Optimism));
        assert!(!supported(Chain::Base));
        assert!(entrypoint(Chain::Mainnet).is_some());
    }

    #[test]
    fn ragequit_proof_generates_and_verifies_in_binary() {
        // Exercises circom-witnesscalc + arkworks + the bundled commitment.zkey
        // end-to-end inside Kao's binary — proof-of-life for the whole proving
        // stack, not just the SDK's own harness.
        let note = Commitment::new(
            Field::from(1_000_000u64),
            Field::from(7u64),
            Field::from(11u64),
            Field::from(22u64),
        );
        let inputs = flow::ragequit_plan(&note);
        let proof = prover::prove_ragequit(&inputs).expect("ragequit proof");
        // Commitment circuit: [commitment, nullifierHash, value, label].
        assert_eq!(proof.public_signals_decimal().len(), 4);
    }
}
