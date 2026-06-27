//! ENS resolver — forward (`name → address`) and reverse (`address → name`)
//! lookups against the legacy ENS registry on Ethereum mainnet.
//!
//! Reverse resolution is **forward-verified**: after fetching the name
//! attached to an address's `*.addr.reverse` record, we forward-resolve that
//! name and only return it when the result matches the original address.
//! Without that step the reverse record is whatever the address owner chose
//! to claim — `vitalik.eth` included — and trusting it would let any address
//! impersonate any name in the UI.
//!
//! Both paths normalize names via UTS-46 / ENSIP-15 (`ens-normalize-rs`)
//! before namehashing or display. This rejects bidi-control characters,
//! zero-width joiners outside emoji sequences, NUL bytes, and mixed-script
//! confusables — i.e. all the inputs a naive ASCII-lowercase pass would
//! happily accept and then render as something the user did not type.
//! Forward verification on its own can't catch these because the attacker
//! controls both the reverse record and the forward record they point at.
//!
//! No `sol!` macro: encoding is a 4-byte selector + a single 32-byte
//! `bytes32` argument; decoding is either a left-padded 20-byte address or a
//! single-string ABI head/tail.
//!
//! Every registry/resolver read goes through the Helios-verified
//! [`BalanceFetcher::call`] against Ethereum mainnet — **never** the raw
//! exec-RPC provider. ENS lives on mainnet, and a resolved address feeds the
//! signed recipient (Send) and imported Safe owners; routing the lookups
//! through the light client means a hostile exec RPC can't substitute a
//! resolver or an `addr` record, because Helios re-executes the call against
//! proof-verified state. When a read can't be verified (light client
//! unavailable, raw-RPC fallback) we **fail closed**: the lookup returns
//! `Err` instead of an address. Forward resolution surfaces that error to the
//! user (who can paste the address directly); the reverse-display call sites
//! already map any error to "no name", so a hostile RPC can never fabricate a
//! name on the review surface. Mainnet ships with a built-in checkpoint and a
//! default consensus RPC, so the verified path is the normal case, not an
//! opt-in.
//!
//! `alloy` 2.0.1 does not ship ENS resolution under the hood — there is no
//! `Provider::resolve_name` or equivalent in the published crate — so we
//! own the registry/resolver round-trip rather than delegating.

use std::sync::LazyLock;

use alloy::primitives::{Address, B256, Bytes, U256, address, keccak256};
use ens_normalize_rs::EnsNameNormalizer;
use tracing::{debug, trace};

use crate::chain::Chain;
use crate::net::BalanceFetcher;

/// Process-wide normalizer. Constructing one parses an embedded JSON spec
/// (~MB of code-point tables); we want that cost paid once at first use,
/// not per ENS lookup.
static NORMALIZER: LazyLock<EnsNameNormalizer> = LazyLock::new(EnsNameNormalizer::default);

/// Normalize an ENS name (UTS-46 / ENSIP-15). Returns the normalized form
/// on success; on rejection (disallowed character, mixed-script confusable,
/// stray bidi control, etc.) returns a short reason string.
pub fn normalize(name: &str) -> Result<String, String> {
    NORMALIZER
        .normalize(name)
        .map_err(|e| format!("invalid ENS name: {e}"))
}

/// Display-friendly form: re-applies emoji ZWJ sequences and similar
/// presentational variants that `normalize` strips. Always returns a
/// string — falls back to the input on `beautify` errors so callers can't
/// be tripped by a name that round-trips through `process` without issue
/// but trips a beautify edge case.
pub fn beautify(name: &str) -> String {
    NORMALIZER
        .beautify(name)
        .unwrap_or_else(|_| name.to_string())
}

/// ENS Registry on Ethereum mainnet (ENSIP-1).
pub(crate) const ENS_REGISTRY: Address = address!("0x00000000000C2E074eC69A0dFb2997BA6C7d2e1e");

// keccak256("resolver(bytes32)")[..4]
pub(crate) const RESOLVER_SELECTOR: [u8; 4] = [0x01, 0x78, 0xb8, 0xbf];
// keccak256("addr(bytes32)")[..4]
const ADDR_SELECTOR: [u8; 4] = [0x3b, 0x3b, 0x57, 0xde];
// keccak256("name(bytes32)")[..4]
pub(crate) const NAME_SELECTOR: [u8; 4] = [0x69, 0x1f, 0x34, 0x31];

/// Error returned when a name-service read completed but did not cross
/// Helios's verified path — the light client was unavailable and the value
/// came back over the raw-RPC fallback. These lookups feed signing decisions,
/// so an unverified answer is a hard failure rather than something to silently
/// trust. Shared by ENS and the GNS/WNS namespaces ([`crate::names`]), hence
/// the namespace-neutral wording.
const UNVERIFIED: &str =
    "Name lookup could not be verified by the light client; enter the address directly";

