//! Install a process-wide proxy by setting reqwest's proxy environment
//! variables.
//!
//! # Why this is its own crate
//!
//! The `kao` crate is `#![forbid(unsafe_code)]`. Setting an environment
//! variable requires [`std::env::set_var`], which is `unsafe` on the 2024
//! edition because it mutates global process state that is not synchronized
//! against concurrent `getenv`/`setenv` from other threads.
//!
//! There is no safe alternative for the job: `helios` (and the `alloy` /
//! `reqwest` clients it builds internally) construct their own HTTP clients
//! with no hook to inject a pre-configured, proxied client. Those clients do,
//! however, honour reqwest's `ALL_PROXY` environment variable. Setting that
//! variable is therefore the only way to route *all* of the wallet's outbound
//! traffic — light-client consensus/execution RPC included — through a SOCKS5
//! proxy.
//!
//! Keeping the single `unsafe` block here, in a minimal crate that does
//! nothing else, lets the main wallet keep its blanket `forbid(unsafe_code)`
//! and confines the audit surface to this one function.

/// Install `proxy_url` as the process-wide proxy for every reqwest-based
/// client (sets `ALL_PROXY` / `all_proxy`). `proxy_url` is a full proxy URL,
/// e.g. `socks5h://127.0.0.1:9050`.
///
/// Once set, any [`reqwest::Client`](https://docs.rs/reqwest) built afterwards
/// that has not opted out via `no_proxy()` (or an explicit `.proxy(..)`, which
/// also disables env-proxy detection) routes through `proxy_url`.
///
/// # Preconditions
///
/// Call this **before any other thread is spawned** — i.e. at the very top of
/// `main`, before the async runtime or UI start, and before any HTTP client is
/// built. `set_var` is unsound to race against environment reads/writes on
/// other threads; calling it once, single-threaded, at startup satisfies that.
/// Because the value is read by each client at build time, a change only takes
/// effect on the next process launch.
pub fn set_all_proxy(proxy_url: &str) {
    // SAFETY: the documented precondition is that this runs single-threaded at
    // startup, before any thread that could concurrently read or write the
    // process environment. Under that contract `set_var` has no data race.
    unsafe {
        std::env::set_var("ALL_PROXY", proxy_url);
        std::env::set_var("all_proxy", proxy_url);
    }
}
