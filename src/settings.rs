//! User-tunable runtime settings, persisted to disk.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::B256;
use serde::{Deserialize, Serialize};

use crate::ui::kao_theme::ThemeKind;

/// Default execution RPC. LlamaRPC's public mainnet endpoint serves
/// `eth_getProof`, which Helios needs to verify state (plain
/// `eth_getBalance` isn't enough). Users can swap to their own provider via
/// the Custom RPC option in setup.
const DEFAULT_RPCS: &[&str] = &["https://eth.llamarpc.com"];
/// Default beacon-chain LC API endpoint. PublicNode's beacon API serves the
/// `/eth/v1/beacon/light_client/{bootstrap,finality_update}` calls Helios
/// needs to verify consensus.
///
/// HTTPS only. Plain-HTTP endpoints would let a network attacker tamper with
/// the light-client bootstrap before any consensus signatures get verified.
const DEFAULT_CONSENSUS_RPCS: &[&str] = &["https://ethereum-beacon-api.publicnode.com"];

/// Hex hash of the built-in mainnet checkpoint shipped with the binary.
/// Update at release time together with `BUILTIN_CHECKPOINT_PUBLISHED`.
/// If older than `BUILTIN_FRESHNESS_DAYS`, the auto-resolver prefers a
/// freshly fetched community fallback (see `crate::net::refresh_auto_checkpoint`).
///
/// IMPORTANT: this must be a finalized beacon block root from a sync-committee
/// period that public LC servers still index — they prune older periods (~27h
/// rotation), so a checkpoint that's more than a day or two stale will fail
/// every bootstrap call.
pub const BUILTIN_CHECKPOINT: &str =
    "0x56d275d9bdf4afb040ecbbba7da0dff9ed384b062c321d5f2b9a4a4f0eb83b4d";
pub const BUILTIN_CHECKPOINT_PUBLISHED: u64 = 1777188238;
pub const BUILTIN_FRESHNESS_DAYS: u64 = 14;

/// Third-party indexer used for transaction history and unverified balance
/// fan-outs. Helios verification of native ETH stays in `crate::net`; this
/// picks who answers `Indexer::transactions` / `Indexer::balances`. `None`
/// disables the indexer entirely — balances fall back to the on-chain
/// `portfolio::fetch_portfolio` walk and tx history is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IndexerProvider {
    #[default]
    Blockscout,
    Etherscan,
    Alchemy,
    None,
}

