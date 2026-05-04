//! Live ERC-20 portfolio: token metadata, on-chain balance reads, and
//! Uniswap V3 on-chain price lookups. Replaces the hardcoded `TOKENS` array
//! that the dashboard used to carry.
//!
//! Prices are derived entirely on-chain by reading `slot0()` from Uniswap V3
//! pools — no external API calls, no API keys, no rate limits. ETH/USD comes
//! from the USDC/WETH 0.05 % pool; each ERC-20 is priced via its most-liquid
//! WETH pool.

use alloy::eips::BlockId;
use alloy::primitives::{Address, Bytes, U256, address, keccak256};
use alloy::providers::{Provider, RootProvider};
use alloy::network::Ethereum;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, trace, warn};

use crate::chain::Chain;
use crate::ui::token_logos::NATIVE_ETH;
use crate::wallet::short_address;

// ── Retry on RPC rate-limit ─────────────────────────────────────────────────

/// Public RPCs throttle aggressively (often returning HTTP 429 with a
/// `rate-limited until …` body) when the dashboard fans out a balance + price
/// batch. Without retrying, the user sees a spurious "$0.00" for the throttled
/// token until the next refresh tick. Retry the inner call with exponential
/// backoff when the transport surfaces a 429.
const RETRY_MAX_ATTEMPTS: u32 = 4;
const RETRY_INITIAL_DELAY: Duration = Duration::from_millis(500);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(8);

fn is_rate_limited(msg: &str) -> bool {
    msg.contains("429")
        || msg.contains("rate-lim")
        || msg.contains("rate lim")
        || msg.contains("Too Many Requests")
}

