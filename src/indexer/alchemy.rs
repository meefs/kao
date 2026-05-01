//! Alchemy indexer.
//!
//! * Transactions: `alchemy_getAssetTransfers` (JSON-RPC) — two parallel
//!   calls (sent / received) merged and sorted.
//! * Balances: Portfolio API at
//!   `data/v1/{apiKey}/assets/tokens/by-address` — one POST returns native
//!   ETH plus every ERC-20, with metadata and USD prices included.

use std::collections::HashSet;
use std::str::FromStr;

use alloy::primitives::{Address, B256, U256};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::portfolio::{format_eth_balance, format_token_balance};

use super::{
    Indexer, IndexedToken, IndexedTx, TxStatus, classify_direction, http_client, parse_iso8601,
};

const NETWORK: &str = "eth-mainnet";
const PORTFOLIO_BASE: &str = "https://api.g.alchemy.com/data/v1";

#[derive(Debug)]
pub struct AlchemyClient {
    api_key: String,
}

impl AlchemyClient {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }

    fn rpc_url(&self) -> String {
        format!("https://{NETWORK}.g.alchemy.com/v2/{}", self.api_key)
    }

    fn portfolio_tokens_url(&self) -> String {
        format!("{PORTFOLIO_BASE}/{}/assets/tokens/by-address", self.api_key)
    }
}

// ── JSON-RPC envelope ────────────────────────────────────────────────────────

#[derive(Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'a str,
    id: u32,
    method: &'a str,
    params: Value,
}

#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct RpcResponse<T> {
    #[serde(default = "Option::default")]
    result: Option<T>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}