impl IndexerProvider {
    fn key(self) -> &'static str {
        match self {
            Self::Blockscout => "blockscout",
            Self::Etherscan => "etherscan",
            Self::Alchemy => "alchemy",
            Self::None => "none",
        }
    }

    fn from_key(s: &str) -> Option<Self> {
        match s {
            "blockscout" => Some(Self::Blockscout),
            "etherscan" => Some(Self::Etherscan),
            "alchemy" => Some(Self::Alchemy),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct State {
    theme: ThemeKind,
    rpcs: Vec<String>,
    consensus_rpcs: Vec<String>,
    checkpoint_override: Option<B256>,
    /// Resolved checkpoint used when the user hasn't pasted an override.
    /// Starts as the built-in constant; the network layer may swap in a
    /// fresher community-fallback hash on app startup.
    auto_checkpoint: B256,
    indexer_provider: IndexerProvider,
    etherscan_api_key: Option<String>,
    alchemy_api_key: Option<String>,
    /// Custom Blockscout instance base URL (e.g. for L2s). `None` falls back
    /// to the public mainnet endpoint baked into the indexer.
    blockscout_base_url: Option<String>,
    blockscout_api_key: Option<String>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn ensure() -> &'static Mutex<State> {
    STATE.get_or_init(|| Mutex::new(default_state()))
}

fn default_state() -> State {
    State {
        theme: ThemeKind::Mint,
        rpcs: DEFAULT_RPCS.iter().map(|s| s.to_string()).collect(),
        consensus_rpcs: DEFAULT_CONSENSUS_RPCS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        checkpoint_override: None,
        auto_checkpoint: B256::from_str(BUILTIN_CHECKPOINT)
            .expect("built-in checkpoint constant must be valid hex"),
        indexer_provider: IndexerProvider::default(),
        etherscan_api_key: None,
        alchemy_api_key: None,
        blockscout_base_url: None,
        blockscout_api_key: None,
    }
}

fn settings_path() -> PathBuf {
    crate::paths::config_dir().join("settings.toml")
}

/// Load settings from disk into the in-memory cache. Silently falls back to
/// defaults if the file is missing or unreadable.
pub fn load() {
    let Ok(contents) = std::fs::read_to_string(settings_path()) else {
        // Force initialization so subsequent reads see the defaults.
        let _ = ensure();
        return;
    };
    let state = parse(&contents);
    let mutex = ensure();
    *mutex.lock().expect("settings mutex poisoned") = state;
}

/// On-disk TOML schema. All fields are optional so a partial or older config
/// file falls back to defaults per-key rather than failing the whole load.
/// `auto_checkpoint` is intentionally absent — it's rederived on each app
/// start from the network and never persisted.
#[derive(Debug, Default, Serialize, Deserialize)]
struct OnDisk {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rpcs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    consensus_rpcs: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    indexer_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etherscan_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    alchemy_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blockscout_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blockscout_api_key: Option<String>,
}

/// True iff `s` parses as an HTTPS URL. Non-HTTPS endpoints are dropped on
/// load so a hand-edited `settings.toml` can't bypass the UI's HTTPS check
/// and steer the wallet onto a MITM-able transport.
fn is_https_url(s: &str) -> bool {
    url::Url::parse(s)
        .map(|u| u.scheme() == "https")
        .unwrap_or(false)
}

/// Pure parser: produce a `State` from settings-file TOML. A malformed TOML
/// document, missing keys, and unparseable values all silently fall back to
/// defaults so a hand-edited config can never brick startup.
fn parse(text: &str) -> State {
    let mut state = default_state();
    let Ok(on_disk): Result<OnDisk, _> = toml::from_str(text) else {
        return state;
    };
    if let Some(t) = on_disk.theme.as_deref()
        && let Some(k) = ThemeKind::from_key(t)
    {
        state.theme = k;
    }
    if let Some(rpcs) = on_disk.rpcs {
        let filtered: Vec<String> = rpcs.into_iter().filter(|s| is_https_url(s)).collect();
        if !filtered.is_empty() {
            state.rpcs = filtered;
        }
    }
    if let Some(cls) = on_disk.consensus_rpcs {
        let filtered: Vec<String> = cls.into_iter().filter(|s| is_https_url(s)).collect();
        if !filtered.is_empty() {
            state.consensus_rpcs = filtered;
        }
    }
    if let Some(s) = on_disk.checkpoint_override.as_deref() {
        state.checkpoint_override = if s.is_empty() {
            None
        } else {
            B256::from_str(s).ok()
        };
    }
    if let Some(p) = on_disk.indexer_provider.as_deref()
        && let Some(parsed) = IndexerProvider::from_key(p)
    {
        state.indexer_provider = parsed;
    }
    state.etherscan_api_key = on_disk.etherscan_api_key.filter(|s| !s.is_empty());
    state.alchemy_api_key = on_disk.alchemy_api_key.filter(|s| !s.is_empty());
    state.blockscout_base_url = on_disk
        .blockscout_base_url
        .filter(|s| !s.is_empty() && is_https_url(s));
    state.blockscout_api_key = on_disk.blockscout_api_key.filter(|s| !s.is_empty());
    state
}

/// Pure serializer: emit TOML for a `State`. Mirrors `parse` — `auto_checkpoint`
/// is intentionally omitted, and `checkpoint_override` is dropped from the
/// output when unset rather than being written as an empty string.
fn serialize(state: &State) -> String {
    let on_disk = OnDisk {
        theme: Some(state.theme.key().to_string()),
        rpcs: Some(state.rpcs.clone()),
        consensus_rpcs: Some(state.consensus_rpcs.clone()),
        checkpoint_override: state
            .checkpoint_override
            .map(|b| format!("0x{}", alloy::hex::encode(b.as_slice()))),
        indexer_provider: Some(state.indexer_provider.key().to_string()),
        etherscan_api_key: state.etherscan_api_key.clone(),
        alchemy_api_key: state.alchemy_api_key.clone(),
        blockscout_base_url: state.blockscout_base_url.clone(),
        blockscout_api_key: state.blockscout_api_key.clone(),
    };
    toml::to_string(&on_disk).expect("serializing settings cannot fail")
}

pub fn theme() -> ThemeKind {
    ensure().lock().expect("settings mutex poisoned").theme
}

pub fn set_theme(kind: ThemeKind) {
    ensure().lock().expect("settings mutex poisoned").theme = kind;
    write_all();
}

/// The built-in execution-RPC list. Exposed so the setup UI can show the
/// user exactly which endpoints "use defaults" enrolls them in.
pub fn default_rpcs() -> &'static [&'static str] {
    DEFAULT_RPCS
}

/// The built-in consensus (beacon-chain LC) RPC list. Mirrors `default_rpcs`
/// so the setup flow can reset both lists in one go.
pub fn default_consensus_rpcs() -> &'static [&'static str] {
    DEFAULT_CONSENSUS_RPCS
}

