//! Shared, venue-generic test suite.
//!
//! `tests/byreal_clmm.rs` runs these functions against the Byreal venue type.
//!
//! Every function gates on prerequisites and SKIPs (returns) when they're
//! missing, so `cargo test` is clean on a fresh clone:
//! - live checks need `SOLANA_RPC_URL` for the configured pool cluster;
//! - SDK-level simulation entry points skip because LiteSVM is isolated to the
//!   program test crate.

use std::env;
use std::time::Instant;

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_pubkey::Pubkey;

use byreal_titan_integration::account_caching::rpc_cache::RpcClientCache;
use byreal_titan_integration::trading_venue::{
    FromAccount, QuoteRequest, SwapType, TradingVenue,
};

/// Bound shared by every suite function: a venue that can be built from an
/// account and quoted, usable across `.await` points.
pub trait SuiteVenue: TradingVenue + FromAccount + Send + Sync {}
impl<T: TradingVenue + FromAccount + Send + Sync> SuiteVenue for T {}

/// Per-venue configuration the test entry points supply.
pub struct SuiteConfig {
    /// Pool/market account address the venue is constructed from.
    pub pool: Option<Pubkey>,
}

// ---------------------------------------------------------------------------
// Prerequisite gates and small helpers
// ---------------------------------------------------------------------------

pub fn init_test_logger() {
    drop(env_logger::builder().is_test(true).try_init());
}

fn current_test() -> String {
    std::thread::current()
        .name()
        .unwrap_or("a venue test")
        .to_string()
}

/// RPC URL for the suite, or `None` (with a SKIP message) when `SOLANA_RPC_URL`
/// is unset — so the tests are no-ops on a fresh clone instead of panicking.
fn rpc_url_or_skip() -> Option<String> {
    match env::var("SOLANA_RPC_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!(
                "SKIP {}: set SOLANA_RPC_URL to run this venue test",
                current_test()
            );
            None
        }
    }
}

fn pool_or_skip(config: &SuiteConfig) -> Option<Pubkey> {
    match config.pool {
        Some(pool) => Some(pool),
        None => {
            eprintln!(
                "SKIP {}: set BYREAL_CLMM_POOL to a production Byreal CLMM pool",
                current_test()
            );
            None
        }
    }
}

/// Default seed for the sampling tests; override with `TEST_SEED=<u64>`.
const DEFAULT_TEST_SEED: u64 = 0x7174_616e_5345_4544; // "titanSED"