/// Compute the ENSIP-1 namehash of `name`.
///
/// Callers must pass an already-normalized name (`normalize()` applied).
/// `namehash` itself only splits on `.` and keccaks each label; pre-
/// normalization is what guarantees that two visually-equal inputs hash to
/// the same node. The `Vitalik.eth == vitalik.eth` test case below relies
/// on this contract — we lowercase here as a thin safety net for callers
/// inside the module that already normalized, but the canonical entry
/// points are `resolve_name` / `lookup_address`.
pub fn namehash(name: &str) -> B256 {
    let normalized = name.to_ascii_lowercase();
    if normalized.is_empty() {
        return B256::ZERO;
    }
    let mut node = B256::ZERO;
    for label in normalized.split('.').rev() {
        if label.is_empty() {
            // ".." or trailing/leading dot — treat the empty label as the
            // root, i.e. skip. ENS itself rejects these but the user might
            // still type "vitalik.eth." or "..eth"; degrading to a normal
            // hash gives a deterministic miss.
            continue;
        }
        let label_hash = keccak256(label.as_bytes());
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(node.as_slice());
        buf[32..].copy_from_slice(label_hash.as_slice());
        node = keccak256(buf);
    }
    node
}

/// Cheap heuristic for "the user typed something that looks like an ENS
/// name". Anything containing a dot and not parsing as a hex address is fair
/// game; the resolver will return None for unregistered names so we don't
/// have to be precise here.
pub fn looks_like_ens(input: &str) -> bool {
    let s = input.trim();
    !s.is_empty() && s.contains('.') && s.parse::<Address>().is_err()
}

/// Forward resolution: `name.eth → 0xAddress`.
///
/// The input is normalized first; an unnormalisable name returns `Err`
/// (the user typed something that ENS itself would reject — surfacing
/// that distinctly from "registered but no address" lets the UI explain
/// *why* the name didn't resolve).
///
/// `Ok(None)` when the name has no resolver, the resolver has no `addr`
/// record, or the recorded address is the zero address. Network/RPC
/// failures bubble up as `Err` — as does a resolution that completed but
/// could not be verified by the light client: the resolved address becomes
/// the signed recipient, so an unverified answer is never returned as
/// success (the caller surfaces it as a resolution error, and the user can
/// still paste the address directly).
pub async fn resolve_name(net: &dyn BalanceFetcher, name: &str) -> Result<Option<Address>, String> {
    let normalized = normalize(name)?;
    let node = namehash(&normalized);
    let result = resolver_addr(net, node).await?;
    if result.is_none() {
        trace!(name = %normalized, "ens forward: no resolver / no record");
    }
    Ok(result)
}

/// Reverse resolution with **forward verification** and **input
/// normalization**: `0xAddress → name.eth`.
///
/// Returns `Ok(Some(name))` only when:
///   1. The address has a `*.addr.reverse` resolver and `name` record
///   2. The recorded name normalizes cleanly under ENSIP-15
///   3. Forward-resolving the normalized name yields the original address
///
/// Anything else returns `Ok(None)`. Both steps matter:
///   - Without forward verification, the reverse record is whatever the
///     address owner wrote (`vitalik.eth` included).
///   - Without normalization, an attacker can register a name with a
///     bidi-override or zero-width joiner that *renders* as a popular
///     name, set both the reverse record and a matching forward record,
///     and pass forward verification while still spoofing the display.
///     ENSIP-15 rejects those inputs at the boundary.
///
/// The returned string is the **beautified** form of the normalized name —
/// suitable for direct rendering. Forward verification used the
/// normalized form so an on-chain record that round-trips with non-canonical
/// bytes still resolves correctly.
///
/// A read that could not be verified by the light client returns `Err`
/// (every registry/resolver call goes through Helios); the display call
/// sites map that to "no name", so a hostile RPC can't fabricate one.
pub async fn lookup_address(
    net: &dyn BalanceFetcher,
    addr: Address,
) -> Result<Option<String>, String> {
    let reverse_label = reverse_node_name(addr);
    let node = namehash(&reverse_label);
    let resolver = registry_resolver(net, node).await?;
    if resolver == Address::ZERO {
        trace!(addr = %addr, "ens reverse: no resolver");
        return Ok(None);
    }
    let result = verified_call(net, resolver, NAME_SELECTOR, node).await?;
    let claimed = match decode_string(&result) {
        Some(s) if !s.is_empty() => s,
        _ => return Ok(None),
    };

    // Normalize the on-chain name. Anything ENSIP-15 disallows — bidi
    // controls, zero-width joiners outside emoji sequences, mixed-script
    // confusables, raw NULs — gets rejected here, before display.
    let normalized = match normalize(&claimed) {
        Ok(n) => n,
        Err(e) => {
            debug!(addr = %addr, claimed = %claimed, error = %e, "ens reverse rejected: bad normalization");
            return Ok(None);
        }
    };

    // Forward-verify the normalized form. We deliberately do not call
    // `resolve_name` (which would normalize again) — we already have the
    // namehash inputs we want, and re-running the normalizer obscures the
    // round-trip in tracing.
    let forward_node = namehash(&normalized);
    let forward_addr = match resolver_addr(net, forward_node).await? {
        Some(a) => a,
        None => {
            debug!(addr = %addr, name = %normalized, "ens reverse rejected: no forward record");
            return Ok(None);
        }
    };
    if forward_addr != addr {
        debug!(addr = %addr, claimed = %normalized, forward = %forward_addr, "ens reverse rejected: forward mismatch");
        return Ok(None);
    }
    debug!(addr = %addr, name = %normalized, "ens reverse verified");
    Ok(Some(beautify(&normalized)))
}

