//! Byreal CLMM venue test suite.
//!
//! RPC-backed tests require `SOLANA_RPC_URL` and `BYREAL_CLMM_POOL`, where the
//! pool must be owned by the production Byreal CLMM program. LiteSVM simulation
//! entry points skip because route execution is not wired for this dependency line.

mod common;

use std::env;
use std::str::FromStr;

use common::SuiteConfig;
use byreal_titan_integration::byreal_clmm::ByrealClmmVenue;
use solana_pubkey::Pubkey;

// Installs the allocation guard that powers the construction test's
// `assert_no_alloc` checks. The Makefile runs that test under `release-debug`
// so the guard is active; speed tests run under true `--release`.
#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

fn pool() -> Option<Pubkey> {
    env::var("BYREAL_CLMM_POOL")
        .ok()
        .map(|value| Pubkey::from_str(&value).expect("BYREAL_CLMM_POOL must be a pubkey"))
}

fn config() -> SuiteConfig {
    SuiteConfig { pool: pool() }
}

#[tokio::test]
async fn construction() {
    common::construction::<ByrealClmmVenue>(&config()).await;
}

#[tokio::test]
async fn zero_input_spot_price() {
    common::zero_input_spot_price::<ByrealClmmVenue>(&config()).await;
}

#[tokio::test]
async fn bound_simulation() {
    common::bound_simulation::<ByrealClmmVenue>(&config()).await;
}

#[tokio::test]
async fn random_samples() {
    common::random_samples::<ByrealClmmVenue>(&config()).await;
}

#[tokio::test]
async fn monotone() {
    common::monotone::<ByrealClmmVenue>(&config()).await;
}

#[tokio::test]
async fn quoting_speed() {
    common::quoting_speed::<ByrealClmmVenue>(&config()).await;
}

#[tokio::test]
async fn price_monotone() {
    common::price_monotone::<ByrealClmmVenue>(&config()).await;
}

#[tokio::test]
async fn mean_value_theorem() {
    common::mean_value_theorem::<ByrealClmmVenue>(&config()).await;
}
