# Byreal CLMM Titan Integration

Byreal CLMM venue integration for Titan quoting and routed swaps.

## Repository Shape

- `src/byreal_clmm/`: Byreal venue implementation: pool creation parsing, account loading, exact-in quote, marginal price, dynamic-fee math, and swap instruction construction.
- `src/trading_venue/`: Titan's off-chain venue interface and shared support types.
- `src/swap_route/`: off-chain helper for building Titan `swap_route_v3` route legs from a Byreal venue swap instruction.
- `src/account_caching/`: RPC-backed account cache used by the test harness and venue state refresh.
- `program/`: Anchor route program used for TitanPDA custody and Byreal CLMM CPI dispatch.
- `tests/`: Byreal parser, quote, scorecard, and route-builder tests.

This repo is a Byreal-specific Titan integration package. Titan template
placeholders such as `your_venue` and the Raydium example are not part of the
finished integration.

## Pinned ABI

The implementation targets the Byreal CLMM program crate revision used by the
Jupiter reference integration:

- Program crate: `https://github.com/byreal-git/byreal-clmm`
- Cargo rev: `650afbdf9d1b3bd6f6996eafd49254fde5f3e1c8`
- IDL source in that rev: `idl/byreal_clmm.json`
- IDL SHA-256: `4aa87153b22848c8d6287addd3effbc7579b295cee44429b1aea26c60cc0ef76`
- Drift-check reference: `byreal-git/byreal-clmm` at `650afbdf9d1b3bd6f6996eafd49254fde5f3e1c8`
- Production program id: `REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2`

Instruction discriminators:

- `create_pool`: `[233, 146, 209, 142, 207, 104, 64, 188]`
- `create_pool_decay_fee`: `[252, 154, 210, 191, 22, 217, 136, 252]`
- `swap_v3_dyn`: `[229, 46, 213, 132, 105, 40, 40, 228]`

## Runtime Scope

- Titan routes exact-in only. `SwapType::ExactOut` returns `ExactOutNotSupported`.
- Quotes and CPI amounts use raw token atoms. No UI decimal scaling is applied.
- Dynamic-fee pools require valid Pyth receiver `PriceUpdateV2` accounts and fresh positive prices.
- Route building fails closed when either route input or output mint has Token-2022 transfer fees, because route-level custody does not yet model gross/net settlement.
- Per-leg `other_amount_threshold` is `0`; Titan's route-level slippage guard remains the protection.
- LiteSVM is isolated to the program test crate. The SDK crate does not import or depend on LiteSVM.

## Main Implementation

| Layer | Files |
| --- | --- |
| Byreal venue adapter | `src/byreal_clmm/mod.rs` |
| Byreal quote/swap core | `src/byreal_clmm/core.rs` |
| Titan venue traits | `src/trading_venue/mod.rs`, `src/trading_venue/error.rs`, `src/trading_venue/token_info.rs`, `src/trading_venue/venue_creation.rs` |
| Route builder | `src/swap_route/mod.rs` |
| Program CPI adapter | `program/programs/byreal-titan-venue-program/src/instructions/venues/byreal_clmm.rs` |
| Program route dispatch | `program/programs/byreal-titan-venue-program/src/instructions/swap_route_v3.rs` |
| Program enum parity | `program/programs/byreal-titan-venue-program/tests/venue_parity.rs` |
| Byreal tests | `tests/byreal_clmm.rs`, `tests/byreal_clmm_creation.rs`, `program/.../tests/byreal_clmm_route.rs` |

## Tests

```bash
make build-program
make check-structure
make test-venue
make scorecard
```

Targeted commands:

```bash
cargo test --quiet --release --lib --test scorecard
cargo test --quiet --release --test byreal_clmm_creation
cargo test --quiet --release --lib swap_route
cargo test --quiet --manifest-path program/Cargo.toml --release --lib --test venue_parity
cargo test --quiet --manifest-path program/Cargo.toml --release --lib byreal_clmm
cargo test --quiet --manifest-path program/Cargo.toml --release --test byreal_clmm_route -- --nocapture
```

RPC-backed quote tests skip unless both `SOLANA_RPC_URL` and `BYREAL_CLMM_POOL`
are set. `BYREAL_CLMM_POOL` must be a pool owned by the production Byreal CLMM
program, not the test contract:

```bash
export SOLANA_RPC_URL=https://...
export BYREAL_CLMM_POOL=<production-pool-pubkey>
cargo test --quiet --release --test byreal_clmm -- --skip construction --nocapture
```

LiteSVM-backed route execution lives under
`program/programs/byreal-titan-venue-program/tests`. It additionally requires a
built route program and a dumped production Byreal CLMM program:

```bash
make build-program
make dump-programs
cargo test --quiet --manifest-path program/Cargo.toml --release --test byreal_clmm_route -- --nocapture
```
