//! Byreal route-test smoke entry point.
//!
//! LiteSVM is isolated to the program test crate and only runs when the route
//! program, Byreal program dump, RPC URL, and production pool are available.

mod common;

use common::run_swap_route;
use byreal_titan_integration::byreal_clmm::ByrealClmmVenue;

#[tokio::test]
async fn swap_route_both_directions() {
    run_swap_route::<ByrealClmmVenue>().await;
}