async fn with_rate_limit_retry<T, E, F, Fut>(label: &str, f: F) -> Result<T, E>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut delay = RETRY_INITIAL_DELAY;
    for attempt in 1..=RETRY_MAX_ATTEMPTS {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < RETRY_MAX_ATTEMPTS && is_rate_limited(&e.to_string()) => {
                warn!(
                    label,
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "rate-limited; retrying",
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(RETRY_MAX_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop returns on success or final error");
}

// ── Uniswap V3 constants ────────────────────────────────────────────────────

/// Fee tier of the USDC/WETH pool used to derive ETH/USD on every chain.
/// 0.05% is the highest-liquidity tier on Mainnet, Base, and Optimism.
const ETH_PRICE_FEE: u32 = 500; // 0.05 %

/// Pool init-code hash — same byte-for-byte across every Uniswap V3
/// deployment (Mainnet, Base, Optimism), so we don't key it by chain.
#[rustfmt::skip]
const POOL_INIT_CODE_HASH: [u8; 32] = [
    0xe3, 0x4f, 0x19, 0x9b, 0x19, 0xb2, 0xb4, 0xf4,
    0x7f, 0x68, 0x44, 0x26, 0x19, 0xd5, 0x55, 0x52,
    0x7d, 0x24, 0x4f, 0x78, 0xa3, 0x29, 0x7e, 0xa8,
    0x93, 0x25, 0xf8, 0x43, 0xf8, 0x7b, 0x8b, 0x54,
];

/// Uniswap V3 factory address. Mainnet and Optimism share the same
/// canonical address; Base has its own deployment.
fn factory_for(chain: Chain) -> Address {
    match chain {
        Chain::Mainnet | Chain::Optimism => {
            address!("0x1f98431c8ad98523631ae4a59f267346ea31f984")
        }
        Chain::Base => address!("0x33128a8fC17869897dcE68Ed026d694621f6FDfD"),
    }
}

/// Canonical WETH (or wrapped-native) address for the chain. Both L2s use
/// the OP-Stack predeploy at 0x4200…0006.
fn weth_for(chain: Chain) -> Address {
    match chain {
        Chain::Mainnet => address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
        Chain::Base | Chain::Optimism => {
            address!("0x4200000000000000000000000000000000000006")
        }
    }
}

/// Canonical (native) USDC address for the chain. Used as the
/// quote-currency leg of the ETH/USD pool.
fn usdc_for(chain: Chain) -> Address {
    match chain {
        Chain::Mainnet => address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        Chain::Base => address!("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
        Chain::Optimism => address!("0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85"),
    }
}

// ── Curated token list ───────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum PriceSource {
    /// Price derived from a Uniswap V3 pool paired with WETH.
    UniswapWethPool { fee: u32 },
    /// 1:1 peg with ETH (for WETH itself).
    EthPeg,
}

struct TokenMeta {
    symbol: &'static str,
    name: &'static str,
    address: Address,
    decimals: u8,
    logo_id: Option<&'static str>,
    price_source: PriceSource,
}

/// Mainnet curated token list. Iterated by the on-chain-walk fallback
/// path when the user has no indexer configured. The indexer (when
/// configured) is the source of truth for token *holdings* — this list
/// is just the metadata + price-source recipe for each token whose
/// balance we want to surface without an indexer.
const MAINNET_TOKENS: &[TokenMeta] = &[
    TokenMeta {
        symbol: "USDC",
        name: "USD Coin",
        address: address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        decimals: 6,
        logo_id: Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        price_source: PriceSource::UniswapWethPool { fee: 500 },
    },
    TokenMeta {
        symbol: "USDT",
        name: "Tether",
        address: address!("0xdac17f958d2ee523a2206206994597c13d831ec7"),
        decimals: 6,
        logo_id: None,
        price_source: PriceSource::UniswapWethPool { fee: 500 },
    },
    TokenMeta {
        symbol: "WBTC",
        name: "Wrapped BTC",
        address: address!("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599"),
        decimals: 8,
        logo_id: Some("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599"),
        price_source: PriceSource::UniswapWethPool { fee: 3000 },
    },
    TokenMeta {
        symbol: "WETH",
        name: "Wrapped Ether",
        address: address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
        decimals: 18,
        logo_id: None,
        price_source: PriceSource::EthPeg,
    },
    TokenMeta {
        symbol: "LINK",
        name: "Chainlink",
        address: address!("0x514910771af9ca656af840dff83e8264ecf986ca"),
        decimals: 18,
        logo_id: Some("0x514910771af9ca656af840dff83e8264ecf986ca"),
        price_source: PriceSource::UniswapWethPool { fee: 3000 },
    },
    TokenMeta {
        symbol: "UNI",
        name: "Uniswap",
        address: address!("0x1f9840a85d5af5bf1d1762f925bdaddc4201f984"),
        decimals: 18,
        logo_id: None,
        price_source: PriceSource::UniswapWethPool { fee: 3000 },
    },
    TokenMeta {
        symbol: "DAI",
        name: "Dai",
        address: address!("0x6b175474e89094c44da98b954eedeac495271d0f"),
        decimals: 18,
        logo_id: None,
        price_source: PriceSource::UniswapWethPool { fee: 3000 },
    },
];

/// Per-chain curated token list. L2 lists are intentionally empty: the
/// indexer is the source of truth at runtime, and a user without an
/// indexer configured stays scoped to native ETH on L2 rather than
/// shipping a subset that would diverge from what the indexer reports.
fn tokens_for(chain: Chain) -> &'static [TokenMeta] {
    match chain {
        Chain::Mainnet => MAINNET_TOKENS,
        Chain::Base | Chain::Optimism => &[],
    }
}

// ── LiveToken ────────────────────────────────────────────────────────────────

/// A single portfolio entry with live data.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LiveToken {
    pub symbol: String,
    pub name: String,
    pub balance: String,
    pub balance_f64: f64,
    /// Raw on-chain balance in the token's smallest units.
    /// Carried alongside `balance_f64` so the Send flow can compare an entered
    /// amount to the held balance without re-parsing the formatted string.
    pub balance_raw: U256,
    pub decimals: u8,
    /// `None` for native ETH; `Some(addr)` for ERC-20s. The Send flow reads
    /// this to choose between an ETH-value transfer and a `transfer(...)`
    /// calldata to the contract.
    pub contract: Option<Address>,
    pub usd_price: f64,
    pub usd_value: f64,
    pub logo_id: Option<&'static str>,
    /// Which chain this entry was fetched from. The asset-row UI reads
    /// this to suffix L2 entries with their chain (e.g. "USDC (Base)")
    /// while leaving Mainnet entries bare ("USDC").
    pub chain: Chain,
}

// ── Cache ────────────────────────────────────────────────────────────────────

/// In-memory portfolio cache keyed by `(owner, chain)`. Shared across
/// the `App` and each `WalletScreen` rebuild so account switches can
/// render the previously-fetched token list immediately while a fresh
/// fetch refreshes it in the background. Each chain has its own slot so
/// a Base portfolio refresh doesn't clobber the cached Mainnet rows.
/// Process-lifetime only — cleared on app restart.
pub type PortfolioCache = Arc<Mutex<HashMap<(Address, Chain), Vec<LiveToken>>>>;

pub fn new_cache() -> PortfolioCache {
    Arc::new(Mutex::new(HashMap::new()))
}

// ── ERC-20 balance helpers ───────────────────────────────────────────────────

/// `balanceOf(address)` — first 4 bytes of keccak256("balanceOf(address)")
const BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];

