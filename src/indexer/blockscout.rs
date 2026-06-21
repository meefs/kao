//! Blockscout V2 API indexer.
//!
//! Defaults to the public mainnet instance at <https://eth.blockscout.com>;
//! pass a custom `base_url` to point at any Blockscout V2 deployment (L2s,
//! self-hosted, etc.). An optional API key is appended as `?apikey=…` and is
//! accepted by Blockscout Cloud and several public instances for higher rate
//! limits — public eth.blockscout.com ignores it harmlessly.
//!
//! ERC-20 balances come from `/addresses/{addr}/tokens?type=ERC-20`, the
//! paginated/type-filtered endpoint. Only the first page is fetched (default
//! 50 items) — that's plenty for an unverified balance overview, and avoids
//! pagination round-trips for accounts with hundreds of dust tokens.

use std::str::FromStr;

use alloy::primitives::{Address, B256, U256};
use async_trait::async_trait;
use serde::Deserialize;

use crate::portfolio::{format_eth_balance, format_token_balance};

use crate::chain::Chain;

use super::{
    IndexedToken, IndexedTx, Indexer, TokenTransfer, TxStatus, classify_direction,
    http_client_or_err, parse_iso8601, redact_url_in_err,
};

const DEFAULT_BASE: &str = "https://eth.blockscout.com";

#[derive(Default)]
pub struct BlockscoutClient {
    /// Instance base URL without `/api/v2` (e.g. `https://eth.blockscout.com`).
    /// `None` falls back to `DEFAULT_BASE`.
    base_url: Option<String>,
    api_key: Option<String>,
}

impl BlockscoutClient {
    pub fn new(base_url: Option<String>, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.map(strip_trailing_slash),
            api_key,
        }
    }

    fn base(&self) -> &str {
        self.base_url.as_deref().unwrap_or(DEFAULT_BASE)
    }

    /// Build a full URL for `path` (which must start with `/api/v2/...`).
    /// `extra_query` is appended verbatim if non-empty (no leading `?`/`&`),
    /// then `apikey` is tacked on when configured.
    fn build_url(&self, path: &str, extra_query: &str) -> String {
        let mut url = format!("{}{path}", self.base());
        let mut sep = if path.contains('?') { '&' } else { '?' };
        if !extra_query.is_empty() {
            url.push(sep);
            url.push_str(extra_query);
            sep = '&';
        }
        if let Some(key) = self.api_key.as_deref() {
            url.push(sep);
            url.push_str("apikey=");
            url.push_str(&urlencode(key));
        }
        url
    }
}

// ── HTTP shapes ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TxListResponse {
    items: Vec<RawTx>,
}

#[derive(Deserialize)]
struct RawTx {
    hash: String,
    #[serde(alias = "block")]
    block_number: Option<u64>,
    timestamp: Option<String>,
    from: AddressRef,
    to: Option<AddressRef>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    gas_used: Option<String>,
    #[serde(default)]
    gas_price: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    method: Option<String>,
}

#[derive(Deserialize)]
struct AddressRef {
    hash: String,
}

/// `/api/v2/addresses/{addr}/token-transfers` row. Only the fields the
/// activity feed needs are decoded — the live API returns a much larger
/// envelope (block_hash, log_index, address metadata, …) that we drop.
#[derive(Deserialize)]
struct RawTokenTransfer {
    transaction_hash: String,
    #[serde(default)]
    block_number: Option<u64>,
    #[serde(default)]
    timestamp: Option<String>,
    from: AddressRef,
    #[serde(default)]
    to: Option<AddressRef>,
    total: TransferTotal,
    token: TokenMeta,
    #[serde(default)]
    method: Option<String>,
    /// `"ERC-20"` after the server-side `type=ERC-20` filter, but we
    /// double-check defensively because some forks ignore the param.
    #[serde(default)]
    token_type: Option<String>,
}

#[derive(Deserialize)]
struct TransferTotal {
    value: String,
    #[serde(default)]
    decimals: Option<String>,
}

#[derive(Deserialize)]
struct TokenTransfersResponse {
    items: Vec<RawTokenTransfer>,
}