/// Issue one name-service `eth_call` (`selector` followed by a single 32-byte
/// argument `node`) against `to` through the light-client-verified path on
/// Ethereum mainnet. Shared by ENS and by the single-contract GNS/WNS
/// namespaces ([`crate::names`]); `node` is a namehash for `addr(bytes32)` or a
/// left-padded address for `reverseResolve(address)`.
///
/// Returns the raw return bytes on a verified read, or `Err` ([`UNVERIFIED`])
/// when the value came back over the raw-RPC fallback. These namespaces are
/// mainnet-only, so the chain is pinned to [`Chain::Mainnet`] regardless of
/// which chain the caller is viewing — see the module docs for why every read
/// fails closed.
pub(crate) async fn verified_call(
    net: &dyn BalanceFetcher,
    to: Address,
    selector: [u8; 4],
    node: B256,
) -> Result<Bytes, String> {
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&selector);
    data.extend_from_slice(node.as_slice());
    let read = net.call(to, Bytes::from(data), Chain::Mainnet).await?;
    if !read.verified {
        return Err(UNVERIFIED.to_string());
    }
    Ok(read.value)
}

/// Resolve the `addr` record for an already-namehashed node. Shared by
/// `resolve_name` and the forward-verification step in `lookup_address`.
async fn resolver_addr(net: &dyn BalanceFetcher, node: B256) -> Result<Option<Address>, String> {
    let resolver = registry_resolver(net, node).await?;
    if resolver == Address::ZERO {
        return Ok(None);
    }
    let result = verified_call(net, resolver, ADDR_SELECTOR, node).await?;
    if result.len() < 32 {
        return Ok(None);
    }
    let addr = decode_address(&result);
    Ok(if addr == Address::ZERO {
        None
    } else {
        Some(addr)
    })
}

/// Build the `<addr>.addr.reverse` label used by ENS reverse lookups.
/// `<addr>` is 40 lowercase hex chars without a `0x` prefix.
fn reverse_node_name(addr: Address) -> String {
    // `LowerHex` for `Address` formats 40 hex chars without prefix.
    format!("{addr:x}.addr.reverse")
}

/// Call `ENS.resolver(bytes32)` against the registry and decode the address.
async fn registry_resolver(net: &dyn BalanceFetcher, node: B256) -> Result<Address, String> {
    let result = verified_call(net, ENS_REGISTRY, RESOLVER_SELECTOR, node).await?;
    if result.len() < 32 {
        debug!(
            len = result.len(),
            "registry.resolver returned short data; treating as no resolver"
        );
        return Ok(Address::ZERO);
    }
    Ok(decode_address(&result))
}

/// Decode a left-padded 32-byte ABI-encoded address.
pub(crate) fn decode_address(data: &[u8]) -> Address {
    debug_assert!(data.len() >= 32);
    Address::from_slice(&data[12..32])
}