fn balance_of_calldata(owner: Address) -> Bytes {
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&BALANCE_OF_SELECTOR);
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(owner.as_slice());
    Bytes::from(data)
}

pub(crate) fn format_token_balance(raw: U256, decimals: u8) -> (String, f64) {
    if raw.is_zero() {
        return ("0".into(), 0.0);
    }
    let divisor = 10u64.pow(decimals as u32) as f64;
    let raw_f64 = raw.to_string().parse::<f64>().unwrap_or(0.0);
    let value = raw_f64 / divisor;
    let formatted = if value >= 1000.0 {
        format!("{:.2}", value)
    } else if value >= 1.0 {
        format!("{:.4}", value)
    } else {
        format!("{:.6}", value)
    };
    (formatted, value)
}

pub(crate) fn format_eth_balance(raw: U256) -> (String, f64) {
    let formatted = alloy::primitives::utils::format_ether(raw);
    let f = formatted.parse::<f64>().unwrap_or(0.0);
    let display = if f >= 1000.0 {
        format!("{:.2}", f)
    } else {
        format!("{:.4}", f)
    };
    (display, f)
}

// ── Uniswap V3 price helpers ────────────────────────────────────────────────

/// Compute the CREATE2 address of a Uniswap V3 pool. `factory` is the
/// chain-specific Uniswap V3 factory deployment.
fn pool_address(factory: Address, token_a: Address, token_b: Address, fee: u32) -> Address {
    let (token0, token1) = if token_a < token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    };

    // salt = keccak256(abi.encode(token0, token1, fee))
    let mut encoded = [0u8; 96];
    encoded[12..32].copy_from_slice(token0.as_slice());
    encoded[44..64].copy_from_slice(token1.as_slice());
    // fee (uint24) right-aligned in the last 32-byte word
    encoded[93] = ((fee >> 16) & 0xFF) as u8;
    encoded[94] = ((fee >> 8) & 0xFF) as u8;
    encoded[95] = (fee & 0xFF) as u8;
    let salt = keccak256(encoded);

    // CREATE2: keccak256(0xff ++ factory ++ salt ++ init_code_hash)[12..]
    let mut buf = Vec::with_capacity(85);
    buf.push(0xff);
    buf.extend_from_slice(factory.as_slice());
    buf.extend_from_slice(salt.as_slice());
    buf.extend_from_slice(&POOL_INIT_CODE_HASH);
    let hash = keccak256(&buf);
    Address::from_slice(&hash[12..])
}

