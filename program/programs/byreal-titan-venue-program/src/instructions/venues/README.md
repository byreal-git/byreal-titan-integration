# Venue Modules

This directory contains CPI adapters called by the Byreal Titan route program.

- `byreal_clmm.rs` is the Byreal CLMM `swap_v3_dyn` exact-in adapter.

Each route leg passes its venue CPI accounts followed by the venue program id as
the final account. `swap_route_v3.rs` removes that final account from the CPI
`AccountMeta` list and passes it to the venue adapter as the target program id.

Byreal CLMM uses the pinned `swap_v3_dyn` discriminator and serializes:

```text
amount: u64
other_amount_threshold: u64 = 0
sqrt_price_limit_x64: u128 = 0
is_base_input: bool = true
```

The off-chain account order is built by `src/byreal_clmm/mod.rs`, and
`tests/venue_parity.rs` guards that the off-chain and on-chain `Venue` enums stay
byte-compatible.
