//! Etherscan V2 indexer (`api.etherscan.io/v2/api?chainid=1`).
//!
//! ERC-20 balances use the `account&action=addresstokenbalance` endpoint —
//! one call returns every token the address holds with symbol/name/decimals.
//! That endpoint requires a Pro tier API key; on a free key it returns an
//! error envelope which we surface to the caller. Native ETH balance and
//! ETH/USD price use the free `account&action=balance` and
//! `stats&action=ethprice` endpoints.
//!
//! Per-token USD prices are not fetched here — `module=token&action=tokeninfo`
//! is Pro-only and the addresstokenbalance response itself doesn't include
//! prices, so `usd_price` is always `None` for ERC-20s.

use std::str::FromStr;

use alloy::primitives::{Address, B256, U256};
use async_trait::async_trait;
use serde::Deserialize;

use crate::portfolio::{format_eth_balance, format_token_balance};

use super::{
    Indexer, IndexedToken, IndexedTx, TxStatus, classify_direction, http_client,
};

const BASE: &str = "https://api.etherscan.io/v2/api";
const CHAIN_ID: &str = "1";
/// Page size for `addresstokenbalance`. The endpoint paginates; one page is
/// enough for any practically-sized portfolio and avoids fan-out round-trips.
const TOKEN_PAGE_SIZE: usize = 100;

#[derive(Debug)]
pub struct EtherscanClient {
    api_key: String,
}

impl EtherscanClient {
    pub fn new(api_key: String) -> Self {
        Self { api_key }
    }

    fn url(&self, params: &[(&str, &str)]) -> String {
        let mut url = format!("{BASE}?chainid={CHAIN_ID}");
        for (k, v) in params {
            url.push('&');
            url.push_str(k);
            url.push('=');
            url.push_str(&urlencode(v));
        }
        url.push_str("&apikey=");
        url.push_str(&urlencode(&self.api_key));
        url
    }
}

// ── HTTP shapes ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Envelope<T> {
    status: String,
    #[serde(default)]
    message: String,
    result: T,
}

/// Etherscan returns `result: "Error! ..."` (a string) when status="0", and
/// `result: [...]` when status="1". `untagged` lets serde pick the right shape.
#[derive(Deserialize)]
#[serde(untagged)]
enum EnvelopeResult<T> {
    Ok(T),
    Err(String),
}

#[derive(Deserialize)]
struct RawTx {
    #[serde(rename = "blockNumber")]
    block_number: String,
    #[serde(rename = "timeStamp")]
    timestamp: String,
    hash: String,
    from: String,
    to: String,
    value: String,
    #[serde(rename = "gasUsed")]
    gas_used: String,
    #[serde(rename = "gasPrice")]
    gas_price: String,
    #[serde(rename = "isError", default)]
    is_error: String,
    #[serde(rename = "txreceipt_status", default)]
    txreceipt_status: String,
    #[serde(rename = "functionName", default)]
    function_name: String,
}

#[derive(Deserialize)]
struct RawTokenBalance {
    #[serde(rename = "TokenAddress")]
    token_address: String,
    #[serde(rename = "TokenName", default)]
    token_name: String,
    #[serde(rename = "TokenSymbol", default)]
    token_symbol: String,
    #[serde(rename = "TokenQuantity")]
    token_quantity: String,
    /// Number of decimals for the token (e.g. "18"), per the V2 docs. The
    /// `Divisor` name is historical — it's a decimal count, not 10^decimals.
    #[serde(rename = "TokenDivisor")]
    token_divisor: String,
}

#[derive(Deserialize)]
struct EthPrice {
    ethusd: String,
}

// ── Indexer impl ─────────────────────────────────────────────────────────────