pub fn rpcs() -> Vec<String> {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .rpcs
        .clone()
}

pub fn set_rpcs(list: Vec<String>) {
    ensure().lock().expect("settings mutex poisoned").rpcs = list;
    write_all();
}

pub fn consensus_rpcs() -> Vec<String> {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .consensus_rpcs
        .clone()
}

pub fn set_consensus_rpcs(list: Vec<String>) {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .consensus_rpcs = list;
    write_all();
}

pub fn checkpoint_override() -> Option<B256> {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .checkpoint_override
}

pub fn set_checkpoint_override(value: Option<B256>) {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .checkpoint_override = value;
    write_all();
}

pub fn indexer_provider() -> IndexerProvider {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .indexer_provider
}

pub fn set_indexer_provider(value: IndexerProvider) {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .indexer_provider = value;
    write_all();
}

pub fn etherscan_api_key() -> Option<String> {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .etherscan_api_key
        .clone()
}

pub fn set_etherscan_api_key(value: Option<String>) {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .etherscan_api_key = value.filter(|s| !s.is_empty());
    write_all();
}

pub fn alchemy_api_key() -> Option<String> {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .alchemy_api_key
        .clone()
}

pub fn set_alchemy_api_key(value: Option<String>) {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .alchemy_api_key = value.filter(|s| !s.is_empty());
    write_all();
}

pub fn blockscout_base_url() -> Option<String> {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .blockscout_base_url
        .clone()
}

pub fn set_blockscout_base_url(value: Option<String>) {
    let cleaned = value.filter(|s| !s.is_empty() && is_https_url(s));
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .blockscout_base_url = cleaned;
    write_all();
}

pub fn blockscout_api_key() -> Option<String> {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .blockscout_api_key
        .clone()
}

pub fn set_blockscout_api_key(value: Option<String>) {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .blockscout_api_key = value.filter(|s| !s.is_empty());
    write_all();
}

/// Resolved checkpoint (built-in or freshly fetched fallback) used when no
/// user override is set. Mutated by `crate::net` on startup.
pub fn auto_checkpoint() -> B256 {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .auto_checkpoint
}

pub fn set_auto_checkpoint(value: B256) {
    ensure()
        .lock()
        .expect("settings mutex poisoned")
        .auto_checkpoint = value;
    // Not persisted to disk: this value is rederived on each app start.
}

/// True when the binary's built-in checkpoint is recent enough that we don't
/// need to fetch a community fallback before the first sync.
pub fn builtin_is_fresh() -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age_secs = now.saturating_sub(BUILTIN_CHECKPOINT_PUBLISHED);
    age_secs < BUILTIN_FRESHNESS_DAYS * 86400
}

fn write_all() {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
        let _ = restrict_to_owner(parent, 0o700);
    }
    let snapshot = ensure().lock().expect("settings mutex poisoned").clone();
    let _ = std::fs::write(&path, serialize(&snapshot));
    let _ = restrict_to_owner(&path, 0o600);
}

