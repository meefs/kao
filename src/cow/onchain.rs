//! On-chain pieces of a CoW swap: the ERC-20 allowance read + approval to the
//! vault relayer, and a generic "sign and broadcast a contract call" helper
//! shared by the approval and the EthFlow `createOrder` / `invalidateOrder`
//! paths.
//!
//! [`send_contract_call`] is the only genuinely new broadcast code the
//! integration needs; it mirrors [`crate::wallet::tx::sign_and_send`] (fill a
//! `TxEip1559`, route it through `KaoSigner::sign_tx`, broadcast the raw
//! envelope) but for an arbitrary `(to, value, calldata)` rather than a
//! `SendPlan`.

use std::time::Duration;

use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::eip2718::Encodable2718;
use alloy::network::Ethereum;
use alloy::primitives::{Address, Bytes, TxHash, TxKind, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use alloy::sol;
use alloy::sol_types::SolCall;
use tracing::{info, warn};

use crate::chain::Chain;
use crate::wallet::KaoSigner;

use super::VAULT_RELAYER;

sol! {
    function allowance(address owner, address spender) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
}

/// Read the ERC-20 allowance the seller has granted the vault relayer on
/// `token`. A sell order can only settle once this covers the sell amount.
pub async fn read_allowance(
    provider: &RootProvider<Ethereum>,
    token: Address,
    owner: Address,
) -> Result<U256, String> {
    let input = allowanceCall {
        owner,
        spender: VAULT_RELAYER,
    }
    .abi_encode();
    let req = TransactionRequest::default()
        .to(token)
        .input(TransactionInput::new(Bytes::from(input)));
    let out = provider
        .call(req)
        .await
        .map_err(|e| format!("allowance call: {e}"))?;
    allowanceCall::abi_decode_returns(&out).map_err(|e| format!("allowance decode: {e}"))
}

/// `approve(vaultRelayer, amount)` calldata for an ERC-20.
pub fn approve_calldata(amount: U256) -> Bytes {
    Bytes::from(
        approveCall {
            spender: VAULT_RELAYER,
            amount,
        }
        .abi_encode(),
    )
}

/// Sign and broadcast `approve(vaultRelayer, amount)` on `token`. Callers
/// typically pass `U256::MAX` for a one-time unlimited approval so repeat swaps
/// of the same token skip this step.
pub async fn approve_relayer(
    provider: &RootProvider<Ethereum>,
    signer: &KaoSigner,
    chain: Chain,
    token: Address,
    amount: U256,
) -> Result<TxHash, String> {
    send_contract_call(
        provider,
        signer,
        chain,
        token,
        U256::ZERO,
        approve_calldata(amount),
    )
    .await
}

/// Build, sign, and broadcast an arbitrary contract call from the active
/// account. Estimates gas + EIP-1559 fees + the pending nonce, fills a
/// `TxEip1559`, signs it via `KaoSigner::sign_tx` (the one path that works for
/// software and hardware), and broadcasts the raw envelope. Returns the tx
/// hash; it does NOT wait for inclusion — the caller polls the receipt.
pub async fn send_contract_call(
    provider: &RootProvider<Ethereum>,
    signer: &KaoSigner,
    chain: Chain,
    to: Address,
    value: U256,
    calldata: Bytes,
) -> Result<TxHash, String> {
    let from = signer.address();
    let req = TransactionRequest::default()
        .from(from)
        .to(to)
        .value(value)
        .input(TransactionInput::new(calldata.clone()));

    let gas_limit = provider
        .estimate_gas(req)
        .await
        .map_err(|e| format!("estimate_gas: {e}"))?;
    let fees = provider
        .estimate_eip1559_fees()
        .await
        .map_err(|e| format!("estimate_eip1559_fees: {e}"))?;
    let nonce = provider
        .get_transaction_count(from)
        .pending()
        .await
        .map_err(|e| format!("get_transaction_count: {e}"))?;

    // Pre-flight: make sure the account can cover value + worst-case gas before
    // we prompt for a signature. A native-ETH (EthFlow) order sends the amount
    // as `value`, so the user needs amount + fee + gas all in ETH — a common
    // surprise that otherwise surfaces as a raw "insufficient funds" RPC error
    // only after signing.
    let balance = provider
        .get_balance(from)
        .await
        .map_err(|e| format!("get_balance: {e}"))?;
    let max_gas_cost = U256::from(gas_limit).saturating_mul(U256::from(fees.max_fee_per_gas));
    let required = value.saturating_add(max_gas_cost);
    if balance < required {
        let what = if value > U256::ZERO {
            "the swap amount + fee + network gas"
        } else {
            "network gas"
        };
        return Err(format!(
            "not enough ETH for {what}: need ~{} ETH, have {} ETH",
            fmt_eth(required),
            fmt_eth(balance),
        ));
    }

    info!(
        chain_id = chain.chain_id(),
        from = %from,
        to = %to,
        value_wei = %value,
        input_len = calldata.len(),
        gas_limit,
        nonce,
        "cow: signing contract call",
    );

    let mut tx = TxEip1559 {
        chain_id: chain.chain_id(),
        nonce,
        gas_limit,
        max_fee_per_gas: fees.max_fee_per_gas,
        max_priority_fee_per_gas: fees.max_priority_fee_per_gas,
        to: TxKind::Call(to),
        value,
        access_list: Default::default(),
        input: calldata,
    };

    let sig = signer.sign_tx(&mut tx).await.map_err(|e| {
        warn!(error = %e, "cow: sign failed");
        format!("sign failed: {e}")
    })?;
    let envelope: TxEnvelope = tx.into_signed(sig).into();
    let raw = envelope.encoded_2718();
    let pending = provider.send_raw_transaction(&raw).await.map_err(|e| {
        warn!(error = %e, "cow: broadcast failed");
        let msg = e.to_string();
        if msg.to_lowercase().contains("insufficient funds") {
            // Belt-and-suspenders: the pre-flight above should catch this, but a
            // gas-price spike between estimate and broadcast can still trip it.
            "not enough ETH to cover the swap amount + network gas".to_string()
        } else {
            format!("broadcast failed: {msg}")
        }
    })?;
    let hash = *pending.tx_hash();
    info!(hash = %format!("{hash:#x}"), "cow: contract call broadcast ok");
    Ok(hash)
}

/// Format wei as a short ETH string (6 dp) for user-facing error messages.
fn fmt_eth(wei: U256) -> String {
    let s = alloy::primitives::utils::format_ether(wei);
    match s.parse::<f64>() {
        Ok(v) => format!("{v:.6}"),
        Err(_) => s,
    }
}

/// Poll for `hash`'s receipt, returning once it's mined. Errors if the tx
/// reverted, or if it hasn't confirmed within `max_polls` × 3s. Used to gate an
/// order submission on its approval (or an EthFlow `createOrder`) landing first.
pub async fn wait_for_receipt(
    provider: &RootProvider<Ethereum>,
    hash: TxHash,
    max_polls: u32,
) -> Result<(), String> {
    for _ in 0..max_polls {
        match provider.get_transaction_receipt(hash).await {
            Ok(Some(r)) => {
                return if r.status() {
                    Ok(())
                } else {
                    Err("transaction reverted".into())
                };
            }
            Ok(None) => {}
            Err(e) => {
                // Transient RPC hiccup — keep polling rather than failing the
                // whole placement.
                warn!(error = %e, "cow: receipt poll error (retrying)");
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Err("transaction not confirmed in time".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_calldata_targets_vault_relayer() {
        let cd = approve_calldata(U256::MAX);
        let b: &[u8] = cd.as_ref();
        assert_eq!(b.len(), 68, "selector + spender + amount");
        // `approve(address,uint256)` selector.
        assert_eq!(&b[0..4], &[0x09, 0x5e, 0xa7, 0xb3]);
        assert_eq!(&b[4..16], &[0u8; 12]);
        assert_eq!(
            &b[16..36],
            VAULT_RELAYER.as_slice(),
            "spender is the relayer"
        );
        assert_eq!(&b[36..68], &[0xFFu8; 32], "max approval");
    }

    #[test]
    fn allowance_calldata_uses_canonical_selector() {
        let input = allowanceCall {
            owner: Address::repeat_byte(0x11),
            spender: VAULT_RELAYER,
        }
        .abi_encode();
        // `allowance(address,address)` selector.
        assert_eq!(&input[0..4], &[0xdd, 0x62, 0xed, 0x3e]);
        assert_eq!(&input[16..36], Address::repeat_byte(0x11).as_slice());
        assert_eq!(&input[48..68], VAULT_RELAYER.as_slice());
    }
}