/// `slot0()` selector — first 4 bytes of keccak256("slot0()")
const SLOT0_SELECTOR: [u8; 4] = [0x38, 0x50, 0xc7, 0xbd];

/// Read `sqrtPriceX96` from a Uniswap V3 pool's `slot0` at `block`.
async fn read_sqrt_price(
    provider: &RootProvider<Ethereum>,
    pool: Address,
    block: BlockId,
) -> Result<U256, String> {
    let label = format!("slot0({pool})");
    let result = with_rate_limit_retry(&label, || async {
        let tx = alloy::rpc::types::TransactionRequest::default()
            .to(pool)
            .input(alloy::rpc::types::TransactionInput::new(Bytes::from(
                SLOT0_SELECTOR.to_vec(),
            )));
        provider.call(tx).block(block).await
    })
    .await
    .map_err(|e| format!("{label}: {e}"))?;
    if result.len() < 32 {
        return Err(format!("{label} returned {} bytes", result.len()));
    }
    Ok(U256::from_be_slice(&result[..32]))
}

/// Convert `sqrtPriceX96` to a float ratio (token1 / token0 in smallest units).
fn sqrt_price_to_raw(sqrt_price_x96: U256) -> f64 {
    let s: f64 = sqrt_price_x96.to_string().parse().unwrap_or(0.0);
    let q96: f64 = (1u128 << 96) as f64;
    (s / q96).powi(2)
}

/// Derive ETH/USD from the chain's USDC/WETH pool by inverting the USDC
/// price quoted in ETH. The token0/token1 ordering depends on the
/// chain — Base has `USDC > WETH` so the USDC leg is token1, while
/// Mainnet and Optimism have `USDC < WETH` (USDC is token0). Routing
/// through `token_price_in_eth` handles both cases without us
/// hand-tracking the inversion per chain.
fn eth_usd_from_sqrt_price(
    sqrt_price_x96: U256,
    usdc_addr: Address,
    weth_addr: Address,
) -> f64 {
    let usdc_in_eth = token_price_in_eth(sqrt_price_x96, usdc_addr, 6, weth_addr);
    if usdc_in_eth == 0.0 {
        return 0.0;
    }
    1.0 / usdc_in_eth
}

/// Given a WETH-paired pool's `sqrtPriceX96`, compute the token's ETH price.
///
/// If the token address < WETH it is token0 (WETH is token1):
///   `token_in_eth = raw_price × 10^(token_dec − 18)`
///
/// Otherwise WETH is token0 (token is token1):
///   `token_in_eth = 10^(token_dec − 18) / raw_price`
fn token_price_in_eth(
    sqrt_price_x96: U256,
    token_addr: Address,
    token_decimals: u8,
    weth_addr: Address,
) -> f64 {
    let raw = sqrt_price_to_raw(sqrt_price_x96);
    if raw == 0.0 {
        return 0.0;
    }
    let dec_diff = token_decimals as i32 - 18;
    if token_addr < weth_addr {
        // token is token0, WETH is token1
        raw * 10f64.powi(dec_diff)
    } else {
        // WETH is token0, token is token1
        10f64.powi(dec_diff) / raw
    }
}

// ── Fetch portfolio ───────────────────────────────────────────────────────���──

