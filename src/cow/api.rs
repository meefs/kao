//! CoW orderbook REST client — quotes, order submission, status, and
//! cancellation.
//!
//! Every call rides the shared proxied HTTP client
//! ([`crate::indexer::http_client_or_err`]) and routes reqwest errors through
//! [`crate::indexer::redact_url_in_err`] so an API key or path never lands in a
//! log line. Nothing here runs on a timer — each function is invoked directly
//! in response to an explicit user action (or, for [`get_order`], a poll that
//! only exists because the user already placed an order).
//!
//! Amounts cross the wire as decimal strings (`"1000000"`), not hex — hence the
//! [`u256_dec`] serde adapter.

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use crate::chain::Chain;
use crate::indexer::{http_client_or_err, redact_url_in_err};

use super::tracked::OrderStatus;

/// Resolve the orderbook base URL for `chain`, or a user-facing error if CoW
/// doesn't run there.
fn base(chain: Chain) -> Result<&'static str, String> {
    super::api_base(chain).ok_or_else(|| format!("CoW Swap is not available on {}", chain.label()))
}

// ── Wire DTOs ────────────────────────────────────────────────────────────────

/// `POST /quote` body for a sell quote. `valid_for` is a relative window (secs)
/// so we never depend on local-clock accuracy; the response echoes the absolute
/// `valid_to` we then sign. For a native-ETH (EthFlow) quote, `from` is the
/// EthFlow contract, `signing_scheme` is `"eip1271"`, and `onchain_order` is
/// true.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteRequest {
    pub sell_token: Address,
    pub buy_token: Address,
    pub from: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver: Option<Address>,
    pub kind: String,
    #[serde(with = "u256_dec")]
    pub sell_amount_before_fee: U256,
    pub valid_for: u32,
    pub app_data: String,
    pub signing_scheme: String,
    pub onchain_order: bool,
    pub partially_fillable: bool,
}

/// `POST /quote` response. Only the fields we consume are modelled — serde
/// ignores the rest (`gasAmount`, `sellTokenPrice`, …). `from`/`expiration`/
/// `verified` are kept for wire fidelity and future use (quote-expiry UI).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct QuoteResponse {
    pub quote: QuoteParams,
    #[serde(default)]
    pub from: Option<Address>,
    #[serde(default)]
    pub expiration: Option<String>,
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub verified: Option<bool>,
}

/// The solver-proposed order inside a [`QuoteResponse`]. We sign the amount
/// fields (with slippage applied to `buy_amount`); the echoed token/kind/balance
/// fields are kept for wire fidelity.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct QuoteParams {
    pub sell_token: Address,
    pub buy_token: Address,
    #[serde(default)]
    pub receiver: Option<Address>,
    #[serde(with = "u256_dec")]
    pub sell_amount: U256,
    #[serde(with = "u256_dec")]
    pub buy_amount: U256,
    pub valid_to: u32,
    pub app_data: String,
    #[serde(with = "u256_dec")]
    pub fee_amount: U256,
    pub kind: String,
    pub partially_fillable: bool,
}

/// `POST /orders` body — the signed order plus signing metadata. `app_data` is
/// the full JSON pre-image (the orderbook hashes it and checks it equals the
/// `appData` bytes32 the signature covers) and `app_data_hash` is that same
/// keccak256 (the cow-sdk posts both); `signature` is `0x`+130 hex for eip712,
/// or `"0x"` for an eip1271 (EthFlow) order.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderCreation {
    pub sell_token: Address,
    pub buy_token: Address,
    pub receiver: Address,
    #[serde(with = "u256_dec")]
    pub sell_amount: U256,
    #[serde(with = "u256_dec")]
    pub buy_amount: U256,
    pub valid_to: u32,
    pub app_data: String,
    pub app_data_hash: String,
    #[serde(with = "u256_dec")]
    pub fee_amount: U256,
    pub kind: String,
    pub partially_fillable: bool,
    pub sell_token_balance: String,
    pub buy_token_balance: String,
    pub signing_scheme: String,
    pub signature: String,
    pub from: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_id: Option<i64>,
}

