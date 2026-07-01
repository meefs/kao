//! The 0xbow relayer HTTP API.
//!
//! A relayer submits a withdrawal's `Entrypoint.relay(...)` tx on the user's
//! behalf and pays the gas, taking a fee out of the withdrawn amount (the
//! gasless / IP-hiding path, analogous to CoW's solver model). This crate builds
//! the calldata; the relayer broadcasts it. Three endpoints, per the 0xbow
//! reference relayer:
//!   GET  /relayer/details?chainId&assetAddress  → fee bounds + fee receiver
//!   POST /relayer/quote                          → gas-adjusted feeBPS + a
//!                                                  signed fee commitment
//!   POST /relayer/request                        → submit proof; returns txHash
//!
//! The quote's `feeCommitment.withdrawalData` is the exact `Withdrawal.data`
//! the relayer signed — the withdrawal proof's `context` MUST bind to it, so we
//! decode it back into the [`super::flow`] destination rather than rebuilding.
//!
//! All requests go through the shared proxied HTTP client, and every error is
//! URL-redacted before it can reach a log or the UI.

use alloy::primitives::{Address, Bytes};
use serde::Deserialize;
use serde_json::json;

use crate::indexer::{http_client_or_err, redact_url_in_err};

use super::PoolError;

/// A named relayer endpoint (Settings-configurable; these are the 0xbow
/// defaults shown in the native app).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relayer {
    pub name: String,
    pub url: String,
}

/// The Mainnet relayers 0xbow's own client ships with.
pub const DEFAULT_RELAYERS: &[(&str, &str)] = &[
    ("Fast Relay", "https://fastrelay.xyz"),
    ("Cloaked Relay", "https://api.clkd.xyz"),
];

/// Default relayer list as owned [`Relayer`]s.
pub fn default_relayers() -> Vec<Relayer> {
    DEFAULT_RELAYERS
        .iter()
        .map(|(n, u)| Relayer {
            name: (*n).into(),
            url: (*u).into(),
        })
        .collect()
}

/// `GET /relayer/details` — per-asset fee bounds and the fee-receiver address.
#[derive(Debug, Clone, Deserialize)]
pub struct RelayerDetails {
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    #[serde(rename = "feeBPS")]
    pub fee_bps: String,
    #[serde(rename = "minWithdrawAmount")]
    pub min_withdraw_amount: String,
    #[serde(rename = "feeReceiverAddress")]
    pub fee_receiver: Address,
    #[serde(rename = "assetAddress")]
    pub asset: Address,
    #[serde(rename = "maxGasPrice")]
    pub max_gas_price: String,
}

/// `POST /relayer/quote` response. `fee_commitment` is kept as an opaque JSON
/// value so it round-trips to `/relayer/request` byte-for-byte (the relayer
/// signed over it); we only reach into it for `withdrawalData`.
#[derive(Debug, Clone, Deserialize)]
pub struct QuoteResponse {
    #[serde(rename = "baseFeeBPS")]
    pub base_fee_bps: String,
    #[serde(rename = "feeBPS")]
    pub fee_bps: String,
    #[serde(rename = "feeCommitment")]
    pub fee_commitment: serde_json::Value,
}

impl QuoteResponse {
    /// The relay fee in basis points the relayer committed to.
    pub fn fee_bps(&self) -> Result<u64, PoolError> {
        self.fee_bps
            .parse()
            .map_err(|_| PoolError::Relayer(format!("bad feeBPS '{}'", self.fee_bps)))
    }

    /// The `Withdrawal.data` (abi-encoded RelayData) the relayer signed — the
    /// proof's context must bind to exactly these bytes.
    pub fn withdrawal_data(&self) -> Result<Bytes, PoolError> {
        let s = self
            .fee_commitment
            .get("withdrawalData")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                PoolError::Relayer("quote missing feeCommitment.withdrawalData".into())
            })?;
        s.parse::<Bytes>()
            .map_err(|e| PoolError::Relayer(format!("bad withdrawalData: {e}")))
    }
}

/// `POST /relayer/request` response.
#[derive(Debug, Clone, Deserialize)]
pub struct RelayResult {
    pub success: bool,
    #[serde(rename = "txHash")]
    pub tx_hash: Option<String>,
    #[serde(rename = "requestId")]
    pub request_id: Option<String>,
}

fn trim(url: &str) -> &str {
    url.trim_end_matches('/')
}

/// Fetch a relayer's fee configuration for `asset` on `chain_id`.
pub async fn fetch_details(
    relayer_url: &str,
    chain_id: u64,
    asset: Address,
) -> Result<RelayerDetails, PoolError> {
    let client = http_client_or_err().map_err(PoolError::Relayer)?;
    let url = format!(
        "{}/relayer/details?chainId={chain_id}&assetAddress={asset}",
        trim(relayer_url)
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| PoolError::Relayer(format!("details: {}", redact_url_in_err(e))))?;
    let resp = check_status(resp).await?;
    resp.json()
        .await
        .map_err(|e| PoolError::Relayer(format!("details decode: {}", redact_url_in_err(e))))
}

/// Request a fee quote (gas-adjusted) + signed fee commitment.
pub async fn fetch_quote(
    relayer_url: &str,
    chain_id: u64,
    amount: alloy::primitives::U256,
    asset: Address,
    recipient: Address,
) -> Result<QuoteResponse, PoolError> {
    let client = http_client_or_err().map_err(PoolError::Relayer)?;
    let url = format!("{}/relayer/quote", trim(relayer_url));
    let body = json!({
        "chainId": chain_id,
        "amount": amount.to_string(),
        "asset": asset.to_string(),
        "recipient": recipient.to_string(),
    });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| PoolError::Relayer(format!("quote: {}", redact_url_in_err(e))))?;
    let resp = check_status(resp).await?;
    resp.json()
        .await
        .map_err(|e| PoolError::Relayer(format!("quote decode: {}", redact_url_in_err(e))))
}

/// Submit a relayed withdrawal. `proof` is the snarkjs-form proof
/// (`Groth16Proof::to_snarkjs_json()`), `public_signals` the decimal signals,
/// `withdrawal` the `{processooor, data}` bound into the proof, and
/// `fee_commitment` the opaque value from the quote (passed back verbatim).
#[allow(clippy::too_many_arguments)]
pub async fn submit(
    relayer_url: &str,
    chain_id: u64,
    scope: &str,
    processooor: Address,
    data: &Bytes,
    proof: serde_json::Value,
    public_signals: Vec<String>,
    fee_commitment: serde_json::Value,
) -> Result<RelayResult, PoolError> {
    let client = http_client_or_err().map_err(PoolError::Relayer)?;
    let url = format!("{}/relayer/request", trim(relayer_url));
    let body = json!({
        "withdrawal": { "processooor": processooor.to_string(), "data": data.to_string() },
        "proof": proof,
        "publicSignals": public_signals,
        "scope": scope,
        "chainId": chain_id,
        "feeCommitment": fee_commitment,
    });
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| PoolError::Relayer(format!("relay: {}", redact_url_in_err(e))))?;
    let resp = check_status(resp).await?;
    resp.json()
        .await
        .map_err(|e| PoolError::Relayer(format!("relay decode: {}", redact_url_in_err(e))))
}

/// Turn a non-2xx response into a redacted error, reading the body for context.
async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, PoolError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let body = crate::sanitize::sanitize_display(&body, 300);
    Err(PoolError::Relayer(format!(
        "relayer returned {status}: {body}"
    )))
}
