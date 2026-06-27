//! Byreal CLMM venue test suite.
//!
//! RPC-backed tests require `SOLANA_RPC_URL` and `BYREAL_CLMM_POOL`, where the
//! pool must be owned by the production Byreal CLMM program. This SDK-level
//! suite intentionally does not depend on LiteSVM; route execution lives in the
//! program test crate.

mod common;

use std::env;
use std::str::FromStr;

use common::SuiteConfig;
use byreal_titan_integration::byreal_clmm::ByrealClmmVenue;
use solana_pubkey::Pubkey;

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
async fn reported_price_positive() {
    common::reported_price_positive::<ByrealClmmVenue>(&config()).await;
}

#[tokio::test]
async fn local_price_probe_consistent() {
    common::local_price_probe_consistent::<ByrealClmmVenue>(&config()).await;
}
