//! The 0xbow Association-Set Provider (ASP) feed — the **opt-in** compliance
//! data source.
//!
//! A private (compliant) withdrawal must prove the deposit's `label` is in the
//! approved Association Set. Only the set's Merkle root lives on-chain
//! (`Entrypoint.latestRoot()`); the leaves themselves are published off-chain by
//! 0xbow. This is the one place the pool feature phones home, so it is gated
//! behind a Settings toggle (default endpoint `https://api.0xbow.io`), disclosed
//! to the user, and requested through the shared proxied client.
//!
//! Public endpoints (no auth), per-chain:
//!   GET /{chainId}/public/mt-leaves  → { aspLeaves, stateTreeLeaves }
//!   GET /{chainId}/public/mt-roots   → { mtRoot, onchainMtRoot }

use serde::Deserialize;

use privacy_pools::Field;

use crate::indexer::{http_client_or_err, redact_url_in_err};

use super::PoolError;

/// Default 0xbow production ASP endpoint (Mainnet).
pub const DEFAULT_ASP_URL: &str = "https://api.0xbow.io";

#[derive(Debug, Deserialize)]
struct MtLeavesRaw {
    #[serde(rename = "aspLeaves", default)]
    asp_leaves: Vec<String>,
    #[serde(rename = "stateTreeLeaves", default)]
    state_tree_leaves: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct MtRootsRaw {
    #[serde(rename = "mtRoot")]
    mt_root: String,
    #[serde(rename = "onchainMtRoot")]
    onchain_mt_root: String,
}

/// The Association Set leaves (approved labels) and the pool's state-tree leaves
/// as the ASP feed reports them.
#[derive(Debug, Clone)]
pub struct AspLeaves {
    pub asp_leaves: Vec<Field>,
    pub state_leaves: Vec<Field>,
}

/// The ASP root (off-chain feed) plus the root the feed claims is on-chain — a
/// caller cross-checks the latter against `Entrypoint.latestRoot()`.
#[derive(Debug, Clone, Copy)]
pub struct AspRoots {
    pub mt_root: Field,
    pub onchain_mt_root: Field,
}

fn parse_leaves(v: &[String]) -> Result<Vec<Field>, PoolError> {
    v.iter()
        .map(|s| Field::from_decimal(s).map_err(|e| PoolError::Asp(format!("bad ASP leaf: {e}"))))
        .collect()
}

fn trim(url: &str) -> &str {
    url.trim_end_matches('/')
}

/// Fetch the ASP + state-tree leaves for `chain_id` (decimal-encoded field
/// elements). `asp_leaves` builds the Association-Set Merkle tree the withdrawal
/// proof needs; `state_leaves` is a convenience cross-check for the log-derived
/// state tree.
pub async fn fetch_mt_leaves(base_url: &str, chain_id: u64) -> Result<AspLeaves, PoolError> {
    let client = http_client_or_err().map_err(PoolError::Asp)?;
    let url = format!("{}/{chain_id}/public/mt-leaves", trim(base_url));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| PoolError::Asp(format!("mt-leaves: {}", redact_url_in_err(e))))?;
    if !resp.status().is_success() {
        return Err(PoolError::Asp(format!("ASP returned {}", resp.status())));
    }
    let raw: MtLeavesRaw = resp
        .json()
        .await
        .map_err(|e| PoolError::Asp(format!("mt-leaves decode: {}", redact_url_in_err(e))))?;
    Ok(AspLeaves {
        asp_leaves: parse_leaves(&raw.asp_leaves)?,
        state_leaves: parse_leaves(&raw.state_tree_leaves)?,
    })
}

/// Fetch the ASP roots for `chain_id`.
pub async fn fetch_mt_roots(base_url: &str, chain_id: u64) -> Result<AspRoots, PoolError> {
    let client = http_client_or_err().map_err(PoolError::Asp)?;
    let url = format!("{}/{chain_id}/public/mt-roots", trim(base_url));
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| PoolError::Asp(format!("mt-roots: {}", redact_url_in_err(e))))?;
    if !resp.status().is_success() {
        return Err(PoolError::Asp(format!("ASP returned {}", resp.status())));
    }
    let raw: MtRootsRaw = resp
        .json()
        .await
        .map_err(|e| PoolError::Asp(format!("mt-roots decode: {}", redact_url_in_err(e))))?;
    Ok(AspRoots {
        mt_root: Field::from_decimal(&raw.mt_root)
            .map_err(|e| PoolError::Asp(format!("bad mtRoot: {e}")))?,
        onchain_mt_root: Field::from_decimal(&raw.onchain_mt_root)
            .map_err(|e| PoolError::Asp(format!("bad onchainMtRoot: {e}")))?,
    })
}