#[async_trait]
impl Indexer for EtherscanClient {
    async fn transactions(&self, addr: Address, limit: usize) -> Result<Vec<IndexedTx>, String> {
        let addr_str = format!("{addr:#x}");
        let limit_str = limit.to_string();
        let url = self.url(&[
            ("module", "account"),
            ("action", "txlist"),
            ("address", &addr_str),
            ("startblock", "0"),
            ("endblock", "99999999"),
            ("page", "1"),
            ("offset", &limit_str),
            ("sort", "desc"),
        ]);
        let rows: Vec<RawTx> = fetch_envelope(&url, "etherscan txlist").await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| convert_tx(r, addr))
            .collect())
    }

    async fn balances(&self, addr: Address) -> Result<Vec<IndexedToken>, String> {
        let addr_str = format!("{addr:#x}");
        let page_size = TOKEN_PAGE_SIZE.to_string();

        let balance_url = self.url(&[
            ("module", "account"),
            ("action", "balance"),
            ("address", &addr_str),
            ("tag", "latest"),
        ]);
        let price_url = self.url(&[("module", "stats"), ("action", "ethprice")]);
        let tokens_url = self.url(&[
            ("module", "account"),
            ("action", "addresstokenbalance"),
            ("address", &addr_str),
            ("page", "1"),
            ("offset", &page_size),
        ]);

        let (eth_raw_str, eth_price, token_rows): (
            Result<String, String>,
            Result<EthPrice, String>,
            Result<Vec<RawTokenBalance>, String>,
        ) = futures::future::join3(
            fetch_envelope::<String>(&balance_url, "etherscan balance"),
            fetch_envelope::<EthPrice>(&price_url, "etherscan ethprice"),
            fetch_envelope::<Vec<RawTokenBalance>>(&tokens_url, "etherscan addresstokenbalance"),
        )
        .await;

        let eth_raw = eth_raw_str
            .ok()
            .and_then(|s| U256::from_str(&s).ok())
            .unwrap_or(U256::ZERO);
        let eth_usd = eth_price.ok().and_then(|p| p.ethusd.parse::<f64>().ok());

        let (eth_str, eth_f64) = format_eth_balance(eth_raw);
        let mut out = vec![IndexedToken {
            symbol: "ETH".into(),
            name: "Ethereum".into(),
            contract: None,
            decimals: 18,
            balance_raw: eth_raw,
            balance_f64: eth_f64,
            balance: eth_str,
            usd_price: eth_usd,
            usd_value: eth_usd.map(|p| p * eth_f64),
            logo_url: None,
        }];

        // `addresstokenbalance` is a Pro endpoint. On a free key the call
        // fails with an envelope error; surface it so the caller can prompt
        // the user to upgrade or switch providers.
        let rows = token_rows?;
        out.extend(parse_token_balances(rows));

        out[1..].sort_by(|a, b| {
            let av = a.usd_value.unwrap_or(0.0);
            let bv = b.usd_value.unwrap_or(0.0);
            bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(out)
    }
}

fn parse_token_balances(rows: Vec<RawTokenBalance>) -> Vec<IndexedToken> {
    rows.into_iter()
        .filter_map(|row| {
            let contract = Address::from_str(&row.token_address).ok()?;
            let decimals = row.token_divisor.parse::<u8>().unwrap_or(18);
            let raw = U256::from_str(&row.token_quantity).unwrap_or(U256::ZERO);
            if raw.is_zero() {
                return None;
            }
            let (bal_str, bal_f64) = format_token_balance(raw, decimals);
            Some(IndexedToken {
                symbol: row.token_symbol,
                name: row.token_name,
                contract: Some(contract),
                decimals,
                balance_raw: raw,
                balance_f64: bal_f64,
                balance: bal_str,
                usd_price: None,
                usd_value: None,
                logo_url: None,
            })
        })
        .collect()
}

// ── Helpers ──────────────────────────────────────────────────────────────────

async fn fetch_envelope<T: for<'de> Deserialize<'de>>(
    url: &str,
    label: &'static str,
) -> Result<T, String> {
    let body: Envelope<EnvelopeResult<T>> = http_client()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{label} GET: {e}"))?
        .error_for_status()
        .map_err(|e| format!("{label} status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("{label} decode: {e}"))?;

    if body.status != "1" {
        let err = match body.result {
            EnvelopeResult::Err(s) => s,
            EnvelopeResult::Ok(_) => body.message,
        };
        return Err(format!("{label}: {err}"));
    }
    match body.result {
        EnvelopeResult::Ok(v) => Ok(v),
        EnvelopeResult::Err(s) => Err(format!("{label}: {s}")),
    }
}

fn convert_tx(r: RawTx, owner: Address) -> Option<IndexedTx> {
    let hash = B256::from_str(&r.hash).ok()?;
    let block_number = r.block_number.parse::<u64>().ok()?;
    let timestamp = r.timestamp.parse::<u64>().unwrap_or(0);
    let from = Address::from_str(&r.from).ok()?;
    let to = if r.to.is_empty() {
        None
    } else {
        Address::from_str(&r.to).ok()
    };
    let value = U256::from_str(&r.value).unwrap_or(U256::ZERO);
    let gas_used = r.gas_used.parse::<u64>().ok();
    let gas_price = r.gas_price.parse::<u128>().ok();
    // Etherscan uses `txreceipt_status` post-Byzantium (the receipt's status
    // bit). Pre-Byzantium txs have `isError` — treat both: any "1" in
    // is_error wins; otherwise fall back to txreceipt_status.
    let status = if r.is_error == "1" {
        TxStatus::Failure
    } else if r.txreceipt_status.is_empty() || r.txreceipt_status == "1" {
        TxStatus::Success
    } else {
        TxStatus::Failure
    };
    let method = if r.function_name.is_empty() {
        None
    } else {
        // Strip the (...) parameter list to match Blockscout's "transfer"-style.
        Some(
            r.function_name
                .split('(')
                .next()
                .unwrap_or(&r.function_name)
                .to_string(),
        )
    };
    Some(IndexedTx {
        hash,
        block_number,
        timestamp,
        from,
        to,
        value,
        gas_used,
        gas_price,
        status,
        direction: classify_direction(from, to, owner),
        method,
    })
}

/// Minimal RFC 3986 percent-encoder. The crate doesn't depend on
/// `percent-encoding`, and we only ever encode address strings + simple
/// alphanumeric query values — anything outside that gets percent-encoded.
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

    #[test]
    fn urlencode_passes_through_alnum() {
        assert_eq!(urlencode("abc123"), "abc123");
        assert_eq!(urlencode("0xDeadBeef"), "0xDeadBeef");
        // Anything outside RFC 3986 unreserved must be percent-encoded so a
        // `/` or `&` in an API key can't escape the query value.
        assert_eq!(urlencode("a&b"), "a%26b");
        assert_eq!(urlencode("p/q"), "p%2Fq");
    }

    #[test]
    fn parses_envelope_error_response() {
        let json = r#"{ "status": "0", "message": "NOTOK", "result": "Invalid API key" }"#;
        let env: Envelope<EnvelopeResult<Vec<RawTx>>> = serde_json::from_str(json).unwrap();
        assert_eq!(env.status, "0");
        assert!(matches!(env.result, EnvelopeResult::Err(ref s) if s.contains("Invalid API key")));
    }

    #[test]
    fn convert_tx_handles_contract_creation_and_post_byzantium_failure() {
        let owner: Address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045".parse().unwrap();

        // Contract creation: empty `to` field.
        let creation = RawTx {
            block_number: "200".into(),
            timestamp: "1700000001".into(),
            hash: "0x0000000000000000000000000000000000000000000000000000000000000002".into(),
            from: format!("{owner:#x}"),
            to: String::new(),
            value: "0".into(),
            gas_used: "1500000".into(),
            gas_price: "20000000000".into(),
            is_error: "0".into(),
            txreceipt_status: "1".into(),
            function_name: String::new(),
        };
        let tx = convert_tx(creation, owner).expect("converts");
        assert!(tx.to.is_none());
        assert!(matches!(tx.direction, TxDirection::Out));
        assert!(matches!(tx.status, TxStatus::Success));

        // Post-Byzantium failure: txreceipt_status="0".
        let failed = RawTx {
            block_number: "300".into(),
            timestamp: "1700000002".into(),
            hash: "0x0000000000000000000000000000000000000000000000000000000000000003".into(),
            from: "0x000000000000000000000000000000000000beef".into(),
            to: format!("{owner:#x}"),
            value: "0".into(),
            gas_used: "21000".into(),
            gas_price: "20000000000".into(),
            is_error: "0".into(),
            txreceipt_status: "0".into(),
            function_name: "transfer(address,uint256)".into(),
        };
        let tx = convert_tx(failed, owner).expect("converts");
        assert!(matches!(tx.status, TxStatus::Failure));
        assert!(matches!(tx.direction, TxDirection::In));
        assert_eq!(tx.method.as_deref(), Some("transfer"));
    }

    #[test]
    fn parse_token_balances_skips_zero_and_invalid() {
        let rows: Vec<RawTokenBalance> = serde_json::from_str(
            r#"[
              {
                "TokenAddress": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "TokenName": "USD Coin",
                "TokenSymbol": "USDC",
                "TokenQuantity": "5000000",
                "TokenDivisor": "6"
              },
              {
                "TokenAddress": "0xdac17f958d2ee523a2206206994597c13d831ec7",
                "TokenName": "Tether",
                "TokenSymbol": "USDT",
                "TokenQuantity": "0",
                "TokenDivisor": "6"
              },
              {
                "TokenAddress": "not-a-valid-address",
                "TokenName": "Junk",
                "TokenSymbol": "JNK",
                "TokenQuantity": "1",
                "TokenDivisor": "0"
              }
            ]"#,
        )
        .unwrap();
        let out = parse_token_balances(rows);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].symbol, "USDC");
        assert_eq!(out[0].decimals, 6);
        assert_eq!(out[0].balance_raw, U256::from(5_000_000u64));
        assert_eq!(out[0].usd_price, None);
    }

    #[test]
    fn classifies_pre_byzantium_tx_as_success_when_no_error() {
        let owner: Address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045".parse().unwrap();
        let raw = RawTx {
            block_number: "100".into(),
            timestamp: "1700000000".into(),
            hash: "0x0000000000000000000000000000000000000000000000000000000000000001".into(),
            from: format!("{owner:#x}"),
            to: "0x000000000000000000000000000000000000dead".into(),
            value: "1000000000000000000".into(),
            gas_used: "21000".into(),
            gas_price: "1000000000".into(),
            is_error: "0".into(),
            txreceipt_status: String::new(),
            function_name: String::new(),
        };
        let tx = convert_tx(raw, owner).expect("converts");
        assert!(matches!(tx.status, TxStatus::Success));
    }
}