#[derive(Deserialize)]
struct AddressDetail {
    #[serde(default)]
    coin_balance: Option<String>,
    #[serde(default)]
    exchange_rate: Option<String>,
}

#[derive(Deserialize)]
struct TokenListResponse {
    items: Vec<TokenBalanceRow>,
}

#[derive(Deserialize)]
struct TokenBalanceRow {
    value: String,
    token: TokenMeta,
    /// Present on NFT rows; lets us drop them defensively even when an
    /// instance somehow makes it past the `type=ERC-20` server filter.
    #[serde(default)]
    token_id: Option<String>,
}

#[derive(Deserialize)]
struct TokenMeta {
    /// Newer Blockscout instances (and the official docs) use
    /// `address_hash`; older deployments still serve `address`. Accept
    /// both so a custom-URL pointing at either variant works.
    #[serde(rename = "address_hash", alias = "address")]
    address_hash: String,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    name: Option<String>,
    decimals: Option<String>,
    #[serde(default)]
    exchange_rate: Option<String>,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

// ── Indexer impl ─────────────────────────────────────────────────────────────

#[async_trait]
impl Indexer for BlockscoutClient {
    async fn transactions(&self, addr: Address, limit: usize) -> Result<Vec<IndexedTx>, String> {
        let txs_url = self.build_url(
            // No `filter=…` — Blockscout V2's filter only accepts the
            // literal values `to` or `from` (one direction each).
            // Omitting the param returns both incoming and outgoing.
            &format!("/api/v2/addresses/{addr:#x}/transactions"),
            "",
        );
        let token_url = self.build_url(
            &format!("/api/v2/addresses/{addr:#x}/token-transfers"),
            "type=ERC-20",
        );

        let client = http_client_or_err()?;
        let (txs_res, tokens_res): (
            Result<TxListResponse, String>,
            Result<TokenTransfersResponse, String>,
        ) = futures::future::join(
            async {
                client
                    .get(&txs_url)
                    .send()
                    .await
                    .map_err(|e| format!("blockscout transactions GET: {}", redact_url_in_err(e)))?
                    .error_for_status()
                    .map_err(|e| {
                        format!("blockscout transactions status: {}", redact_url_in_err(e))
                    })?
                    .json::<TxListResponse>()
                    .await
                    .map_err(|e| {
                        format!("blockscout transactions decode: {}", redact_url_in_err(e))
                    })
            },
            async {
                client
                    .get(&token_url)
                    .send()
                    .await
                    .map_err(|e| {
                        format!("blockscout token-transfers GET: {}", redact_url_in_err(e))
                    })?
                    .error_for_status()
                    .map_err(|e| {
                        format!(
                            "blockscout token-transfers status: {}",
                            redact_url_in_err(e)
                        )
                    })?
                    .json::<TokenTransfersResponse>()
                    .await
                    .map_err(|e| {
                        format!(
                            "blockscout token-transfers decode: {}",
                            redact_url_in_err(e)
                        )
                    })
            },
        )
        .await;

        // Native-tx fetch is the must-have; the token call is best-effort
        // so a token-endpoint hiccup doesn't blank the activity feed.
        let normal = txs_res?;
        let tokens = tokens_res.map(|r| r.items).unwrap_or_default();
        Ok(merge_normal_and_token(normal.items, tokens, addr, limit))
    }

    async fn balances(&self, addr: Address) -> Result<Vec<IndexedToken>, String> {
        let detail_url = self.build_url(&format!("/api/v2/addresses/{addr:#x}"), "");
        let tokens_url = self.build_url(
            &format!("/api/v2/addresses/{addr:#x}/tokens"),
            "type=ERC-20",
        );

        let client = http_client_or_err()?;
        let (detail, tokens) = futures::future::join(
            async {
                client
                    .get(&detail_url)
                    .send()
                    .await
                    .map_err(|e| format!("blockscout address GET: {}", redact_url_in_err(e)))?
                    .error_for_status()
                    .map_err(|e| format!("blockscout address status: {}", redact_url_in_err(e)))?
                    .json::<AddressDetail>()
                    .await
                    .map_err(|e| format!("blockscout address decode: {}", redact_url_in_err(e)))
            },
            async {
                client
                    .get(&tokens_url)
                    .send()
                    .await
                    .map_err(|e| format!("blockscout tokens GET: {}", redact_url_in_err(e)))?
                    .error_for_status()
                    .map_err(|e| format!("blockscout tokens status: {}", redact_url_in_err(e)))?
                    .json::<TokenListResponse>()
                    .await
                    .map_err(|e| format!("blockscout tokens decode: {}", redact_url_in_err(e)))
            },
        )
        .await;

        let detail = detail?;
        let tokens = tokens?;
        Ok(parse_balances(detail, tokens.items))
    }
}

impl std::fmt::Debug for BlockscoutClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockscoutClient")
            .field("base_url", &self.base())
            .field("api_key", &self.api_key.as_deref().map(|_| "<redacted>"))
            .finish()
    }
}

