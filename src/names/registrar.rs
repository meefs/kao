//! Registration / renewal / record-management calldata for the three
//! first-class namespaces — ENS (`.eth`), GNS (`.gwei`), WNS (`.wei`).
//!
//! This is the **pure** layer: contract addresses, lifecycle constants, token-id
//! derivation, ABI encoding (via `sol!`, so the byte layouts are correct by
//! construction) and return decoding. It performs no I/O — the async
//! orchestration (verified reads, broadcasting) lives in [`super::manage`].
//!
//! ## Why the three look symmetric here
//!
//! All three use the **same** lifecycle: a two-transaction commit/reveal
//! registration (a mandatory 60s..24h wait between the commit and the
//! reveal/register), an expiry with a 90-day grace window, and a renewal. They
//! diverge in three places, which is exactly what [`Namespace`] encapsulates:
//!
//! 1. **Topology.** ENS is multi-contract (a registrar *controller* for
//!    register/renew/price, a `BaseRegistrar` ERC-721 for expiry/ownership, and a
//!    separate `PublicResolver` for the address record). GNS/WNS are
//!    *single-contract* `NameNFT`s that are registrar + resolver + NFT at once.
//! 2. **Token id.** An ENS 2LD's id is the bare `labelhash` (`keccak256(label)`);
//!    a GNS/WNS id is the full EIP-137 `namehash(label.tld)`. The resolver *node*
//!    (what `setAddr` keys on) is `namehash(label.eth)` for ENS and `bytes32(id)`
//!    for GNS/WNS. Conflating these three hashes targets a nonexistent token, so
//!    they are derived in exactly one place ([`Namespace::token_id`] /
//!    [`Namespace::node`]).
//! 3. **Pricing.** ENS prices via a USD→ETH oracle (`rentPrice → base+premium`);
//!    GNS/WNS price by UTF-8 byte length (`getFee`) plus a post-grace dutch-
//!    auction `getPremium`. Both are read live — never hardcoded — because WNS's
//!    fees are owner-mutable and the ENS oracle drifts with ETH price.
//!
//! ## Commitments are computed on-chain
//!
//! The commit step's `bytes32` is produced by the contract's own
//! `makeCommitment` (a `pure`/`view` read), not re-derived client-side. The
//! commitment binds the full registration parameters; replicating its hashing
//! (ABI-encoding a dynamic `bytes`, matching the on-chain label normalization,
//! ordering an 8-field struct) is the easiest place to introduce a mismatch that
//! would silently waste the commit gas. Letting the contract hash it — and
//! reusing the *identical* parameter values for the reveal/register — makes the
//! two steps agree by construction. See [`super::manage`] for the flow.

use alloy::primitives::{Address, B256, Bytes, U256, address, keccak256};
use alloy::sol_types::SolCall;

use crate::names::ens;

// ── Mainnet contract addresses (EIP-55 checksummed; `address!` validates the
//    checksum at compile time, so a typo is a build error, not a fund-loss bug).

/// ENS ETHRegistrarController **v2** (the current controller `controller.ens.eth`
/// resolves to). Takes a single `Registration` struct and a `uint8`
/// `reverseRecord` bitmask. The legacy controller `0x2535…303b` has *different*
/// selectors and a flat 8-arg signature — do not target it.
pub(crate) const ENS_CONTROLLER: Address = address!("0x59E16fcCd424Cc24e280Be16E11Bcd56fb0CE547");

/// ENS `BaseRegistrarImplementation` — the ERC-721 `.eth` collection. Source of
/// `nameExpires` / `ownerOf` for a 2LD (keyed by `labelhash`).
pub(crate) const ENS_BASE_REGISTRAR: Address =
    address!("0x57f1887a8BF19b14fC0dF6Fd9B2acc9Af147eA85");

/// ENS `PublicResolver` (what `resolver.ens.eth` points to). The default
/// resolver new registrations are pointed at, and the target of `setAddr`.
pub(crate) const ENS_PUBLIC_RESOLVER: Address =
    address!("0x231b0Ee14048e9dCcD1d247744d114a4EB5E8E63");

// ── Lifecycle constants (all verified against live mainnet `eth_call`) ──

/// Minimum age a commitment must reach before the reveal/register is accepted.
/// Identical (60s) for all three namespaces.
pub const MIN_COMMITMENT_AGE: u64 = 60;
/// Maximum age before a commitment expires and must be redone (24h, all three).
pub const MAX_COMMITMENT_AGE: u64 = 86_400;
/// Post-expiry window during which only the prior owner may renew (90 days, all
/// three). The name does **not** resolve during grace — it is purely a renewal
/// window.
pub const GRACE_PERIOD: u64 = 7_776_000;
/// ENS minimum registration duration (28 days). GNS/WNS ignore caller duration —
/// they always grant a fixed 365-day period.
pub const ENS_MIN_DURATION: u64 = 2_419_200;
/// One 365-day year in seconds. The GNS/WNS fixed term, and the unit the ENS
/// duration slider works in.
pub const YEAR_SECONDS: u64 = 31_536_000;