/// `GET /orders/{uid}` — only the status + fill fields we render. The full
/// `Order` schema carries far more; serde drops the rest.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderStatusResponse {
    pub status: OrderStatus,
    #[serde(default, deserialize_with = "u256_dec_opt::deserialize")]
    pub executed_sell_amount: Option<U256>,
    #[serde(default, deserialize_with = "u256_dec_opt::deserialize")]
    pub executed_buy_amount: Option<U256>,
}

impl OrderStatusResponse {
    /// The `(executedSell, executedBuy)` pair if any fill is present.
    pub fn executed(&self) -> Option<(U256, U256)> {
        match (self.executed_sell_amount, self.executed_buy_amount) {
            (Some(s), Some(b)) if !s.is_zero() => Some((s, b)),
            _ => None,
        }
    }
}

/// One order from `GET /account/{owner}/orders` — the subset of the full
/// `Order` schema the Apps order list renders. Token symbols/decimals aren't on
/// the wire (only addresses), so the caller resolves those from the curated
/// token list. A non-null `ethflowData` marks a native-ETH (EthFlow) order;
/// such orders' on-chain `owner` is the EthFlow contract, so callers tag them
/// with the queried user address instead.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountOrder {
    pub uid: String,
    pub sell_token: Address,
    pub buy_token: Address,
    #[serde(with = "u256_dec")]
    pub sell_amount: U256,
    #[serde(with = "u256_dec")]
    pub buy_amount: U256,
    pub valid_to: u32,
    pub kind: String,
    pub status: OrderStatus,
    #[serde(default, deserialize_with = "u256_dec_opt::deserialize")]
    pub executed_sell_amount: Option<U256>,
    #[serde(default, deserialize_with = "u256_dec_opt::deserialize")]
    pub executed_buy_amount: Option<U256>,
    /// Present (non-null) only for EthFlow orders — its presence is how we
    /// detect them, and `userValidTo` is the real `validTo` (the on-chain one
    /// is `uint256::MAX`).
    #[serde(default)]
    pub ethflow_data: Option<EthflowData>,
}

/// EthFlow-specific fields nested in an [`AccountOrder`]. Only `userValidTo` is
/// modelled (the rest — `refundTxHash` — isn't rendered).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EthflowData {
    #[serde(default)]
    pub user_valid_to: Option<u32>,
}

impl AccountOrder {
    /// The `(executedSell, executedBuy)` pair if any fill is present — same
    /// rule as [`OrderStatusResponse::executed`].
    pub fn executed(&self) -> Option<(U256, U256)> {
        match (self.executed_sell_amount, self.executed_buy_amount) {
            (Some(s), Some(b)) if !s.is_zero() => Some((s, b)),
            _ => None,
        }
    }
}

/// `DELETE /orders` body — signed bulk cancellation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancellationBody {
    pub order_uids: Vec<String>,
    pub signature: String,
    pub signing_scheme: String,
}

/// CoW's structured error body (`{"errorType": "...", "description": "..."}`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiError {
    #[serde(default)]
    error_type: String,
    #[serde(default)]
    description: Option<String>,
}

// ── Endpoints ────────────────────────────────────────────────────────────────

/// Fetch a sell quote. The returned [`QuoteParams`] are the values to sign.
pub async fn get_quote(chain: Chain, req: &QuoteRequest) -> Result<QuoteResponse, String> {
    let url = format!("{}/quote", base(chain)?);
    let resp = http_client_or_err()?
        .post(&url)
        .json(req)
        .send()
        .await
        .map_err(|e| format!("cow quote: {}", redact_url_in_err(e)))?;
    let resp = check_status(resp, "quote").await?;
    resp.json::<QuoteResponse>()
        .await
        .map_err(|e| format!("cow quote decode: {}", redact_url_in_err(e)))
}

/// Submit a signed order. Returns the order UID (`0x`+112 hex).
pub async fn post_order(chain: Chain, body: &OrderCreation) -> Result<String, String> {
    let url = format!("{}/orders", base(chain)?);
    let resp = http_client_or_err()?
        .post(&url)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("cow create order: {}", redact_url_in_err(e)))?;
    let resp = check_status(resp, "create order").await?;
    // The body is a bare JSON string: the order UID.
    resp.json::<String>()
        .await
        .map_err(|e| format!("cow create order decode: {}", redact_url_in_err(e)))
}

