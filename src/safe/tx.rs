//! Safe-transaction primitives — build, hash, sign, pack, and execute
//! a Safe 1.3+ multisig transaction.
//!
//! This module is the layer below the UI. It exposes:
//!
//! - `SafeTxInput` — ergonomic builder input (recipient + value + calldata).
//! - `build_safe_tx` — reads the Safe's authoritative on-chain nonce and
//!   returns a fully-populated `SafeTx` with relay fields zeroed.
//! - `safe_tx_hash` — local EIP-712 signing hash.
//! - `verify_safe_tx_hash_on_chain` / `verify_domain_separator_on_chain` —
//!   cross-checks against the Safe's own `getTransactionHash` /
//!   `domainSeparator` views. Defense-in-depth before signing.
//! - `pack_owner_signatures` — Safe's wire format: 65-byte `r ‖ s ‖ v`
//!   per owner, sorted ascending by signer address, concatenated.
//! - `execute_safe_tx` — ABI-encodes `execTransaction`, builds an
//!   EIP-1559 envelope from the executor EOA, signs it via
//!   `KaoSigner::sign_tx`, and broadcasts.
//!
//! Out of scope (this slice): UI, hardware EIP-712 signing,
//! Safe Transaction Service integration, `contractSignatures`
//! (nested-Safe-as-signer), module txs, DelegateCall, gas-refund
//! parameters, and ERC-20/721 calldata building.

use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::eip2718::Encodable2718;
use alloy::network::Ethereum;
use alloy::primitives::{Address, B256, Bytes, Signature, TxHash, TxKind, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::sol_types::{Eip712Domain, SolCall, SolStruct};

use crate::chain::Chain;
use crate::net::BalanceFetcher;
use crate::wallet::KaoSigner;

use super::{
    SafeTx, decode_ret, execTransactionCall, getTransactionHashCall, nonceCall,
};

/// Safe-spec operation byte. Only `Call` is exposed in v1 —
/// DelegateCall runs arbitrary code under the Safe's identity and is
/// reserved for the future MultiSendCallOnly / module flow.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Call = 0,
    // DelegateCall = 1,  // intentionally not constructible in v1
}

/// Ergonomic input for `build_safe_tx`. Carries everything the
/// caller needs to express (target, value, calldata, op) plus the
/// `(safe, chain)` pair that locates the Safe on-chain so we can read
/// its authoritative nonce.
#[derive(Debug, Clone)]
pub struct SafeTxInput {
    pub safe: Address,
    pub chain: Chain,
    pub to: Address,
    pub value: U256,
    pub data: Bytes,
    pub operation: Operation,
}

/// Build the Safe 1.3+ EIP-712 domain.
///
/// Per Safe spec since 1.3.0: `EIP712Domain(uint256 chainId, address
/// verifyingContract)` — chainId and verifyingContract only; no name,
/// no version, no salt. Pre-1.3 Safes used a different shape
/// (`EIP712Domain(address verifyingContract)`) and aren't supported —
/// `SafeTrust::Canonical` only certifies 1.3.0+.
pub fn safe_domain(safe: Address, chain: Chain) -> Eip712Domain {
    Eip712Domain {
        name: None,
        version: None,
        chain_id: Some(U256::from(chain.chain_id())),
        verifying_contract: Some(safe),
        salt: None,
    }
}

/// Local EIP-712 signing hash for a SafeTx —
/// `keccak256(0x1901 ‖ domainSeparator ‖ hashStruct(tx))`. This is
/// the same 32 bytes the Safe contract recovers signatures against
/// inside `checkSignatures`.
///
/// Correctness leans entirely on the `SafeTx` struct's field order in
/// `safe::mod`'s `sol!` block matching the Safe 1.3+ canonical
/// typehash. `safe_tx_typehash_matches_spec` pins that.
pub fn safe_tx_hash(tx: &SafeTx, domain: &Eip712Domain) -> B256 {
    tx.eip712_signing_hash(domain)
}