/// Registration duration in seconds for `years`, floored at the 28-day ENS
/// minimum. A no-op for the ≥1-year UI, but it keeps the on-chain floor explicit
/// (and is harmless for GNS/WNS, which ignore the caller's duration and always
/// grant a fixed year).
pub fn ens_duration_secs(years: u32) -> u64 {
    (years as u64 * YEAR_SECONDS).max(ENS_MIN_DURATION)
}

// ── ABI definitions ──────────────────────────────────────────────────────────
//
// Each contract gets its own `sol!` module so overloaded names that exist across
// contracts (`makeCommitment`, `renew`, `ownerOf`, `setAddr`) don't collide in
// the generated Rust.

/// ENS ETHRegistrarController v2 (`ENS_CONTROLLER`).
pub(crate) mod ens_ctrl {
    use alloy::sol;
    sol! {
        struct Registration {
            string label;
            address owner;
            uint256 duration;
            bytes32 secret;
            address resolver;
            bytes[] data;
            uint8 reverseRecord;
            bytes32 referrer;
        }
        function rentPrice(string name, uint256 duration) external view returns (uint256 base, uint256 premium);
        function available(string name) external view returns (bool);
        function valid(string name) external pure returns (bool);
        function makeCommitment(Registration registration) external pure returns (bytes32);
        function commit(bytes32 commitment) external;
        function register(Registration registration) external payable;
        function renew(string name, uint256 duration, bytes32 referrer) external payable;
    }
}

/// ENS BaseRegistrar (`ENS_BASE_REGISTRAR`) + Registry owner read.
pub(crate) mod ens_reg {
    use alloy::sol;
    sol! {
        function nameExpires(uint256 id) external view returns (uint256);
        function ownerOf(uint256 id) external view returns (address);
        // ENS Registry owner(node) — used to detect wrapped names / manager.
        function owner(bytes32 node) external view returns (address);
    }
}

/// ENS PublicResolver (`ENS_PUBLIC_RESOLVER`).
pub(crate) mod ens_resolver {
    use alloy::sol;
    sol! {
        function setAddr(bytes32 node, address a) external;
        function addr(bytes32 node) external view returns (address);
    }
}

/// GNS/WNS single-contract `NameNFT` (registrar + resolver + NFT).
pub(crate) mod nft {
    use alloy::sol;
    sol! {
        function makeCommitment(string label, address owner, bytes32 secret) external view returns (bytes32);
        function commit(bytes32 commitment) external;
        function reveal(string label, bytes32 secret) external payable returns (uint256 tokenId);
        function renew(uint256 tokenId) external payable;
        function isAvailable(string label, uint256 parentId) external view returns (bool);
        function expiresAt(uint256 tokenId) external view returns (uint256);
        function getFee(uint256 length) external view returns (uint256);
        function getPremium(uint256 tokenId) external view returns (uint256);
        function computeId(string name) external view returns (uint256);
        function setAddr(uint256 tokenId, address a) external;
        function resolve(uint256 tokenId) external view returns (address);
        function ownerOf(uint256 tokenId) external view returns (address);
    }
}

/// The three namespaces this wallet can register, renew and manage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// ENS — `.eth` second-level domains.
    Ens,
    /// GNS — `.gwei` (gwei-names).
    Gns,
    /// WNS — `.wei` (wei-names).
    Wns,
}