/// Upload an appData document's pre-image (`PUT /app_data/{hash}`) so the
/// orderbook can resolve the `appData` hash to its metadata (notably
/// `orderClass`). Only the **native EthFlow** path needs this: it creates the
/// order on-chain and never POSTs an order body, so the orderbook would
/// otherwise see only the bare hash and book the order as a limit order. ERC-20
/// orders carry the full pre-image in their POST and skip this.
pub async fn upload_app_data(chain: Chain, hash: &str, full_app_data: &str) -> Result<(), String> {
    let url = format!("{}/app_data/{hash}", base(chain)?);
    let resp = http_client_or_err()?
        .put(&url)
        .json(&serde_json::json!({ "fullAppData": full_app_data }))
        .send()
        .await
        .map_err(|e| format!("cow upload appData: {}", redact_url_in_err(e)))?;
    check_status(resp, "upload appData").await?;
    Ok(())
}

/// Poll a single order's status.
pub async fn get_order(chain: Chain, uid: &str) -> Result<OrderStatusResponse, String> {
    let url = format!("{}/orders/{uid}", base(chain)?);
    let resp = http_client_or_err()?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("cow order status: {}", redact_url_in_err(e)))?;
    let resp = check_status(resp, "order status").await?;
    resp.json::<OrderStatusResponse>()
        .await
        .map_err(|e| format!("cow order status decode: {}", redact_url_in_err(e)))
}

/// Fetch the most recent orders the orderbook holds for `owner` (newest
/// first), up to `limit` (CoW caps it at 1000). Backs the Apps "Fetch" action:
/// surfaces the address's full CoW order history — including orders placed in
/// past sessions, which the in-memory tracked list doesn't carry — not just
/// this session's. A clean account with no orders returns an empty array.
pub async fn get_account_orders(
    chain: Chain,
    owner: Address,
    limit: u16,
) -> Result<Vec<AccountOrder>, String> {
    let url = format!("{}/account/{owner}/orders?limit={limit}", base(chain)?);
    let resp = http_client_or_err()?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("cow account orders: {}", redact_url_in_err(e)))?;
    let resp = check_status(resp, "account orders").await?;
    resp.json::<Vec<AccountOrder>>()
        .await
        .map_err(|e| format!("cow account orders decode: {}", redact_url_in_err(e)))
}

/// Cancel one or more open orders (off-chain signed). Used for EOA/ERC-20
/// orders; EthFlow orders cancel on-chain instead.
pub async fn delete_orders(chain: Chain, body: &CancellationBody) -> Result<(), String> {
    let url = format!("{}/orders", base(chain)?);
    let resp = http_client_or_err()?
        .delete(&url)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("cow cancel: {}", redact_url_in_err(e)))?;
    check_status(resp, "cancel order").await?;
    Ok(())
}

/// Turn a non-2xx response into an error string, surfacing CoW's `description`
/// when present (e.g. "InsufficientAllowance", "SellAmountDoesNotCoverFee").
async fn check_status(resp: reqwest::Response, label: &str) -> Result<reqwest::Response, String> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let code = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<ApiError>(&body)
        .ok()
        .map(|e| e.description.unwrap_or(e.error_type))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| body.chars().take(200).collect());
    Err(format!("cow {label}: {code}: {detail}"))
}

// ── serde adapters ───────────────────────────────────────────────────────────

/// U256 ⇄ decimal string (CoW's atom-amount wire form).
mod u256_dec {
    use alloy::primitives::U256;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &U256, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<U256, D::Error> {
        let s = String::deserialize(d)?;
        s.parse::<U256>().map_err(serde::de::Error::custom)
    }
}

