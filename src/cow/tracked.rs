//! In-session record of an order the user has placed, plus the orderbook's
//! status enum. The dashboard owns a `Vec<TrackedOrder>` so a placed order
//! survives closing the swap modal and navigating between panes; a polling
//! subscription refreshes each non-terminal order's [`OrderStatus`].

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use crate::chain::Chain;

use super::order::OrderKind;

/// Orderbook order status. Wire values: `presignaturePending`, `open`,
/// `fulfilled`, `cancelled`, `expired`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OrderStatus {
    /// Awaiting an on-chain pre-signature (presign orders) — transient.
    PresignaturePending,
    /// Live in the orderbook, not yet (fully) settled.
    Open,
    /// Fully settled.
    Fulfilled,
    /// Cancelled (off-chain signed cancel, or on-chain for EthFlow).
    Cancelled,
    /// `validTo` passed without (full) settlement.
    Expired,
}

impl OrderStatus {
    /// Whether the order has reached a final state and no longer needs
    /// polling. The poll subscription self-disables once every tracked order
    /// is terminal.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderStatus::Fulfilled | OrderStatus::Cancelled | OrderStatus::Expired
        )
    }

    /// Short human label for the status badge.
    pub fn label(self) -> &'static str {
        match self {
            OrderStatus::PresignaturePending => "Pending",
            OrderStatus::Open => "Open",
            OrderStatus::Fulfilled => "Filled",
            OrderStatus::Cancelled => "Cancelled",
            OrderStatus::Expired => "Expired",
        }
    }
}

/// One order the user placed this session, with enough metadata to render a row
/// (symbols + amounts + decimals) without re-fetching. Fields are scalar /
/// serde-able so a future redb-backed persistent store is a clean follow-up.
#[derive(Debug, Clone)]
pub struct TrackedOrder {
    /// 56-byte UID, `0x`-prefixed hex (112 hex chars). Orderbook poll key.
    pub uid: String,
    pub chain: Chain,
    /// The wallet address that placed the order. The Apps list filters to the
    /// active account so switching accounts doesn't show another's orders.
    pub owner: Address,
    /// Order side — always `Sell` in v1; kept for wire/display fidelity.
    #[allow(dead_code)]
    pub kind: OrderKind,
    /// The ERC-20 sold (WETH for a native sell — see `is_ethflow`). Carried so
    /// the post-fill targeted balance refresh knows which contract to refetch.
    pub sell_token: Address,
    /// The ERC-20 bought. Same purpose as `sell_token` for the buy leg.
    pub buy_token: Address,
    pub sell_symbol: String,
    pub buy_symbol: String,
    /// Amount sold (atoms).
    pub sell_amount: U256,
    /// Minimum buy amount signed (atoms) — the user's slippage floor.
    pub buy_amount: U256,
    pub sell_decimals: u8,
    pub buy_decimals: u8,
    pub status: OrderStatus,
    /// `(executedSellAmount, executedBuyAmount)` once (partially) filled.
    pub executed: Option<(U256, U256)>,
    /// Native-ETH order placed via EthFlow — cancellation is on-chain, not a
    /// signed DELETE.
    pub is_ethflow: bool,
}

impl TrackedOrder {
    /// Fold a fresh status poll into this record. Executed amounts only ever
    /// move forward, so a poll that omits them (e.g. a 404 mapped elsewhere)
    /// never clears a known fill.
    pub fn apply_status(&mut self, status: OrderStatus, executed: Option<(U256, U256)>) {
        self.status = status;
        if executed.is_some() {
            self.executed = executed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn order() -> TrackedOrder {
        TrackedOrder {
            uid: "0x00".to_string(),
            chain: Chain::Base,
            owner: address!("0x1111111111111111111111111111111111111111"),
            kind: OrderKind::Sell,
            sell_token: address!("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
            buy_token: address!("0x4200000000000000000000000000000000000006"),
            sell_symbol: "USDC".into(),
            buy_symbol: "WETH".into(),
            sell_amount: U256::from(1_000_000u64),
            buy_amount: U256::from(1u64),
            sell_decimals: 6,
            buy_decimals: 18,
            status: OrderStatus::Open,
            executed: None,
            is_ethflow: false,
        }
    }

    #[test]
    fn terminal_classification() {
        assert!(!OrderStatus::Open.is_terminal());
        assert!(!OrderStatus::PresignaturePending.is_terminal());
        assert!(OrderStatus::Fulfilled.is_terminal());
        assert!(OrderStatus::Cancelled.is_terminal());
        assert!(OrderStatus::Expired.is_terminal());
    }

    #[test]
    fn apply_status_updates_and_preserves_fill() {
        let mut o = order();
        o.apply_status(
            OrderStatus::Fulfilled,
            Some((U256::from(1_000_000u64), U256::from(5u64))),
        );
        assert_eq!(o.status, OrderStatus::Fulfilled);
        assert_eq!(
            o.executed,
            Some((U256::from(1_000_000u64), U256::from(5u64)))
        );
        // A later poll without executed amounts must not wipe the fill.
        o.apply_status(OrderStatus::Fulfilled, None);
        assert_eq!(
            o.executed,
            Some((U256::from(1_000_000u64), U256::from(5u64)))
        );
    }

    #[test]
    fn status_serde_wire_values() {
        assert_eq!(
            serde_json::to_string(&OrderStatus::PresignaturePending).unwrap(),
            "\"presignaturePending\""
        );
        assert_eq!(
            serde_json::from_str::<OrderStatus>("\"fulfilled\"").unwrap(),
            OrderStatus::Fulfilled
        );
    }
}