impl Namespace {
    /// Lowercase TLD suffix including the dot (`".eth"` / `".gwei"` / `".wei"`).
    pub fn tld(self) -> &'static str {
        match self {
            Namespace::Ens => ".eth",
            Namespace::Gns => ".gwei",
            Namespace::Wns => ".wei",
        }
    }

    /// Strip this namespace's TLD from a full name, returning the bare label.
    /// Returns the input unchanged if it doesn't carry the TLD.
    pub fn strip_tld(self, name: &str) -> String {
        let lower = name.trim();
        lower.strip_suffix(self.tld()).unwrap_or(lower).to_string()
    }

    /// The single contract for a single-contract namespace (GNS/WNS). `None` for
    /// ENS, which is multi-contract.
    pub fn nft_contract(self) -> Option<Address> {
        match self {
            Namespace::Ens => None,
            Namespace::Gns => Some(crate::names::gns::GNS.registry),
            Namespace::Wns => Some(crate::names::wns::WNS.registry),
        }
    }

    // ── hashing (the one place each hash is derived) ──────────────────────────

    /// The ERC-721/NFT **token id** for a bare `label`.
    ///
    /// - ENS: `uint256(labelhash)` = `uint256(keccak256(label))`.
    /// - GNS/WNS: `uint256(namehash(label.tld))` (full EIP-137 namehash).
    ///
    /// `label` must already be normalized (the caller applies [`ens::normalize`]).
    pub fn token_id(self, label: &str) -> U256 {
        match self {
            Namespace::Ens => U256::from_be_bytes(keccak256(label).0),
            Namespace::Gns | Namespace::Wns => U256::from_be_bytes(self.node(label).0),
        }
    }

    /// The resolver **node** `setAddr`/`addr` key for a bare `label`.
    ///
    /// - ENS: `namehash("label.eth")`.
    /// - GNS/WNS: `bytes32(token_id)` = `namehash("label.tld")`.
    pub fn node(self, label: &str) -> B256 {
        let full = format!("{label}{}", self.tld());
        ens::namehash(&full)
    }

    // ── read calldata (paired with a decoder below) ───────────────────────────

    /// `(to, calldata)` for an availability check on a bare `label`.
    /// Decode with [`decode_bool`].
    pub fn availability_call(self, label: &str) -> (Address, Bytes) {
        match self {
            Namespace::Ens => (
                ENS_CONTROLLER,
                Bytes::from(
                    ens_ctrl::availableCall {
                        name: label.to_string(),
                    }
                    .abi_encode(),
                ),
            ),
            Namespace::Gns | Namespace::Wns => (
                self.nft_contract().expect("nft namespace"),
                Bytes::from(
                    nft::isAvailableCall {
                        label: label.to_string(),
                        parentId: U256::ZERO,
                    }
                    .abi_encode(),
                ),
            ),
        }
    }

    /// `(to, calldata)` reading the on-chain expiry timestamp (unix seconds) for a
    /// bare `label`. Decode with [`decode_u256`] → seconds. A value of 0 means
    /// "no such name" (or a never-expiring subdomain, which this wallet doesn't
    /// register).
    pub fn expiry_call(self, label: &str) -> (Address, Bytes) {
        let id = self.token_id(label);
        match self {
            Namespace::Ens => (
                ENS_BASE_REGISTRAR,
                Bytes::from(ens_reg::nameExpiresCall { id }.abi_encode()),
            ),
            Namespace::Gns | Namespace::Wns => (
                self.nft_contract().expect("nft namespace"),
                Bytes::from(nft::expiresAtCall { tokenId: id }.abi_encode()),
            ),
        }
    }

    /// `(to, calldata)` reading the current owner of a bare `label`. Decode with
    /// [`decode_address`]; zero means "no owner / nonexistent".
    ///
    /// ENS uses the **Registry** `owner(node)` rather than `BaseRegistrar.ownerOf`
    /// on purpose: `ownerOf` *reverts* the instant a name passes its expiry
    /// (during the 90-day grace window), which would make exactly the
    /// "renew-now-or-lose-it" names invisible. The registry's node owner persists
    /// through grace and is only reset on re-registration, so it keeps reporting
    /// the registrant (for an unwrapped name) the whole time the name is at risk.
    /// GNS/WNS keep `ownerOf` returning the registrant during grace, so they read
    /// it directly.
    pub fn owner_of_call(self, label: &str) -> (Address, Bytes) {
        match self {
            Namespace::Ens => (
                crate::names::ens::ENS_REGISTRY,
                Bytes::from(
                    ens_reg::ownerCall {
                        node: self.node(label),
                    }
                    .abi_encode(),
                ),
            ),
            Namespace::Gns | Namespace::Wns => (
                self.nft_contract().expect("nft namespace"),
                Bytes::from(
                    nft::ownerOfCall {
                        tokenId: self.token_id(label),
                    }
                    .abi_encode(),
                ),
            ),
        }
    }

    // ── write calldata: `(to, value, calldata)` ───────────────────────────────

    /// The commit transaction. `commitment` comes from the contract's
    /// `makeCommitment` (see [`Namespace::make_commitment_call`]). Value is zero.
    pub fn commit_call(self, commitment: B256) -> (Address, U256, Bytes) {
        match self {
            Namespace::Ens => (
                ENS_CONTROLLER,
                U256::ZERO,
                Bytes::from(ens_ctrl::commitCall { commitment }.abi_encode()),
            ),
            Namespace::Gns | Namespace::Wns => (
                self.nft_contract().expect("nft namespace"),
                U256::ZERO,
                Bytes::from(nft::commitCall { commitment }.abi_encode()),
            ),
        }
    }

    /// `(to, calldata)` to compute the commitment on-chain for `plan`. Decode the
    /// returned bytes with [`decode_b256`].
    pub fn make_commitment_call(self, plan: &RegisterPlan) -> (Address, Bytes) {
        match self {
            Namespace::Ens => (
                ENS_CONTROLLER,
                Bytes::from(
                    ens_ctrl::makeCommitmentCall {
                        registration: plan.ens_registration(),
                    }
                    .abi_encode(),
                ),
            ),
            Namespace::Gns | Namespace::Wns => (
                self.nft_contract().expect("nft namespace"),
                Bytes::from(
                    nft::makeCommitmentCall {
                        label: plan.label.clone(),
                        owner: plan.owner,
                        secret: plan.secret,
                    }
                    .abi_encode(),
                ),
            ),
        }
    }

    /// The reveal/register transaction that actually creates the name.
    /// `value` is the cost to send (caller adds a small buffer; the contracts
    /// refund any excess). For ENS this is `register(Registration)`; for GNS/WNS
    /// `reveal(label, secret)`.
    pub fn register_call(self, plan: &RegisterPlan, value: U256) -> (Address, U256, Bytes) {
        match self {
            Namespace::Ens => (
                ENS_CONTROLLER,
                value,
                Bytes::from(
                    ens_ctrl::registerCall {
                        registration: plan.ens_registration(),
                    }
                    .abi_encode(),
                ),
            ),
            Namespace::Gns | Namespace::Wns => (
                self.nft_contract().expect("nft namespace"),
                value,
                Bytes::from(
                    nft::revealCall {
                        label: plan.label.clone(),
                        secret: plan.secret,
                    }
                    .abi_encode(),
                ),
            ),
        }
    }

    /// The renewal ("prolong") transaction for a bare `label`. `duration_secs` is
    /// used only by ENS (GNS/WNS always add a fixed 365-day term and ignore it).
    /// `value` is the cost to send (excess refunded).
    pub fn renew_call(
        self,
        label: &str,
        duration_secs: u64,
        value: U256,
    ) -> (Address, U256, Bytes) {
        match self {
            Namespace::Ens => (
                ENS_CONTROLLER,
                value,
                Bytes::from(
                    ens_ctrl::renewCall {
                        name: label.to_string(),
                        duration: U256::from(duration_secs),
                        referrer: B256::ZERO,
                    }
                    .abi_encode(),
                ),
            ),
            Namespace::Gns | Namespace::Wns => (
                self.nft_contract().expect("nft namespace"),
                value,
                Bytes::from(
                    nft::renewCall {
                        tokenId: self.token_id(label),
                    }
                    .abi_encode(),
                ),
            ),
        }
    }

    /// The "set recipient" transaction: point `label` at `recipient` (the address
    /// it resolves to, which may differ from the NFT owner). For ENS this is
    /// `setAddr(node, addr)` on the PublicResolver; for GNS/WNS it's
    /// `setAddr(tokenId, addr)` on the single contract. Value is zero.
    pub fn set_addr_call(self, label: &str, recipient: Address) -> (Address, U256, Bytes) {
        match self {
            Namespace::Ens => (
                ENS_PUBLIC_RESOLVER,
                U256::ZERO,
                Bytes::from(
                    ens_resolver::setAddrCall {
                        node: self.node(label),
                        a: recipient,
                    }
                    .abi_encode(),
                ),
            ),
            Namespace::Gns | Namespace::Wns => (
                self.nft_contract().expect("nft namespace"),
                U256::ZERO,
                Bytes::from(
                    nft::setAddrCall {
                        tokenId: self.token_id(label),
                        a: recipient,
                    }
                    .abi_encode(),
                ),
            ),
        }
    }

    // ── pricing read calldata ─────────────────────────────────────────────────

    /// `(to, calldata)` for a registration/renewal price quote.
    ///
    /// - ENS: `rentPrice(label, duration)` → `(base, premium)` (decode with
    ///   [`decode_price_pair`]).
    /// - GNS/WNS: `getFee(byteLen)` → fee (decode with [`decode_u256`]); the
    ///   dutch-auction premium is a separate [`Namespace::premium_call`].
    pub fn price_call(self, label: &str, duration_secs: u64) -> (Address, Bytes) {
        match self {
            Namespace::Ens => (
                ENS_CONTROLLER,
                Bytes::from(
                    ens_ctrl::rentPriceCall {
                        name: label.to_string(),
                        duration: U256::from(duration_secs),
                    }
                    .abi_encode(),
                ),
            ),
            Namespace::Gns | Namespace::Wns => (
                self.nft_contract().expect("nft namespace"),
                Bytes::from(
                    nft::getFeeCall {
                        // GNS/WNS price by UTF-8 *byte* length, not codepoints
                        // (`str::len` is the byte length).
                        length: U256::from(label.len()),
                    }
                    .abi_encode(),
                ),
            ),
        }
    }

    /// `(to, calldata)` for the GNS/WNS post-grace dutch-auction premium on a bare
    /// `label`. `None` for ENS (its premium is folded into `rentPrice`). Decode
    /// with [`decode_u256`].
    pub fn premium_call(self, label: &str) -> Option<(Address, Bytes)> {
        match self {
            Namespace::Ens => None,
            Namespace::Gns | Namespace::Wns => Some((
                self.nft_contract().expect("nft namespace"),
                Bytes::from(
                    nft::getPremiumCall {
                        tokenId: self.token_id(label),
                    }
                    .abi_encode(),
                ),
            )),
        }
    }
}

