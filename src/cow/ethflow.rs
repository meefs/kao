//! Native-ETH selling via the CoWSwapEthFlow contract.
//!
//! CoW can't settle native ETH directly (orders are over ERC-20s). For an ETH
//! sell we call `createOrder` on the EthFlow contract ([`super::ETHFLOW`]) with
//! the ETH as `msg.value` (= `sellAmount + feeAmount`); the contract holds the
//! ETH, sells WETH on the user's behalf, and validates the order via EIP-1271.
//!
//! The resulting GPv2 order the orderbook tracks has `owner = ETHFLOW`,
//! `sellToken = WETH`, and `validTo = u32::MAX` (the contract overrides the
//! caller's validTo — the struct's own `validTo` only governs the user's
//! on-chain refund-after-expiry path). [`ethflow_uid`] reproduces those
//! overrides so we poll the right UID; getting them wrong means polling a UID
//! the orderbook never knew about.

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;

use crate::chain::Chain;

use super::ETHFLOW;
use super::order::{Order, apply_slippage, cow_domain, order_digest, order_uid};

sol! {
    /// `EthFlowOrder.Data`. Field order/types are ABI-load-bearing — they must
    /// match the contract or `createOrder` calldata is malformed. Verified by
    /// `create_order_selector_matches_contract` (the selector derived here is
    /// present in the deployed bytecode).
    #[derive(Debug)]
    struct EthFlowData {
        address buyToken;
        address receiver;
        uint256 sellAmount;
        uint256 buyAmount;
        bytes32 appData;
        uint256 feeAmount;
        uint32 validTo;
        bool partiallyFillable;
        int64 quoteId;
    }

    function createOrder(EthFlowData order) external payable returns (bytes32 orderHash);
    function invalidateOrder(EthFlowData order) external;
}

/// Build the EthFlow order data from a quote. `buy_token` is the token the user
/// wants; `sell_amount` is the FULL ETH the user parts with (quote's
/// `sellAmount + feeAmount`); `valid_to`/`quote_id` come from the quote; the
/// signed `buyAmount` is the quoted amount minus `slippage_bps`. `receiver` must
/// be the user's address (the contract rejects the zero sentinel).
///
/// `feeAmount` is fixed to **0** — like EOA orders, the modern orderbook takes
/// the network fee dynamically, so `msg.value` is just the sell amount.
///
/// `app_data` is the keccak256 of the order's appData document (see
/// [`super::market_app_data`]). Because the native path never POSTs an order
/// body, the caller must separately upload the pre-image
/// ([`super::api::upload_app_data`]) so the orderbook can read its `orderClass`.
#[allow(clippy::too_many_arguments)]
pub fn build_ethflow_data(
    buy_token: Address,
    receiver: Address,
    sell_amount: U256,
    quoted_buy_amount: U256,
    valid_to: u32,
    quote_id: i64,
    slippage_bps: u16,
    app_data: B256,
) -> EthFlowData {
    EthFlowData {
        buyToken: buy_token,
        receiver,
        sellAmount: sell_amount,
        buyAmount: apply_slippage(quoted_buy_amount, slippage_bps),
        appData: app_data,
        feeAmount: U256::ZERO,
        validTo: valid_to,
        partiallyFillable: false,
        quoteId: quote_id,
    }
}

/// `createOrder(order)` calldata.
pub fn create_order_calldata(d: &EthFlowData) -> Bytes {
    Bytes::from(createOrderCall { order: d.clone() }.abi_encode())
}

/// `invalidateOrder(order)` calldata — the on-chain cancel that refunds the
/// deposited ETH to the owner. Wired into the engine for the deferred EthFlow
/// cancellation path (the Apps pane hides cancel for EthFlow orders in v1).
#[allow(dead_code)]
pub fn invalidate_calldata(d: &EthFlowData) -> Bytes {
    Bytes::from(invalidateOrderCall { order: d.clone() }.abi_encode())
}

/// ETH to send with `createOrder`: the sell amount plus the signed fee (which is
/// 0 in the modern fee-less model, so this is just the sell amount). The
/// contract reverts if underfunded; any surplus is only recoverable via
/// `invalidateOrder`.
pub fn msg_value(d: &EthFlowData) -> U256 {
    d.sellAmount.saturating_add(d.feeAmount)
}

/// The orderbook UID for the GPv2 order the EthFlow contract creates. The
/// equivalent order sells **WETH**, is owned by **ETHFLOW**, and has
/// `validTo = u32::MAX` (both in the signed struct and the UID suffix).
pub fn ethflow_uid(d: &EthFlowData, chain: Chain) -> Result<[u8; 56], String> {
    let weth = super::wrapped_native(chain)
        .ok_or_else(|| format!("no wrapped-native token for {}", chain.label()))?;
    let order = Order {
        sellToken: weth,
        buyToken: d.buyToken,
        receiver: d.receiver,
        sellAmount: d.sellAmount,
        buyAmount: d.buyAmount,
        validTo: u32::MAX,
        appData: d.appData,
        feeAmount: d.feeAmount,
        kind: "sell".to_string(),
        partiallyFillable: d.partiallyFillable,
        sellTokenBalance: "erc20".to_string(),
        buyTokenBalance: "erc20".to_string(),
    };
    let digest = order_digest(&order, &cow_domain(chain));
    Ok(order_uid(digest, ETHFLOW, u32::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn sample() -> EthFlowData {
        build_ethflow_data(
            address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            address!("0x1111111111111111111111111111111111111111"),
            U256::from(1_000_000_000_000_000_000u64),
            U256::from(2_500_000_000u64),
            1_900_000_000,
            42,
            50,
            super::super::market_app_data(50).1,
        )
    }

    #[test]
    fn create_order_selector_matches_contract() {
        // 0x322bba21 is the selector present in the deployed EthFlow bytecode
        // on Mainnet and Base; pinning it guards the struct field order.
        let cd = create_order_calldata(&sample());
        assert_eq!(&cd.as_ref()[0..4], &[0x32, 0x2b, 0xba, 0x21]);
    }

    #[test]
    fn invalidate_order_selector_matches_contract() {
        let cd = invalidate_calldata(&sample());
        assert_eq!(&cd.as_ref()[0..4], &[0x7b, 0xc4, 0x1b, 0x96]);
    }

    #[test]
    fn msg_value_is_sell_amount_with_zero_fee() {
        let d = sample();
        assert_eq!(d.feeAmount, U256::ZERO, "modern orders carry a zero fee");
        // With fee = 0, msg.value is exactly the sell amount.
        assert_eq!(msg_value(&d), U256::from(1_000_000_000_000_000_000u64));
    }

    #[test]
    fn ethflow_uid_uses_ethflow_owner_and_max_validto() {
        let uid = ethflow_uid(&sample(), Chain::Base).unwrap();
        assert_eq!(uid.len(), 56);
        // owner segment is the EthFlow contract, NOT the user.
        assert_eq!(&uid[32..52], ETHFLOW.as_slice());
        // validTo segment is u32::MAX.
        assert_eq!(&uid[52..56], &[0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn ethflow_uid_unsupported_chain_errs() {
        assert!(ethflow_uid(&sample(), Chain::Optimism).is_err());
    }
}