/// Pack owner signatures into the `bytes signatures` argument
/// `execTransaction` expects.
///
/// Wire format: `concat(s_1 ‖ s_2 ‖ … ‖ s_k)` where each `s_i` is
/// 65 bytes `r ‖ s ‖ v` and the signatures are SORTED ASCENDING by
/// recovered signer address. Safe's `checkSignatures` rejects with
/// `GS026` if the recovered addresses aren't monotonically
/// increasing, so unsorted packing is a silent revert at submit.
///
/// `v` byte meanings inside Safe's checkSignatures:
/// - `27` / `28` → EIP-712 signature over the SafeTx hash (our case).
/// - `31` / `32` → eth_sign (EIP-191) signature.
/// - `0` → smart-contract signature (contractSignatures /
///   nested-Safe-as-owner). **Not supported in v1** — any nested-Safe
///   owner hits `UnsupportedOperation` in `KaoSigner::sign_hash`.
///   Follow-up PR.
/// - `1` → pre-validated signature.
///
/// We emit the {27, 28} variant exclusively. alloy's
/// `Signature::as_bytes` writes `r ‖ s ‖ (27 + y_parity)` directly,
/// so we copy its 65 bytes verbatim.
pub fn pack_owner_signatures(mut sigs: Vec<(Address, Signature)>) -> Bytes {
    // `Address` is a `FixedBytes<20>`; its `Ord` impl is lexicographic
    // byte order, which is also numeric big-endian order — matching
    // what Safe sorts on.
    sigs.sort_by_key(|(addr, _)| *addr);
    let mut out = Vec::with_capacity(sigs.len() * 65);
    for (_, sig) in sigs {
        out.extend_from_slice(&sig.as_bytes());
    }
    Bytes::from(out)
}

/// Read the AUTHORITATIVE Safe nonce on chain. The cached
/// `SafeDescriptor.nonce` snapshot is stale by definition at sign
/// time — any other co-signer's executed tx will have bumped the
/// contract's nonce without going through this client. Always
/// re-read before quoting.
///
/// Returns `u64` because real Safes never approach overflow (one
/// increment per `execTransaction`); the `try_from` cast guards
/// against a pathological return rather than truncating silently.
pub async fn current_safe_nonce(
    net: &dyn BalanceFetcher,
    safe: Address,
    chain: Chain,
) -> Result<u64, String> {
    let calldata = nonceCall {}.abi_encode();
    let ret = net.call_raw(safe, Bytes::from(calldata), chain).await?;
    let n = decode_ret::<nonceCall>(&ret.value)?;
    u64::try_from(n).map_err(|_| format!("safe nonce overflows u64: {n}"))
}

/// Cross-check the local `safe_tx_hash` against the Safe's own
/// `getTransactionHash` view. Defense-in-depth before signing: a
/// divergence means either we built the domain/fields wrong or the
/// contract isn't actually the Safe we believe it is — caller MUST
/// refuse to sign on mismatch.
///
/// Uses `call_raw` (skips helios). The trust posture matches the
/// rest of the read-only Safe inspection: if a malicious RPC lies
/// about the contract's reply, it gains nothing it doesn't already
/// have via broadcast censorship; a quiet agreement between local
/// and remote rules out our own field-order or domain mistakes.
pub async fn verify_safe_tx_hash_on_chain(
    net: &dyn BalanceFetcher,
    tx: &SafeTx,
    safe: Address,
    chain: Chain,
) -> Result<B256, String> {
    let calldata = getTransactionHashCall {
        to: tx.to,
        value: tx.value,
        data: tx.data.clone(),
        operation: tx.operation,
        safeTxGas: tx.safeTxGas,
        baseGas: tx.baseGas,
        gasPrice: tx.gasPrice,
        gasToken: tx.gasToken,
        refundReceiver: tx.refundReceiver,
        _nonce: tx.nonce,
    }
    .abi_encode();
    let ret = net.call_raw(safe, Bytes::from(calldata), chain).await?;
    decode_ret::<getTransactionHashCall>(&ret.value)
}

/// Assemble a `SafeTx` from `input`, reading the Safe's
/// authoritative nonce on chain. All relay-flow fields (`safeTxGas`,
/// `baseGas`, `gasPrice`, `gasToken`, `refundReceiver`) are zero —
/// this slice doesn't support the Safe Transaction Service / Gelato
/// refund flow. The contract's `execTransaction` accepts these as
/// no-ops when `gasPrice == 0`.
pub async fn build_safe_tx(
    net: &dyn BalanceFetcher,
    input: SafeTxInput,
) -> Result<SafeTx, String> {
    let nonce_u64 = current_safe_nonce(net, input.safe, input.chain).await?;
    Ok(SafeTx {
        to: input.to,
        value: input.value,
        data: input.data,
        operation: input.operation as u8,
        safeTxGas: U256::ZERO,
        baseGas: U256::ZERO,
        gasPrice: U256::ZERO,
        gasToken: Address::ZERO,
        refundReceiver: Address::ZERO,
        nonce: U256::from(nonce_u64),
    })
}