async fn rpc<T: for<'de> Deserialize<'de>>(
    url: &str,
    method: &str,
    params: Value,
    label: &'static str,
) -> Result<T, String> {
    let body = RpcRequest {
        jsonrpc: "2.0",
        id: 1,
        method,
        params,
    };
    let resp: RpcResponse<T> = http_client()
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("{label} POST: {e}"))?
        .error_for_status()
        .map_err(|e| format!("{label} status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("{label} decode: {e}"))?;
    if let Some(err) = resp.error {
        return Err(format!("{label}: {}", err.message));
    }
    resp.result.ok_or_else(|| format!("{label}: empty result"))
}

// ── Asset transfers (transactions endpoint) ─────────────────────────────────

#[derive(Deserialize)]
struct AssetTransfersResult {
    transfers: Vec<RawTransfer>,
}

#[derive(Deserialize, Clone)]
struct RawTransfer {
    hash: String,
    #[serde(rename = "blockNum")]
    block_num: String,
    from: Option<String>,
    to: Option<String>,
    #[serde(default)]
    category: String,
    #[serde(default, rename = "rawContract")]
    raw_contract: Option<RawContract>,
    #[serde(default)]
    metadata: Option<TransferMetadata>,
}

#[derive(Deserialize, Clone)]
struct RawContract {
    #[serde(default)]
    value: Option<String>,
}

#[derive(Deserialize, Clone)]
struct TransferMetadata {
    #[serde(default, rename = "blockTimestamp")]
    block_timestamp: Option<String>,
}

// ── Portfolio API (balances endpoint) ───────────────────────────────────────

#[derive(Deserialize)]
struct PortfolioResponse {
    data: PortfolioData,
}

#[derive(Deserialize)]
struct PortfolioData {
    #[serde(default)]
    tokens: Vec<PortfolioToken>,
}

#[derive(Deserialize)]
struct PortfolioToken {
    /// `null` for the native asset (ETH on eth-mainnet).
    #[serde(default, rename = "tokenAddress")]
    token_address: Option<String>,
    #[serde(rename = "tokenBalance")]
    token_balance: String,
    #[serde(default, rename = "tokenMetadata")]
    token_metadata: Option<PortfolioMetadata>,
    #[serde(default, rename = "tokenPrices")]
    token_prices: Vec<PriceQuote>,
}

#[derive(Deserialize, Default)]
struct PortfolioMetadata {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    decimals: Option<u8>,
    #[serde(default)]
    logo: Option<String>,
}

#[derive(Deserialize)]
struct PriceQuote {
    #[serde(default)]
    currency: String,
    #[serde(default)]
    value: String,
}

fn extract_usd(prices: &[PriceQuote]) -> Option<f64> {
    prices
        .iter()
        .find(|p| p.currency.eq_ignore_ascii_case("usd"))
        .and_then(|p| p.value.parse::<f64>().ok())
}

// ── Indexer impl ─────────────────────────────────────────────────────────────

#[async_trait]
impl Indexer for AlchemyClient {
    async fn transactions(&self, addr: Address, limit: usize) -> Result<Vec<IndexedTx>, String> {
        let url = self.rpc_url();
        let categories = json!(["external", "erc20", "internal"]);
        let max_count_hex = format!("0x{:x}", limit.max(1));
        let from_params = json!([{
            "fromBlock": "0x0",
            "toBlock": "latest",
            "fromAddress": format!("{addr:#x}"),
            "category": categories,
            "maxCount": max_count_hex,
            "order": "desc",
            "withMetadata": true,
        }]);
        let to_params = json!([{
            "fromBlock": "0x0",
            "toBlock": "latest",
            "toAddress": format!("{addr:#x}"),
            "category": categories,
            "maxCount": max_count_hex,
            "order": "desc",
            "withMetadata": true,
        }]);

        let (sent, received): (
            Result<AssetTransfersResult, String>,
            Result<AssetTransfersResult, String>,
        ) = futures::future::join(
            rpc(&url, "alchemy_getAssetTransfers", from_params, "alchemy transfers (sent)"),
            rpc(&url, "alchemy_getAssetTransfers", to_params, "alchemy transfers (received)"),
        )
        .await;

        let mut all: Vec<RawTransfer> = Vec::new();
        if let Ok(s) = sent {
            all.extend(s.transfers);
        } else if let Err(e) = sent {
            warn!(error = %e, "alchemy sent transfers failed");
        }
        if let Ok(r) = received {
            all.extend(r.transfers);
        } else if let Err(e) = received {
            warn!(error = %e, "alchemy received transfers failed");
        }

        let mut seen: HashSet<String> = HashSet::new();
        let mut unique: Vec<RawTransfer> = Vec::with_capacity(all.len());
        for t in all {
            if seen.insert(t.hash.clone()) {
                unique.push(t);
            }
        }

        unique.sort_by(|a, b| {
            let ab = u64::from_str_radix(a.block_num.trim_start_matches("0x"), 16).unwrap_or(0);
            let bb = u64::from_str_radix(b.block_num.trim_start_matches("0x"), 16).unwrap_or(0);
            bb.cmp(&ab)
        });

        Ok(unique
            .into_iter()
            .take(limit)
            .filter_map(|t| convert_transfer(t, addr))
            .collect())
    }

    async fn balances(&self, addr: Address) -> Result<Vec<IndexedToken>, String> {
        let url = self.portfolio_tokens_url();
        let body = json!({
            "addresses": [{
                "address": format!("{addr:#x}"),
                "networks": [NETWORK],
            }],
            "withMetadata": true,
            "withPrices": true,
            "includeNativeTokens": true,
        });

        let resp: PortfolioResponse = http_client()
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("alchemy portfolio POST: {e}"))?
            .error_for_status()
            .map_err(|e| format!("alchemy portfolio status: {e}"))?
            .json()
            .await
            .map_err(|e| format!("alchemy portfolio decode: {e}"))?;

        Ok(parse_portfolio(resp.data.tokens))
    }
}

