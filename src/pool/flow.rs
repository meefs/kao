//! Deposit / withdraw / ragequit orchestration — the pure transformations that
//! turn synced state + account keys into signable transactions and proof
//! inputs. Network I/O lives in [`super::sync`] / [`super::asp`] /
//! [`super::relayer`]; proving in [`super::prover`]; the dashboard coordinator
//! sequences the async steps.

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};

use privacy_pools::{
    Account, Commitment, Destination, Field, Groth16Proof, LeanImt, PoolAccount, RelayData,
    WithdrawalPlan, build_withdrawal, erc20_deposit as sdk_erc20_deposit, field_to_u256,
    native_deposit as sdk_native_deposit, ragequit_calldata as sdk_ragequit_calldata,
    ragequit_inputs, withdraw_calldata,
};

use super::relayer::QuoteResponse;
use super::{PoolError, PoolInfo};

sol! {
    /// Minimal ERC-20 approval used before an ERC-20 pool deposit.
    function approve(address spender, uint256 amount) external returns (bool);
}

// ── deposits ──────────────────────────────────────────────────────────────

/// A ready-to-review deposit transaction (the wallet sets from/gas/nonce).
#[derive(Debug, Clone)]
pub struct DepositTx {
    /// The precommitment the deposit registers (persist to recover the note).
    pub precommitment: Field,
    pub to: Address,
    pub value: U256,
    pub calldata: Bytes,
}

/// `approve(Entrypoint, amount)` calldata for an ERC-20 pool deposit — sent to
/// the token contract before [`erc20_deposit`].
pub fn erc20_approve_calldata(info: &PoolInfo, amount: U256) -> Bytes {
    approveCall {
        spender: info.entrypoint,
        amount,
    }
    .abi_encode()
    .into()
}

/// Build a native (ETH) deposit at `index` for `amount` wei.
pub fn native_deposit(
    account: &Account,
    info: &PoolInfo,
    index: u64,
    amount: U256,
) -> Result<DepositTx, PoolError> {
    let (precommitment, calldata) = sdk_native_deposit(account, info.scope, index)?;
    Ok(DepositTx {
        precommitment,
        to: info.entrypoint,
        value: amount,
        calldata,
    })
}

/// Build an ERC-20 deposit at `index` for `value` (needs a prior approve to the
/// Entrypoint). The tx carries no ETH value.
pub fn erc20_deposit(
    account: &Account,
    info: &PoolInfo,
    index: u64,
    value: U256,
) -> Result<DepositTx, PoolError> {
    let (precommitment, calldata) =
        sdk_erc20_deposit(account, info.scope, index, info.asset, value)?;
    Ok(DepositTx {
        precommitment,
        to: info.entrypoint,
        value: U256::ZERO,
        calldata,
    })
}

// ── withdrawals ─────────────────────────────────────────────────────────────

/// A relayed destination decoded from a relayer's signed quote, so the proof's
/// `context` binds to exactly the `Withdrawal.data` the relayer committed to.
///
/// The relayer is untrusted: the recipient and fee it puts in `withdrawalData`
/// are what the withdrawal proof's `context` will cryptographically commit to,
/// and the Entrypoint pays out to exactly that recipient. So a malicious or
/// MITM'd relayer could substitute its own address (or an inflated fee) and the
/// wallet would happily prove over it — silently redirecting the whole note.
/// Guard against that here: the decoded recipient MUST equal the address the
/// user chose (`expected_recipient`), and the relay fee MUST NOT exceed the
/// pool's on-chain `max_relay_fee_bps`. Neither is checked anywhere else on the
/// relayed path (`matches_quote` only proves the plan is self-consistent with
/// the same quote), so this is the sole binding of proof → user intent.
pub fn destination_from_quote(
    info: &PoolInfo,
    quote: &QuoteResponse,
    expected_recipient: Address,
) -> Result<Destination, PoolError> {
    let data = quote.withdrawal_data()?;
    let rd = RelayData::abi_decode(&data)
        .map_err(|e| PoolError::Relayer(format!("decode RelayData: {e}")))?;
    if rd.recipient != expected_recipient {
        return Err(PoolError::Relayer(
            "relayer quote recipient does not match the withdrawal target — refusing to sign"
                .into(),
        ));
    }
    if rd.relayFeeBPS > info.max_relay_fee_bps {
        return Err(PoolError::Relayer(format!(
            "relayer fee {} bps exceeds this pool's maximum of {} bps",
            rd.relayFeeBPS, info.max_relay_fee_bps
        )));
    }
    Ok(Destination::Relayed {
        entrypoint: info.entrypoint,
        recipient: rd.recipient,
        fee_recipient: rd.feeRecipient,
        relay_fee_bps: rd.relayFeeBPS,
    })
}

