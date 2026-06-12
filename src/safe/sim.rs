//! Local revm preflight for Safe (multisig) flows.
//!
//! Two simulation shapes, both advisory (revert softens the action
//! buttons to "… anyway ⚠", never blocks — same philosophy as the EOA
//! send preflight in `wallet::sim`):
//!
//! - [`simulate_safe_inner`] — sign-time: simulate the *inner* call the
//!   Safe would make (`from = Safe address`, to/value/data from the
//!   SafeTx). Skipped for `operation = 1` (delegatecall): a plain CALL
//!   sim would run the target's code against the *target's* storage,
//!   which misrepresents what a delegatecall does under the Safe's
//!   identity — better no answer than a wrong one.
//! - [`simulate_safe_execution`] — execute-time: simulate the full
//!   `execTransaction` calldata (`from = executor EOA`, `to = Safe`).
//!   Catches GS-code failures (bad signatures, stale nonce). Faithful
//!   even for delegatecall, because revm runs the real Safe code which
//!   performs the delegatecall itself — so this one is NOT gated on
//!   `operation`.
//!
//! Kept separate from `safe::tx` so the signing/broadcast layer stays
//! free of the `wallet::sim` (revm) dependency.

use std::sync::Arc;

use alloy::primitives::{Address, Bytes, U256};

use crate::chain::Chain;
use crate::net::BalanceFetcher;
use crate::wallet::sim::{CallSpec, SimError, SimOutcome, SimulationResult, simulate_call};

use super::SafeTx;
use super::tx::encode_exec_transaction;

/// The `CallSpec` for the *inner* call of `tx` as the Safe would make
/// it. Pure — split out so tests don't need a runtime. Nonce is 0 per
/// the contract-caller convention documented on [`CallSpec`].
fn inner_call_spec(safe: Address, tx: &SafeTx, chain: Chain) -> CallSpec {
    CallSpec {
        chain,
        from: safe,
        to: tx.to,
        value: tx.value,
        input: tx.data.clone(),
        nonce: 0,
    }
}

/// The `CallSpec` for the full `execTransaction(...)` outer call from
/// `executor`. The envelope's value is always zero — the Safe pays the
/// inner `tx.value` from its own balance, not from `msg.value`.
fn exec_call_spec(
    executor: Address,
    safe: Address,
    tx: &SafeTx,
    signatures: Bytes,
    chain: Chain,
) -> CallSpec {
    CallSpec {
        chain,
        from: executor,
        to: safe,
        value: U256::ZERO,
        input: encode_exec_transaction(tx, signatures),
        nonce: 0,
    }
}

/// Sign-time preflight of the inner call. `operation != 0`
/// (delegatecall) returns `unavailable()` — see module docs.
///
/// An underfunded Safe (`tx.value` above the Safe's balance) bounces
/// off revm's upfront balance check as `SimError::Evm("LackOfFund…")`
/// before the EVM runs. That's a *real* predicted failure, not a
/// simulator gap — surface it as a Revert outcome instead of letting
/// the caller degrade it to "unavailable".
pub async fn simulate_safe_inner(
    network: Arc<dyn BalanceFetcher>,
    safe: Address,
    tx: &SafeTx,
    chain: Chain,
) -> Result<SimulationResult, SimError> {
    if tx.operation != 0 {
        return Ok(SimulationResult::unavailable());
    }
    let spec = inner_call_spec(safe, tx, chain);
    match simulate_call(network, &spec).await {
        Err(SimError::Evm(msg)) if msg.contains("LackOfFund") => Ok(SimulationResult {
            outcome: SimOutcome::Revert {
                reason: "Safe balance below transfer value".to_string(),
                raw: Bytes::new(),
            },
            gas_used: 0,
            transfers: Vec::new(),
            verified: false,
            base_fee_per_gas: 0,
        }),
        other => other,
    }
}

