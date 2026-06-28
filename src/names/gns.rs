//! Gwei Name Service (GNS) — the `.gwei` namespace
//! (<https://github.com/lucadonnoh/gwei-names>).
//!
//! An ownerless, non-upgradeable fork of WNS ([`crate::names::wns`]). A single
//! `NameNFT` contract is both registry and resolver, so resolution reuses the
//! shared [`crate::names::NftNameService`] core — this module only supplies the
//! deployment constant. Forward goes through `addr(bytes32)`, reverse through
//! `reverseResolve(address)`; both ride the Helios-verified mainnet path and
//! fail closed (see [`crate::names`]).
//!
//! Pinned to Ethereum mainnet. GNS is also deployed at the *same* address on
//! Sepolia (identical deployer + nonces), but every name read goes through the
//! verified mainnet light client regardless of the chain being viewed —
//! matching the ENS posture in [`crate::names::ens`].

use alloy::primitives::address;

use crate::names::NftNameService;

/// GNS `NameNFT` (registry + resolver), Ethereum mainnet.
/// <https://etherscan.io/address/0x9D51D507BC7264d4fE8Ad1cf7Fe191933A0a81d6>
pub(crate) const GNS: NftNameService = NftNameService {
    registry: address!("0x9D51D507BC7264d4fE8Ad1cf7Fe191933A0a81d6"),
    tld: ".gwei",
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::ens::namehash;
    use alloy::primitives::B256;

    #[test]
    fn namehash_of_gwei_tld_matches_contract_constant() {
        // GNS's NameNFT hard-codes `GWEI_NODE = namehash("gwei")` and derives
        // every token id as `namehash(label.gwei)` from it (EIP-137). Our
        // forward path calls `addr(namehash(full_name))` against the contract,
        // so `namehash("gwei")` MUST equal the contract's `GWEI_NODE` — else
        // every lookup hits the wrong node and silently misses. Value pinned
        // from `cast namehash gwei` and the on-chain `GWEI_NODE` constant.
        let gwei_node: B256 = "0xcca9c7f2dbe2808af0de2982fc84314bfa68a82a6a60ad5cd757f91a233d7d7f"
            .parse()
            .unwrap();
        assert_eq!(namehash("gwei"), gwei_node);
    }

    #[test]
    fn tld_is_dot_gwei() {
        assert_eq!(GNS.tld, ".gwei");
    }
}
