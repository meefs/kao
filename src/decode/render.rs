//! Argument decoding + humanization. End-to-end pipeline:
//!
//! ```text
//! (chain, to, calldata)
//!     │
//!     ├─ proxy::resolve_implementation  → impl address (verified)
//!     ├─ net.get_code(impl)             → bytecode (verified)
//!     ├─ bytecode::extract              → selector → arg-type list
//!     ├─ fourbyte::lookup(selector)     → human signatures
//!     ├─ matcher::resolve               → Resolved::{Unique|Ambiguous|TypesOnly|Unknown}
//!     ├─ alloy::dyn_abi::abi_decode     → DynSolValue per arg
//!     └─ humanize:
//!          • addresses    → reverse ENS (Mainnet only)
//!          • amount + ERC-20 target → "1.234 USDC"
//!          • approve(_, MAX) → InfiniteApproval warning
//! ```
//!
//! Sync `decode_args` is the pure abi-decode step (callable from
//! tests with no RPC). Async `decode_call` is the full pipeline; the
//! Phase 7 dashboard kicks it off via `Task::perform`.

use alloy::dyn_abi::{DynSolType, DynSolValue};
use alloy::primitives::utils::format_units;
use alloy::primitives::{Address, B256, Bytes, I256, U256};

use crate::chain::Chain;
use crate::decode::{bytecode, fourbyte, matcher, proxy};
use crate::ens;
use crate::net::BalanceFetcher;

/// `symbol()` selector — first 4 bytes of `keccak256("symbol()")`.
const SYMBOL_SELECTOR: [u8; 4] = [0x95, 0xd8, 0x9b, 0x41];
/// `decimals()` selector.
const DECIMALS_SELECTOR: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];

/// Top-level decode result. Contains everything the FunctionPanel needs
/// to render a review row, plus a few fields (the call target, raw
/// arg ints) the v1 panel doesn't display but downstream consumers
/// (a WalletConnect request modal, future analyzers) will.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DecodedCall {
    pub to: Address,
    pub selector: [u8; 4],
    pub raw_calldata: Bytes,
    /// Function name when 4byte resolved; `None` for `TypesOnly` /
    /// `Unknown` / `Empty`.
    pub function_name: Option<String>,
    pub args: Vec<DecodedArg>,
    pub state: ResolutionState,
    pub warnings: Vec<Warning>,
    /// Chain of intermediate proxy addresses, in order. Empty when the
    /// call lands on a non-proxy contract.
    pub proxy_hops: Vec<Address>,
    /// All on-chain reads in this decode (storage slots + bytecode)
    /// went through Helios's verified path. False if any fell back —
    /// downstream UI should warn.
    pub all_verified: bool,
    /// Symbol/decimals of `to`, if it answers the standard probes.
    /// Drives "1.234 USDC" amount formatting.
    pub target_token: Option<TokenInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionState {
    /// Decoded with name + types from 4byte. Function name and per-arg
    /// types are accurate; values are valid abi-decoded.
    Resolved,
    /// Decoded with types-only — bytecode said "selector takes (X, Y)"
    /// but 4byte has no name registered. `function_name` is `None`.
    TypesOnly,
    /// Multiple 4byte signatures match this selector and bytecode
    /// couldn't narrow it down. We pick the first; UI surfaces the
    /// `AmbiguousSignature` warning so the user can review the raw
    /// calldata before signing.
    Ambiguous,
    /// No bytecode and no 4byte. Show selector + raw calldata.
    Unknown,
    /// Native ETH transfer — no calldata to decode.
    Empty,
}

