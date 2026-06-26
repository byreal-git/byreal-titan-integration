#![allow(clippy::result_large_err)] // `TradingVenueError` is large. Crate level because the type is used everywhere

pub mod account_caching;
pub mod byreal_clmm;
pub mod swap_route;
pub mod trading_venue;