fn parse_portfolio(tokens: Vec<PortfolioToken>) -> Vec<IndexedToken> {
    let mut native: Option<IndexedToken> = None;
    let mut erc20: Vec<IndexedToken> = Vec::with_capacity(tokens.len());

    for t in tokens {
        let raw = U256::from_str_radix(t.token_balance.trim_start_matches("0x"), 16)
            .unwrap_or(U256::ZERO);
        let meta = t.token_metadata.unwrap_or_default();
        let price = extract_usd(&t.token_prices);

        match t.token_address.as_deref() {
            None | Some("") => {
                let (eth_str, eth_f64) = format_eth_balance(raw);
                native = Some(IndexedToken {
                    symbol: meta.symbol.unwrap_or_else(|| "ETH".into()),
                    name: meta.name.unwrap_or_else(|| "Ethereum".into()),
                    contract: None,
                    decimals: 18,
                    balance_raw: raw,
                    balance_f64: eth_f64,
                    balance: eth_str,
                    usd_price: price,
                    usd_value: price.map(|p| p * eth_f64),
                    logo_url: meta.logo,
                });
            }
            Some(addr_str) => {
                if raw.is_zero() {
                    continue;
                }
                let Ok(contract) = Address::from_str(addr_str) else {
                    continue;
                };
                let decimals = meta.decimals.unwrap_or(18);
                let (bal_str, bal_f64) = format_token_balance(raw, decimals);
                erc20.push(IndexedToken {
                    symbol: meta.symbol.unwrap_or_default(),
                    name: meta.name.unwrap_or_default(),
                    contract: Some(contract),
                    decimals,
                    balance_raw: raw,
                    balance_f64: bal_f64,
                    balance: bal_str,
                    usd_price: price,
                    usd_value: price.map(|p| p * bal_f64),
                    logo_url: meta.logo,
                });
            }
        }
    }

    erc20.sort_by(|a, b| {
        let av = a.usd_value.unwrap_or(0.0);
        let bv = b.usd_value.unwrap_or(0.0);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out = Vec::with_capacity(erc20.len() + 1);
    out.push(native.unwrap_or_else(|| {
        // Defensive: includeNativeTokens=true should always yield an ETH row,
        // but if the API drops it, surface a zero placeholder so the
        // ETH-first invariant the UI relies on stays intact.
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

fn convert_transfer(t: RawTransfer, owner: Address) -> Option<IndexedTx> {
    let hash = B256::from_str(&t.hash).ok()?;
    let block_number =
        u64::from_str_radix(t.block_num.trim_start_matches("0x"), 16).unwrap_or(0);
    let from = t.from.as_deref().and_then(|s| Address::from_str(s).ok())?;
    let to = t.to.as_deref().and_then(|s| Address::from_str(s).ok());
    let value = t
        .raw_contract
        .as_ref()
        .and_then(|c| c.value.as_deref())
        .and_then(|h| U256::from_str_radix(h.trim_start_matches("0x"), 16).ok())
        .unwrap_or(U256::ZERO);
    let timestamp = t
        .metadata
        .as_ref()
        .and_then(|m| m.block_timestamp.as_deref())
        .map(parse_iso8601)
        .unwrap_or(0);
    Some(IndexedTx {
        hash,
        block_number,
        timestamp,
        from,
        to,
        value,
        gas_used: None,
        gas_price: None,
        // alchemy_getAssetTransfers only surfaces successful transfers — failed
        // txs don't move value and don't appear here.
        status: TxStatus::Success,
        direction: classify_direction(from, to, owner),
        method: Some(t.category),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::TxDirection;

    #[test]
    fn extract_usd_picks_usd_quote() {
        let quotes = vec![
            PriceQuote {
                currency: "eur".into(),
                value: "0.92".into(),
            },
            PriceQuote {
                currency: "USD".into(),
                value: "2300.50".into(),
            },
        ];
        assert_eq!(extract_usd(&quotes), Some(2300.50));
    }

    #[test]
    fn extract_usd_returns_none_when_missing() {
        let quotes = vec![PriceQuote {
            currency: "btc".into(),
            value: "0.05".into(),
        }];
        assert_eq!(extract_usd(&quotes), None);
    }

    #[test]
    fn convert_transfer_decodes_hex_block_and_value() {
        let owner: Address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045".parse().unwrap();
        let raw: RawTransfer = serde_json::from_str(
            r#"{
                "hash": "0x4444444444444444444444444444444444444444444444444444444444444444",
                "blockNum": "0x112a880",
                "from": "0x000000000000000000000000000000000000beef",
                "to": "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
                "category": "external",
                "rawContract": { "value": "0xde0b6b3a7640000" },
                "metadata": { "blockTimestamp": "2024-01-01T00:00:00.000000Z" }
            }"#,
        )
        .unwrap();
        let tx = convert_transfer(raw, owner).expect("converts");
        assert_eq!(tx.block_number, 0x112a880);
        assert_eq!(tx.value, U256::from(1_000_000_000_000_000_000u128));
        assert_eq!(tx.timestamp, 1_704_067_200);
        assert!(matches!(tx.direction, TxDirection::In));
        assert_eq!(tx.method.as_deref(), Some("external"));
    }

    #[test]
    fn rpc_response_decodes_error_envelope() {
        let json = r#"{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32600, "message": "boom" } }"#;
        let resp: RpcResponse<Value> = serde_json::from_str(json).unwrap();
        assert!(resp.result.is_none());
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().message, "boom");
    }

    #[test]
    fn parse_portfolio_orders_eth_first_and_filters_zero() {
        let resp: PortfolioResponse = serde_json::from_str(
            r#"{
              "data": {
                "tokens": [
                  {
                    "address": "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
                    "network": "eth-mainnet",
                    "tokenAddress": null,
                    "tokenBalance": "0x22b1c8c1227a00000",
                    "tokenMetadata": { "name": "Ethereum", "symbol": "ETH", "decimals": 18 },
                    "tokenPrices": [ { "currency": "usd", "value": "2000.00" } ]
                  },
                  {
                    "address": "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
                    "network": "eth-mainnet",
                    "tokenAddress": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                    "tokenBalance": "0x4c4b40",
                    "tokenMetadata": {
                      "name": "USD Coin", "symbol": "USDC", "decimals": 6,
                      "logo": "https://example/usdc.png"
                    },
                    "tokenPrices": [ { "currency": "usd", "value": "1.00" } ]
                  },
                  {
                    "address": "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
                    "network": "eth-mainnet",
                    "tokenAddress": "0xdac17f958d2ee523a2206206994597c13d831ec7",
                    "tokenBalance": "0x0",
                    "tokenMetadata": { "name": "Tether", "symbol": "USDT", "decimals": 6 },
                    "tokenPrices": []
                  },
                  {
                    "address": "0xd8da6bf26964af9d7eed9e03e53415d37aa96045",
                    "network": "eth-mainnet",
                    "tokenAddress": "0x514910771af9ca656af840dff83e8264ecf986ca",
                    "tokenBalance": "0xde0b6b3a7640000",
                    "tokenMetadata": { "name": "Chainlink", "symbol": "LINK", "decimals": 18 },
                    "tokenPrices": [ { "currency": "usd", "value": "15.00" } ]
                  }
                ]
              }
            }"#,
        )
        .unwrap();
        let out = parse_portfolio(resp.data.tokens);

        assert_eq!(out[0].symbol, "ETH");
        assert!(out[0].contract.is_none());
        assert_eq!(out[0].usd_price, Some(2000.0));

        // Two non-zero ERC-20s, sorted by USD value desc.
        // ETH is 40 ETH * 2000 = 80,000; LINK is 1 * 15 = 15; USDC is 5 * 1 = 5.
        // (Within ERC-20s only: LINK > USDC.)
        assert_eq!(out.len(), 3, "ETH + LINK + USDC; USDT is zero-balance");
        assert_eq!(out[1].symbol, "LINK");
        assert_eq!(out[2].symbol, "USDC");
    }

    #[test]
    fn parse_portfolio_emits_zero_eth_placeholder_when_native_missing() {
        // No tokens at all — the helper should still surface a zero-ETH row.
        let out = parse_portfolio(Vec::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].symbol, "ETH");
        assert_eq!(out[0].balance_raw, U256::ZERO);
    }
}