#[derive(Debug, Clone)]
pub struct DecodedArg {
    /// The 4byte signature is just `transfer(address,uint256)`, no
    /// argument names; populated only when bytecode introspection
    /// names them (rare today; placeholder for future evmole versions).
    pub name: Option<String>,
    pub ty: DynSolType,
    pub display: ArgDisplay,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ArgDisplay {
    /// Address; `ens` populated when the chain is Mainnet and reverse
    /// resolution succeeded (forward-verified by `ens::lookup_address`).
    Address {
        addr: Address,
        ens: Option<String>,
    },
    /// Unsigned integer. `formatted` is the decimal-aware display
    /// string when the call target is an ERC-20 and this arg is
    /// reasonably an "amount"; otherwise the raw decimal U256.
    Uint {
        raw: U256,
        formatted: String,
    },
    Int {
        raw: I256,
        formatted: String,
    },
    Bool(bool),
    String(String),
    /// Hex-encoded; truncated for display in the UI layer.
    Bytes(Bytes),
    /// Catch-all canonical-string render for tuple / array / fixed-bytes /
    /// other types we don't yet have a specialized renderer for.
    Raw(String),
}

#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub symbol: String,
    pub decimals: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// `approve(spender, type(uint256).max)` — unbounded allowance.
    InfiniteApproval { spender: Address, token: Address },
    /// At least one storage / bytecode read fell back to raw RPC.
    UnverifiedBytecode,
    /// 4byte returned multiple candidates and bytecode types couldn't
    /// narrow. UI lists candidates; signing this is "trust me bro".
    AmbiguousSignature { candidates: Vec<String> },
}

// ---------------------------------------------------------------------------
// Top-level pipeline

pub async fn decode_call(
    net: &dyn BalanceFetcher,
    chain: Chain,
    to: Address,
    calldata: Bytes,
) -> DecodedCall {
    let mut out = DecodedCall {
        to,
        selector: [0; 4],
        raw_calldata: calldata.clone(),
        function_name: None,
        args: Vec::new(),
        state: ResolutionState::Empty,
        warnings: Vec::new(),
        proxy_hops: Vec::new(),
        all_verified: true,
        target_token: None,
    };

    if calldata.is_empty() {
        return out;
    }
    if calldata.len() < 4 {
        out.state = ResolutionState::Unknown;
        return out;
    }
    let mut selector = [0u8; 4];
    selector.copy_from_slice(&calldata[..4]);
    out.selector = selector;
    out.state = ResolutionState::Unknown;

    // Phase 3: walk the proxy chain rooted at `to`.
    let resolved = proxy::resolve_implementation(net, chain, to).await;
    out.proxy_hops = resolved.hops.clone();
    if !resolved.all_verified {
        out.all_verified = false;
    }

    // Phase 1+4: fetch verified bytecode for the implementation, run evmole.
    let code = match net.get_code(resolved.implementation, chain).await {
        Ok(read) => {
            if !read.verified {
                out.all_verified = false;
            }
            read.value
        }
        Err(_) => Bytes::new(),
    };
    let bytecode_types = if code.is_empty() {
        None
    } else {
        bytecode::lookup(&code, selector)
    };

    // Phase 2+5: 4byte lookup, signature matcher.
    let candidates = fourbyte::lookup(selector);
    let resolved_sig = matcher::resolve(&candidates, bytecode_types.as_deref());

    // Decide arg-type list and function name from the matcher result.
    let (function_name, arg_types, state) = match resolved_sig {
        matcher::Resolved::Unique { name, arg_types } => {
            (Some(name), arg_types, ResolutionState::Resolved)
        }
        matcher::Resolved::TypesOnly(types) => (None, types, ResolutionState::TypesOnly),
        matcher::Resolved::Ambiguous(list) => {
            // Pick the first parsed candidate as our best guess; flag
            // the rest in the warning so the UI can show alternatives.
            let mut iter = list.into_iter();
            let chosen = iter.next();
            let rest: Vec<String> = iter.map(|(n, _)| n).collect();
            let mut all_names: Vec<String> = chosen.iter().map(|(n, _)| n.clone()).collect();
            all_names.extend(rest);
            out.warnings.push(Warning::AmbiguousSignature {
                candidates: all_names,
            });
            match chosen {
                Some((name, arg_types)) => (Some(name), arg_types, ResolutionState::Ambiguous),
                None => (None, Vec::new(), ResolutionState::Unknown),
            }
        }
        matcher::Resolved::Unknown => (None, Vec::new(), ResolutionState::Unknown),
    };
    out.function_name = function_name;
    out.state = state;

    // Decode arguments via alloy.
    let raw_args = decode_args_inner(&arg_types, &calldata[4..]);

    // Probe the call target (NOT the implementation — `to` is what the
    // user thinks they're interacting with) for symbol() / decimals().
    // Goes through the verified `eth_call` path so an attacker on the
    // RPC can't relabel a hostile contract as "USDC". Probe failures
    // are non-fatal (the contract just doesn't expose the standard
    // ERC-20 selectors); a fallback (verified=false) marks
    // `all_verified=false` and the `UnverifiedBytecode` warning fires
    // alongside the existing bytecode/storage warnings.
    if let Some((meta, verified)) = read_token_meta(net, chain, to).await {
        out.target_token = Some(meta);
        if !verified {
            out.all_verified = false;
        }
    }

    // Humanize each arg in turn.
    let mut humanized: Vec<DecodedArg> = Vec::with_capacity(raw_args.len());
    for (ty, value) in arg_types.iter().zip(raw_args.iter()) {
        humanized.push(humanize_arg(ty, value, &out, net, chain).await);
    }
    out.args = humanized;

    // Heuristic warnings.
    if matches!(out.state, ResolutionState::Resolved | ResolutionState::Ambiguous)
        && out.function_name.as_deref() == Some("approve")
        && out.args.len() == 2
        && let ArgDisplay::Address { addr: spender, .. } = out.args[0].display
        && let ArgDisplay::Uint { raw, .. } = &out.args[1].display
        && *raw == U256::MAX
    {
        out.warnings.push(Warning::InfiniteApproval {
            spender,
            token: to,
        });
    }
    if !out.all_verified {
        out.warnings.push(Warning::UnverifiedBytecode);
    }

    out
}

