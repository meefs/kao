//! Wei Name Service (WNS) — the `.wei` namespace
//! (<https://github.com/z0r0z/wei-names>).
//!
//! A single `NameNFT` contract is both registry and resolver, so resolution
//! reuses the shared [`crate::names::NftNameService`] core — this module only
//! supplies the deployment constant. Forward goes through `addr(bytes32)`,
//! reverse through `reverseResolve(address)`; both ride the Helios-verified
//! mainnet path and fail closed (see [`crate::names`]). GNS ([`crate::names::gns`]) is
//! an ownerless fork of this contract with an identical resolver interface.
//!
//! Pinned to Ethereum mainnet (the only chain WNS is deployed on).

use alloy::primitives::address;

use crate::names::NftNameService;

/// WNS `NameNFT` (registry + resolver), Ethereum mainnet.
/// <https://etherscan.io/address/0x0000000000696760E15f265e828DB644A0c242EB>
pub(crate) const WNS: NftNameService = NftNameService {
    registry: address!("0x0000000000696760E15f265e828DB644A0c242EB"),
    tld: ".wei",
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::ens::namehash;
    use alloy::primitives::B256;

    #[test]
    fn namehash_of_wei_tld_matches_contract_constant() {
        // WNS's NameNFT hard-codes `WEI_NODE = namehash("wei")` and derives
        // every token id as `namehash(label.wei)` from it (EIP-137). Our
        // forward path calls `addr(namehash(full_name))` against the contract,
        // so `namehash("wei")` MUST equal the contract's `WEI_NODE` — else
        // every lookup hits the wrong node and silently misses. Value pinned
        // from `cast namehash wei` and the on-chain `WEI_NODE` constant.
        let wei_node: B256 = "0xa82820059d5df798546bcc2985157a77c3eef25eba9ba01899927333efacbd6f"
            .parse()
            .unwrap();
        assert_eq!(namehash("wei"), wei_node);
    }

    #[test]
    fn tld_is_dot_wei() {
        assert_eq!(WNS.tld, ".wei");
    }
}