/// Execute-time preflight of the full `execTransaction` calldata with
/// the already-gathered `signatures` blob. Runs the real Safe code, so
/// signature-set and nonce staleness failures (GS0xx reverts) surface
/// here. Not gated on `operation` — revm performs the delegatecall
/// faithfully inside the Safe's own execution.
pub async fn simulate_safe_execution(
    network: Arc<dyn BalanceFetcher>,
    executor: Address,
    safe: Address,
    tx: &SafeTx,
    signatures: Bytes,
    chain: Chain,
) -> Result<SimulationResult, SimError> {
    let spec = exec_call_spec(executor, safe, tx, signatures, chain);
    simulate_call(network, &spec).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use alloy::sol_types::SolCall;

    use crate::net::MockFetcher;
    use crate::safe::execTransactionCall;

    fn safe_addr() -> Address {
        address!("0x1111111111111111111111111111111111111111")
    }

    fn native_send_tx(operation: u8, value: U256) -> SafeTx {
        SafeTx {
            to: address!("0x000000000000000000000000000000000000dEaD"),
            value,
            data: Bytes::new(),
            operation,
            safeTxGas: U256::ZERO,
            baseGas: U256::ZERO,
            gasPrice: U256::ZERO,
            gasToken: Address::ZERO,
            refundReceiver: Address::ZERO,
            nonce: U256::ZERO,
        }
    }

    #[tokio::test]
    async fn simulate_safe_inner_skips_delegatecall() {
        // A plain-CALL sim of a delegatecall target would execute the
        // wrong semantics; the inner sim must decline rather than lie.
        let network: Arc<dyn BalanceFetcher> = Arc::new(MockFetcher::new());
        let tx = native_send_tx(/* operation */ 1, U256::ZERO);
        let result = simulate_safe_inner(network, safe_addr(), &tx, Chain::Mainnet)
            .await
            .expect("delegatecall skip is not an error");
        assert!(result.is_unavailable());
    }

    /// MockFetcher serves empty code for every address, and the EVM
    /// treats a CALL to a codeless account as a successful no-op (same
    /// quirk pinned by `wallet::sim`'s
    /// `simulate_erc20_transfer_to_codeless_target_is_silent_success`).
    /// Pin it here too so a mock or revm change doesn't silently flip
    /// the Safe inner sim to a misleading Revert.
    #[tokio::test]
    async fn simulate_safe_inner_codeless_safe_is_silent_success() {
        let network: Arc<dyn BalanceFetcher> = Arc::new(MockFetcher::new());
        let tx = native_send_tx(/* operation */ 0, U256::ZERO);
        let result = simulate_safe_inner(network, safe_addr(), &tx, Chain::Mainnet)
            .await
            .expect("sim should not fail");
        assert!(
            matches!(result.outcome, SimOutcome::Success { .. }),
            "expected silent success, got {:?}",
            result.outcome,
        );
        assert!(result.transfers.is_empty());
    }

    /// MockFetcher reports zero balance everywhere, so a nonzero inner
    /// `value` trips revm's upfront balance check. That must surface as
    /// the synthesized underfunded-Safe Revert, not "unavailable".
    #[tokio::test]
    async fn simulate_safe_inner_underfunded_value_is_revert() {
        let network: Arc<dyn BalanceFetcher> = Arc::new(MockFetcher::new());
        let tx = native_send_tx(/* operation */ 0, U256::from(1_000u64));
        let result = simulate_safe_inner(network, safe_addr(), &tx, Chain::Mainnet)
            .await
            .expect("underfunded maps to a Revert outcome, not an error");
        match &result.outcome {
            SimOutcome::Revert { reason, .. } => {
                assert!(reason.contains("Safe balance"), "got {reason}");
            }
            other => panic!("expected Revert, got {other:?}"),
        }
        assert!(result.is_revert());
    }

    #[test]
    fn exec_call_spec_encodes_exec_transaction_with_zero_value() {
        let executor = address!("0x00000000000000000000000000000000000Ec5e0");
        let tx = native_send_tx(/* operation */ 0, U256::from(7u64));
        let sigs = Bytes::from(vec![0x42u8; 65]);
        let spec = exec_call_spec(executor, safe_addr(), &tx, sigs, Chain::Mainnet);
        assert_eq!(spec.from, executor);
        assert_eq!(spec.to, safe_addr());
        // Outer envelope never carries value — the Safe pays the inner
        // transfer from its own balance.
        assert_eq!(spec.value, U256::ZERO);
        assert_eq!(&spec.input[..4], &execTransactionCall::SELECTOR);
        assert_eq!(spec.nonce, 0);
    }

    #[test]
    fn inner_call_spec_mirrors_safe_tx_fields() {
        let tx = native_send_tx(/* operation */ 0, U256::from(9u64));
        let spec = inner_call_spec(safe_addr(), &tx, Chain::Base);
        assert_eq!(spec.from, safe_addr());
        assert_eq!(spec.to, tx.to);
        assert_eq!(spec.value, tx.value);
        assert_eq!(spec.input, tx.data);
        assert_eq!(spec.nonce, 0);
        assert_eq!(spec.chain, Chain::Base);
    }
}
