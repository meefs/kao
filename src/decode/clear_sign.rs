//! ERC-7730 descriptor-based clear signing. Tries curated JSON
//! descriptors first (via `clear_signing::BundledRegistrySource`);
//! falls back to the existing heuristic pipeline when no descriptor
//! matches.
//!
//! The `KaoDataProvider` bridges Kao's Helios-verified RPC layer into
//! the `clear_signing::DataProvider` trait, so token metadata and ENS
//! reverse lookups go through the same verified path the heuristic
//! pipeline already uses.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alloy::primitives::{Address, Bytes, U256};
use tracing::{debug, info, trace, warn};

use clear_signing::{
    BundledRegistrySource, DataProvider, DisplayModel, FormatDiagnostic, FormatOutcome,
    ResolvedDescriptorResolution, TokenMeta, TransactionContext, format_calldata,
    resolve_descriptors_for_tx,
};

use crate::chain::Chain;
use crate::decode::proxy;
use crate::decode::render::{DecodedCall, decode_call, read_token_meta};
use crate::ens;
use crate::net::BalanceFetcher;

// ---------------------------------------------------------------------------
// Data provider

/// Bridges Kao's Helios-verified network layer into the `clear_signing`
/// crate's `DataProvider` trait. Token metadata goes through verified
/// `eth_call`; ENS goes through forward-verified reverse resolution.
/// Local names (contacts + own accounts) are resolved from a pre-built
/// snapshot so no locking happens on the async path.
pub struct KaoDataProvider<'a> {
    net: &'a dyn BalanceFetcher,
    chain: Chain,
    all_verified: Arc<AtomicBool>,
    /// Snapshot of contacts + own account names, keyed by address.
    /// Built once at task-spawn time so the async decode doesn't need
    /// the `RwLock<ContactsBook>`.
    local_names: HashMap<Address, String>,
}

impl<'a> KaoDataProvider<'a> {
    pub fn new(
        net: &'a dyn BalanceFetcher,
        chain: Chain,
        local_names: HashMap<Address, String>,
    ) -> Self {
        Self {
            net,
            chain,
            all_verified: Arc::new(AtomicBool::new(true)),
            local_names,
        }
    }

    pub fn all_verified(&self) -> bool {
        self.all_verified.load(Ordering::Relaxed)
    }
}