/// Everything needed to register one name — held identically across the commit
/// and the reveal/register so the on-chain `makeCommitment` agrees with the
/// reveal by construction. The `secret` is a client-generated nonce that must be
/// persisted between the two transactions (the 60s..24h window).
#[derive(Debug, Clone)]
pub struct RegisterPlan {
    pub namespace: Namespace,
    /// Bare, already-normalized label (no TLD), e.g. `"vitalik"`.
    pub label: String,
    /// Who receives the name — may differ from the signing account.
    pub owner: Address,
    /// Requested term in seconds. ENS honours it (≥ 28 days); GNS/WNS grant a
    /// fixed 365-day term regardless, but it's retained for display.
    pub duration_secs: u64,
    /// Random commit/reveal nonce.
    pub secret: B256,
}

impl RegisterPlan {
    /// Build the ENS `Registration` struct used for *both* `makeCommitment` and
    /// `register`. New registrations are pointed at the PublicResolver with an
    /// atomic `setAddr(node, owner)` so the name resolves to its owner the moment
    /// it's created (parity with GNS/WNS, whose `resolve` falls back to the NFT
    /// owner). `reverseRecord = 0` (we don't auto-set a primary name) and
    /// `referrer = 0`.
    pub(crate) fn ens_registration(&self) -> ens_ctrl::Registration {
        let node = self.namespace.node(&self.label);
        let set_addr = ens_resolver::setAddrCall {
            node,
            a: self.owner,
        }
        .abi_encode();
        ens_ctrl::Registration {
            label: self.label.clone(),
            owner: self.owner,
            duration: U256::from(self.duration_secs),
            secret: self.secret,
            resolver: ENS_PUBLIC_RESOLVER,
            data: vec![Bytes::from(set_addr)],
            reverseRecord: 0,
            referrer: B256::ZERO,
        }
    }
}

