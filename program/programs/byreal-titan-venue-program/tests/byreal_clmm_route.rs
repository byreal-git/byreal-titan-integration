//! Byreal route-test smoke entry point.
//!
//! LiteSVM route execution is not wired for this integration's dependency line,
//! so the shared runner compiles and skips with an explicit message.

mod common;

use common::run_swap_route;
use byreal_titan_integration::byreal_clmm::ByrealClmmVenue;

#[tokio::test]
async fn swap_route_both_directions() {
    run_swap_route::<ByrealClmmVenue>().await;
}
