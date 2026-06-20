//! dRPC indexer.
//!
//! Both endpoints sit on the same `lb.drpc.live` host and accept the
//! API key as a path segment, so a single `dkey` covers RPC and Wallet
//! API alike.
//!
//! * Transactions: `GET /{chain}/{key}/lambda/v1/transactions/{address}/history`
//! * Balances: `GET /{chain}/{key}/lambda/v2/wallets/{address}/balances`
//!   — one call returns native ETH plus every ERC-20 with metadata and
//!   USD prices included.

use std::str::FromStr;

use alloy::primitives::{Address, B256, U256};
use async_trait::async_trait;
use serde::Deserialize;
use tracing::debug;

use crate::chain::Chain;
use crate::portfolio::{format_eth_balance, format_token_balance};

use super::{
    IndexedToken, IndexedTx, Indexer, TokenTransfer, TxStatus, classify_direction, http_client,
    redact_url_in_err,
};

const BASE: &str = "https://lb.drpc.live";

/// dRPC's chain slug — matches the path segment used in both the RPC
/// and Wallet-API URLs.
pub(crate) fn drpc_chain(chain: Chain) -> &'static str {
    match chain {
        Chain::Mainnet => "ethereum",
        Chain::Base => "base",
        Chain::Optimism => "optimism",
    }
}

#[derive(Debug)]
pub struct DrpcClient {
    api_key: String,
    chain: Chain,
}

impl DrpcClient {
    pub fn new(api_key: String, chain: Chain) -> Self {
        Self { api_key, chain }
    }

    fn balances_url(&self, addr: Address) -> String {
        // `asset_type=TOKEN` skips DeFi positions — those don't fit the
        // wallet's flat token list. `include_zero_price_tokens=false`
        // drops airdrop spam from the response.
        format!(
            "{BASE}/{}/{}/lambda/v2/wallets/{:#x}/balances?asset_type=TOKEN&include_zero_price_tokens=false",
            drpc_chain(self.chain),
            self.api_key,
            addr,
        )
    }

    fn history_url(&self, addr: Address, limit: usize) -> String {
        format!(
            "{BASE}/{}/{}/lambda/v1/transactions/{:#x}/history?limit={}",
            drpc_chain(self.chain),
            self.api_key,
            addr,
            limit.max(1),
        )
    }
}

// ── Wallet API: balances ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BalancesResponse {
    data: BalancesData,
}

#[derive(Deserialize)]
struct BalancesData {
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    /// "token" / "defi" / "liquid_staking_token" — we only handle "token".
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    attributes: Option<TokenAttributes>,
}

#[derive(Deserialize, Default)]
struct TokenAttributes {
    /// `"eth"` for native ETH; otherwise the token contract address as
    /// a lowercase hex string.
    #[serde(default)]
    token_id: Option<String>,
    #[serde(default)]
    token_symbol: Option<String>,
    #[serde(default)]
    token_name: Option<String>,
    #[serde(default)]
    contract_address: Option<String>,
    #[serde(default)]
    decimals: Option<u8>,
    /// Raw integer in smallest unit. dRPC sends this as a JSON string so
    /// values that overflow `u64`/`f64` survive the wire intact.
    #[serde(default)]
    amount_string: Option<String>,
    #[serde(default)]
    price_usd: Option<f64>,
    #[serde(default)]
    icon_url: Option<String>,
}

// ── Wallet API: transactions ────────────────────────────────────────────────

#[derive(Deserialize)]
struct HistoryResponse {
    #[serde(default)]
    data: Vec<RawTx>,
}

#[derive(Deserialize)]
struct RawTx {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    block: u64,
    /// Unix milliseconds.
    #[serde(default)]
    timestamp: u64,
    #[serde(default)]
    sender_address: Option<String>,
    #[serde(default)]
    recipient_address: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    transfers: Vec<RawTransfer>,
}

#[derive(Deserialize)]
struct RawTransfer {
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    sender_address: Option<String>,
    #[serde(default)]
    recipient_address: Option<String>,
    #[serde(default)]
    token: Option<TransferToken>,
    #[serde(default)]
    decimals: Option<u8>,
    #[serde(default)]
    amount_string: Option<String>,
}