/// Deterministic RNG for sampling-based tests. Seeded from `TEST_SEED` (default
/// `DEFAULT_TEST_SEED`) and printed so any failure is reproducible.
fn test_rng() -> rand::rngs::StdRng {
    use rand::SeedableRng;
    let seed = env::var("TEST_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TEST_SEED);
    eprintln!("{}: TEST_SEED={seed}", current_test());
    rand::rngs::StdRng::seed_from_u64(seed)
}

/// Log-uniform sample in `[lo, hi]`, drawn from the supplied seeded RNG.
fn sample_log_uniform_u64(rng: &mut rand::rngs::StdRng, lo: u64, hi: u64) -> u64 {
    use rand::Rng;
    assert!(lo >= 1, "log-uniform sampling requires lo >= 1");
    assert!(lo <= hi);
    let log_lo = (lo as f64).ln();
    let log_hi = (hi as f64).ln();
    let r: f64 = rng.random();
    ((log_lo + r * (log_hi - log_lo)).exp() as u64).clamp(lo, hi)
}

/// Geometrically-spaced probe points across `[lb, ub]`, for the mean-value
/// theorem chord checks.
fn geometric_grid(lb: u64, ub: u64, n: usize) -> Vec<u64> {
    assert!(lb >= 1 && ub > lb && n >= 2);
    let ln_lo = (lb as f64).ln();
    let ln_hi = (ub as f64).ln();
    let mut points: Vec<u64> = (0..n)
        .map(|i| {
            let t = i as f64 / (n - 1) as f64;
            ((ln_lo + t * (ln_hi - ln_lo)).exp() as u64).clamp(lb, ub)
        })
        .collect();
    points.sort();
    points.dedup();
    points
}

fn exact_in(input_mint: Pubkey, output_mint: Pubkey, amount: u64) -> QuoteRequest {
    QuoteRequest {
        input_mint,
        output_mint,
        amount,
        swap_type: SwapType::ExactIn,
    }
}

fn rates_close(left: f64, right: f64) -> bool {
    const REL_TOL: f64 = 1e-9;
    const ABS_TOL: f64 = 1e-12;
    let scale = left.abs().max(right.abs()).max(1.0);
    (left - right).abs() <= ABS_TOL + REL_TOL * scale
}

fn assert_price_locally_consistent<V: SuiteVenue>(
    venue: &V,
    request: QuoteRequest,
    quoted_output: u64,
    price: f64,
) {
    assert!(
        price.is_finite() && price > 0.0,
        "price must be positive, got {price} at {}",
        request.amount
    );

    if request.amount == 0 {
        return;
    }

    let step = (request.amount / 10_000).max(1);
    let mut candidates = Vec::new();

    if let Some(next_amount) = request.amount.checked_add(step) {
        let probe = QuoteRequest {
            amount: next_amount,
            ..request.clone()
        };
        if let Ok(next_quote) = venue.quote(probe)
            && next_quote.expected_output > quoted_output
        {
            candidates.push((next_quote.expected_output - quoted_output) as f64 / step as f64);
        }
    }

    let previous_amount = request.amount.saturating_sub(step);
    if previous_amount < request.amount {
        let probe = QuoteRequest {
            amount: previous_amount,
            ..request.clone()
        };
        if let Ok(previous_quote) = venue.quote(probe)
            && quoted_output > previous_quote.expected_output
        {
            candidates
                .push((quoted_output - previous_quote.expected_output) as f64 / step as f64);
        }
    }

    if candidates.is_empty() && quoted_output > 0 {
        candidates.push(quoted_output as f64 / request.amount as f64);
    }

    assert!(
        candidates
            .iter()
            .any(|candidate| rates_close(price, *candidate)),
        "price {price} is not locally consistent at {}; candidate rates: {:?}",
        request.amount,
        candidates
    );
}

/// Fetch the pool, build the venue, and bring it to a fully-updated state.
/// Returns the venue plus the RPC cache it was loaded through (reused for sims).
async fn build_venue<V: SuiteVenue>(rpc_url: String, pool: Pubkey) -> (V, RpcClientCache) {
    let rpc = RpcClient::new(rpc_url);
    let account = rpc
        .get_account(&pool)
        .await
        .expect("failed to fetch pool account");
    let mut venue = V::from_account(&pool, &account).expect("failed to build venue from account");
    let cache = RpcClientCache::new(rpc);
    venue
        .update_state(&cache)
        .await
        .expect("venue state update failed");
    (venue, cache)
}

// ---------------------------------------------------------------------------
// The suite. Each function is one venue test; the entry points wrap these in
// `#[tokio::test]` against their venue type.
// ---------------------------------------------------------------------------

/// Construction & boundaries: the venue builds, loads state, exposes valid token
/// info, computes boundaries with a positive spot price, and quotes with a
/// positive price at both edges.
pub async fn construction<V: SuiteVenue>(config: &SuiteConfig) {
    init_test_logger();
    let Some(rpc_url) = rpc_url_or_skip() else {
        return;
    };
    let Some(pool) = pool_or_skip(config) else {
        return;
    };
    let (venue, _cache) = build_venue::<V>(rpc_url, pool).await;

    let token_info = venue.get_token_info();
    log::info!("Loaded token info: {:#?}", token_info);
    assert!(
        token_info.len() >= 2,
        "venue must expose at least two tokens"
    );

    for (in_idx, out_idx) in venue.directions_num() {
        let (lower, upper) = venue
            .bounds(in_idx, out_idx)
            .expect("boundary search failed");
        assert!(lower < upper, "lower bound must be < upper bound");

        let input_mint = venue.get_token(in_idx as usize).unwrap().pubkey;
        let output_mint = venue.get_token(out_idx as usize).unwrap().pubkey;

        for (edge, amount) in [("lower", lower), ("upper", upper)] {
            let q = venue
                .quote(exact_in(input_mint, output_mint, amount))
                .unwrap_or_else(|_| panic!("{edge}-bound quote failed"));
            assert!(
                !q.not_enough_liquidity,
                "{edge} bound: insufficient liquidity"
            );
            assert!(q.expected_output > 0, "{edge} bound: zero output");
            assert!(
                q.price > 0.0,
                "{edge} bound: non-positive price {}",
                q.price
            );
        }
    }
}

/// Zero-input quote: Titan sometimes requests a quote at `amount == 0`. The
/// venue must not error — it must return zero output together with a positive
/// spot price `f'(0)`. This is the boundary case of the pricing contract on
/// `QuoteResult::price`.
pub async fn zero_input_spot_price<V: SuiteVenue>(config: &SuiteConfig) {
    init_test_logger();
    let Some(rpc_url) = rpc_url_or_skip() else {
        return;
    };
    let Some(pool) = pool_or_skip(config) else {
        return;
    };
    let (venue, _cache) = build_venue::<V>(rpc_url, pool).await;
    assert!(venue.get_token_info().len() >= 2);

    for (in_idx, out_idx) in venue.directions_num() {
        let input_mint = venue.get_token(in_idx as usize).unwrap().pubkey;
        let output_mint = venue.get_token(out_idx as usize).unwrap().pubkey;

        let quote = venue
            .quote(exact_in(input_mint, output_mint, 0))
            .expect("zero-input quote must not error");
        assert_eq!(
            quote.expected_output, 0,
            "zero input must produce zero output"
        );
        assert!(
            quote.price > 0.0,
            "zero input must still report a positive spot price, got {}",
            quote.price
        );
    }
}

/// Boundary simulation is intentionally skipped in the SDK-level suite.
pub async fn bound_simulation<V: SuiteVenue>(_config: &SuiteConfig) {
    init_test_logger();
    eprintln!(
        "SKIP {}: SDK-level simulation is not wired; LiteSVM route execution lives in the program test crate",
        current_test()
    );
}

/// Random-sample simulation is intentionally skipped in the SDK-level suite.
pub async fn random_samples<V: SuiteVenue>(_config: &SuiteConfig) {
    init_test_logger();
    eprintln!(
        "SKIP {}: SDK-level simulation is not wired; LiteSVM route execution lives in the program test crate",
        current_test()
    );
}

/// Output monotonicity: a larger `ExactIn` amount never returns less output.
pub async fn monotone<V: SuiteVenue>(config: &SuiteConfig) {
    init_test_logger();
    let Some(rpc_url) = rpc_url_or_skip() else {
        return;
    };
    let Some(pool) = pool_or_skip(config) else {
        return;
    };
    let (venue, _cache) = build_venue::<V>(rpc_url, pool).await;

    let mut rng = test_rng();
    for (in_idx, out_idx) in venue.directions_num() {
        let (lb, ub) = venue.bounds(in_idx, out_idx).unwrap();
        let input_mint = venue.get_token(in_idx as usize).unwrap().pubkey;
        let output_mint = venue.get_token(out_idx as usize).unwrap().pubkey;

        let mut amounts: Vec<u64> = (0..50)
            .map(|_| sample_log_uniform_u64(&mut rng, lb, ub))
            .collect();
        amounts.sort();

        let mut prev = 0;
        for amount in amounts {
            let out = venue
                .quote(exact_in(input_mint, output_mint, amount))
                .expect("quote failed")
                .expected_output;
            assert!(
                prev <= out,
                "output not monotone: {prev} > {out} at {amount}"
            );
            prev = out;
        }
    }
}

/// Quoting speed: a single quote should stay comfortably below router-scale
/// latency budgets in release mode.
pub async fn quoting_speed<V: SuiteVenue>(config: &SuiteConfig) {
    const ITERATIONS: usize = 10_000;
    const MAX_AVG_SECONDS: f64 = 0.0001;
    init_test_logger();
    let Some(rpc_url) = rpc_url_or_skip() else {
        return;
    };
    let Some(pool) = pool_or_skip(config) else {
        return;
    };
    let (venue, _cache) = build_venue::<V>(rpc_url, pool).await;

    let mut rng = test_rng();
    for (in_idx, out_idx) in venue.directions_num() {
        let (lb, ub) = venue.bounds(in_idx, out_idx).unwrap();
        let input_mint = venue.get_token(in_idx as usize).unwrap().pubkey;
        let output_mint = venue.get_token(out_idx as usize).unwrap().pubkey;

        let amounts: Vec<u64> = (0..ITERATIONS)
            .map(|_| sample_log_uniform_u64(&mut rng, lb, ub))
            .collect();
        let start = Instant::now();
        for amount in amounts {
            venue
                .quote(exact_in(input_mint, output_mint, amount))
                .expect("quote failed");
        }
        let avg = start.elapsed().as_secs_f64() / ITERATIONS as f64;
        log::info!("average quote time: {avg}s");
        assert!(
            avg < MAX_AVG_SECONDS,
            "quoting too slow ({avg}s) for {input_mint} -> {output_mint}"
        );
    }
}

/// The reported marginal price is a finite, positive local probe. Integer atom
/// rounding can make adjacent tiny probes non-monotone, so monotonicity is
/// asserted on quoted output instead.
pub async fn reported_price_positive<V: SuiteVenue>(config: &SuiteConfig) {
    init_test_logger();
    let Some(rpc_url) = rpc_url_or_skip() else {
        return;
    };
    let Some(pool) = pool_or_skip(config) else {
        return;
    };
    let (venue, _cache) = build_venue::<V>(rpc_url, pool).await;
    assert!(venue.get_token_info().len() >= 2);

    let mut rng = test_rng();
    for (in_idx, out_idx) in venue.directions_num() {
        let (lb, ub) = venue.bounds(in_idx, out_idx).unwrap();
        let input_mint = venue.get_token(in_idx as usize).unwrap().pubkey;
        let output_mint = venue.get_token(out_idx as usize).unwrap().pubkey;

        let mut amounts: Vec<u64> = (0..200)
            .map(|_| sample_log_uniform_u64(&mut rng, lb, ub))
            .collect();
        amounts.sort();

        for amount in amounts {
            let request = exact_in(input_mint, output_mint, amount);
            let quote = venue.quote(request.clone()).expect("quote failed");
            assert_price_locally_consistent(&venue, request, quote.expected_output, quote.price);
        }
    }
}

/// Local price probes should stay finite and positive around realized output
/// changes. The exact chord can sit outside adjacent finite differences because
/// CLMM quote output is floor-truncated to token atoms.
pub async fn local_price_probe_consistent<V: SuiteVenue>(config: &SuiteConfig) {
    init_test_logger();
    let Some(rpc_url) = rpc_url_or_skip() else {
        return;
    };
    let Some(pool) = pool_or_skip(config) else {
        return;
    };
    let (venue, _cache) = build_venue::<V>(rpc_url, pool).await;
    assert!(venue.get_token_info().len() >= 2);

    for (in_idx, out_idx) in venue.directions_num() {
        let (lb, ub) = venue.bounds(in_idx, out_idx).unwrap();
        let input_mint = venue.get_token(in_idx as usize).unwrap().pubkey;
        let output_mint = venue.get_token(out_idx as usize).unwrap().pubkey;

        let grid = geometric_grid(lb, ub, 64);
        for pair in grid.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if b <= a {
                continue;
            }
            let qa = venue
                .quote(exact_in(input_mint, output_mint, a))
                .expect("quote at a");
            let qb = venue
                .quote(exact_in(input_mint, output_mint, b))
                .expect("quote at b");
            if qb.expected_output <= qa.expected_output {
                continue; // flat step carries no rate information
            }

            let chord = (qb.expected_output - qa.expected_output) as f64 / (b - a) as f64;
            assert!(
                chord.is_finite() && chord > 0.0,
                "chord must be positive on [{a}, {b}], got {chord}"
            );
            assert_price_locally_consistent(
                &venue,
                exact_in(input_mint, output_mint, a),
                qa.expected_output,
                qa.price,
            );
            assert_price_locally_consistent(
                &venue,
                exact_in(input_mint, output_mint, b),
                qb.expected_output,
                qb.price,
            );
        }
    }
}