// ---------------------------------------------------------------------------
// Pure decoding (no RPC)

/// Decode the argument bytes against `arg_types`. Returns one
/// `DynSolValue` per type, or empty if alloy refuses to decode (e.g.
/// truncated calldata).
fn decode_args_inner(arg_types: &[DynSolType], data: &[u8]) -> Vec<DynSolValue> {
    if arg_types.is_empty() {
        return Vec::new();
    }
    let tuple_ty = DynSolType::Tuple(arg_types.to_vec());
    match tuple_ty.abi_decode_params(data) {
        Ok(DynSolValue::Tuple(values)) => values,
        Ok(other) => vec![other],
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Humanization

async fn humanize_arg(
    ty: &DynSolType,
    value: &DynSolValue,
    parent: &DecodedCall,
    net: &dyn BalanceFetcher,
    chain: Chain,
) -> DecodedArg {
    let display = match value {
        DynSolValue::Address(addr) => {
            // Reverse ENS only on Mainnet — reverse records live there.
            // Cross-chain reverse would need a separate Mainnet provider
            // (a future deliberate addition). The ENS resolver itself
            // already forward-verifies the result; we don't need to add
            // a second verification layer here.
            let ens = if matches!(chain, Chain::Mainnet) {
                match net.provider(chain).await {
                    Some(provider) => ens::lookup_address(&provider, *addr)
                        .await
                        .ok()
                        .flatten(),
                    None => None,
                }
            } else {
                None
            };
            ArgDisplay::Address { addr: *addr, ens }
        }
        DynSolValue::Uint(raw, _) => {
            let formatted = match &parent.target_token {
                Some(tok) => format_units(*raw, tok.decimals)
                    .map(|s| format!("{s} {}", tok.symbol))
                    .unwrap_or_else(|_| raw.to_string()),
                None => raw.to_string(),
            };
            ArgDisplay::Uint { raw: *raw, formatted }
        }
        DynSolValue::Int(raw, _) => ArgDisplay::Int {
            raw: *raw,
            formatted: raw.to_string(),
        },
        DynSolValue::Bool(b) => ArgDisplay::Bool(*b),
        DynSolValue::String(s) => ArgDisplay::String(s.clone()),
        DynSolValue::Bytes(b) => ArgDisplay::Bytes(Bytes::copy_from_slice(b)),
        DynSolValue::FixedBytes(word, n) => {
            ArgDisplay::Bytes(Bytes::copy_from_slice(&word.as_slice()[..*n]))
        }
        // Tuple / Array / FixedArray / Function get the canonical-string
        // fallback for now; the FunctionPanel can iterate later.
        other => ArgDisplay::Raw(format!("{:?}", other)),
    };
    DecodedArg {
        name: None,
        ty: ty.clone(),
        display,
    }
}

// ---------------------------------------------------------------------------
// Token metadata probe (Helios-verified)

/// Probe the standard ERC-20 metadata selectors. Returns `None` when
/// either probe fails (the contract isn't ERC-20-shaped, doesn't
/// implement these views, or the call reverted). The bool in the
/// success tuple is `verified` — `true` when both probes went through
/// Helios's verified path; `false` when at least one fell back to raw
/// RPC.
///
/// We deliberately go through the verified `BalanceFetcher::call`
/// rather than the raw provider: an attacker on the RPC could
/// otherwise relabel any contract as "USDC" with 6 decimals and the
/// review screen would format the amount accordingly. With
/// verification, the symbol/decimals returned were re-executed by
/// Helios against proof-checked bytecode and storage.
async fn read_token_meta(
    net: &dyn BalanceFetcher,
    chain: Chain,
    addr: Address,
) -> Option<(TokenInfo, bool)> {
    let (symbol, sym_v) = call_decode_string(net, chain, addr, SYMBOL_SELECTOR).await?;
    let (decimals, dec_v) = call_decode_u8(net, chain, addr, DECIMALS_SELECTOR).await?;
    Some((TokenInfo { symbol, decimals }, sym_v && dec_v))
}

async fn verified_call(
    net: &dyn BalanceFetcher,
    chain: Chain,
    to: Address,
    selector: [u8; 4],
) -> Option<(Bytes, bool)> {
    let calldata = Bytes::copy_from_slice(&selector);
    match net.call(to, calldata, chain).await {
        Ok(read) => Some((read.value, read.verified)),
        Err(_) => None,
    }
}

async fn call_decode_string(
    net: &dyn BalanceFetcher,
    chain: Chain,
    addr: Address,
    selector: [u8; 4],
) -> Option<(String, bool)> {
    let (raw, verified) = verified_call(net, chain, addr, selector).await?;
    if raw.is_empty() {
        return None;
    }
    if let Ok(DynSolValue::String(s)) = DynSolType::String.abi_decode(&raw)
        && !s.is_empty()
    {
        return Some((s, verified));
    }
    // Some old tokens (MKR, etc.) return a fixed bytes32 instead of
    // dynamic string. Try decoding as bytes32 and trimming nulls.
    if raw.len() == 32 {
        let trimmed: Vec<u8> = raw.iter().copied().take_while(|b| *b != 0).collect();
        if let Ok(s) = String::from_utf8(trimmed)
            && !s.is_empty()
        {
            return Some((s, verified));
        }
    }
    None
}

async fn call_decode_u8(
    net: &dyn BalanceFetcher,
    chain: Chain,
    addr: Address,
    selector: [u8; 4],
) -> Option<(u8, bool)> {
    let (raw, verified) = verified_call(net, chain, addr, selector).await?;
    // ERC-20 `decimals()` returns uint8 ABI-padded to 32 bytes.
    if raw.len() < 32 {
        return None;
    }
    let last = raw[31];
    // Sanity bound: a legitimate token decimals is ≤ ~30.
    if last > 36 { None } else { Some((last, verified)) }
}

/// Suppress dead-code warnings until pipeline.rs is wired into Phase 7.
/// `B256` import currently only used by tests; keep the import explicit
/// so future warning hunts don't accidentally drop it.
#[allow(dead_code)]
fn _b256_alive_marker(_: B256) {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn decode_transfer_args() {
        // `transfer(address,uint256)` calldata for sending 1 USDC (6 decimals)
        // to 0xdEaD. Selector + 32-byte recipient + 32-byte amount.
        let mut cd = Vec::with_capacity(68);
        cd.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
        cd.extend_from_slice(&[0u8; 12]);
        cd.extend_from_slice(address!("000000000000000000000000000000000000dEaD").as_slice());
        let amount = U256::from(1_000_000u64);
        cd.extend_from_slice(&amount.to_be_bytes::<32>());

        let arg_types = vec![DynSolType::Address, DynSolType::Uint(256)];
        let values = decode_args_inner(&arg_types, &cd[4..]);
        assert_eq!(values.len(), 2);
        assert!(matches!(values[0], DynSolValue::Address(_)));
        assert!(matches!(values[1], DynSolValue::Uint(_, _)));
        if let DynSolValue::Uint(v, _) = values[1] {
            assert_eq!(v, amount);
        }
    }

    #[test]
    fn decode_empty_args() {
        let values = decode_args_inner(&[], &[]);
        assert!(values.is_empty());
    }

    #[test]
    fn decode_truncated_returns_empty() {
        let arg_types = vec![DynSolType::Address, DynSolType::Uint(256)];
        // 31 bytes = halfway through the address word — alloy refuses.
        let values = decode_args_inner(&arg_types, &[0u8; 31]);
        assert!(values.is_empty());
    }

    #[test]
    fn decode_bool_string_bytes() {
        // Encode `(bool, string, bytes)` via DynSolValue's own encoder
        // and round-trip through the same decoder we use in production.
        let original = DynSolValue::Tuple(vec![
            DynSolValue::Bool(true),
            DynSolValue::String("hello".into()),
            DynSolValue::Bytes(vec![0xab, 0xcd]),
        ]);
        let encoded = original.abi_encode_params();
        let arg_types = vec![DynSolType::Bool, DynSolType::String, DynSolType::Bytes];
        let values = decode_args_inner(&arg_types, &encoded);
        assert_eq!(values.len(), 3);
        match &values[0] {
            DynSolValue::Bool(b) => assert!(*b),
            other => panic!("expected Bool, got {other:?}"),
        }
        match &values[1] {
            DynSolValue::String(s) => assert_eq!(s, "hello"),
            other => panic!("expected String, got {other:?}"),
        }
        match &values[2] {
            DynSolValue::Bytes(b) => assert_eq!(&b[..], &[0xab, 0xcd]),
            other => panic!("expected Bytes, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn decode_call_empty_calldata_yields_empty() {
        use crate::chain::Chain;
        use crate::net::MockFetcher;
        let net = MockFetcher::new();
        let out = decode_call(&net, Chain::Mainnet, Address::ZERO, Bytes::new()).await;
        assert!(matches!(out.state, ResolutionState::Empty));
        assert!(out.warnings.is_empty());
        assert!(out.args.is_empty());
        assert!(out.function_name.is_none());
    }

    #[tokio::test]
    async fn decode_call_short_calldata_yields_unknown() {
        use crate::chain::Chain;
        use crate::net::MockFetcher;
        let net = MockFetcher::new();
        // 3 bytes of "calldata" — not even a full selector.
        let out = decode_call(
            &net,
            Chain::Mainnet,
            Address::ZERO,
            Bytes::from_static(&[0xa9, 0x05, 0x9c]),
        )
        .await;
        assert!(matches!(out.state, ResolutionState::Unknown));
        // Short-circuits before any RPC, so no warnings (in particular,
        // no UnverifiedBytecode — we never even tried to fetch).
        assert!(out.warnings.is_empty());
        assert!(out.args.is_empty());
    }
}