#[derive(Deserialize)]
struct TransferToken {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
    /// Empty string for native ETH; ERC-20 contract address otherwise.
    #[serde(default)]
    address: Option<String>,
}

// ── Indexer impl ─────────────────────────────────────────────────────────────

#[async_trait]
impl Indexer for DrpcClient {
    async fn transactions(&self, addr: Address, limit: usize) -> Result<Vec<IndexedTx>, String> {
        let url = self.history_url(addr, limit);
        let resp: HistoryResponse = fetch_json(&url, "history").await?;
        Ok(resp
            .data
            .into_iter()
            .filter_map(|t| convert_tx(t, addr))
            .take(limit)
            .collect())
    }

    async fn balances(&self, addr: Address) -> Result<Vec<IndexedToken>, String> {
        let url = self.balances_url(addr);
        let resp: BalancesResponse = fetch_json(&url, "balances").await?;
        Ok(parse_balances(resp.data.assets))
    }
}

// ── Kao proxy client ─────────────────────────────────────────────────────────

/// Indexer backed by the Kao privacy proxy.
///
/// The proxy is a thin per-chain front for dRPC's Wallet API: it injects the
/// shared dRPC key, forwards our query string verbatim, and returns dRPC's
/// response body unchanged. So the wire types ([`BalancesResponse`],
/// [`HistoryResponse`]) and the parsing ([`parse_balances`], [`convert_tx`])
/// are shared with [`DrpcClient`] — only the URL differs. Two privacy wins
/// over talking to dRPC directly: the API key lives on the proxy (never in a
/// URL the wallet builds, so there's nothing to redact), and the proxy
/// originates the upstream request, so no client IP reaches dRPC.
#[derive(Debug)]
pub struct KaoClient {
    /// Base URL of the user's Kao proxy, e.g. `https://api.kaowallet.com`.
    base: String,
    chain: Chain,
}

impl KaoClient {
    pub fn new(base: String, chain: Chain) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            chain,
        }
    }

    fn balances_url(&self, addr: Address) -> String {
        // Same query knobs the direct dRPC client uses; the proxy forwards
        // them verbatim to the Wallet API.
        format!(
            "{}/v1/{}/balances/{:#x}?asset_type=TOKEN&include_zero_price_tokens=false",
            self.base,
            drpc_chain(self.chain),
            addr,
        )
    }

    fn history_url(&self, addr: Address, limit: usize) -> String {
        format!(
            "{}/v1/{}/history/{:#x}?limit={}",
            self.base,
            drpc_chain(self.chain),
            addr,
            limit.max(1),
        )
    }
}

#[async_trait]
impl Indexer for KaoClient {
    async fn transactions(&self, addr: Address, limit: usize) -> Result<Vec<IndexedTx>, String> {
        let url = self.history_url(addr, limit);
        let resp: HistoryResponse = fetch_json(&url, "kao history").await?;
        Ok(resp
            .data
            .into_iter()
            .filter_map(|t| convert_tx(t, addr))
            .take(limit)
            .collect())
    }

    async fn balances(&self, addr: Address) -> Result<Vec<IndexedToken>, String> {
        let url = self.balances_url(addr);
        let resp: BalancesResponse = fetch_json(&url, "kao balances").await?;
        Ok(parse_balances(resp.data.assets))
    }
}

/// GET `url`, log the raw response body at `debug!`, then deserialize.
///
/// dRPC's Wallet API surfaces a lot of fields the wallet doesn't render
/// (DeFi positions, PnL links, change-1d, icon URLs, …). Keeping the
/// raw body in the log under `RUST_LOG=kao=debug` lets a user — or us —
/// see exactly what the provider returned without re-running with a
/// proxy. The API key sits in the URL path; we redact it before logging.
///
/// On a non-2xx response we surface the response body in the error
/// string. dRPC encodes its useful diagnostics there
/// (e.g. `{"message":"method is not available on freetier","code":35}`
/// for the Wallet API on a free-tier key) and `error_for_status` would
/// throw it away.
async fn fetch_json<T>(url: &str, label: &'static str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let resp = http_client()
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("drpc {label} GET: {}", redact_url_in_err(e)))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("drpc {label} read: {}", redact_url_in_err(e)))?;
    debug!(
        endpoint = label,
        url = %redact_drpc_key(url),
        status = status.as_u16(),
        bytes = body.len(),
        body = %body,
        "drpc response",
    );
    if !status.is_success() {
        return Err(format!(
            "drpc {label} status {}: {}",
            status.as_u16(),
            body.trim(),
        ));
    }
    serde_json::from_str(&body).map_err(|e| format!("drpc {label} decode: {e}"))
}