// ── Pure parsers ─────────────────────────────────────────────────────────────

fn parse_txs(rows: Vec<RawTx>, owner: Address, limit: usize) -> Vec<IndexedTx> {
    rows.into_iter()
        .take(limit)
        .filter_map(|r| convert_tx(r, owner))
        .collect()
}

fn convert_tx(r: RawTx, owner: Address) -> Option<IndexedTx> {
    let hash = B256::from_str(&r.hash).ok()?;
    let from = parse_address(&r.from.hash)?;
    let to = r.to.as_ref().and_then(|a| parse_address(&a.hash));
    let value = r
        .value
        .as_deref()
        .and_then(|s| U256::from_str(s).ok())
        .unwrap_or(U256::ZERO);
    let status = match r.status.as_deref() {
        Some("ok") => TxStatus::Success,
        Some("error") => TxStatus::Failure,
        _ => TxStatus::Pending,
    };
    Some(IndexedTx {
        hash,
        block_number: r.block_number.unwrap_or(0),
        timestamp: r.timestamp.as_deref().map(parse_iso8601).unwrap_or(0),
        from,
        to,
        value,
        gas_used: r.gas_used.as_deref().and_then(|s| s.parse().ok()),
        gas_price: r.gas_price.as_deref().and_then(|s| s.parse().ok()),
        status,
        direction: classify_direction(from, to, owner),
        method: r.method,
        token: None,
        chain: Chain::Mainnet,
    })
}

fn convert_token_transfer(r: RawTokenTransfer, owner: Address) -> Option<IndexedTx> {
    if r.token_type
        .as_deref()
        .is_some_and(|t| !t.eq_ignore_ascii_case("ERC-20"))
    {
        return None;
    }
    let hash = B256::from_str(&r.transaction_hash).ok()?;
    let from = parse_address(&r.from.hash)?;
    let to = r.to.as_ref().and_then(|a| parse_address(&a.hash));
    let contract = parse_address(&r.token.address_hash)?;
    let amount_raw = U256::from_str(&r.total.value).unwrap_or(U256::ZERO);
    // `total.decimals` is authoritative for the actual transferred amount;
    // fall back to the token-level decimals if the per-row field is
    // missing on a quirky deployment.
    let decimals = r
        .total
        .decimals
        .as_deref()
        .or(r.token.decimals.as_deref())
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(18);
    Some(IndexedTx {
        hash,
        block_number: r.block_number.unwrap_or(0),
        timestamp: r.timestamp.as_deref().map(parse_iso8601).unwrap_or(0),
        from,
        to,
        value: U256::ZERO,
        gas_used: None,
        gas_price: None,
        // `/token-transfers` only enumerates successful Transfer logs.
        status: TxStatus::Success,
        direction: classify_direction(from, to, owner),
        method: r.method,
        token: Some(TokenTransfer {
            contract,
            symbol: r.token.symbol.unwrap_or_default(),
            decimals,
            amount_raw,
            is_nft: false,
            token_id: None,
        }),
        chain: Chain::Mainnet,
    })
}

