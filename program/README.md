# Byreal Titan Venue Program

Standalone exact-in Anchor program for the Byreal CLMM CPI adapter used by
Titan-style routed swaps.

The program focuses on the integration surface:

- `initialize` creates the TitanPDA route signer.
- `swap_route_v3` validates route accounts, TitanPDA custody, and route-leg
  serialization.
- `instructions/venues/byreal_clmm.rs` serializes the Byreal CLMM
  `swap_v3_dyn` CPI instruction.

## Build

```bash
cargo check --manifest-path program/Cargo.toml
make build-program
```

## Route Instruction Interface

```rust
#[instruction(discriminator = [42])]
pub fn swap_route_v3<'info>(
    ctx: Context<'_, '_, 'info, 'info, SwapRouteV3<'info>>,
    amount: u64,
    mints: u8,
    swaps: Vec<SwapSpecInputV2>,
) -> Result<()>
```

This program models exact-in execution: `amount` is the exact input amount the
router will spend.

## Remaining Accounts Layout

`swap_route_v3` expects remaining accounts in this order:

```text
[0..mints]         TitanPDA token accounts, one per route mint
[mints..2*mints]  mint accounts, aligned with the ATAs above
[2*mints..N]      venue CPI accounts for each swap leg
```

For each swap leg:

- `n_accounts` is the number of accounts for that leg.
- `n_accounts` includes the venue program id as the final account.
- The router passes all `n_accounts` accounts to `invoke_signed`.
- The router passes only the first `n_accounts - 1` accounts as `AccountMeta`s
  to the Byreal venue module.

The off-chain `swap_route::build_swap_leg` helper clears the TitanPDA signer
flag, appends the venue program id, and sets `n_accounts`.

## Byreal CLMM Adapter

`instructions/venues/byreal_clmm.rs` serializes the pinned `swap_v3_dyn`
instruction:

- discriminator `[229, 46, 213, 132, 105, 40, 40, 228]`
- `amount = input_amount`
- `other_amount_threshold = 0`
- `sqrt_price_limit_x64 = 0`
- `is_base_input = true`

The off-chain route builder appends the venue program id as the final account for
each leg. The dispatcher uses that final account as the CPI program id, so the
same adapter executes against the production Byreal CLMM program id supplied by
the off-chain account list.

## Tests

Route execution is a compile-time smoke test right now. LiteSVM execution is not
wired for this integration because the simulator dependency tree conflicts with
the Solana 2.3/Byreal CLMM dependency line.

```bash
cargo test --manifest-path program/Cargo.toml --release --lib --test venue_parity
cargo test --manifest-path program/Cargo.toml --release --lib byreal_clmm
cargo test --manifest-path program/Cargo.toml --release --test byreal_clmm_route -- --nocapture
```