/// ABI-encode the `execTransaction(...)` calldata for `tx` with the
/// packed `signatures` blob. Pure compute — no provider, no signer.
/// Split out from `execute_safe_tx` so the encoding step is unit-
/// testable independently of the broadcast plumbing.
pub fn encode_exec_transaction(tx: &SafeTx, signatures: Bytes) -> Bytes {
    Bytes::from(
        execTransactionCall {
            to: tx.to,
            value: tx.value,
            data: tx.data.clone(),
            operation: tx.operation,
            safeTxGas: tx.safeTxGas,
            baseGas: tx.baseGas,
            gasPrice: tx.gasPrice,
            gasToken: tx.gasToken,
            refundReceiver: tx.refundReceiver,
            signatures,
        }
        .abi_encode(),
    )
}

/// Build an EIP-1559 envelope from `executor` to the Safe address
/// carrying `execTransaction(...)` calldata, sign, and broadcast.
///
/// `executor` is the EOA paying gas. In v1's solo path this is one
/// of the linked owner accounts, but the Safe contract validates
/// signatures against the owner set rather than `msg.sender`, so any
/// account willing to relay a pre-signed bundle works.
///
/// The envelope's `value` is always zero — `execTransaction` doesn't
/// use `msg.value` for the inner transfer; the Safe sends `tx.value`
/// from its own balance during the internal call.
///
/// Mirrors the build → sign → broadcast pattern in
/// `wallet::tx::sign_and_send` rather than refactoring that fn to
/// take a `(to, value, input)` triple. Convergence is a later PR
/// once the Safe-mode Send UI lands and the shape stabilizes.
pub async fn execute_safe_tx(
    provider: &RootProvider<Ethereum>,
    executor: &KaoSigner,
    safe: Address,
    chain: Chain,
    tx: SafeTx,
    signatures: Bytes,
) -> Result<TxHash, String> {
    if signatures.is_empty() {
        // Safe reverts GS020 ("Signatures data too short") on empty
        // input — reject upfront so the user sees a clear error
        // instead of an opaque revert after paying gas.
        return Err("safe-tx: no signatures provided".to_string());
    }

    let calldata = encode_exec_transaction(&tx, signatures);

    let from = executor.address();
    let req = alloy::rpc::types::TransactionRequest::default()
        .from(from)
        .to(safe)
        .input(alloy::rpc::types::TransactionInput::new(calldata.clone()));
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

    let mut envelope = TxEip1559 {
        chain_id: chain.chain_id(),
        nonce,
        gas_limit,
        max_fee_per_gas: fees.max_fee_per_gas,
        max_priority_fee_per_gas: fees.max_priority_fee_per_gas,
        to: TxKind::Call(safe),
        value: U256::ZERO,
        access_list: Default::default(),
        input: calldata,
    };
    let sig = executor
        .sign_tx(&mut envelope)
        .await
        .map_err(|e| format!("sign failed: {e}"))?;
    let raw = TxEnvelope::from(envelope.into_signed(sig)).encoded_2718();
    let pending = provider
        .send_raw_transaction(&raw)
        .await
        .map_err(|e| format!("broadcast failed: {e}"))?;
    Ok(*pending.tx_hash())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256, keccak256};
    use alloy::signers::Signer;
    use alloy::signers::local::PrivateKeySigner;
    use alloy::sol_types::{SolCall, SolValue};

    use crate::net::CallMock;

    fn safe_addr() -> Address {
        address!("0x1111111111111111111111111111111111111111")
    }

    fn empty_safe_tx() -> SafeTx {
        SafeTx {
            to: Address::ZERO,
            value: U256::ZERO,
            data: Bytes::new(),
            operation: 0,
            safeTxGas: U256::ZERO,
            baseGas: U256::ZERO,
            gasPrice: U256::ZERO,
            gasToken: Address::ZERO,
            refundReceiver: Address::ZERO,
            nonce: U256::ZERO,
        }
    }

    #[test]
    fn safe_tx_typehash_matches_spec() {
        // Re-derive the canonical Safe 1.3+ SafeTx typehash from the
        // type-encoding string. If the `sol!` struct fields ever
        // drift out of canonical order, alloy's `eip712_encode_type`
        // diverges and this fails before any silent
        // wrong-hash signing can happen.
        let canonical = b"SafeTx(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce)";
        let expected = keccak256(canonical);
        let encoded = <SafeTx as SolStruct>::eip712_encode_type();
        assert_eq!(encoded.as_bytes(), canonical);
        let derived = empty_safe_tx().eip712_type_hash();
        assert_eq!(derived, expected);
        // Pin the literal so a change to the canonical string is
        // visible at code-review time.
        assert_eq!(
            expected,
            b256!("0xbb8310d486368db6bd6f849402fdd73ad53d316b5a4b2644ad6efe0f941286d8"),
        );
    }

    #[test]
    fn safe_domain_separator_matches_chain_id_and_safe() {
        // Manual derivation of the Safe 1.3+ domain separator:
        //   keccak256(abi.encode(
        //     keccak256("EIP712Domain(uint256 chainId,address verifyingContract)"),
        //     chainId,
        //     verifyingContract))
        let safe = safe_addr();
        let chain = Chain::Mainnet;
        let domain_typehash = keccak256(
            b"EIP712Domain(uint256 chainId,address verifyingContract)",
        );
        let chain_id = U256::from(chain.chain_id());
        let manual = keccak256(
            (domain_typehash, chain_id, safe).abi_encode(),
        );
        let derived = safe_domain(safe, chain).separator();
        assert_eq!(derived, manual);
        // Pin the domain typehash too.
        assert_eq!(
            domain_typehash,
            b256!("0x47e79534a245952e8b16893a336b85a3d9ea9fa8c573f3d803afb92a79469218"),
        );
    }

    #[test]
    fn safe_tx_hash_differs_for_different_chain_id() {
        let safe = safe_addr();
        let tx = empty_safe_tx();
        let h_mainnet = safe_tx_hash(&tx, &safe_domain(safe, Chain::Mainnet));
        let h_base = safe_tx_hash(&tx, &safe_domain(safe, Chain::Base));
        assert_ne!(
            h_mainnet, h_base,
            "same SafeTx must produce different hashes on different chains (cross-chain replay protection)",
        );
    }

    #[test]
    fn operation_call_byte_is_zero() {
        assert_eq!(Operation::Call as u8, 0);
    }

    async fn sig_for(hash: B256, signer: &PrivateKeySigner) -> (Address, Signature) {
        let sig = signer.sign_hash(&hash).await.unwrap();
        (signer.address(), sig)
    }

    #[tokio::test]
    async fn pack_owner_signatures_handles_single_signer() {
        let s = PrivateKeySigner::random();
        let hash = B256::repeat_byte(0xab);
        let pair = sig_for(hash, &s).await;
        let bytes = pack_owner_signatures(vec![pair]);
        assert_eq!(bytes.len(), 65);
        assert!(matches!(bytes[64], 27 | 28));
    }

    #[tokio::test]
    async fn pack_owner_signatures_sorts_by_address_ascending() {
        // Generate two signers and order them by address so we
        // know which one is "lower".
        let (lo, hi) = loop {
            let a = PrivateKeySigner::random();
            let b = PrivateKeySigner::random();
            if a.address() < b.address() {
                break (a, b);
            }
            if a.address() > b.address() {
                break (b, a);
            }
            // ridiculously unlikely tie — re-roll
        };
        let hash = B256::repeat_byte(0xcd);
        let pair_lo = sig_for(hash, &lo).await;
        let pair_hi = sig_for(hash, &hi).await;

        // Feed in HIGH-then-LOW order; expect packing to swap them.
        let bytes = pack_owner_signatures(vec![pair_hi, pair_lo]);
        assert_eq!(bytes.len(), 130);
        // First 65 bytes should be the low-address signer's signature.
        assert_eq!(&bytes[..65], &pair_lo.1.as_bytes()[..]);
        assert_eq!(&bytes[65..], &pair_hi.1.as_bytes()[..]);
        // Both v bytes in the EIP-712 domain {27, 28}.
        assert!(matches!(bytes[64], 27 | 28));
        assert!(matches!(bytes[129], 27 | 28));
    }

    #[tokio::test]
    async fn pack_owner_signatures_order_independence() {
        let s_a = PrivateKeySigner::random();
        let s_b = PrivateKeySigner::random();
        let hash = B256::repeat_byte(0xef);
        let pa = sig_for(hash, &s_a).await;
        let pb = sig_for(hash, &s_b).await;
        let ab = pack_owner_signatures(vec![pa, pb]);
        let ba = pack_owner_signatures(vec![pb, pa]);
        assert_eq!(ab, ba);
    }

    #[test]
    fn pack_owner_signatures_empty_returns_empty_bytes() {
        let out = pack_owner_signatures(Vec::new());
        assert!(out.is_empty());
    }

    // ── On-chain reader tests against CallMock ────────────────────

    #[tokio::test]
    async fn current_safe_nonce_reads_via_call_raw() {
        let mock = CallMock::new();
        let safe = safe_addr();
        mock.set_call(
            safe,
            Bytes::from(nonceCall {}.abi_encode()),
            Bytes::from(U256::from(99u64).abi_encode()),
            true,
        );
        let n = current_safe_nonce(&mock, safe, Chain::Mainnet).await.unwrap();
        assert_eq!(n, 99);
    }

    #[tokio::test]
    async fn build_safe_tx_uses_on_chain_nonce_and_zeros_relay_fields() {
        let mock = CallMock::new();
        let safe = safe_addr();
        mock.set_call(
            safe,
            Bytes::from(nonceCall {}.abi_encode()),
            Bytes::from(U256::from(7u64).abi_encode()),
            true,
        );
        let input = SafeTxInput {
            safe,
            chain: Chain::Mainnet,
            to: address!("0x000000000000000000000000000000000000dEaD"),
            value: U256::from(1_000_000_000_000_000_000u128),
            data: Bytes::new(),
            operation: Operation::Call,
        };
        let tx = build_safe_tx(&mock, input).await.unwrap();
        assert_eq!(tx.nonce, U256::from(7u64));
        assert_eq!(tx.operation, 0);
        assert_eq!(tx.safeTxGas, U256::ZERO);
        assert_eq!(tx.baseGas, U256::ZERO);
        assert_eq!(tx.gasPrice, U256::ZERO);
        assert_eq!(tx.gasToken, Address::ZERO);
        assert_eq!(tx.refundReceiver, Address::ZERO);
    }

    #[tokio::test]
    async fn verify_safe_tx_hash_on_chain_round_trips() {
        let mock = CallMock::new();
        let safe = safe_addr();
        let tx = empty_safe_tx();
        let planted = b256!("0xdeadbeef00000000000000000000000000000000000000000000000000000000");
        let calldata = getTransactionHashCall {
            to: tx.to,
            value: tx.value,
            data: tx.data.clone(),
            operation: tx.operation,
            safeTxGas: tx.safeTxGas,
            baseGas: tx.baseGas,
            gasPrice: tx.gasPrice,
            gasToken: tx.gasToken,
            refundReceiver: tx.refundReceiver,
            _nonce: tx.nonce,
        }
        .abi_encode();
        mock.set_call(
            safe,
            Bytes::from(calldata),
            Bytes::from(planted.abi_encode()),
            true,
        );
        let got = verify_safe_tx_hash_on_chain(&mock, &tx, safe, Chain::Mainnet)
            .await
            .unwrap();
        assert_eq!(got, planted);
    }

    #[test]
    fn encode_exec_transaction_round_trips() {
        // Round-trip the calldata: encode → ABI-decode → fields match.
        // Catches selector drift, argument permutation, or alloy ABI
        // bugs before they bubble up as opaque on-chain reverts.
        let tx = SafeTx {
            to: address!("0x000000000000000000000000000000000000dEaD"),
            value: U256::from(123u64),
            data: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
            operation: 0,
            safeTxGas: U256::ZERO,
            baseGas: U256::ZERO,
            gasPrice: U256::ZERO,
            gasToken: Address::ZERO,
            refundReceiver: Address::ZERO,
            nonce: U256::from(5u64),
        };
        let sigs = Bytes::from(vec![0x42u8; 65]);
        let calldata = encode_exec_transaction(&tx, sigs.clone());
        // First 4 bytes are the selector.
        assert_eq!(&calldata[..4], &execTransactionCall::SELECTOR);
        // execTransaction does NOT carry SafeTx's `nonce` field —
        // the nonce is consumed when the Safe verifies signatures
        // (it's mixed into the EIP-712 hash). On the wire there
        // are 10 args: to, value, data, operation, safeTxGas,
        // baseGas, gasPrice, gasToken, refundReceiver, signatures.
        let decoded = execTransactionCall::abi_decode(&calldata).unwrap();
        assert_eq!(decoded.to, tx.to);
        assert_eq!(decoded.value, tx.value);
        assert_eq!(decoded.data, tx.data);
        assert_eq!(decoded.operation, tx.operation);
        assert_eq!(decoded.safeTxGas, tx.safeTxGas);
        assert_eq!(decoded.baseGas, tx.baseGas);
        assert_eq!(decoded.gasPrice, tx.gasPrice);
        assert_eq!(decoded.gasToken, tx.gasToken);
        assert_eq!(decoded.refundReceiver, tx.refundReceiver);
        assert_eq!(decoded.signatures, sigs);
    }

}
