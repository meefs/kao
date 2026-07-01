//! Pool discovery — fetch the pool list from the 0xbow API, then verify each
//! against on-chain state via Kao's Helios `call()`.
//!
//! Scanning the Entrypoint's `PoolRegistered` events was too heavy for public
//! RPCs (a single-address `getLogs` over millions of blocks). Instead we read
//! the pool set (scope + asset + symbol) from 0xbow's `pools-stats` endpoint —
//! one HTTP GET — and then confirm each pool on-chain with *verified* reads:
//! `Entrypoint.assetConfig(asset)` yields the pool + fee bounds, and
//! `pool.SCOPE()` must equal the API's scope (so a lying API can't point us at
//! the wrong pool). Only the two Entrypoint addresses are hardcoded (see
//! [`super::entrypoint`]).

use alloy::primitives::Address;
use alloy::sol;
use alloy::sol_types::SolCall;
use serde::Deserialize;

use privacy_pools::{Field, IEntrypoint, IPrivacyPool, field_to_u256};

use crate::chain::Chain;
use crate::indexer::{http_client_or_err, redact_url_in_err};
use crate::net::BalanceFetcher;

use super::{NATIVE_ASSET, PoolError, PoolInfo, entrypoint};

sol! {
    interface IERC20Meta {
        function decimals() external view returns (uint8);
    }
}

#[derive(Debug, Deserialize)]
struct PoolsStatsResponse {
    #[serde(default)]
    pools: Vec<PoolStatRaw>,
}

#[derive(Debug, Deserialize)]
struct PoolStatRaw {
    scope: String,
    #[serde(rename = "tokenSymbol", default)]
    token_symbol: String,
    #[serde(rename = "tokenAddress")]
    token_address: String,
}

/// A pool as the 0xbow API lists it, before on-chain verification.
struct ApiPool {
    scope: Field,
    asset: Address,
    symbol: String,
}