/// Merge `/transactions` (outer txs) and `/token-transfers` (ERC-20
/// movements). Outer txs whose hash also produced a token-transfer row
/// are dropped — they'd otherwise render as a redundant "0 ETH"
/// alongside the real movement.
fn merge_normal_and_token(
    normal: Vec<RawTx>,
    tokens: Vec<RawTokenTransfer>,
    owner: Address,
    limit: usize,
) -> Vec<IndexedTx> {
    use std::collections::HashSet;

    let mut out: Vec<IndexedTx> = Vec::with_capacity(normal.len() + tokens.len());
    let mut token_hashes: HashSet<B256> = HashSet::with_capacity(tokens.len());
    for r in tokens {
        if let Some(tx) = convert_token_transfer(r, owner) {
            token_hashes.insert(tx.hash);
            out.push(tx);
        }
    }
    for r in normal {
        let Some(tx) = convert_tx(r, owner) else {
            continue;
        };
        if token_hashes.contains(&tx.hash) {
            continue;
        }
        out.push(tx);
    }
    out.sort_by_key(|tx| std::cmp::Reverse(tx.block_number));
    out.truncate(limit);
    out
}

fn parse_balances(detail: AddressDetail, rows: Vec<TokenBalanceRow>) -> Vec<IndexedToken> {
    let mut out: Vec<IndexedToken> = Vec::with_capacity(rows.len() + 1);

    let eth_raw = detail
        .coin_balance
        .as_deref()
        .and_then(|s| U256::from_str(s).ok())
        .unwrap_or(U256::ZERO);
    let eth_price: Option<f64> = detail
        .exchange_rate
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok());
    let (eth_str, eth_f64) = format_eth_balance(eth_raw);
    out.push(IndexedToken {
        symbol: "ETH".into(),
        name: "Ethereum".into(),
        contract: None,
        decimals: 18,
        balance_raw: eth_raw,
        balance_f64: eth_f64,
        balance: eth_str,
        usd_price: eth_price,
        usd_value: eth_price.map(|p| p * eth_f64),
        logo_url: None,
    });

    for row in rows {
        // The `type=ERC-20` query filter SHOULD prevent NFT rows server-side,
        // but custom Blockscout instances behave inconsistently — drop any
        // non-ERC-20 row defensively, and skip rows that carry a `token_id`
        // (only NFTs have one).
        if row.token_id.is_some() {
            continue;
        }
        if row.token.kind.as_deref().is_some_and(|k| k != "ERC-20") {
            continue;
        }
        let Some(contract) = parse_address(&row.token.address_hash) else {
            continue;
        };
        let decimals = row
            .token
            .decimals
            .as_deref()
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(18);
        let raw = U256::from_str(&row.value).unwrap_or(U256::ZERO);
        if raw.is_zero() {
            continue;
        }
        let (bal_str, bal_f64) = format_token_balance(raw, decimals);
        let price: Option<f64> = row
            .token
            .exchange_rate
            .as_deref()
            .and_then(|s| s.parse::<f64>().ok());
        out.push(IndexedToken {
            symbol: row.token.symbol.unwrap_or_default(),
            name: row.token.name.unwrap_or_default(),
            contract: Some(contract),
            decimals,
            balance_raw: raw,
            balance_f64: bal_f64,
            balance: bal_str,
            usd_price: price,
            usd_value: price.map(|p| p * bal_f64),
            logo_url: row.token.icon_url,
        });
    }

    out[1..].sort_by(|a, b| {
        let av = a.usd_value.unwrap_or(0.0);
        let bv = b.usd_value.unwrap_or(0.0);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn parse_address(s: &str) -> Option<Address> {
    Address::from_str(s).ok()
}

fn strip_trailing_slash(mut s: String) -> String {
    while s.ends_with('/') {
        s.pop();
    }
    s
}

/// Tiny RFC 3986 percent-encoder for query values. Only used for the
/// `apikey` parameter — keeps a stray `&` in a key from breaking the URL.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::TxDirection;

    const OWNER: &str = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";
    const OTHER: &str = "0x000000000000000000000000000000000000beef";

    fn owner() -> Address {
        Address::from_str(OWNER).unwrap()
    }

    #[test]
    fn build_url_appends_apikey_with_correct_separator() {
        let no_key = BlockscoutClient::new(None, None);
        assert_eq!(
            no_key.build_url("/api/v2/addresses/0xabc", ""),
            "https://eth.blockscout.com/api/v2/addresses/0xabc",
        );
        assert_eq!(
            no_key.build_url("/api/v2/addresses/0xabc/tokens", "type=ERC-20"),
            "https://eth.blockscout.com/api/v2/addresses/0xabc/tokens?type=ERC-20",
        );

        let with_key = BlockscoutClient::new(
            Some("https://base.blockscout.com/".into()),
            Some("MYKEY".into()),
        );
        // Trailing slash stripped from base; apikey uses `?` when no other query.
        assert_eq!(
            with_key.build_url("/api/v2/addresses/0xabc", ""),
            "https://base.blockscout.com/api/v2/addresses/0xabc?apikey=MYKEY",
        );
        // Existing query → apikey appended with `&`.
        assert_eq!(
            with_key.build_url("/api/v2/addresses/0xabc/tokens", "type=ERC-20"),
            "https://base.blockscout.com/api/v2/addresses/0xabc/tokens?type=ERC-20&apikey=MYKEY",
        );
    }

    #[test]
    fn build_url_percent_encodes_apikey() {
        let c = BlockscoutClient::new(None, Some("a&b=c".into()));
        let url = c.build_url("/api/v2/x", "");
        assert!(url.ends_with("apikey=a%26b%3Dc"), "got: {url}");
    }

    #[test]
    fn debug_redacts_api_key() {
        let c = BlockscoutClient::new(None, Some("super-secret".into()));
        let s = format!("{c:?}");
        assert!(!s.contains("super-secret"), "key leaked into Debug: {s}");
        assert!(s.contains("<redacted>"));
    }

    #[test]
    fn parses_tx_list_response() {
        let json = format!(
            r#"{{
              "items": [
                {{
                  "hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
                  "block_number": 18000000,
                  "timestamp": "2024-01-01T00:00:00.000000Z",
                  "from": {{ "hash": "{OTHER}" }},
                  "to":   {{ "hash": "{OWNER}" }},
                  "value": "1000000000000000000",
                  "gas_used": "21000",
                  "gas_price": "12000000000",
                  "status": "ok",
                  "method": "transfer"
                }},
                {{
                  "hash": "0x2222222222222222222222222222222222222222222222222222222222222222",
                  "block_number": 18000001,
                  "timestamp": "2024-06-15T12:34:56.000000Z",
                  "from": {{ "hash": "{OWNER}" }},
                  "to":   null,
                  "value": "0",
                  "gas_used": "100000",
                  "gas_price": "20000000000",
                  "status": "error"
                }}
              ]
            }}"#
        );
        let raw: TxListResponse = serde_json::from_str(&json).expect("parses");
        let txs = parse_txs(raw.items, owner(), 10);
        assert_eq!(txs.len(), 2);

        assert_eq!(txs[0].block_number, 18_000_000);
        assert_eq!(txs[0].timestamp, 1_704_067_200);
        assert!(matches!(txs[0].status, TxStatus::Success));
        assert!(matches!(txs[0].direction, TxDirection::In));
        assert_eq!(txs[0].value, U256::from(1_000_000_000_000_000_000u128));
        assert_eq!(txs[0].method.as_deref(), Some("transfer"));

        assert!(txs[1].to.is_none());
        assert!(matches!(txs[1].status, TxStatus::Failure));
        assert!(matches!(txs[1].direction, TxDirection::Out));
    }

    #[test]
    fn parse_balances_filters_zero_and_orders_eth_first() {
        let detail: AddressDetail = serde_json::from_str(
            r#"{ "coin_balance": "2500000000000000000", "exchange_rate": "2000.00" }"#,
        )
        .unwrap();
        // /tokens?type=ERC-20 wraps items in an envelope, but the
        // `parse_balances` helper takes the inner `items` directly.
        // Mirrors the documented Blockscout response shape:
        // - `token.address_hash` (not `address`),
        // - NFT rows carry a `token_id` value.
        let envelope: TokenListResponse = serde_json::from_str(
            r#"{
              "items": [
                {
                  "value": "5000000",
                  "token_id": null,
                  "token_instance": null,
                  "token": {
                    "address_hash": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                    "symbol": "USDC",
                    "name": "USD Coin",
                    "decimals": "6",
                    "exchange_rate": "1.00",
                    "icon_url": "https://example/usdc.png",
                    "type": "ERC-20"
                  }
                },
                {
                  "value": "0",
                  "token": {
                    "address_hash": "0xdac17f958d2ee523a2206206994597c13d831ec7",
                    "symbol": "USDT",
                    "name": "Tether",
                    "decimals": "6",
                    "type": "ERC-20"
                  }
                },
                {
                  "value": "1",
                  "token_id": "42",
                  "token": {
                    "address_hash": "0x0000000000000000000000000000000000001234",
                    "symbol": "NFT",
                    "name": "Some NFT",
                    "decimals": "0",
                    "type": "ERC-721"
                  }
                }
              ],
              "next_page_params": null
            }"#,
        )
        .unwrap();
        let out = parse_balances(detail, envelope.items);

        assert_eq!(out.len(), 2, "ETH + 1 surviving ERC-20");
        assert_eq!(out[0].symbol, "ETH");
        assert!(out[0].contract.is_none());
        assert_eq!(out[0].usd_price, Some(2000.0));
        assert_eq!(out[0].usd_value, Some(2.5 * 2000.0));

        assert_eq!(out[1].symbol, "USDC");
        assert_eq!(out[1].decimals, 6);
        assert_eq!(out[1].balance_raw, U256::from(5_000_000u64));
        assert_eq!(out[1].usd_price, Some(1.0));
        assert_eq!(out[1].logo_url.as_deref(), Some("https://example/usdc.png"));
    }

    #[test]
    fn urlencode_unreserved_passes_through() {
        // RFC 3986 unreserved: ALPHA / DIGIT / "-" / "." / "_" / "~"
        let s = "abcXYZ012-._~";
        assert_eq!(urlencode(s), s);
    }

    #[test]
    fn urlencode_reserved_percent_encodes() {
        assert_eq!(urlencode("&=?#/+ "), "%26%3D%3F%23%2F%2B%20");
    }

    #[test]
    fn urlencode_utf8_each_byte() {
        // Pound sign U+00A3 = 0xC2 0xA3 in UTF-8.
        assert_eq!(urlencode("£"), "%C2%A3");
    }

    #[test]
    fn parse_address_valid_hex() {
        let a = parse_address(OWNER).unwrap();
        assert_eq!(format!("{a:#x}"), OWNER);
    }

    #[test]
    fn parse_address_rejects_malformed() {
        assert!(parse_address("not-an-address").is_none());
        assert!(parse_address("0x1234").is_none()); // too short
        assert!(parse_address("").is_none());
    }

    #[test]
    fn strip_trailing_slash_basic() {
        assert_eq!(strip_trailing_slash("https://x/".into()), "https://x");
        assert_eq!(strip_trailing_slash("https://x".into()), "https://x");
        assert_eq!(strip_trailing_slash("".into()), "");
    }

    #[test]
    fn strip_trailing_slash_idempotent_for_many_slashes() {
        assert_eq!(strip_trailing_slash("https://x///".into()), "https://x");
    }

    #[test]
    fn convert_token_transfer_erc20_happy_path() {
        let json = format!(
            r#"{{
              "transaction_hash": "0xaaaa0000000000000000000000000000000000000000000000000000000000aa",
              "block_number": 19000000,
              "timestamp": "2024-08-01T00:00:00.000000Z",
              "from":   {{ "hash": "{OWNER}" }},
              "to":     {{ "hash": "{OTHER}" }},
              "total":  {{ "value": "1500000", "decimals": "6" }},
              "token":  {{
                  "address_hash": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                  "symbol": "USDC",
                  "decimals": "6",
                  "type": "ERC-20"
              }},
              "token_type": "ERC-20",
              "method": "transfer"
            }}"#
        );
        let raw: RawTokenTransfer = serde_json::from_str(&json).unwrap();
        let tx = convert_token_transfer(raw, owner()).expect("ERC-20 transfer converts");
        assert_eq!(tx.value, U256::ZERO, "ETH value zero on token transfers");
        assert!(matches!(tx.status, TxStatus::Success));
        assert!(matches!(tx.direction, TxDirection::Out));
        assert_eq!(tx.block_number, 19_000_000);
        let token = tx.token.unwrap();
        assert_eq!(token.symbol, "USDC");
        assert_eq!(token.decimals, 6);
        assert_eq!(token.amount_raw, U256::from(1_500_000u64));
        assert!(!token.is_nft);
    }

    #[test]
    fn convert_token_transfer_rejects_non_erc20() {
        let json = format!(
            r#"{{
              "transaction_hash": "0xaaaa0000000000000000000000000000000000000000000000000000000000aa",
              "from":   {{ "hash": "{OWNER}" }},
              "to":     {{ "hash": "{OTHER}" }},
              "total":  {{ "value": "1" }},
              "token":  {{
                  "address_hash": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                  "type": "ERC-721"
              }},
              "token_type": "ERC-721"
            }}"#
        );
        let raw: RawTokenTransfer = serde_json::from_str(&json).unwrap();
        assert!(convert_token_transfer(raw, owner()).is_none());
    }

    #[test]
    fn convert_token_transfer_falls_back_to_token_decimals() {
        // total.decimals missing — convert_token_transfer must fall back to
        // token.decimals.
        let json = format!(
            r#"{{
              "transaction_hash": "0xaaaa0000000000000000000000000000000000000000000000000000000000aa",
              "from":   {{ "hash": "{OWNER}" }},
              "to":     {{ "hash": "{OTHER}" }},
              "total":  {{ "value": "1000" }},
              "token":  {{
                  "address_hash": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                  "decimals": "3"
              }}
            }}"#
        );
        let raw: RawTokenTransfer = serde_json::from_str(&json).unwrap();
        let tx = convert_token_transfer(raw, owner()).unwrap();
        assert_eq!(tx.token.unwrap().decimals, 3);
    }

    #[test]
    fn merge_normal_and_token_drops_outer_when_token_present() {
        let hash_match = "0x1111111111111111111111111111111111111111111111111111111111111111";
        let hash_other = "0x2222222222222222222222222222222222222222222222222222222222222222";

        let normal_json = format!(
            r#"[
              {{ "hash": "{hash_match}", "block_number": 100, "from": {{ "hash": "{OTHER}" }}, "to": {{ "hash": "{OWNER}" }}, "value": "0", "status": "ok" }},
              {{ "hash": "{hash_other}", "block_number": 99,  "from": {{ "hash": "{OWNER}" }}, "to": {{ "hash": "{OTHER}" }}, "value": "100", "status": "ok" }}
            ]"#
        );
        let token_json = format!(
            r#"[
              {{
                "transaction_hash": "{hash_match}",
                "from":  {{ "hash": "{OTHER}" }},
                "to":    {{ "hash": "{OWNER}" }},
                "total": {{ "value": "5000000", "decimals": "6" }},
                "token": {{ "address_hash": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", "symbol": "USDC", "decimals": "6" }},
                "token_type": "ERC-20",
                "block_number": 100
              }}
            ]"#
        );
        let normal: Vec<RawTx> = serde_json::from_str(&normal_json).unwrap();
        let tokens: Vec<RawTokenTransfer> = serde_json::from_str(&token_json).unwrap();
        let merged = merge_normal_and_token(normal, tokens, owner(), 10);

        assert_eq!(merged.len(), 2);
        // The "match" hash appears once (token row), not twice.
        let occurrences = merged
            .iter()
            .filter(|t| format!("{:#x}", t.hash) == hash_match)
            .count();
        assert_eq!(occurrences, 1);
        // Ordered desc by block_number.
        assert!(merged[0].block_number >= merged[1].block_number);
    }

    #[test]
    fn token_meta_accepts_legacy_address_field_alias() {
        // Older Blockscout deployments serve `address` instead of
        // `address_hash`; the alias must keep them working.
        let envelope: TokenListResponse = serde_json::from_str(
            r#"{
              "items": [
                {
                  "value": "1",
                  "token": {
                    "address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                    "symbol": "USDC",
                    "name": "USD Coin",
                    "decimals": "6",
                    "type": "ERC-20"
                  }
                }
              ]
            }"#,
        )
        .unwrap();
        assert_eq!(envelope.items.len(), 1);
        assert_eq!(
            envelope.items[0].token.address_hash,
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
        );
    }
}