/// Optional U256 from a decimal string (absent or null → `None`).
mod u256_dec_opt {
    use alloy::primitives::U256;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<U256>, D::Error> {
        let opt = Option::<String>::deserialize(d)?;
        match opt {
            Some(s) => s
                .parse::<U256>()
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use serde_json::json;

    #[test]
    fn quote_request_serializes_with_camelcase_and_no_nulls() {
        let req = QuoteRequest {
            sell_token: address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            buy_token: address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            from: address!("0x1111111111111111111111111111111111111111"),
            receiver: Some(address!("0x1111111111111111111111111111111111111111")),
            kind: "sell".into(),
            sell_amount_before_fee: U256::from(1_000_000_000_000_000_000u64),
            valid_for: 1800,
            app_data: "{}".into(),
            signing_scheme: "eip712".into(),
            onchain_order: false,
            partially_fillable: false,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["sellAmountBeforeFee"], "1000000000000000000");
        assert_eq!(v["validFor"], 1800);
        assert_eq!(v["kind"], "sell");
        assert_eq!(v["signingScheme"], "eip712");
        assert_eq!(v["onchainOrder"], false);
        assert!(v.get("appData").is_some());
    }

    #[test]
    fn quote_request_omits_receiver_when_none() {
        let req = QuoteRequest {
            sell_token: address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            buy_token: address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            from: address!("0x1111111111111111111111111111111111111111"),
            receiver: None,
            kind: "sell".into(),
            sell_amount_before_fee: U256::from(1u64),
            valid_for: 1800,
            app_data: "{}".into(),
            signing_scheme: "eip1271".into(),
            onchain_order: true,
            partially_fillable: false,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("receiver").is_none());
        assert_eq!(v["onchainOrder"], true);
    }

    #[test]
    fn quote_response_deserializes_and_ignores_extra_fields() {
        let body = json!({
            "quote": {
                "sellToken": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "buyToken": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "receiver": "0x1111111111111111111111111111111111111111",
                "sellAmount": "999869351784000000",
                "buyAmount": "2512345678",
                "validTo": 1900000000u64,
                "appData": "0xb48d38f93eaa084033fc5970bf96e559c33c4cdc07d889ab00b4d63f9590739d",
                "feeAmount": "130648216000000",
                "kind": "sell",
                "partiallyFillable": false,
                "sellTokenBalance": "erc20",
                "buyTokenBalance": "erc20",
                "signingScheme": "eip712",
                "gasAmount": "120000",
                "sellTokenPrice": "1.0"
            },
            "from": "0x1111111111111111111111111111111111111111",
            "expiration": "2026-06-25T12:00:00.000Z",
            "id": 123456,
            "verified": true
        });
        let resp: QuoteResponse = serde_json::from_value(body).unwrap();
        assert_eq!(resp.id, Some(123456));
        assert_eq!(resp.verified, Some(true));
        assert_eq!(resp.quote.sell_amount, U256::from(999869351784000000u64));
        assert_eq!(resp.quote.buy_amount, U256::from(2512345678u64));
        assert_eq!(resp.quote.fee_amount, U256::from(130648216000000u64));
        assert_eq!(resp.quote.valid_to, 1900000000);
        assert_eq!(resp.quote.kind, "sell");
    }

    #[test]
    fn order_status_deserializes_with_fill() {
        let body = json!({
            "status": "fulfilled",
            "executedSellAmount": "999869351784000000",
            "executedBuyAmount": "2512345678",
            "executedFeeAmount": "130648216000000",
            "owner": "0x1111111111111111111111111111111111111111",
            "class": "market"
        });
        let resp: OrderStatusResponse = serde_json::from_value(body).unwrap();
        assert_eq!(resp.status, OrderStatus::Fulfilled);
        assert_eq!(
            resp.executed(),
            Some((U256::from(999869351784000000u64), U256::from(2512345678u64)))
        );
    }

    #[test]
    fn order_status_open_with_zero_fill_is_none() {
        let body = json!({
            "status": "open",
            "executedSellAmount": "0",
            "executedBuyAmount": "0"
        });
        let resp: OrderStatusResponse = serde_json::from_value(body).unwrap();
        assert_eq!(resp.status, OrderStatus::Open);
        assert_eq!(resp.executed(), None);
    }

    #[test]
    fn order_status_presignature_pending_parses() {
        let body = json!({ "status": "presignaturePending" });
        let resp: OrderStatusResponse = serde_json::from_value(body).unwrap();
        assert_eq!(resp.status, OrderStatus::PresignaturePending);
        assert_eq!(resp.executed(), None);
    }

    #[test]
    fn order_creation_serializes_default_appdata_and_balances() {
        let body = OrderCreation {
            sell_token: address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"),
            buy_token: address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
            receiver: address!("0x1111111111111111111111111111111111111111"),
            sell_amount: U256::from(1_000_000_000_000_000_000u64),
            buy_amount: U256::from(2_500_000_000u64),
            valid_to: 1_900_000_000,
            app_data: "{}".into(),
            app_data_hash: "0xb48d38f93eaa084033fc5970bf96e559c33c4cdc07d889ab00b4d63f9590739d"
                .into(),
            fee_amount: U256::ZERO,
            kind: "sell".into(),
            partially_fillable: false,
            sell_token_balance: "erc20".into(),
            buy_token_balance: "erc20".into(),
            signing_scheme: "eip712".into(),
            signature: "0x".to_string() + &"ab".repeat(65),
            from: address!("0x1111111111111111111111111111111111111111"),
            quote_id: Some(123456),
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["appData"], "{}");
        assert_eq!(
            v["appDataHash"],
            "0xb48d38f93eaa084033fc5970bf96e559c33c4cdc07d889ab00b4d63f9590739d"
        );
        assert_eq!(v["sellTokenBalance"], "erc20");
        assert_eq!(v["buyTokenBalance"], "erc20");
        assert_eq!(v["feeAmount"], "0");
        assert_eq!(v["quoteId"], 123456);
        assert_eq!(v["signingScheme"], "eip712");
    }

    #[test]
    fn order_creation_omits_quote_id_when_none() {
        let body = OrderCreation {
            sell_token: Address::ZERO,
            buy_token: Address::ZERO,
            receiver: Address::ZERO,
            sell_amount: U256::ZERO,
            buy_amount: U256::ZERO,
            valid_to: 0,
            app_data: "{}".into(),
            app_data_hash: "0x".into(),
            fee_amount: U256::ZERO,
            kind: "sell".into(),
            partially_fillable: false,
            sell_token_balance: "erc20".into(),
            buy_token_balance: "erc20".into(),
            signing_scheme: "eip712".into(),
            signature: "0x".into(),
            from: Address::ZERO,
            quote_id: None,
        };
        let v = serde_json::to_value(&body).unwrap();
        assert!(v.get("quoteId").is_none());
    }

    #[test]
    fn account_orders_deserialize_array_and_resolve_executed() {
        // A page as `/account/{owner}/orders` returns it: an ERC-20 sell and a
        // native EthFlow buy, extra fields ignored.
        let body = json!([
            {
                "uid": "0xaaaa",
                "sellToken": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "buyToken": "0x6b175474e89094c44da98b954eedeac495271d0f",
                "sellAmount": "1000000",
                "buyAmount": "990000000000000000",
                "validTo": 1900000000u64,
                "kind": "sell",
                "status": "fulfilled",
                "executedSellAmount": "1000000",
                "executedBuyAmount": "991000000000000000",
                "owner": "0x1111111111111111111111111111111111111111",
                "class": "market"
            },
            {
                "uid": "0xbbbb",
                "sellToken": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "buyToken": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "sellAmount": "500000000000000000",
                "buyAmount": "1200000000",
                "validTo": 4294967295u64,
                "kind": "sell",
                "status": "open",
                "ethflowData": { "refundTxHash": null, "userValidTo": 1899999999u64 }
            }
        ]);
        let orders: Vec<AccountOrder> = serde_json::from_value(body).unwrap();
        assert_eq!(orders.len(), 2);

        let erc20 = &orders[0];
        assert_eq!(erc20.uid, "0xaaaa");
        assert!(
            erc20.ethflow_data.is_none(),
            "ERC-20 order has no ethflowData"
        );
        assert_eq!(
            erc20.executed(),
            Some((
                U256::from(1_000_000u64),
                U256::from(991_000_000_000_000_000u64)
            ))
        );

        let ethflow = &orders[1];
        assert!(
            ethflow.ethflow_data.is_some(),
            "EthFlow order detected via ethflowData",
        );
        assert_eq!(
            ethflow.ethflow_data.as_ref().unwrap().user_valid_to,
            Some(1_899_999_999),
            "userValidTo carries the real validity for EthFlow",
        );
        assert_eq!(
            ethflow.executed(),
            None,
            "unfilled open order → no executed"
        );
    }

    #[test]
    fn cancellation_body_camelcase() {
        let body = CancellationBody {
            order_uids: vec!["0xabcd".into()],
            signature: "0x12".into(),
            signing_scheme: "eip712".into(),
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["orderUids"][0], "0xabcd");
        assert_eq!(v["signingScheme"], "eip712");
    }
}
