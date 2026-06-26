//! Shared route-test smoke suite for the on-chain program.
//!
//! LiteSVM route execution is intentionally not wired in this integration:
//! the simulator dependency tree conflicts with the Solana 2.3/Byreal CLMM
//! dependency line. These entry points still compile the route-test harness and
//! skip with an explicit message.

use byreal_titan_integration::trading_venue::{FromAccount, TradingVenue};

/// A venue usable by the route suite: buildable from an account, quotable, and
/// usable across `.await`.
pub trait RouteVenue: TradingVenue + FromAccount + Send + Sync {}
impl<T: TradingVenue + FromAccount + Send + Sync> RouteVenue for T {}

fn init_test_logger() {
    drop(env_logger::builder().is_test(true).try_init());
}

fn current_test() -> String {
    std::thread::current()
        .name()
        .unwrap_or("a route test")
        .to_string()
}

/// Route execution is intentionally skipped in this integration.
pub async fn run_swap_route<V: RouteVenue>() {
    init_test_logger();
    eprintln!(
        "SKIP {}: LiteSVM route execution is not wired in this integration; use program parity and CPI data unit tests",
        current_test()
    );
}