impl DataProvider for KaoDataProvider<'_> {
    fn resolve_token(
        &self,
        chain_id: u64,
        address: &str,
    ) -> Pin<Box<dyn Future<Output = Option<TokenMeta>> + Send + '_>> {
        let address = address.to_string();
        Box::pin(async move {
            let Some(chain) = Chain::from_chain_id(chain_id) else {
                debug!(
                    chain_id,
                    address, "clear-sign: resolve_token: unsupported lookup chain"
                );
                return None;
            };
            let addr: Address = match address.parse() {
                Ok(a) => a,
                Err(_) => {
                    debug!(address, "clear-sign: resolve_token: bad address");
                    return None;
                }
            };
            match read_token_meta(self.net, chain, addr).await {
                Some((info, verified)) => {
                    if !verified {
                        self.all_verified.store(false, Ordering::Relaxed);
                    }
                    debug!(
                        symbol = %info.symbol,
                        decimals = info.decimals,
                        verified,
                        lookup_chain = ?chain,
                        %addr,
                        "clear-sign: resolved token metadata"
                    );
                    Some(TokenMeta {
                        symbol: info.symbol.clone(),
                        decimals: info.decimals,
                        name: info.symbol,
                    })
                }
                None => {
                    trace!(%addr, "clear-sign: no token metadata for address");
                    None
                }
            }
        })
    }

    fn resolve_local_name(
        &self,
        address: &str,
        _chain_id: u64,
        _types: Option<&[String]>,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        let hit = address
            .parse::<Address>()
            .ok()
            .and_then(|addr| self.local_names.get(&addr).cloned());
        match &hit {
            Some(name) => debug!(address, %name, "clear-sign: resolved local name"),
            None => debug!(
                address,
                known = self.local_names.len(),
                "clear-sign: no local name"
            ),
        }
        Box::pin(async move { hit })
    }

    fn resolve_ens_name(
        &self,
        address: &str,
        _chain_id: u64,
        _types: Option<&[String]>,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        // Reverse ENS only on Mainnet — reverse records live there.
        if !matches!(self.chain, Chain::Mainnet) {
            trace!(address, chain = ?self.chain, "clear-sign: skipping ENS (non-mainnet)");
            return Box::pin(async { None });
        }
        let address = address.to_string();
        Box::pin(async move {
            let addr: Address = match address.parse() {
                Ok(a) => a,
                Err(_) => {
                    debug!(address, "clear-sign: resolve_ens_name: bad address");
                    return None;
                }
            };
            // Verified (Helios, mainnet-only) reverse lookup — an unverified
            // read fails closed inside `lookup_address`, so a hostile RPC
            // can't fabricate a name on the clear-signing review surface.
            match ens::lookup_address(self.net, addr).await {
                Ok(Some(name)) => {
                    debug!(%addr, %name, "clear-sign: resolved ENS name");
                    Some(name)
                }
                Ok(None) => {
                    trace!(%addr, "clear-sign: no ENS reverse record");
                    None
                }
                Err(e) => {
                    debug!(%addr, error = %e, "clear-sign: ENS lookup failed");
                    None
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Decode result

/// Union of ERC-7730 clear-signing and heuristic decode results.
/// The function panel dispatches on this to pick the right renderer.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DecodeResult {
    /// ERC-7730 descriptor matched. Intent + labeled entries.
    ClearSigned {
        model: DisplayModel,
        diagnostics: Vec<FormatDiagnostic>,
        proxy_hops: Vec<Address>,
        all_verified: bool,
    },
    /// Descriptor returned Fallback (partial match). Show DisplayModel
    /// but carry heuristic decode for cross-reference.
    Fallback {
        model: DisplayModel,
        reason: clear_signing::FallbackReason,
        diagnostics: Vec<FormatDiagnostic>,
        all_verified: bool,
        heuristic: DecodedCall,
    },
    /// No descriptor or format failure. Existing heuristic pipeline.
    Heuristic(DecodedCall),
    /// Native ETH transfer -- no calldata.
    Empty,
}

// ---------------------------------------------------------------------------
// Orchestrator

/// Top-level decode entry point. Tries ERC-7730 descriptors first, then
/// falls back to the heuristic pipeline.
pub async fn decode_transaction(
    net: &dyn BalanceFetcher,
    chain: Chain,
    from: Address,
    to: Address,
    calldata: Bytes,
    value: U256,
    local_names: HashMap<Address, String>,
) -> DecodeResult {
    if calldata.is_empty() {
        debug!(%to, "clear-sign: empty calldata, native transfer");
        return DecodeResult::Empty;
    }

    let selector = if calldata.len() >= 4 {
        format!(
            "0x{:02x}{:02x}{:02x}{:02x}",
            calldata[0], calldata[1], calldata[2], calldata[3]
        )
    } else {
        format!("0x{}", alloy::hex::encode(&calldata))
    };

    info!(
        %to,
        %from,
        %selector,
        calldata_len = calldata.len(),
        chain = ?chain,
        local_names = local_names.len(),
        "clear-sign: decoding transaction"
    );

    // Walk the proxy chain so we can pass the implementation address to
    // the descriptor resolver. The heuristic path re-walks this (cheap,
    // cached in Helios); keeping it self-contained simplifies the
    // fallback.
    let resolved = proxy::resolve_implementation(net, chain, to).await;
    let impl_addr = resolved.implementation;
    let proxy_hops = resolved.hops.clone();
    let all_verified = resolved.all_verified;

    if !proxy_hops.is_empty() {
        debug!(
            %to,
            %impl_addr,
            hops = proxy_hops.len(),
            all_verified,
            "clear-sign: proxy resolved"
        );
    }

    // Build the descriptor-resolver context.
    let to_str = format!("{to:#x}");
    let from_str = format!("{from:#x}");
    let impl_str = format!("{impl_addr:#x}");
    let value_bytes = value.to_be_bytes::<32>();

    let tx_ctx = TransactionContext {
        chain_id: chain.chain_id(),
        to: &to_str,
        calldata: &calldata,
        value: if value.is_zero() {
            None
        } else {
            Some(&value_bytes[..])
        },
        from: Some(&from_str),
        implementation_address: if impl_addr != to {
            Some(&impl_str)
        } else {
            None
        },
    };

    let data_provider = KaoDataProvider::new(net, chain, local_names);

    // Try the bundled registry.
    match BundledRegistrySource::new() {
        Ok(source) => {
            debug!("clear-sign: bundled registry loaded");
            match resolve_descriptors_for_tx(&tx_ctx, &source, Some(&data_provider)).await {
                Ok(ResolvedDescriptorResolution::Found(descriptors)) => {
                    info!(
                        count = descriptors.len(),
                        %selector,
                        "clear-sign: descriptor(s) found"
                    );
                    match format_calldata(&descriptors, &tx_ctx, &data_provider).await {
                        Ok(FormatOutcome::ClearSigned { model, diagnostics }) => {
                            info!(
                                intent = %model.intent,
                                entries = model.entries.len(),
                                diagnostics = diagnostics.len(),
                                "clear-sign: clear-signed result"
                            );
                            return DecodeResult::ClearSigned {
                                model,
                                diagnostics,
                                proxy_hops,
                                all_verified: all_verified && data_provider.all_verified(),
                            };
                        }
                        Ok(FormatOutcome::Fallback {
                            model,
                            reason,
                            diagnostics,
                        }) => {
                            info!(
                                intent = %model.intent,
                                reason = ?reason,
                                diagnostics = diagnostics.len(),
                                "clear-sign: fallback result, running heuristic too"
                            );
                            let heuristic = decode_call(net, chain, to, calldata).await;
                            return DecodeResult::Fallback {
                                model,
                                reason,
                                diagnostics,
                                all_verified: all_verified && data_provider.all_verified(),
                                heuristic,
                            };
                        }
                        Err(e) => {
                            warn!(
                                error = ?e,
                                %selector,
                                "clear-sign: format_calldata failed, falling back to heuristic"
                            );
                        }
                    }
                }
                Ok(ResolvedDescriptorResolution::NotFound) => {
                    debug!(%selector, %to, "clear-sign: no descriptor found");
                }
                Err(e) => {
                    warn!(
                        error = ?e,
                        %selector,
                        "clear-sign: descriptor resolution error"
                    );
                }
            }
        }
        Err(e) => {
            warn!(error = ?e, "clear-sign: failed to load bundled registry");
        }
    }

    // Heuristic fallback.
    debug!(%selector, "clear-sign: using heuristic pipeline");
    let decoded = decode_call(net, chain, to, calldata).await;
    DecodeResult::Heuristic(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{BalanceFetcher, LatestBlock, VerificationStatus, VerifiedRead};
    use alloy::network::Ethereum;
    use alloy::providers::RootProvider;
    use async_trait::async_trait;
    use clear_signing::DataProvider;
    use std::sync::Mutex;

    fn abi_encode_string(s: &str) -> Bytes {
        let mut buf = Vec::with_capacity(64 + s.len().next_multiple_of(32));
        let mut offset = [0u8; 32];
        offset[31] = 0x20;
        buf.extend_from_slice(&offset);
        let mut len = [0u8; 32];
        len[24..32].copy_from_slice(&(s.len() as u64).to_be_bytes());
        buf.extend_from_slice(&len);
        buf.extend_from_slice(s.as_bytes());
        let pad = (32 - (s.len() % 32)) % 32;
        buf.extend(std::iter::repeat_n(0u8, pad));
        Bytes::from(buf)
    }

    fn abi_encode_uint8(v: u8) -> Bytes {
        let mut buf = [0u8; 32];
        buf[31] = v;
        Bytes::from(buf.to_vec())
    }

    #[derive(Debug)]
    struct TokenMetaMock {
        calls: Mutex<Vec<Chain>>,
        verified: bool,
    }

    impl TokenMetaMock {
        fn new(verified: bool) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                verified,
            }
        }
        fn called_chains(&self) -> Vec<Chain> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl BalanceFetcher for TokenMetaMock {
        async fn balance(&self, _: Address, _: Chain) -> Result<String, String> {
            Ok("0".into())
        }
        async fn invalidate(&self) {}
        fn last_status(&self, _: Chain) -> VerificationStatus {
            VerificationStatus::Verified
        }
        async fn provider(&self, _: Chain) -> Option<RootProvider<Ethereum>> {
            None
        }
        async fn get_code(&self, _: Address, _: Chain) -> Result<VerifiedRead<Bytes>, String> {
            Ok(VerifiedRead {
                value: Bytes::new(),
                verified: true,
            })
        }
        async fn get_storage_at(
            &self,
            _: Address,
            _: U256,
            _: Chain,
        ) -> Result<VerifiedRead<alloy::primitives::B256>, String> {
            Ok(VerifiedRead {
                value: alloy::primitives::B256::ZERO,
                verified: true,
            })
        }
        async fn call(
            &self,
            _: Address,
            data: Bytes,
            chain: Chain,
        ) -> Result<VerifiedRead<Bytes>, String> {
            self.calls.lock().unwrap().push(chain);
            let value = match data.as_ref() {
                [0x95, 0xd8, 0x9b, 0x41] => abi_encode_string("TOK"),
                [0x31, 0x3c, 0xe5, 0x67] => abi_encode_uint8(18),
                _ => Bytes::new(),
            };
            Ok(VerifiedRead {
                value,
                verified: self.verified,
            })
        }
        async fn get_balance_raw(
            &self,
            _: Address,
            _: Chain,
        ) -> Result<VerifiedRead<U256>, String> {
            Ok(VerifiedRead {
                value: U256::ZERO,
                verified: true,
            })
        }
        async fn get_transaction_count(
            &self,
            _: Address,
            _: Chain,
        ) -> Result<VerifiedRead<u64>, String> {
            Ok(VerifiedRead {
                value: 0,
                verified: true,
            })
        }
        async fn latest_block(&self, _: Chain) -> Result<VerifiedRead<LatestBlock>, String> {
            Ok(VerifiedRead {
                value: LatestBlock {
                    number: 0,
                    hash: alloy::primitives::B256::ZERO,
                    timestamp: 0,
                    gas_limit: 30_000_000,
                    base_fee_per_gas: 0,
                    prevrandao: alloy::primitives::B256::ZERO,
                    beneficiary: Address::ZERO,
                    excess_blob_gas: None,
                },
                verified: true,
            })
        }
        async fn get_code_raw(
            &self,
            addr: Address,
            chain: Chain,
        ) -> Result<VerifiedRead<Bytes>, String> {
            self.get_code(addr, chain).await
        }
        async fn get_storage_at_raw(
            &self,
            addr: Address,
            slot: U256,
            chain: Chain,
        ) -> Result<VerifiedRead<alloy::primitives::B256>, String> {
            self.get_storage_at(addr, slot, chain).await
        }
        async fn call_raw(
            &self,
            to: Address,
            data: Bytes,
            chain: Chain,
        ) -> Result<VerifiedRead<Bytes>, String> {
            self.call(to, data, chain).await
        }
    }

    #[tokio::test]
    async fn resolve_token_honors_descriptor_selected_chain_id() {
        let net = TokenMetaMock::new(true);
        let provider = KaoDataProvider::new(&net, Chain::Mainnet, HashMap::new());
        let token = Address::repeat_byte(0x22).to_checksum(None);

        let meta = provider
            .resolve_token(Chain::Base.chain_id(), &token)
            .await
            .expect("token metadata");

        assert_eq!(meta.symbol, "TOK");
        assert_eq!(meta.decimals, 18);
        assert_eq!(net.called_chains(), vec![Chain::Base, Chain::Base]);
        assert!(provider.all_verified());
    }

    #[tokio::test]
    async fn resolve_token_marks_unverified_metadata() {
        let net = TokenMetaMock::new(false);
        let provider = KaoDataProvider::new(&net, Chain::Mainnet, HashMap::new());
        let token = Address::repeat_byte(0x33).to_checksum(None);

        let meta = provider
            .resolve_token(Chain::Mainnet.chain_id(), &token)
            .await
            .expect("token metadata");

        assert_eq!(meta.symbol, "TOK");
        assert!(!provider.all_verified());
    }

    #[tokio::test]
    async fn resolve_token_rejects_unsupported_lookup_chain() {
        let net = TokenMetaMock::new(true);
        let provider = KaoDataProvider::new(&net, Chain::Mainnet, HashMap::new());
        let token = Address::repeat_byte(0x44).to_checksum(None);

        assert!(provider.resolve_token(999_999, &token).await.is_none());
        assert!(net.called_chains().is_empty());
        assert!(provider.all_verified());
    }
}