/// Fetch the full portfolio for `owner` on `chain`: native ETH plus the
/// chain's curated ERC-20 list (Mainnet has 7 tokens; L2 lists are empty
/// — the indexer is the runtime token source on L2). Prices are read
/// on-chain from the chain's Uniswap V3 deployment (no external API).
/// Returns live tokens sorted ETH-first then by USD value descending,
/// every entry stamped with `chain` so the dashboard can group/render
/// per-chain rows.
///
/// `provider` is supplied by the caller (typically the per-chain
/// `NetworkClient` fallback) so account switches reuse one HTTP
/// transport instead of building a fresh TLS pool every dashboard
/// rebuild. The caller is responsible for handing in a provider that
/// actually points at `chain`'s execution RPC.
pub async fn fetch_portfolio(
    owner: Address,
    chain: Chain,
    provider: &RootProvider<Ethereum>,
) -> Result<Vec<LiveToken>, String> {
    // Pin every call in this batch to the finalized block. Public RPC fleets
    // load-balance across upstreams that drift a block or two apart at the
    // tip; a "latest" tag intermittently lands on a node that doesn't yet
    // have that header and returns -32014 / `header not found`. Finalized is
    // available on every upstream and gives a consistent state snapshot.
    let block = BlockId::finalized();

    // 1. Fetch native ETH balance
    let eth_balance_raw = with_rate_limit_retry("eth_getBalance", || async {
        provider.get_balance(owner).block_id(block).await
    })
    .await
    .map_err(|e| format!("eth_getBalance: {e}"))?;

    trace!(raw = %eth_balance_raw, "ETH raw balance");

    let token_list = tokens_for(chain);
    let factory = factory_for(chain);
    let weth = weth_for(chain);
    let usdc = usdc_for(chain);

    // 2. Fetch ERC-20 balances
    let mut erc20_balances: Vec<(usize, U256)> = Vec::new();
    for (i, token) in token_list.iter().enumerate() {
        let label = format!("balanceOf({})", token.symbol);
        let outcome = with_rate_limit_retry(&label, || async {
            let tx = alloy::rpc::types::TransactionRequest::default()
                .to(token.address)
                .input(alloy::rpc::types::TransactionInput::new(balance_of_calldata(owner)));
            provider.call(tx).block(block).await
        })
        .await;
        match outcome {
            Ok(result) => {
                let balance = if result.len() >= 32 {
                    U256::from_be_slice(&result[..32])
                } else {
                    U256::ZERO
                };
                if !balance.is_zero() {
                    trace!(symbol = token.symbol, raw = %balance, "ERC-20 raw balance");
                }
                erc20_balances.push((i, balance));
            }
            Err(e) => {
                warn!(symbol = token.symbol, error = %e, "balanceOf call failed");
                erc20_balances.push((i, U256::ZERO));
            }
        }
    }

    // 3. ETH/USD price from the chain's Uniswap V3 USDC/WETH pool. ETH
    // on L2 is bridged 1:1 from L1, so the price reads converge across
    // chains modulo arbitrage spread; we still query each chain locally
    // to avoid coupling the L2 fetch to a working mainnet provider.
    let eth_price_pool = pool_address(factory, usdc, weth, ETH_PRICE_FEE);
    let eth_usd = match read_sqrt_price(provider, eth_price_pool, block).await {
        Ok(sqrt) => {
            let price = eth_usd_from_sqrt_price(sqrt, usdc, weth);
            debug!(chain = %chain.label(), price = format!("${price:.2}"), "ETH/USD from Uniswap");
            price
        }
        Err(e) => {
            warn!(chain = %chain.label(), error = %e, "ETH/USD price read failed");
            0.0
        }
    };

    // 4. Build LiveToken vec, fetching prices only for held tokens
    let mut tokens: Vec<LiveToken> = Vec::new();

    // ETH (always included)
    let (eth_bal_str, eth_bal_f64) = format_eth_balance(eth_balance_raw);
    tokens.push(LiveToken {
        symbol: "ETH".into(),
        name: "Ethereum".into(),
        balance: eth_bal_str,
        balance_f64: eth_bal_f64,
        balance_raw: eth_balance_raw,
        decimals: 18,
        contract: None,
        usd_price: eth_usd,
        usd_value: eth_bal_f64 * eth_usd,
        logo_id: Some(NATIVE_ETH),
        chain,
    });

    // ERC-20s (only non-zero balances — skip the price read otherwise)
    for (i, raw_balance) in &erc20_balances {
        if raw_balance.is_zero() {
            continue;
        }
        let meta = &token_list[*i];
        let price = match meta.price_source {
            PriceSource::EthPeg => eth_usd,
            PriceSource::UniswapWethPool { fee } => {
                let pool = pool_address(factory, meta.address, weth, fee);
                match read_sqrt_price(provider, pool, block).await {
                    Ok(sqrt) => {
                        let in_eth = token_price_in_eth(sqrt, meta.address, meta.decimals, weth);
                        let in_usd = in_eth * eth_usd;
                        trace!(
                            symbol = meta.symbol,
                            eth = format!("{in_eth:.6}"),
                            usd = format!("${in_usd:.2}"),
                            "token price",
                        );
                        in_usd
                    }
                    Err(e) => {
                        warn!(symbol = meta.symbol, error = %e, "token price read failed");
                        0.0
                    }
                }
            }
        };
        let (bal_str, bal_f64) = format_token_balance(*raw_balance, meta.decimals);
        tokens.push(LiveToken {
            symbol: meta.symbol.into(),
            name: meta.name.into(),
            balance: bal_str,
            balance_f64: bal_f64,
            balance_raw: *raw_balance,
            decimals: meta.decimals,
            contract: Some(meta.address),
            usd_price: price,
            usd_value: bal_f64 * price,
            logo_id: meta.logo_id,
            chain,
        });
    }

    // 5. Sort: ETH first, then by USD value descending
    tokens[1..].sort_by(|a, b| {
        b.usd_value
            .partial_cmp(&a.usd_value)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for tk in &tokens {
        trace!(
            owner = %short_address(owner),
            balance = %tk.balance,
            symbol = %tk.symbol,
            usd = format!("${:.2}", tk.usd_value),
            "portfolio entry",
        );
    }

    Ok(tokens)
}

#[cfg(test)]
mod pool_address_tests {
    use super::*;

    #[test]
    fn mainnet_usdc_weth_500_pool_matches_canonical() {
        // Canonical Mainnet USDC/WETH 0.05% pool — the highest-liquidity
        // ETH/USD source on L1, used as the algorithm-correctness anchor.
        let factory = factory_for(Chain::Mainnet);
        let usdc = usdc_for(Chain::Mainnet);
        let weth = weth_for(Chain::Mainnet);
        let derived = pool_address(factory, usdc, weth, 500);
        let canonical: Address =
            address!("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640");
        assert_eq!(derived, canonical, "derived: {derived:?}");
    }

    #[test]
    fn base_usdc_weth_500_pool_matches_canonical() {
        // Canonical Base USDC/WETH 0.05% pool. Anchors `usdc_for(Base)` —
        // a typo in the USDC address would put `slot0` at an empty
        // address and silently zero out ETH/USD on Base.
        let factory = factory_for(Chain::Base);
        let usdc = usdc_for(Chain::Base);
        let weth = weth_for(Chain::Base);
        let derived = pool_address(factory, usdc, weth, 500);
        let canonical: Address =
            address!("0xd0b53D9277642d899DF5C87A3966A349A798F224");
        assert_eq!(derived, canonical, "derived: {derived:?}");
    }

    #[test]
    fn optimism_usdc_weth_500_pool_matches_canonical() {
        // Canonical Optimism USDC/WETH 0.05% pool — same regression
        // anchor as the Mainnet/Base tests above.
        let factory = factory_for(Chain::Optimism);
        let usdc = usdc_for(Chain::Optimism);
        let weth = weth_for(Chain::Optimism);
        let derived = pool_address(factory, usdc, weth, 500);
        let canonical: Address =
            address!("0x1fb3cf6e48F1E7B10213E7b6d87D4c073C7Fdb7b");
        assert_eq!(derived, canonical, "derived: {derived:?}");
    }
}
