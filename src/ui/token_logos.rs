//! Bundled ERC-20 token logo bitmaps, sourced from
//! <https://github.com/trustwallet/assets>. Embedded with `include_bytes!`
//! so Kao ships as a single static binary and never has to fetch logos at
//! runtime — fetching would leak the user's holdings to whichever CDN
//! served them, which would be at odds with the privacy posture of the
//! Helios-backed RPC layer.
//!
//! Coverage is intentionally narrow: only the tokens we actually surface
//! get a logo. Anything not in this table falls back to the kaomoji
//! avatar in `kao_widgets::token_avatar`, which is a deliberate part of
//! the wallet's identity rather than a placeholder.
//!
//! Logos are keyed by lowercase ERC-20 contract address. Native ETH uses
//! the sentinel id [`NATIVE_ETH`] since it has no contract address.

use std::sync::LazyLock;

use alloy::primitives::{Address, address};
use iced::widget::image::Handle;

pub const NATIVE_ETH: &str = "eth_native";

const USDC_ADDR: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
const LINK_ADDR: Address = address!("0x514910771af9ca656af840dff83e8264ecf986ca");
const WBTC_ADDR: Address = address!("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599");

static ETH_NATIVE_HANDLE: LazyLock<Handle> = LazyLock::new(|| {
    Handle::from_bytes(
        include_bytes!("../../assets/token_logos/ethereum/_native.png").as_slice(),
    )
});
static USDC_HANDLE: LazyLock<Handle> = LazyLock::new(|| {
    Handle::from_bytes(
        include_bytes!("../../assets/token_logos/ethereum/0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48.png").as_slice(),
    )
});
static LINK_HANDLE: LazyLock<Handle> = LazyLock::new(|| {
    Handle::from_bytes(
        include_bytes!("../../assets/token_logos/ethereum/0x514910771af9ca656af840dff83e8264ecf986ca.png").as_slice(),
    )
});
static WBTC_HANDLE: LazyLock<Handle> = LazyLock::new(|| {
    Handle::from_bytes(
        include_bytes!("../../assets/token_logos/ethereum/0x2260fac5e5542a773aa44fbcfedf7c193bc2c599.png").as_slice(),
    )
});

/// Look up the bundled logo for `id`. Returns `None` for any token we
/// don't ship a bitmap for; the caller is expected to fall back to the
/// kaomoji avatar.
pub fn handle(id: &str) -> Option<Handle> {
    let h = match id {
        NATIVE_ETH => &*ETH_NATIVE_HANDLE,
        "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48" => &*USDC_HANDLE,
        "0x514910771af9ca656af840dff83e8264ecf986ca" => &*LINK_HANDLE,
        "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599" => &*WBTC_HANDLE,
        _ => return None,
    };
    Some(h.clone())
}

/// Look up the bundled `logo_id` for a given ERC-20 contract. Returns
/// `None` when no bundled bitmap exists, matching the semantics of
/// `handle()` on the same id. Used by indexer-driven balance fetches that
/// only know the contract `Address`, not the curated string id.
pub fn logo_id_for(contract: Address) -> Option<&'static str> {
    if contract == USDC_ADDR {
        Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
    } else if contract == LINK_ADDR {
        Some("0x514910771af9ca656af840dff83e8264ecf986ca")
    } else if contract == WBTC_ADDR {
        Some("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599")
    } else {
        None
    }
}