/// Discover a chain's pools: read the list from the 0xbow API at `base_url`,
/// then verify each on-chain via Helios. Pools that don't verify are skipped
/// (logged), never shown.
pub async fn discover_pools(
    net: &dyn BalanceFetcher,
    base_url: &str,
    chain: Chain,
) -> Result<Vec<PoolInfo>, PoolError> {
    let ep = entrypoint(chain).ok_or(PoolError::Unsupported)?;
    let api_pools = fetch_pools_stats(base_url, chain.chain_id()).await?;
    // Verify pools concurrently — each is a few independent verified reads.
    let verified =
        futures::future::join_all(api_pools.iter().map(|ap| verify_pool(net, chain, ep, ap))).await;
    let mut out = Vec::with_capacity(api_pools.len());
    for (ap, res) in api_pools.iter().zip(verified) {
        match res {
            Ok(info) => out.push(info),
            Err(e) => tracing::warn!(
                symbol = %ap.symbol,
                error = %e,
                "privacy pools: skipping unverifiable pool"
            ),
        }
    }
    // Native asset first, then alphabetical by symbol.
    out.sort_by(|a, b| {
        b.is_native
            .cmp(&a.is_native)
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    Ok(out)
}

/// GET `{base}/{chainId}/public/pools-stats` → the pool list (scope + asset +
/// symbol). The stats fields are ignored; we only need the identities.
async fn fetch_pools_stats(base_url: &str, chain_id: u64) -> Result<Vec<ApiPool>, PoolError> {
    let client = http_client_or_err().map_err(PoolError::Chain)?;
    let url = format!(
        "{}/{chain_id}/public/pools-stats",
        base_url.trim_end_matches('/')
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| PoolError::Chain(format!("pool list: {}", redact_url_in_err(e))))?;
    if !resp.status().is_success() {
        return Err(PoolError::Chain(format!(
            "0xbow API returned {}",
            resp.status()
        )));
    }
    let raw: PoolsStatsResponse = resp
        .json()
        .await
        .map_err(|e| PoolError::Chain(format!("pool list decode: {}", redact_url_in_err(e))))?;
    raw.pools
        .into_iter()
        .map(|p| {
            let scope = Field::from_decimal(&p.scope)
                .map_err(|e| PoolError::Chain(format!("bad scope from API: {e}")))?;
            let asset = p
                .token_address
                .parse::<Address>()
                .map_err(|e| PoolError::Chain(format!("bad token address from API: {e}")))?;
            Ok(ApiPool {
                scope,
                asset,
                symbol: crate::sanitize::sanitize_display(&p.token_symbol, 16).into_owned(),
            })
        })
        .collect()
}

/// Verify one API-listed pool on-chain via Helios: resolve the pool + fee bounds
/// from the Entrypoint, and confirm `pool.SCOPE()` matches the API's scope.
async fn verify_pool(
    net: &dyn BalanceFetcher,
    chain: Chain,
    ep: Address,
    ap: &ApiPool,
) -> Result<PoolInfo, PoolError> {
    let cfg_data = IEntrypoint::assetConfigCall { _asset: ap.asset }.abi_encode();
    let cfg_read = net
        .call(ep, cfg_data.into(), chain)
        .await
        .map_err(PoolError::Chain)?;
    let cfg = IEntrypoint::assetConfigCall::abi_decode_returns(&cfg_read.value)
        .map_err(|e| PoolError::Chain(format!("decode assetConfig: {e}")))?;
    if cfg.pool.is_zero() {
        return Err(PoolError::Input("asset has no registered pool".into()));
    }

    let scope_data = IPrivacyPool::SCOPECall {}.abi_encode();
    let scope_read = net
        .call(cfg.pool, scope_data.into(), chain)
        .await
        .map_err(PoolError::Chain)?;
    let onchain_scope = IPrivacyPool::SCOPECall::abi_decode_returns(&scope_read.value)
        .map_err(|e| PoolError::Chain(format!("decode SCOPE: {e}")))?;
    if onchain_scope != field_to_u256(ap.scope) {
        return Err(PoolError::Chain(
            "on-chain scope doesn't match the API".into(),
        ));
    }

    // The anonymity set = the pool's current commitment count.
    let size_data = IPrivacyPool::currentTreeSizeCall {}.abi_encode();
    let size_read = net
        .call(cfg.pool, size_data.into(), chain)
        .await
        .map_err(PoolError::Chain)?;
    let anonymity_set = IPrivacyPool::currentTreeSizeCall::abi_decode_returns(&size_read.value)
        .map_err(|e| PoolError::Chain(format!("decode currentTreeSize: {e}")))?
        .to::<u64>();

    // The pool is "verified" only if every identity/size read came from Helios
    // light-client-verified state (not a raw-RPC fallback).
    let verified = cfg_read.verified && scope_read.verified && size_read.verified;

    let (decimals, is_native) = if ap.asset == NATIVE_ASSET {
        (18u8, true)
    } else {
        let d_data = IERC20Meta::decimalsCall {}.abi_encode();
        let d_read = net
            .call(ap.asset, d_data.into(), chain)
            .await
            .map_err(PoolError::Chain)?;
        let d = IERC20Meta::decimalsCall::abi_decode_returns(&d_read.value)
            .map_err(|e| PoolError::Chain(format!("decode decimals: {e}")))?;
        (d, false)
    };

    Ok(PoolInfo {
        chain,
        entrypoint: ep,
        asset: ap.asset,
        pool: cfg.pool,
        scope: ap.scope,
        symbol: ap.symbol.clone(),
        decimals,
        is_native,
        min_deposit: cfg.minimumDepositAmount,
        vetting_fee_bps: cfg.vettingFeeBPS,
        max_relay_fee_bps: cfg.maxRelayFeeBPS,
        anonymity_set,
        verified,
    })
}
