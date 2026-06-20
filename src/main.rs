#![forbid(unsafe_code)]
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod chain;
mod decode;
mod ens;
mod indexer;
mod net;
mod paths;
mod portfolio;
mod safe;
mod settings;
mod ui;
mod wallet;

use app::App;
use tracing_subscriber::EnvFilter;

pub fn main() -> iced::Result {
    // Default to our own crate at info; everything else (helios, alloy, hyper)
    // stays at warn so their per-request chatter doesn't spam stderr. Override
    // via RUST_LOG, e.g. `RUST_LOG=kao=debug` to see redacted addresses or
    // `RUST_LOG=kao=trace` to see raw addresses and per-token reads.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("kao=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();

    // Load persisted settings and, if the user enabled a proxy, install it
    // process-wide *before* iced spawns any thread or builds any HTTP client.
    // reqwest's `ALL_PROXY` is the only proxy hook helios's internally-built
    // clients honour, so this is what routes ALL outbound traffic (helios
    // consensus/execution, the alloy fallback provider, indexers, the Safe
    // service, ENS) through the proxy. It must run here, single-threaded, at
    // startup: installing it mutates the environment, which is only sound
    // before other threads exist. A proxy change therefore applies on the
    // next launch, not mid-session.
    settings::load();
    if settings::proxy_enabled() {
        let addr = settings::proxy_address();
        let addr = addr.trim();
        if !addr.is_empty() {
            proxy_env::set_all_proxy(&format!("socks5h://{addr}"));
            tracing::info!("routing all outbound traffic through the configured SOCKS5 proxy");
        }
    }

    iced::application(App::new, App::update, App::view)
        .title("Kao Wallet")
        .subscription(App::subscription)
        .run()
}