/// A direct (self-relayed) withdrawal: the submitting EOA is both processooor
/// and recipient. No relay fee, but the caller pays gas and is linkable.
pub fn direct_destination(processooor: Address) -> Destination {
    Destination::Direct { processooor }
}

/// Assemble a [`WithdrawalPlan`] for the pool account's spendable note.
///
/// Builds the note's state-tree membership proof (from the synced leaves) and
/// its ASP membership proof (from the opt-in feed's `asp_leaves`), then delegates
/// to the SDK, which binds `context` to the destination and derives the change
/// note. Errors if the deposit isn't yet in the Association Set (still pending
/// 0xbow review) or if `withdrawn_value` exceeds the note.
#[allow(clippy::too_many_arguments)]
pub fn plan_withdrawal(
    account: &Account,
    info: &PoolInfo,
    pool_account: &PoolAccount,
    state_leaves: &[Field],
    state_tree: &LeanImt,
    asp_leaves: &[Field],
    withdrawn_value: U256,
    dest: &Destination,
) -> Result<WithdrawalPlan, PoolError> {
    let note = pool_account
        .spendable()
        .ok_or_else(|| PoolError::Input("this pool account has nothing to withdraw".into()))?;
    if withdrawn_value > field_to_u256(note.value) {
        return Err(PoolError::Input(
            "amount exceeds the pool account balance".into(),
        ));
    }

    let commitment = note.hash()?;
    let state_pos = state_leaves
        .iter()
        .position(|l| *l == commitment)
        .ok_or_else(|| {
            PoolError::Chain("note commitment not found in the pool state tree".into())
        })?;
    let state_proof = state_tree.generate_proof(state_pos)?;

    let asp_pos = asp_leaves
        .iter()
        .position(|l| *l == note.label)
        .ok_or_else(|| {
            PoolError::Input(
                "this deposit isn't in the approved set yet — 0xbow review can take up to 7 days"
                    .into(),
            )
        })?;
    let asp_tree = LeanImt::from_leaves(asp_leaves)?;
    let asp_proof = asp_tree.generate_proof(asp_pos)?;

    let child_index = pool_account.children.len() as u64;
    let plan = build_withdrawal(
        account,
        info.scope,
        note,
        child_index,
        withdrawn_value,
        &state_proof,
        &asp_proof,
        dest,
    )?;
    Ok(plan)
}

/// Guard: the plan's `Withdrawal.data` must equal the bytes the relayer signed
/// in its quote, or the relayer's fee commitment won't match the proof context.
pub fn matches_quote(plan: &WithdrawalPlan, quote: &QuoteResponse) -> Result<(), PoolError> {
    if plan.withdrawal.data.as_ref() != quote.withdrawal_data()?.as_ref() {
        return Err(PoolError::Relayer(
            "relayer quote data doesn't match the built withdrawal".into(),
        ));
    }
    Ok(())
}

/// The snarkjs-form proof + decimal public signals a relayer expects.
pub fn snarkjs_proof(proof: &Groth16Proof) -> serde_json::Value {
    proof.to_snarkjs_json()
}

pub fn public_signals(proof: &Groth16Proof) -> Vec<String> {
    proof.public_signals_decimal()
}

/// `Pool.withdraw(...)` calldata for a self-relayed (direct) withdrawal.
pub fn self_withdraw_calldata(
    plan: &WithdrawalPlan,
    proof: &Groth16Proof,
) -> Result<Bytes, PoolError> {
    withdraw_calldata(&plan.withdrawal, proof).map_err(PoolError::from)
}

// ── ragequit (original-depositor exit, no ASP) ───────────────────────────────

/// Commitment-circuit inputs to ragequit a note — prove with the commitment
/// prover, then [`ragequit_calldata`].
pub fn ragequit_plan(note: &Commitment) -> privacy_pools::CommitmentInputs {
    ragequit_inputs(note)
}

/// `Pool.ragequit(...)` calldata from a commitment proof.
pub fn ragequit_calldata(proof: &Groth16Proof) -> Result<Bytes, PoolError> {
    sdk_ragequit_calldata(proof).map_err(PoolError::from)
}