/// Replace the API-key path segment in a dRPC URL with `****` so the
/// log line is shareable. Returns the input unchanged for non-dRPC URLs.
fn redact_drpc_key(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    if parsed.host_str() != Some("lb.drpc.live") {
        return url.to_string();
    }
    let segs: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.collect())
        .unwrap_or_default();
    if segs.len() < 2 {
        return url.to_string();
    }
    let mut redacted = segs.clone();
    redacted[1] = "****";
    let new_path = format!("/{}", redacted.join("/"));
    let mut out = parsed.clone();
    out.set_path(&new_path);
    out.to_string()
}

fn parse_balances(assets: Vec<Asset>) -> Vec<IndexedToken> {
    let mut native: Option<IndexedToken> = None;
    let mut erc20: Vec<IndexedToken> = Vec::new();

    for a in assets {
        if a.r#type != "token" {
            continue;
        }
        let attrs = a.attributes.unwrap_or_default();
        let raw = attrs
            .amount_string
            .as_deref()
            .and_then(|s| U256::from_str_radix(s, 10).ok())
            .unwrap_or(U256::ZERO);
        let price = attrs.price_usd;

        // dRPC marks native ETH with `token_id = "eth"` and no
        // `contract_address`. ERC-20 rows always carry a contract.
        let is_native = attrs.contract_address.as_deref().is_none_or(str::is_empty)
            && attrs.token_id.as_deref().map(str::to_ascii_lowercase) == Some("eth".to_string());

        if is_native {
            let (eth_str, eth_f64) = format_eth_balance(raw);
            native = Some(IndexedToken {
                symbol: attrs.token_symbol.unwrap_or_else(|| "ETH".into()),
                name: attrs.token_name.unwrap_or_else(|| "Ethereum".into()),
                contract: None,
                decimals: 18,
                balance_raw: raw,
                balance_f64: eth_f64,
                balance: eth_str,
                usd_price: price,
                usd_value: price.map(|p| p * eth_f64),
                logo_url: attrs.icon_url,
            });
        } else {
            if raw.is_zero() {
                continue;
            }
            let Some(addr_str) = attrs.contract_address.as_deref() else {
                continue;
            };
            let Ok(contract) = Address::from_str(addr_str) else {
                continue;
            };
            let decimals = attrs.decimals.unwrap_or(18);
            let (bal_str, bal_f64) = format_token_balance(raw, decimals);
            erc20.push(IndexedToken {
                symbol: attrs.token_symbol.unwrap_or_default(),
                name: attrs.token_name.unwrap_or_default(),
                contract: Some(contract),
                decimals,
                balance_raw: raw,
                balance_f64: bal_f64,
                balance: bal_str,
                usd_price: price,
                usd_value: price.map(|p| p * bal_f64),
                logo_url: attrs.icon_url,
            });
        }
    }

    erc20.sort_by(|a, b| {
        let av = a.usd_value.unwrap_or(0.0);
        let bv = b.usd_value.unwrap_or(0.0);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out = Vec::with_capacity(erc20.len() + 1);
    // Mirror Alchemy's invariant: ETH always leads, even if the API
    // dropped the row (the dashboard expects a native-token slot).
    out.push(native.unwrap_or_else(|| {
        let (eth_str, eth_f64) = format_eth_balance(U256::ZERO);
        IndexedToken {
            symbol: "ETH".into(),
            name: "Ethereum".into(),
            contract: None,
            decimals: 18,
            balance_raw: U256::ZERO,
            balance_f64: eth_f64,
            balance: eth_str,
            usd_price: None,
            usd_value: None,
            logo_url: None,
        }
    }));
    out.extend(erc20);
    out
}

fn convert_tx(t: RawTx, owner: Address) -> Option<IndexedTx> {
    let hash = B256::from_str(t.hash.as_deref()?).ok()?;
    let from = t
        .sender_address
        .as_deref()
        .and_then(|s| Address::from_str(s).ok())?;
    let to = t
        .recipient_address
        .as_deref()
        .and_then(|s| Address::from_str(s).ok());

    let status = match t.status.as_deref() {
        Some("confirmed") => TxStatus::Success,
        Some("failed") => TxStatus::Failure,
        Some("pending") => TxStatus::Pending,
        _ => TxStatus::Success,
    };

    // dRPC sends transfers as a list. Surface the most relevant one for
    // the owner so the activity feed shows "1.23 USDC" for an ERC-20
    // send instead of "0 ETH". Preference order:
    //   1. The first ERC-20 transfer that touches `owner`.
    //   2. Otherwise the first ETH transfer that touches `owner`.
    //   3. Otherwise the first transfer (or none).
    let transfer = pick_transfer(&t.transfers, owner);
    let (value, token) = match transfer {
        Some(tr) => decode_transfer(tr),
        None => (U256::ZERO, None),
    };

    Some(IndexedTx {
        hash,
        block_number: t.block,
        // dRPC reports milliseconds; the rest of the indexer layer is in seconds.
        timestamp: t.timestamp / 1000,
        from,
        to,
        value,
        gas_used: None,
        gas_price: None,
        status,
        direction: classify_direction(from, to, owner),
        method: Some(t.r#type),
        token,
        chain: Chain::Mainnet,
    })
}

fn pick_transfer(transfers: &[RawTransfer], owner: Address) -> Option<&RawTransfer> {
    let owner_lower = format!("{owner:#x}");
    let touches_owner = |tr: &RawTransfer| -> bool {
        let s = tr.sender_address.as_deref().unwrap_or("");
        let r = tr.recipient_address.as_deref().unwrap_or("");
        s.eq_ignore_ascii_case(&owner_lower) || r.eq_ignore_ascii_case(&owner_lower)
    };
    let is_erc20 = |tr: &RawTransfer| -> bool {
        tr.token
            .as_ref()
            .and_then(|t| t.address.as_deref())
            .is_some_and(|a| !a.is_empty())
    };
    transfers
        .iter()
        .find(|tr| touches_owner(tr) && is_erc20(tr))
        .or_else(|| transfers.iter().find(|tr| touches_owner(tr)))
        .or_else(|| transfers.first())
}

fn decode_transfer(tr: &RawTransfer) -> (U256, Option<TokenTransfer>) {
    let raw_amount = tr
        .amount_string
        .as_deref()
        .and_then(|s| U256::from_str_radix(s, 10).ok())
        .unwrap_or(U256::ZERO);
    let token = tr.token.as_ref();
    let address = token.and_then(|t| t.address.as_deref()).unwrap_or("");
    if address.is_empty() {
        // Native ETH transfer — amount is wei.
        (raw_amount, None)
    } else if let Ok(contract) = Address::from_str(address) {
        let decimals = tr.decimals.unwrap_or(18);
        let symbol = token.and_then(|t| t.symbol.clone()).unwrap_or_default();
        (
            U256::ZERO,
            Some(TokenTransfer {
                contract,
                symbol,
                decimals,
                amount_raw: raw_amount,
                is_nft: false,
                token_id: None,
            }),
        )
    } else {
        (U256::ZERO, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::TxDirection;

    #[test]
    fn parse_balances_orders_eth_first_and_filters_zero() {
        let resp: BalancesResponse = serde_json::from_str(
            r#"{
              "data": {
                "assets": [
                  {
                    "type": "token",
                    "attributes": {
                      "token_id": "eth",
                      "token_symbol": "ETH",
                      "token_name": "Ethereum",
                      "decimals": 18,
                      "amount_string": "40000000000000000000",
                      "price_usd": 2000.0
                    }
                  },
                  {
                    "type": "token",
                    "attributes": {
                      "token_id": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                      "token_symbol": "USDC",
                      "token_name": "USD Coin",
                      "contract_address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                      "decimals": 6,
                      "amount_string": "5000000",
                      "price_usd": 1.0
                    }
                  },
                  {
                    "type": "token",
                    "attributes": {
                      "token_id": "0xdac17f958d2ee523a2206206994597c13d831ec7",
                      "token_symbol": "USDT",
                      "contract_address": "0xdac17f958d2ee523a2206206994597c13d831ec7",
                      "decimals": 6,
                      "amount_string": "0"
                    }
                  },
                  {
                    "type": "token",
                    "attributes": {
                      "token_id": "0x514910771af9ca656af840dff83e8264ecf986ca",
                      "token_symbol": "LINK",
                      "contract_address": "0x514910771af9ca656af840dff83e8264ecf986ca",
                      "decimals": 18,
                      "amount_string": "1000000000000000000",
                      "price_usd": 15.0
                    }
                  },
                  {
                    "type": "defi",
                    "attributes": {
                      "token_id": "ignored",
                      "contract_address": "0x0000000000000000000000000000000000000001",
                      "decimals": 18,
                      "amount_string": "1"
                    }
                  }
                ]
              }
            }"#,
        )
        .unwrap();
        let out = parse_balances(resp.data.assets);

        // ETH leads, defi row dropped, USDT zero-balance skipped, LINK > USDC by USD.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].symbol, "ETH");
        assert!(out[0].contract.is_none());
        assert_eq!(out[0].usd_price, Some(2000.0));
        assert_eq!(out[1].symbol, "LINK");
        assert_eq!(out[2].symbol, "USDC");
    }

    #[test]
    fn parse_balances_emits_zero_eth_placeholder_when_native_missing() {
        let out = parse_balances(Vec::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].symbol, "ETH");
        assert_eq!(out[0].balance_raw, U256::ZERO);
    }

    #[test]
    fn convert_tx_decodes_native_eth_transfer() {
        let owner: Address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045"
            .parse()
            .unwrap();
        let raw: RawTx = serde_json::from_str(
            r#"{
                "type": "receive",
                "hash": "0x4444444444444444444444444444444444444444444444444444444444444444",
                "block": 18000000,
                "timestamp": 1704067200000,
                "sender_address": "0x000000000000000000000000000000000000beef",
                "recipient_address": "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
                "status": "confirmed",
                "transfers": [{
                    "direction": "in",
                    "sender_address": "0x000000000000000000000000000000000000beef",
                    "recipient_address": "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
                    "token": { "name": "Ethereum", "symbol": "ETH", "address": "" },
                    "decimals": 18,
                    "amount_string": "1000000000000000000"
                }]
            }"#,
        )
        .unwrap();
        let tx = convert_tx(raw, owner).expect("converts");
        assert_eq!(tx.block_number, 18_000_000);
        assert_eq!(tx.timestamp, 1_704_067_200);
        assert_eq!(tx.value, U256::from(1_000_000_000_000_000_000u128));
        assert!(matches!(tx.direction, TxDirection::In));
        assert_eq!(tx.method.as_deref(), Some("receive"));
        assert!(tx.token.is_none());
    }

    #[test]
    fn convert_tx_picks_erc20_transfer_over_native() {
        let owner: Address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045"
            .parse()
            .unwrap();
        // The fee transfer (native ETH) shouldn't shadow the USDC send.
        let raw: RawTx = serde_json::from_str(
            r#"{
                "type": "send",
                "hash": "0x5555555555555555555555555555555555555555555555555555555555555555",
                "block": 18000001,
                "timestamp": 1718454896000,
                "sender_address": "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
                "recipient_address": "0x000000000000000000000000000000000000beef",
                "status": "confirmed",
                "transfers": [
                    {
                        "direction": "out",
                        "sender_address": "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
                        "recipient_address": "0x000000000000000000000000000000000000feef",
                        "token": { "name": "Ethereum", "symbol": "ETH", "address": "" },
                        "decimals": 18,
                        "amount_string": "1000000"
                    },
                    {
                        "direction": "out",
                        "sender_address": "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
                        "recipient_address": "0x000000000000000000000000000000000000beef",
                        "token": {
                            "name": "USD Coin",
                            "symbol": "USDC",
                            "address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
                        },
                        "decimals": 6,
                        "amount_string": "5000000"
                    }
                ]
            }"#,
        )
        .unwrap();
        let tx = convert_tx(raw, owner).expect("converts");
        assert_eq!(tx.value, U256::ZERO, "ERC-20 row carries 0 native wei");
        let token = tx.token.expect("token attached");
        assert_eq!(token.symbol, "USDC");
        assert_eq!(token.decimals, 6);
        assert_eq!(token.amount_raw, U256::from(5_000_000u64));
        assert_eq!(
            format!("{:#x}", token.contract),
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
        );
    }

    #[test]
    fn convert_tx_marks_failed_status() {
        let owner: Address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045"
            .parse()
            .unwrap();
        let raw: RawTx = serde_json::from_str(
            r#"{
                "type": "execute",
                "hash": "0x6666666666666666666666666666666666666666666666666666666666666666",
                "block": 18000002,
                "timestamp": 1718454900000,
                "sender_address": "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
                "recipient_address": "0x000000000000000000000000000000000000beef",
                "status": "failed",
                "transfers": []
            }"#,
        )
        .unwrap();
        let tx = convert_tx(raw, owner).expect("converts");
        assert!(matches!(tx.status, TxStatus::Failure));
    }

    #[test]
    fn balances_url_contains_chain_key_and_address() {
        let client = DrpcClient::new("MYKEY".into(), Chain::Base);
        let url = client.balances_url(
            "0xd8da6bf26964af9d7eed9e03e53415d37aa96045"
                .parse()
                .unwrap(),
        );
        assert!(url.starts_with(
            "https://lb.drpc.live/base/MYKEY/lambda/v2/wallets/0xd8da6bf26964af9d7eed9e03e53415d37aa96045/balances"
        ));
    }

    #[test]
    fn kao_balances_url_targets_proxy_v1_route() {
        let client = KaoClient::new("https://api.kaowallet.com/".into(), Chain::Optimism);
        let url = client.balances_url(
            "0xd8da6bf26964af9d7eed9e03e53415d37aa96045"
                .parse()
                .unwrap(),
        );
        // Trailing slash on the base is trimmed; key never appears in the URL.
        assert_eq!(
            url,
            "https://api.kaowallet.com/v1/optimism/balances/0xd8da6bf26964af9d7eed9e03e53415d37aa96045?asset_type=TOKEN&include_zero_price_tokens=false",
        );
    }

    #[test]
    fn kao_history_url_targets_proxy_v1_route() {
        let client = KaoClient::new("https://api.kaowallet.com".into(), Chain::Mainnet);
        let url = client.history_url(
            "0xd8da6bf26964af9d7eed9e03e53415d37aa96045"
                .parse()
                .unwrap(),
            25,
        );
        assert_eq!(
            url,
            "https://api.kaowallet.com/v1/ethereum/history/0xd8da6bf26964af9d7eed9e03e53415d37aa96045?limit=25",
        );
    }

    #[test]
    fn redact_drpc_key_replaces_key_segment() {
        assert_eq!(
            redact_drpc_key(
                "https://lb.drpc.live/ethereum/SECRET/lambda/v2/wallets/0xabc/balances"
            ),
            "https://lb.drpc.live/ethereum/****/lambda/v2/wallets/0xabc/balances",
        );
    }

    #[test]
    fn redact_drpc_key_passes_through_unrelated_urls() {
        let url = "https://eth.llamarpc.com/v2/SECRET";
        assert_eq!(redact_drpc_key(url), url);
    }

    #[test]
    fn drpc_chain_slugs_match_doc_examples() {
        assert_eq!(drpc_chain(Chain::Mainnet), "ethereum");
        assert_eq!(drpc_chain(Chain::Base), "base");
        assert_eq!(drpc_chain(Chain::Optimism), "optimism");
    }
}