// ── return decoders ────────────────────────────────────────────────────────

/// Decode an ABI `bool` return (last byte of the first 32-byte word).
pub fn decode_bool(data: &[u8]) -> bool {
    data.len() >= 32 && data[31] != 0
}

/// Decode a single ABI `uint256` return. Short/empty data → 0.
pub fn decode_u256(data: &[u8]) -> U256 {
    if data.len() < 32 {
        return U256::ZERO;
    }
    U256::from_be_slice(&data[..32])
}

/// Decode a single ABI `uint256`/`bytes32` return as a `B256`. Short data → zero.
pub fn decode_b256(data: &[u8]) -> B256 {
    if data.len() < 32 {
        return B256::ZERO;
    }
    B256::from_slice(&data[..32])
}

/// Decode a left-padded ABI `address` return. Short data → zero address.
pub fn decode_address(data: &[u8]) -> Address {
    if data.len() < 32 {
        return Address::ZERO;
    }
    Address::from_slice(&data[12..32])
}

/// Decode the ENS `rentPrice` `(uint256 base, uint256 premium)` pair. Short data
/// → `(0, 0)`.
pub fn decode_price_pair(data: &[u8]) -> (U256, U256) {
    if data.len() < 64 {
        return (U256::ZERO, U256::ZERO);
    }
    (
        U256::from_be_slice(&data[..32]),
        U256::from_be_slice(&data[32..64]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::sol_types::SolValue;

    // ── selector pins ────────────────────────────────────────────────────────
    // Every 4-byte selector is pinned against the values independently verified
    // (canonical ABI + `cast sig`) in research. A drift here means a malformed
    // call to the wrong function — at best a revert, at worst targeting the wrong
    // contract method with value attached.

    #[test]
    fn ens_controller_selectors() {
        assert_eq!(ens_ctrl::rentPriceCall::SELECTOR, [0x83, 0xe7, 0xf6, 0xff]);
        assert_eq!(ens_ctrl::availableCall::SELECTOR, [0xae, 0xb8, 0xce, 0x9b]);
        assert_eq!(ens_ctrl::validCall::SELECTOR, [0x97, 0x91, 0xc0, 0x97]);
        assert_eq!(
            ens_ctrl::makeCommitmentCall::SELECTOR,
            [0xcf, 0x7d, 0x6e, 0x01]
        );
        assert_eq!(ens_ctrl::commitCall::SELECTOR, [0xf1, 0x4f, 0xcb, 0xc8]);
        assert_eq!(ens_ctrl::registerCall::SELECTOR, [0xef, 0x9c, 0x88, 0x05]);
        assert_eq!(ens_ctrl::renewCall::SELECTOR, [0x18, 0x02, 0x6a, 0xd1]);
    }

    #[test]
    fn ens_registrar_and_resolver_selectors() {
        assert_eq!(ens_reg::nameExpiresCall::SELECTOR, [0xd6, 0xe4, 0xfa, 0x86]);
        assert_eq!(ens_reg::ownerOfCall::SELECTOR, [0x63, 0x52, 0x21, 0x1e]);
        assert_eq!(ens_reg::ownerCall::SELECTOR, [0x02, 0x57, 0x1b, 0xe3]);
        assert_eq!(
            ens_resolver::setAddrCall::SELECTOR,
            [0xd5, 0xfa, 0x2b, 0x00]
        );
        assert_eq!(ens_resolver::addrCall::SELECTOR, [0x3b, 0x3b, 0x57, 0xde]);
    }

    #[test]
    fn nft_selectors() {
        assert_eq!(nft::makeCommitmentCall::SELECTOR, [0xf4, 0x98, 0x26, 0xbe]);
        assert_eq!(nft::commitCall::SELECTOR, [0xf1, 0x4f, 0xcb, 0xc8]);
        assert_eq!(nft::revealCall::SELECTOR, [0xea, 0x93, 0x84, 0xfa]);
        assert_eq!(nft::renewCall::SELECTOR, [0x5b, 0xaa, 0x75, 0x09]);
        assert_eq!(nft::isAvailableCall::SELECTOR, [0x8f, 0x8d, 0xc3, 0x86]);
        assert_eq!(nft::expiresAtCall::SELECTOR, [0x17, 0xc9, 0x57, 0x09]);
        assert_eq!(nft::getFeeCall::SELECTOR, [0xfc, 0xee, 0x45, 0xf4]);
        assert_eq!(nft::getPremiumCall::SELECTOR, [0x1b, 0xf1, 0xff, 0xfb]);
        assert_eq!(nft::computeIdCall::SELECTOR, [0xfb, 0x02, 0x19, 0x39]);
        assert_eq!(nft::setAddrCall::SELECTOR, [0xeb, 0xa3, 0x6d, 0xbd]);
        assert_eq!(nft::resolveCall::SELECTOR, [0x4f, 0x89, 0x6d, 0x4f]);
        assert_eq!(nft::ownerOfCall::SELECTOR, [0x63, 0x52, 0x21, 0x1e]);
    }

    // ── namespace routing ────────────────────────────────────────────────────

    #[test]
    fn strip_tld_is_inverse_of_appending() {
        assert_eq!(Namespace::Ens.strip_tld("vitalik.eth"), "vitalik");
        assert_eq!(Namespace::Gns.strip_tld("apoorv.gwei"), "apoorv");
        assert_eq!(Namespace::Wns.strip_tld("z.wei"), "z");
        // No TLD present → unchanged.
        assert_eq!(Namespace::Ens.strip_tld("vitalik"), "vitalik");
    }

    // ── token id / node derivation ───────────────────────────────────────────

    #[test]
    fn ens_token_id_is_labelhash_not_namehash() {
        // Verified vector: keccak256("vitalik") = 0xaf2caa…03cc.
        let expected = U256::from_be_bytes(keccak256(b"vitalik").0);
        assert_eq!(Namespace::Ens.token_id("vitalik"), expected);
        // And it is NOT the namehash of vitalik.eth.
        assert_ne!(
            Namespace::Ens.token_id("vitalik"),
            U256::from_be_bytes(ens::namehash("vitalik.eth").0)
        );
    }

    #[test]
    fn gns_wns_token_id_is_full_namehash() {
        assert_eq!(
            Namespace::Gns.token_id("apoorv"),
            U256::from_be_bytes(ens::namehash("apoorv.gwei").0)
        );
        assert_eq!(
            Namespace::Wns.token_id("z0r0z"),
            U256::from_be_bytes(ens::namehash("z0r0z.wei").0)
        );
        // For GNS/WNS the resolver node == bytes32(token_id).
        assert_eq!(
            B256::from(Namespace::Gns.token_id("apoorv").to_be_bytes()),
            Namespace::Gns.node("apoorv")
        );
    }

    #[test]
    fn ens_node_is_full_namehash_of_dot_eth() {
        assert_eq!(Namespace::Ens.node("vitalik"), ens::namehash("vitalik.eth"));
    }

    // ── write calldata layout ────────────────────────────────────────────────

    fn plan(ns: Namespace) -> RegisterPlan {
        RegisterPlan {
            namespace: ns,
            label: "vitalik".to_string(),
            owner: address!("0x000000000000000000000000000000000000bEEF"),
            duration_secs: YEAR_SECONDS,
            secret: B256::repeat_byte(0x11),
        }
    }

    #[test]
    fn commit_call_targets_right_contract_with_zero_value() {
        let commitment = B256::repeat_byte(0xAB);
        let (to, value, data) = Namespace::Ens.commit_call(commitment);
        assert_eq!(to, ENS_CONTROLLER);
        assert_eq!(value, U256::ZERO);
        assert_eq!(&data[0..4], &[0xf1, 0x4f, 0xcb, 0xc8]);
        // commitment is the single 32-byte arg.
        assert_eq!(&data[4..36], commitment.as_slice());

        let (to, _, data) = Namespace::Gns.commit_call(commitment);
        assert_eq!(to, crate::names::gns::GNS.registry);
        assert_eq!(&data[0..4], &[0xf1, 0x4f, 0xcb, 0xc8]);
        assert_eq!(&data[4..36], commitment.as_slice());
    }

    #[test]
    fn nft_register_is_reveal_with_label_and_secret() {
        let p = plan(Namespace::Wns);
        let value = U256::from(1_000u64);
        let (to, v, data) = Namespace::Wns.register_call(&p, value);
        assert_eq!(to, crate::names::wns::WNS.registry);
        assert_eq!(v, value, "value is the fee to send");
        assert_eq!(&data[0..4], &[0xea, 0x93, 0x84, 0xfa], "reveal selector");
        // Decodes back to the same label + secret.
        let decoded = nft::revealCall::abi_decode(&data).unwrap();
        assert_eq!(decoded.label, "vitalik");
        assert_eq!(decoded.secret, p.secret);
    }

    #[test]
    fn ens_register_roundtrips_struct_with_atomic_setaddr() {
        let p = plan(Namespace::Ens);
        let value = U256::from(5_000u64);
        let (to, v, data) = Namespace::Ens.register_call(&p, value);
        assert_eq!(to, ENS_CONTROLLER);
        assert_eq!(v, value);
        assert_eq!(&data[0..4], &[0xef, 0x9c, 0x88, 0x05], "register selector");
        let reg = ens_ctrl::registerCall::abi_decode(&data)
            .unwrap()
            .registration;
        assert_eq!(reg.label, "vitalik");
        assert_eq!(reg.owner, p.owner);
        assert_eq!(reg.duration, U256::from(YEAR_SECONDS));
        assert_eq!(reg.secret, p.secret);
        assert_eq!(reg.resolver, ENS_PUBLIC_RESOLVER);
        assert_eq!(reg.reverseRecord, 0);
        assert_eq!(reg.referrer, B256::ZERO);
        // The atomic record-set: data[0] is setAddr(node, owner) on the resolver.
        assert_eq!(reg.data.len(), 1);
        let inner = ens_resolver::setAddrCall::abi_decode(&reg.data[0]).unwrap();
        assert_eq!(inner.node, Namespace::Ens.node("vitalik"));
        assert_eq!(inner.a, p.owner);
    }

    #[test]
    fn make_commitment_and_register_use_identical_registration() {
        // The commit and the register must encode the SAME Registration, or the
        // on-chain makeCommitment won't match the reveal. They share
        // `ens_registration`, so assert the embedded struct is byte-identical.
        let p = plan(Namespace::Ens);
        let (_, mk) = Namespace::Ens.make_commitment_call(&p);
        let (_, _, rg) = Namespace::Ens.register_call(&p, U256::from(1u64));
        let mk_reg = ens_ctrl::makeCommitmentCall::abi_decode(&mk)
            .unwrap()
            .registration;
        let rg_reg = ens_ctrl::registerCall::abi_decode(&rg)
            .unwrap()
            .registration;
        assert_eq!(mk_reg.abi_encode(), rg_reg.abi_encode());
    }

    #[test]
    fn renew_call_per_namespace_shape() {
        // ENS renew takes the string label + duration + referrer.
        let (to, v, data) = Namespace::Ens.renew_call("vitalik", YEAR_SECONDS, U256::from(7u64));
        assert_eq!(to, ENS_CONTROLLER);
        assert_eq!(v, U256::from(7u64));
        assert_eq!(&data[0..4], &[0x18, 0x02, 0x6a, 0xd1]);
        let d = ens_ctrl::renewCall::abi_decode(&data).unwrap();
        assert_eq!(d.name, "vitalik");
        assert_eq!(d.duration, U256::from(YEAR_SECONDS));

        // GNS/WNS renew takes only the tokenId; duration is ignored.
        let (to, v, data) = Namespace::Gns.renew_call("apoorv", 999, U256::from(3u64));
        assert_eq!(to, crate::names::gns::GNS.registry);
        assert_eq!(v, U256::from(3u64));
        assert_eq!(&data[0..4], &[0x5b, 0xaa, 0x75, 0x09]);
        let d = nft::renewCall::abi_decode(&data).unwrap();
        assert_eq!(d.tokenId, Namespace::Gns.token_id("apoorv"));
    }

    #[test]
    fn set_addr_call_per_namespace_shape() {
        let recipient = address!("0x00000000000000000000000000000000DeaDBeef");
        // ENS → resolver.setAddr(node, addr).
        let (to, v, data) = Namespace::Ens.set_addr_call("vitalik", recipient);
        assert_eq!(to, ENS_PUBLIC_RESOLVER);
        assert_eq!(v, U256::ZERO);
        let d = ens_resolver::setAddrCall::abi_decode(&data).unwrap();
        assert_eq!(d.node, Namespace::Ens.node("vitalik"));
        assert_eq!(d.a, recipient);

        // GNS → contract.setAddr(tokenId, addr).
        let (to, _, data) = Namespace::Gns.set_addr_call("apoorv", recipient);
        assert_eq!(to, crate::names::gns::GNS.registry);
        let d = nft::setAddrCall::abi_decode(&data).unwrap();
        assert_eq!(d.tokenId, Namespace::Gns.token_id("apoorv"));
        assert_eq!(d.a, recipient);
    }

    #[test]
    fn ens_owner_read_targets_registry_not_base_registrar() {
        // ENS ownership must be read from the Registry `owner(node)` (persists
        // through the grace window) rather than BaseRegistrar.ownerOf (reverts
        // once expired) — otherwise grace-period names vanish from the UI.
        let (to, data) = Namespace::Ens.owner_of_call("vitalik");
        assert_eq!(to, crate::names::ens::ENS_REGISTRY);
        assert_eq!(
            &data[0..4],
            &[0x02, 0x57, 0x1b, 0xe3],
            "owner(bytes32) selector"
        );
        let d = ens_reg::ownerCall::abi_decode(&data).unwrap();
        assert_eq!(d.node, Namespace::Ens.node("vitalik"));
        // GNS/WNS keep reading ownerOf(tokenId) on their single contract.
        let (gto, gdata) = Namespace::Gns.owner_of_call("apoorv");
        assert_eq!(gto, crate::names::gns::GNS.registry);
        assert_eq!(&gdata[0..4], &[0x63, 0x52, 0x21, 0x1e]);
        assert_eq!(
            nft::ownerOfCall::abi_decode(&gdata).unwrap().tokenId,
            Namespace::Gns.token_id("apoorv")
        );
    }

    #[test]
    fn price_call_uses_byte_length_for_nft() {
        // 4-byte label → getFee(4). Emoji would be >1 byte each (intentional).
        let (to, data) = Namespace::Gns.price_call("alic", YEAR_SECONDS);
        assert_eq!(to, crate::names::gns::GNS.registry);
        assert_eq!(&data[0..4], &[0xfc, 0xee, 0x45, 0xf4]);
        let d = nft::getFeeCall::abi_decode(&data).unwrap();
        assert_eq!(d.length, U256::from(4u64));

        // ENS uses rentPrice(name, duration).
        let (to, data) = Namespace::Ens.price_call("vitalik", YEAR_SECONDS);
        assert_eq!(to, ENS_CONTROLLER);
        assert_eq!(&data[0..4], &[0x83, 0xe7, 0xf6, 0xff]);
        let d = ens_ctrl::rentPriceCall::abi_decode(&data).unwrap();
        assert_eq!(d.duration, U256::from(YEAR_SECONDS));

        // ENS has no separate premium call; GNS/WNS do.
        assert!(Namespace::Ens.premium_call("vitalik").is_none());
        assert!(Namespace::Gns.premium_call("apoorv").is_some());
    }

    // ── decoders ──────────────────────────────────────────────────────────────

    #[test]
    fn decoders_handle_values_and_short_data() {
        let mut t = [0u8; 32];
        t[31] = 1;
        assert!(decode_bool(&t));
        assert!(!decode_bool(&[0u8; 32]));
        assert!(!decode_bool(&[1u8; 4]), "short data is false");

        let mut w = [0u8; 32];
        w[31] = 0x2a;
        assert_eq!(decode_u256(&w), U256::from(42u64));
        assert_eq!(decode_u256(&[0u8; 8]), U256::ZERO);

        let mut pair = [0u8; 64];
        pair[31] = 5; // base
        pair[63] = 9; // premium
        let (base, prem) = decode_price_pair(&pair);
        assert_eq!(base, U256::from(5u64));
        assert_eq!(prem, U256::from(9u64));
        assert_eq!(decode_price_pair(&[0u8; 32]), (U256::ZERO, U256::ZERO));

        let mut a = [0u8; 32];
        a[12..].copy_from_slice(address!("0x00000000000000000000000000000000DeaDBeef").as_slice());
        assert_eq!(
            decode_address(&a),
            address!("0x00000000000000000000000000000000DeaDBeef")
        );
        assert_eq!(decode_address(&[0u8; 4]), Address::ZERO);
    }

    #[test]
    fn lifecycle_constants_match_chain() {
        assert_eq!(MIN_COMMITMENT_AGE, 60);
        assert_eq!(MAX_COMMITMENT_AGE, 86_400);
        assert_eq!(GRACE_PERIOD, 90 * 86_400);
        assert_eq!(ENS_MIN_DURATION, 28 * 86_400);
        assert_eq!(YEAR_SECONDS, 365 * 86_400);
    }
}