/// Decode a single ABI-encoded `string` return: head (32-byte offset) +
/// tail (32-byte length followed by `length` bytes, padded to a 32-byte
/// multiple). Returns `None` on malformed input.
pub(crate) fn decode_string(data: &[u8]) -> Option<String> {
    if data.len() < 64 {
        return None;
    }
    // For a single string return the offset is always 32, but tolerate any
    // value the resolver chooses — only the length+bytes after the offset
    // matter for decoding.
    let offset = U256::from_be_slice(&data[..32]);
    let offset: usize = offset.try_into().ok()?;
    let len_end = offset.checked_add(32)?;
    if data.len() < len_end {
        return None;
    }
    let len = U256::from_be_slice(&data[offset..len_end]);
    let len: usize = len.try_into().ok()?;
    let start = len_end;
    let end = start.checked_add(len)?;
    if data.len() < end {
        return None;
    }
    String::from_utf8(data[start..end].to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors from EIP-137 §3.
    #[test]
    fn namehash_matches_eip137_vectors() {
        // namehash("") = 0x00..00
        assert_eq!(namehash(""), B256::ZERO);

        // namehash("eth")
        let eth: B256 = "0x93cdeb708b7545dc668eb9280176169d1c33cfd8ed6f04690a0bcc88a93fc4ae"
            .parse()
            .unwrap();
        assert_eq!(namehash("eth"), eth);

        // namehash("foo.eth")
        let foo_eth: B256 = "0xde9b09fd7c5f901e23a3f19fecc54828e9c848539801e86591bd9801b019f84f"
            .parse()
            .unwrap();
        assert_eq!(namehash("foo.eth"), foo_eth);
    }

    #[test]
    fn namehash_is_case_insensitive_for_ascii() {
        // `namehash` itself does ASCII-lowercase as a safety net; the
        // canonical entry point is `resolve_name` which normalizes first.
        assert_eq!(namehash("Vitalik.eth"), namehash("vitalik.eth"));
        assert_eq!(namehash("FOO.ETH"), namehash("foo.eth"));
    }

    #[test]
    fn normalize_lowercases_ascii() {
        assert_eq!(normalize("Vitalik.eth").unwrap(), "vitalik.eth");
        assert_eq!(normalize("VITALIK.ETH").unwrap(), "vitalik.eth");
    }

    #[test]
    fn normalize_rejects_bidi_override() {
        // U+202E RIGHT-TO-LEFT OVERRIDE — visually rearranges what follows.
        // Used in display-spoofing attacks; ENSIP-15 disallows it.
        let bad = "vit\u{202e}alik.eth";
        assert!(
            normalize(bad).is_err(),
            "bidi override must not normalize cleanly",
        );
    }

    #[test]
    fn normalize_handles_invisible_characters_safely() {
        // ZWS / ZWNJ / ZWJ / BOM between ASCII letters render as nothing,
        // so `vit{cp}alik.eth` looks identical to `vitalik.eth` on
        // screen. The unsafe outcome is `normalize(homograph)` returning
        // the homograph unchanged — that gives a *different namehash*
        // than the legitimate name, so an attacker who can register the
        // homograph at the registry level can pass forward verification.
        //
        // The safe outcomes are: rejection, OR collapse to the legitimate
        // namehash (so the lookup goes to the real owner, not the
        // attacker). Both are acceptable; we just refuse the third one.
        let legit_node = namehash("vitalik.eth");
        for cp in ['\u{200b}', '\u{200c}', '\u{200d}', '\u{feff}'] {
            let bad = format!("vit{cp}alik.eth");
            match normalize(&bad) {
                Err(_) => {}
                Ok(out) => {
                    let out_node = namehash(&out);
                    assert!(
                        out_node == legit_node || out != bad,
                        "{cp:?} normalized to a distinct namehash without rejection — \
                         this is the spoofing case (attacker registers homograph \
                         directly via raw namehash, passes forward verification)",
                    );
                }
            }
        }
    }

    #[test]
    fn normalize_rejects_nul_byte() {
        let bad = "vita\0lik.eth";
        assert!(normalize(bad).is_err(), "NUL must be rejected");
    }

    #[test]
    fn normalize_rejects_mixed_script_confusable() {
        // Cyrillic 'а' (U+0430) instead of Latin 'a'. Without ENSIP-15 the
        // namehash would silently differ from `vitalik.eth`; with
        // normalization it's flagged as a mixed-script label.
        let bad = "vit\u{0430}lik.eth";
        let result = normalize(bad);
        if let Ok(out) = &result {
            // If the normalizer accepted it, it must NOT be the same
            // string as the all-Latin form — otherwise we've collapsed a
            // confusable into the legitimate name, the worst outcome.
            assert_ne!(out, "vitalik.eth");
        }
    }

    #[test]
    fn looks_like_ens_accepts_dot_names_and_rejects_hex() {
        assert!(looks_like_ens("vitalik.eth"));
        assert!(looks_like_ens("foo.bar.eth"));
        assert!(!looks_like_ens(
            "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"
        ));
        assert!(!looks_like_ens("d8dA6BF26964aF9D7eEd9e03E53415D37aA96045"));
        assert!(!looks_like_ens(""));
        assert!(!looks_like_ens("vitalik"));
    }

    #[test]
    fn reverse_node_name_uses_lowercase_no_prefix() {
        let addr: Address = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"
            .parse()
            .unwrap();
        assert_eq!(
            reverse_node_name(addr),
            "d8da6bf26964af9d7eed9e03e53415d37aa96045.addr.reverse",
        );
    }

    #[test]
    fn decode_string_round_trips_short_ascii() {
        // ABI-encoded "hello":
        //   offset = 0x20
        //   length = 5
        //   data   = "hello" + 27 zero bytes of padding
        let mut buf = Vec::new();
        // offset
        let mut offset = [0u8; 32];
        offset[31] = 0x20;
        buf.extend_from_slice(&offset);
        // length
        let mut len = [0u8; 32];
        len[31] = 5;
        buf.extend_from_slice(&len);
        // data
        let mut data = [0u8; 32];
        data[..5].copy_from_slice(b"hello");
        buf.extend_from_slice(&data);
        assert_eq!(decode_string(&buf).as_deref(), Some("hello"));
    }

    #[test]
    fn decode_string_returns_none_on_short_input() {
        assert_eq!(decode_string(&[]), None);
        assert_eq!(decode_string(&[0u8; 32]), None);
    }

    #[test]
    fn decode_address_strips_left_padding() {
        let raw_addr: Address = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"
            .parse()
            .unwrap();
        let mut padded = [0u8; 32];
        padded[12..].copy_from_slice(raw_addr.as_slice());
        assert_eq!(decode_address(&padded), raw_addr);
    }

    #[test]
    fn selectors_match_keccak_signatures() {
        // The hand-computed selectors at the top of the file must match
        // keccak256(sig)[..4]. If one ever drifts, an ENS call hits the
        // wrong function and returns wrong-shape data with no warning.
        assert_eq!(
            &keccak256(b"resolver(bytes32)").as_slice()[..4],
            RESOLVER_SELECTOR.as_slice(),
        );
        assert_eq!(
            &keccak256(b"addr(bytes32)").as_slice()[..4],
            ADDR_SELECTOR.as_slice(),
        );
        assert_eq!(
            &keccak256(b"name(bytes32)").as_slice()[..4],
            NAME_SELECTOR.as_slice(),
        );
    }

    #[test]
    fn namehash_skips_empty_labels_from_leading_and_trailing_dots() {
        // Leading / trailing / doubled dots produce empty labels;
        // namehash treats those as no-op so the hash matches the cleaner
        // form. (Anti-confusion: it does NOT match the empty-name hash.)
        assert_eq!(namehash(".eth"), namehash("eth"));
        assert_eq!(namehash("eth."), namehash("eth"));
        assert_eq!(namehash("foo..eth"), namehash("foo.eth"));
    }

    #[test]
    fn namehash_distinct_for_different_labels() {
        assert_ne!(namehash("a.eth"), namehash("b.eth"));
        assert_ne!(namehash("foo"), namehash("foo.eth"));
    }

    #[test]
    fn beautify_idempotent_on_simple_ascii() {
        assert_eq!(beautify("vitalik.eth"), "vitalik.eth");
        assert_eq!(beautify("foo.eth"), "foo.eth");
    }

    #[test]
    fn beautify_returns_input_on_unbeautifiable() {
        // beautify falls back to the original string when it can't process.
        // A NUL byte is one case the normalizer rejects outright; pass it
        // through to confirm we don't panic.
        let weird = "vita\0lik.eth";
        let _ = beautify(weird); // must not panic
    }

    #[test]
    fn looks_like_ens_whitespace_trimmed() {
        assert!(looks_like_ens("  vitalik.eth  "));
        assert!(!looks_like_ens("   "));
    }

    #[test]
    fn decode_string_handles_offset_beyond_data() {
        // offset = 0x100, but data is only 64 bytes — must return None,
        // not panic.
        let mut buf = vec![0u8; 64];
        buf[30] = 0x01; // offset[30..32] = 0x0100
        buf[31] = 0x00;
        assert!(decode_string(&buf).is_none());
    }

    #[test]
    fn decode_string_handles_length_beyond_data() {
        let mut buf = Vec::new();
        let mut offset = [0u8; 32];
        offset[31] = 0x20;
        buf.extend_from_slice(&offset);
        let mut len = [0u8; 32];
        len[31] = 100; // claims 100 bytes but we only have 0 after the length word
        buf.extend_from_slice(&len);
        assert!(decode_string(&buf).is_none());
    }

    #[test]
    fn decode_string_rejects_invalid_utf8() {
        let mut buf = Vec::new();
        let mut offset = [0u8; 32];
        offset[31] = 0x20;
        buf.extend_from_slice(&offset);
        let mut len = [0u8; 32];
        len[31] = 1;
        buf.extend_from_slice(&len);
        let mut data = [0u8; 32];
        data[0] = 0xff; // invalid UTF-8 lead byte standing alone
        buf.extend_from_slice(&data);
        assert!(decode_string(&buf).is_none());
    }
}

/// Adversarial / phishing-focused tests. These are deliberately probing —
/// some are expected to fail, in which case the failure characterizes a real
/// gap (either in `ens-normalize-rs`, in the resolver's structural defenses,
/// or in the ABI decoder's robustness against crafted RPC responses).
#[cfg(test)]
mod phishing_tests {
    use super::*;

    // ---- A. Homoglyph classes ----

    #[test]
    fn normalize_rejects_mathematical_alphanumeric() {
        // Mathematical Bold Small Latin (U+1D41A..U+1D433). Renders nearly
        // identically to ASCII in many fonts; an attacker can use this for a
        // visual spoof of `vitalik.eth`. Codepoints below spell "vitalik".
        let bad = "\u{1D42F}\u{1D422}\u{1D42D}\u{1D41A}\u{1D425}\u{1D422}\u{1D424}.eth";
        match normalize(bad) {
            Err(_) => {} // safe — rejected outright
            Ok(out) => {
                // Acceptance is only safe if the form was canonicalized to
                // ASCII so the namehash matches the legitimate name (lookup
                // goes to the real owner, not the attacker's registration).
                assert_eq!(
                    namehash(&out),
                    namehash("vitalik.eth"),
                    "math-bold homograph accepted as {out:?} with distinct namehash — \
                     attacker can register this codepoint sequence and pass forward verification",
                );
            }
        }
    }

    #[test]
    fn normalize_handles_fullwidth_latin_safely() {
        // Fullwidth Latin (U+FF21..U+FF5A). Renders identically to ASCII.
        // Codepoints below spell "vitalik".
        let bad = "\u{FF56}\u{FF49}\u{FF54}\u{FF41}\u{FF4C}\u{FF49}\u{FF4B}.eth";
        match normalize(bad) {
            Err(_) => {}
            Ok(out) => {
                assert_eq!(
                    namehash(&out),
                    namehash("vitalik.eth"),
                    "fullwidth latin accepted as {out:?} with distinct namehash — \
                     visual-identical spoof vector",
                );
            }
        }
    }

    #[test]
    fn normalize_handles_greek_confusables_safely() {
        // Greek small omicron (U+03BF) for Latin 'o'. Mixed Latin + Greek
        // is a classic confusable combination ENSIP-15 should flag.
        let bad = "vitalik\u{03BF}.eth";
        if let Ok(out) = normalize(bad) {
            // If accepted, namehash must NOT collide with any all-Latin form
            // the attacker is impersonating, AND the output must not be a
            // visual-identical drop-in (we just check distinctness from the
            // obvious ascii rendering).
            assert_ne!(
                out, "vitaliko.eth",
                "Greek omicron silently mapped to Latin 'o' — confusable lost its distinguishing identity"
            );
        }
    }

    #[test]
    fn normalize_handles_combining_diacritics_safely() {
        // ASCII "vitalik" + COMBINING ACUTE ACCENT (U+0301). Renders as a
        // distinguishable form, but a careless reader may not notice the
        // accent on a single character. The dangerous case is acceptance with
        // a namehash that collides with `vitalik.eth` (lets attacker bind a
        // visually similar input to the legit hash).
        let bad = "vitalik\u{0301}.eth";
        if let Ok(out) = normalize(bad) {
            // If output is byte-identical to "vitalik.eth", that's a silent
            // strip of the accent — fine for security but worth flagging.
            // The actually-bad case is: distinct from "vitalik.eth" but same
            // namehash. namehash is deterministic on bytes so this is only
            // possible via the empty-label / dot-tricks paths.
            if out != "vitalik.eth" {
                assert_ne!(
                    namehash(&out),
                    namehash("vitalik.eth"),
                    "combining-accent name {out:?} hashes to vitalik.eth without being it — \
                     namehash collision via structural quirk",
                );
            }
        }
    }

    #[test]
    fn normalize_rejects_tag_characters() {
        // Unicode "Tag" block U+E0000..U+E007F is invisible in virtually
        // every font. ENSIP-15 disallows them. U+E0061 = TAG LATIN SMALL
        // LETTER A.
        let bad = "vitalik\u{E0061}.eth";
        assert!(
            normalize(bad).is_err(),
            "Unicode Tag character accepted — invisible-char spoof vector",
        );
    }

    // ---- B. Structural attacks on the namehash construction ----

    #[test]
    fn normalize_rejects_empty_labels() {
        // namehash() (src/ens.rs:88-95) silently skips empty labels — see the
        // `if label.is_empty() { continue; }` branch. That makes
        // namehash("vitalik..eth") == namehash("vitalik.eth"). The defense
        // must live in normalize(): if it accepts a name with empty labels,
        // resolve_name on a malformed input silently routes to a different
        // canonical owner.
        for bad in [
            "vitalik..eth",
            ".vitalik.eth",
            "vitalik.eth.",
            "vitalik.",
            "..eth",
        ] {
            assert!(
                normalize(bad).is_err(),
                "empty-label name {bad:?} accepted by normalize — \
                 namehash collision: an unusual rendering routes to the canonical owner",
            );
        }
    }

    #[test]
    fn normalize_rejects_or_canonicalizes_unicode_dots() {
        // namehash() splits ONLY on ASCII '.'. If normalize() accepts Unicode
        // dot-alikes without canonicalizing them to ASCII '.', the user's
        // mental model of "two labels separated by a dot" diverges from the
        // namehash structure (the whole string becomes one giant label).
        for dot in ['\u{FF0E}', '\u{3002}', '\u{2024}'] {
            let bad = format!("vitalik{dot}eth");
            match normalize(&bad) {
                Err(_) => {}
                Ok(out) => {
                    assert!(
                        out.contains('.') && !out.contains(dot),
                        "Unicode dot {dot:?} survived normalization in {out:?} — \
                         namehash splits only on ASCII '.', so the user's perceived label \
                         boundary doesn't match the hash structure",
                    );
                }
            }
        }
    }

    #[test]
    fn namehash_splits_only_on_ascii_dot() {
        // Defensive pin: confirm namehash() does NOT treat Unicode dot-alikes
        // as label separators. If it ever started, an attacker could craft
        // names that hash to the same node under different label decompositions.
        let with_unicode_dot = "vitalik\u{FF0E}eth";
        // Hash treats the whole thing as one label.
        let single_label_hash = {
            let label_hash = keccak256(with_unicode_dot.as_bytes());
            let mut buf = [0u8; 64];
            buf[..32].copy_from_slice(B256::ZERO.as_slice());
            buf[32..].copy_from_slice(label_hash.as_slice());
            keccak256(buf)
        };
        assert_eq!(namehash(with_unicode_dot), single_label_hash);
        assert_ne!(namehash(with_unicode_dot), namehash("vitalik.eth"));
    }

    // ---- C. Whitespace / control characters ----

    #[test]
    fn normalize_rejects_unicode_whitespace() {
        // Whitespace inside an ENS label is illegal under ENSIP-15. NBSP
        // (U+00A0), IDEOGRAPHIC SPACE (U+3000), LINE SEPARATOR (U+2028) and
        // friends look like a regular space (or like nothing) but bypass
        // ASCII-only whitespace checks.
        for cp in [' ', '\t', '\n', '\u{00A0}', '\u{2028}', '\u{3000}'] {
            let bad = format!("vit{cp}alik.eth");
            assert!(
                normalize(&bad).is_err(),
                "whitespace {cp:?} accepted inside ENS label — invisible-spacer spoof vector",
            );
        }
    }

    // ---- D. beautify ↔ normalize round-trip invariant ----

    #[test]
    fn beautify_roundtrips_through_normalize() {
        // lookup_address (src/ens.rs:212) returns beautify(normalized) to the
        // UI. The user trusts that displayed string. Soundness requires that
        // the displayed string, when re-normalized, yields the same canonical
        // form we forward-verified — otherwise the on-chain identity of what
        // the user sees differs from what we checked.
        let cases = [
            "vitalik.eth",
            "foo.bar.eth",
            "a.b.c.d.eth",
            "hello-world.eth",
        ];
        for input in cases {
            let normalized = normalize(input).unwrap();
            let beautified = beautify(&normalized);
            let renormalized = normalize(&beautified).unwrap_or_else(|e| {
                panic!(
                    "beautify({normalized:?}) -> {beautified:?} which fails to renormalize: {e} — \
                     UI display would not pass a re-verification round-trip"
                )
            });
            assert_eq!(
                renormalized, normalized,
                "beautify({normalized:?}) -> {beautified:?} renormalizes to {renormalized:?} — \
                 UI-displayed name's canonical identity differs from what was forward-verified",
            );
        }
    }

    // ---- E. decode_string robustness against crafted RPC responses ----

    #[test]
    fn decode_string_oversized_length_returns_none() {
        // Adversarial response: valid offset (32) but length = U256::MAX.
        // U256::try_into::<usize>() returns None for values that don't fit;
        // the function should bail early with None rather than allocating
        // or slicing.
        let mut buf = vec![0u8; 96];
        buf[31] = 0x20; // offset = 32
        for byte in buf.iter_mut().skip(32).take(32) {
            *byte = 0xff; // length = U256::MAX
        }
        let result = std::panic::catch_unwind(|| decode_string(&buf));
        match result {
            Ok(v) => assert_eq!(v, None, "expected None for oversized length"),
            Err(_) => panic!(
                "decode_string panicked on oversized length — DoS vector via crafted RPC response"
            ),
        }
    }

    #[test]
    fn decode_string_offset_overflow_does_not_panic() {
        // offset = u64::MAX (low 8 bytes of U256). On 64-bit platforms this
        // fits in usize, so try_into succeeds. Then `start = offset + 32`
        // overflows, which in debug builds panics with "attempt to add with
        // overflow" — a DoS bug. Safe behavior: return None.
        let mut buf = vec![0u8; 96];
        // Set bytes 24..32 to 0xff so the U256's low 64 bits = u64::MAX,
        // and the upper 192 bits stay zero so try_into::<usize>() succeeds.
        for byte in buf.iter_mut().skip(24).take(8) {
            *byte = 0xff;
        }
        let result = std::panic::catch_unwind(|| decode_string(&buf));
        match result {
            Ok(v) => assert_eq!(v, None, "expected None for overflow-prone offset"),
            Err(_) => panic!(
                "decode_string panicked on offset = u64::MAX — \
                 arithmetic overflow in `offset + 32`, DoS via crafted RPC response"
            ),
        }
    }

    #[test]
    fn decode_string_length_overflow_does_not_panic() {
        // Valid offset (32, so start = 64). Length crafted so start + len
        // overflows: len = u64::MAX - 60 → 64 + len wraps. In debug this
        // panics on the addition; in release it wraps and the subsequent
        // slice panics on out-of-bounds.
        let mut buf = vec![0u8; 96];
        buf[31] = 0x20; // offset = 32
        // length field at bytes 32..64; place u64::MAX - 60 in low 8 bytes.
        let len_val: u64 = u64::MAX - 60;
        buf[56..64].copy_from_slice(&len_val.to_be_bytes());
        let result = std::panic::catch_unwind(|| decode_string(&buf));
        match result {
            Ok(v) => assert_eq!(v, None, "expected None for overflow-prone length"),
            Err(_) => panic!(
                "decode_string panicked on length = u64::MAX-60 — \
                 arithmetic overflow in `start + len`, DoS via crafted RPC response"
            ),
        }
    }

    // ---- F. looks_like_ens heuristic boundary ----

    #[test]
    fn looks_like_ens_does_not_match_unicode_dots() {
        // Pinning test: looks_like_ens checks for ASCII '.' only. A homograph
        // input with only fullwidth dots is NOT classified as ENS — it falls
        // through to the address-parse branch, fails, and the UI reports
        // invalid input rather than silently routing the resolution.
        let s = "vitalik\u{FF0E}eth";
        assert!(
            !looks_like_ens(s),
            "looks_like_ens matched a Unicode-dot input — would route a homograph to ENS resolution silently",
        );
    }
}

/// Trust-boundary tests for the verified-call routing: every ENS read must
/// go through Helios's verified `eth_call`, and an unverified answer (raw-RPC
/// fallback) must never be returned as a resolved address. Drives the
/// `CallMock` double — keyed on exact `(target, calldata)` with a per-call
/// `verified` flag — so the registry/resolver round-trip runs without a live
/// light client.
#[cfg(test)]
mod verified_resolution_tests {
    use super::*;
    use crate::net::CallMock;

    /// `selector` + 32-byte `node`, exactly as `verified_call` encodes it —
    /// the key the mock matches against.
    fn calldata(selector: [u8; 4], node: B256) -> Bytes {
        let mut d = Vec::with_capacity(36);
        d.extend_from_slice(&selector);
        d.extend_from_slice(node.as_slice());
        Bytes::from(d)
    }

    /// Left-pad a 20-byte address into a 32-byte ABI word, the shape
    /// `decode_address` expects from `resolver()` / `addr()`.
    fn abi_address(a: Address) -> Bytes {
        let mut b = [0u8; 32];
        b[12..].copy_from_slice(a.as_slice());
        Bytes::from(b.to_vec())
    }

    const RESOLVER: Address = address!("0x1111111111111111111111111111111111111111");
    const TARGET: Address = address!("0x2222222222222222222222222222222222222222");

    #[tokio::test]
    async fn resolve_name_returns_address_when_fully_verified() {
        let net = CallMock::new();
        let node = namehash(&normalize("vitalik.eth").unwrap());
        net.set_call(
            ENS_REGISTRY,
            calldata(RESOLVER_SELECTOR, node),
            abi_address(RESOLVER),
            true,
        );
        net.set_call(
            RESOLVER,
            calldata(ADDR_SELECTOR, node),
            abi_address(TARGET),
            true,
        );

        let got = resolve_name(&net, "vitalik.eth").await.unwrap();
        assert_eq!(got, Some(TARGET));
    }

    #[tokio::test]
    async fn resolve_name_fails_closed_when_registry_read_unverified() {
        // A hostile exec RPC answers `resolver(node)` with an attacker-chosen
        // resolver but cannot produce a Helios proof → verified=false. The
        // resolved address feeds the signed recipient, so this must error
        // rather than return Some(addr).
        let net = CallMock::new();
        let node = namehash(&normalize("vitalik.eth").unwrap());
        net.set_call(
            ENS_REGISTRY,
            calldata(RESOLVER_SELECTOR, node),
            abi_address(RESOLVER),
            false,
        );

        let res = resolve_name(&net, "vitalik.eth").await;
        assert!(
            res.is_err(),
            "unverified registry read must fail closed, got {res:?}",
        );
    }

    #[tokio::test]
    async fn resolve_name_fails_closed_when_addr_read_unverified() {
        // The registry read is verified, but the `addr(node)` record itself
        // comes back unverified — the spoofable value. Still must fail closed.
        let net = CallMock::new();
        let node = namehash(&normalize("vitalik.eth").unwrap());
        net.set_call(
            ENS_REGISTRY,
            calldata(RESOLVER_SELECTOR, node),
            abi_address(RESOLVER),
            true,
        );
        net.set_call(
            RESOLVER,
            calldata(ADDR_SELECTOR, node),
            abi_address(TARGET),
            false,
        );

        let res = resolve_name(&net, "vitalik.eth").await;
        assert!(
            res.is_err(),
            "unverified addr read must fail closed, got {res:?}",
        );
    }

    #[tokio::test]
    async fn lookup_address_fails_closed_when_unverified() {
        // Reverse display: an unverified reverse-resolver read must surface as
        // an error (the call sites render no name) rather than a fabricated one.
        let net = CallMock::new();
        let addr: Address = address!("0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045");
        let reverse_node = namehash(&reverse_node_name(addr));
        net.set_call(
            ENS_REGISTRY,
            calldata(RESOLVER_SELECTOR, reverse_node),
            abi_address(RESOLVER),
            false,
        );

        let res = lookup_address(&net, addr).await;
        assert!(
            res.is_err(),
            "unverified reverse read must fail closed, got {res:?}",
        );
    }
}