/// Restrict a path to owner-only access on Unix; no-op elsewhere.
fn restrict_to_owner(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_returns_defaults() {
        let s = parse("");
        let d = default_state();
        assert_eq!(s.rpcs, d.rpcs);
        assert_eq!(s.consensus_rpcs, d.consensus_rpcs);
        assert_eq!(s.checkpoint_override, d.checkpoint_override);
        assert_eq!(s.theme, d.theme);
    }

    #[test]
    fn parse_ignores_comments_and_blank_lines() {
        let s = parse(
            "\
            # comment\n\
            \n\
            # indented comment\n\
            consensus_rpcs = [\"https://cl.example/\"]\n",
        );
        assert_eq!(s.consensus_rpcs, vec!["https://cl.example/".to_string()]);
    }

    #[test]
    fn parse_ignores_unknown_keys() {
        let s = parse("totally_unknown = \"foo\"\nconsensus_rpcs = [\"https://cl.example/\"]\n");
        assert_eq!(s.consensus_rpcs, vec!["https://cl.example/".to_string()]);
    }

    #[test]
    fn parse_malformed_toml_returns_defaults() {
        let s = parse("this is = = not valid toml\n");
        let d = default_state();
        assert_eq!(s.rpcs, d.rpcs);
        assert_eq!(s.consensus_rpcs, d.consensus_rpcs);
    }

    #[test]
    fn parse_preserves_rpc_array_order() {
        let s =
            parse("rpcs = [\"https://a.example\", \"https://b.example\", \"https://c.example\"]\n");
        assert_eq!(
            s.rpcs,
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string(),
                "https://c.example".to_string(),
            ],
        );
    }

    #[test]
    fn parse_empty_rpc_array_does_not_replace_default() {
        // An explicit empty list must NOT collapse the default RPC list, since
        // the user almost certainly didn't mean to disable all upstreams.
        let s = parse("rpcs = []\n");
        assert_eq!(s.rpcs, default_state().rpcs);
    }

    #[test]
    fn parse_empty_consensus_rpc_array_does_not_replace_default() {
        let s = parse("consensus_rpcs = []\n");
        assert_eq!(s.consensus_rpcs, default_state().consensus_rpcs);
    }

    #[test]
    fn parse_checkpoint_override_empty_clears_value() {
        let s = parse("checkpoint_override = \"\"\n");
        assert_eq!(s.checkpoint_override, None);
    }

    #[test]
    fn parse_checkpoint_override_invalid_hex_clears_value() {
        let s = parse("checkpoint_override = \"not_a_hex\"\n");
        assert_eq!(s.checkpoint_override, None);
    }

    #[test]
    fn parse_then_serialize_roundtrip() {
        let original = State {
            theme: ThemeKind::Mint,
            rpcs: vec!["https://r1.example".into(), "https://r2.example".into()],
            consensus_rpcs: vec!["https://cl1.example".into(), "https://cl2.example".into()],
            checkpoint_override: Some(B256::from_str(BUILTIN_CHECKPOINT).unwrap()),
            auto_checkpoint: B256::from_str(BUILTIN_CHECKPOINT).unwrap(),
            indexer_provider: IndexerProvider::Alchemy,
            etherscan_api_key: Some("ETHERSCAN_TEST_KEY".into()),
            alchemy_api_key: Some("ALCHEMY_TEST_KEY".into()),
            blockscout_base_url: Some("https://base.blockscout.com".into()),
            blockscout_api_key: Some("BLOCKSCOUT_TEST_KEY".into()),
        };
        let serialized = serialize(&original);
        let parsed = parse(&serialized);
        assert_eq!(parsed.theme, original.theme);
        assert_eq!(parsed.rpcs, original.rpcs);
        assert_eq!(parsed.consensus_rpcs, original.consensus_rpcs);
        assert_eq!(parsed.checkpoint_override, original.checkpoint_override);
        assert_eq!(parsed.indexer_provider, original.indexer_provider);
        assert_eq!(parsed.etherscan_api_key, original.etherscan_api_key);
        assert_eq!(parsed.alchemy_api_key, original.alchemy_api_key);
        assert_eq!(parsed.blockscout_base_url, original.blockscout_base_url);
        assert_eq!(parsed.blockscout_api_key, original.blockscout_api_key);
        // auto_checkpoint isn't persisted; parsed value reverts to the default.
        assert_eq!(parsed.auto_checkpoint, default_state().auto_checkpoint);
    }

    #[test]
    fn serialize_emits_valid_toml_arrays() {
        let state = State {
            theme: default_state().theme,
            rpcs: vec!["a".into(), "b".into()],
            consensus_rpcs: vec!["c".into(), "d".into(), "e".into()],
            checkpoint_override: None,
            auto_checkpoint: default_state().auto_checkpoint,
            indexer_provider: IndexerProvider::Blockscout,
            etherscan_api_key: None,
            alchemy_api_key: None,
            blockscout_base_url: None,
            blockscout_api_key: None,
        };
        let text = serialize(&state);
        let reparsed: OnDisk = toml::from_str(&text).expect("output must be valid toml");
        assert_eq!(
            reparsed.rpcs.as_deref(),
            Some(["a".to_string(), "b".to_string()].as_slice())
        );
        assert_eq!(
            reparsed.consensus_rpcs.as_deref(),
            Some(["c".to_string(), "d".to_string(), "e".to_string()].as_slice()),
        );
        // Unset overrides are dropped from the output rather than emitted as "".
        assert!(reparsed.checkpoint_override.is_none());
    }
}
