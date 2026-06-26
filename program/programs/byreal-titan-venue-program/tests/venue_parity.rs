//! Guards that the route-builder `Venue` enum (in the off-chain crate's
//! `swap_route` module) and the program `Venue` enum (this crate's `state.rs`)
//! serialize to identical bytes.
//!
//! Keep route-builder and program venue variants byte-stable on both sides.

use anchor_lang::AnchorSerialize;
use byreal_titan_integration::swap_route::Venue as RouteBuilderVenue;
use byreal_titan_venue_program::state::Venue as ProgramVenue;

#[test]
fn venue_enum_matches_route_builder() {
    let cases = [
        (
            ProgramVenue::ByrealClmm {
                zero_for_one: false,
            },
            RouteBuilderVenue::ByrealClmm {
                zero_for_one: false,
            },
        ),
        (
            ProgramVenue::ByrealClmm { zero_for_one: true },
            RouteBuilderVenue::ByrealClmm { zero_for_one: true },
        ),
    ];

    for (program, route_builder) in cases {
        let program_bytes = program.try_to_vec().unwrap();
        let route_builder_bytes = route_builder.to_borsh_bytes();
        assert_eq!(
            program_bytes, route_builder_bytes,
            "Venue {program:?} serializes differently between program and route builder — the two \
             enums have drifted; check that variants match in name and order",
        );
    }
}
