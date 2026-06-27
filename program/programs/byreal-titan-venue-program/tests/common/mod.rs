//! Shared route-test suite for the on-chain program.
//!
//! LiteSVM is intentionally isolated to this program test crate. The SDK crate
//! is only a dependency under test; it does not import or depend on LiteSVM.

use std::env;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anchor_spl::memo::spl_memo;
use byreal_titan_integration::account_caching::AccountsCache;
use byreal_titan_integration::account_caching::rpc_cache::RpcClientCache;
use byreal_titan_integration::byreal_clmm::BYREAL_CLMM_PROGRAM_ID;
use byreal_titan_integration::swap_route::{
    ROUTE_WEIGHT_ALL, build_swap_leg, encode_swap_route_v3_data,
};
use byreal_titan_integration::trading_venue::error::TradingVenueError;
use byreal_titan_integration::trading_venue::token_info::TokenInfo;
use byreal_titan_integration::trading_venue::{
    FromAccount, QuoteRequest, SwapType, TradingVenue,
};
use byreal_titan_venue_program::state::TitanPda;
use litesvm::LiteSVM;
use solana_account::{Account, ReadableAccount, WritableAccount};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_compute_budget::compute_budget::ComputeBudget;
use solana_instruction::{AccountMeta, Instruction};
use solana_program::native_token::LAMPORTS_PER_SOL;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sysvar::clock::{self, Clock};
use solana_transaction::Transaction;
use spl_associated_token_account::get_associated_token_address_with_program_id;
use spl_token::state::{Account as TokenAccount, AccountState};

pub trait RouteVenue: TradingVenue + FromAccount + Send + Sync {}
impl<T: TradingVenue + FromAccount + Send + Sync> RouteVenue for T {}

fn init_test_logger() {
    drop(env_logger::builder().is_test(true).try_init());
}

fn current_test() -> String {
    std::thread::current()
        .name()
        .unwrap_or("a route test")
        .to_string()
}

fn rpc_url_or_skip() -> Option<String> {
    match env::var("SOLANA_RPC_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!(
                "SKIP {}: set SOLANA_RPC_URL to run this swap-route test",
                current_test()
            );
            None
        }
    }
}

fn pool_or_skip() -> Option<Pubkey> {
    match env::var("BYREAL_CLMM_POOL") {
        Ok(pool) => Some(Pubkey::from_str(&pool).expect("BYREAL_CLMM_POOL must be a pubkey")),
        Err(_) => {
            eprintln!(
                "SKIP {}: set BYREAL_CLMM_POOL to a production Byreal CLMM pool",
                current_test()
            );
            None
        }
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("program crate lives under program/programs/<crate>")
        .to_path_buf()
}

fn route_program_path() -> PathBuf {
    repo_root()
        .join("program")
        .join("target")
        .join("deploy")
        .join("byreal_titan_venue_program.so")
}

fn venue_program_path(program: Pubkey) -> PathBuf {
    repo_root()
        .join("programs")
        .join(format!("{program}.so"))
}

fn require_file_or_skip(path: &Path, message: &str) -> bool {
    if path.exists() {
        true
    } else {
        eprintln!("SKIP {}: {message}", current_test());
        false
    }
}

fn token_for_mint<'a>(venue: &'a dyn TradingVenue, mint: Pubkey) -> &'a TokenInfo {
    venue
        .get_token_info()
        .iter()
        .find(|token| token.pubkey == mint)
        .expect("mint must belong to venue")
}

fn exact_in(input_mint: Pubkey, output_mint: Pubkey, amount: u64) -> QuoteRequest {
    QuoteRequest {
        input_mint,
        output_mint,
        amount,
        swap_type: SwapType::ExactIn,
    }
}

fn setup_litesvm() -> (LiteSVM, Keypair) {
    let mut litesvm = LiteSVM::new()
        .with_compute_budget(ComputeBudget {
            compute_unit_limit: 1_400_000,
            ..Default::default()
        })
        .with_sigverify(false)
        .with_transaction_history(0);
    let payer = Keypair::new();
    litesvm
        .airdrop(&payer.pubkey(), 10_000 * LAMPORTS_PER_SOL)
        .expect("payer airdrop failed");
    (litesvm, payer)
}

fn create_token_account(token: &TokenInfo, owner: Pubkey, amount: u64) -> Account {
    let token_program = token.get_token_program();
    let mut account = Account::new(LAMPORTS_PER_SOL, TokenAccount::LEN, &token_program);
    let token_account = TokenAccount {
        mint: token.pubkey,
        owner,
        amount,
        state: AccountState::Initialized,
        ..Default::default()
    };
    token_account.pack_into_slice(account.data_as_mut_slice());
    account
}

fn token_amount(litesvm: &LiteSVM, token_account: Pubkey) -> u64 {
    let account = litesvm
        .get_account(&token_account)
        .expect("token account missing after route");
    TokenAccount::unpack(account.data())
        .expect("token account must unpack")
        .amount
}

fn send(litesvm: &mut LiteSVM, payer: &Keypair, ix: Instruction) {
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        litesvm.latest_blockhash(),
    );
    litesvm.send_transaction(tx).expect("transaction failed");
}

fn initialize_titan_pda(litesvm: &mut LiteSVM, payer: &Keypair, titan_pda: Pubkey) {
    let data =
        anchor_lang::solana_program::hash::hash(b"global:initialize").to_bytes()[..8].to_vec();
    let ix = Instruction {
        program_id: byreal_titan_venue_program::ID,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(titan_pda, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
        ],
        data,
    };
    send(litesvm, payer, ix);
}

fn build_route_instruction(
    payer: Pubkey,
    titan_pda: Pubkey,
    venue: &dyn TradingVenue,
    request: &QuoteRequest,
) -> Result<Instruction, TradingVenueError> {
    let input_token = token_for_mint(venue, request.input_mint);
    let output_token = token_for_mint(venue, request.output_mint);
    let input_token_program = input_token.get_token_program();
    let output_token_program = output_token.get_token_program();

    let input_ata = get_associated_token_address_with_program_id(
        &payer,
        &request.input_mint,
        &input_token_program,
    );
    let output_ata = get_associated_token_address_with_program_id(
        &payer,
        &request.output_mint,
        &output_token_program,
    );
    let titan_pda_input_ata = get_associated_token_address_with_program_id(
        &titan_pda,
        &request.input_mint,
        &input_token_program,
    );
    let titan_pda_output_ata = get_associated_token_address_with_program_id(
        &titan_pda,
        &request.output_mint,
        &output_token_program,
    );

    let mut accounts = vec![
        AccountMeta::new(payer, true),
        AccountMeta::new(payer, true),
        AccountMeta::new(titan_pda, false),
        AccountMeta::new(input_ata, false),
        AccountMeta::new(output_ata, false),
        AccountMeta::new_readonly(spl_token::ID, false),
        AccountMeta::new_readonly(spl_token_2022::ID, false),
        AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
        AccountMeta::new_readonly(spl_associated_token_account::ID, false),
        AccountMeta::new_readonly(byreal_titan_venue_program::ID, false),
        AccountMeta::new_readonly(byreal_titan_venue_program::ID, false),
        AccountMeta::new_readonly(byreal_titan_venue_program::ID, false),
        AccountMeta::new(titan_pda_input_ata, false),
        AccountMeta::new(titan_pda_output_ata, false),
        AccountMeta::new_readonly(request.input_mint, false),
        AccountMeta::new_readonly(request.output_mint, false),
    ];

    let (spec, leg_accounts) =
        build_swap_leg(venue, request, titan_pda, 0, 1, ROUTE_WEIGHT_ALL)?;
    accounts.extend(leg_accounts);

    Ok(Instruction {
        program_id: byreal_titan_venue_program::ID,
        accounts,
        data: encode_swap_route_v3_data(request.amount, 2, &[spec]),
    })
}

async fn build_venue<V: RouteVenue>(rpc_url: String, pool: Pubkey) -> (V, RpcClientCache) {
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

async fn load_route_accounts(
    litesvm: &mut LiteSVM,
    cache: &RpcClientCache,
    venue: &dyn TradingVenue,
    payer: Pubkey,
    titan_pda: Pubkey,
    request: &QuoteRequest,
    route_ix: &Instruction,
) {
    let input_token = token_for_mint(venue, request.input_mint);
    let output_token = token_for_mint(venue, request.output_mint);
    let input_token_program = input_token.get_token_program();
    let output_token_program = output_token.get_token_program();

    let input_ata = get_associated_token_address_with_program_id(
        &payer,
        &request.input_mint,
        &input_token_program,
    );
    let output_ata = get_associated_token_address_with_program_id(
        &payer,
        &request.output_mint,
        &output_token_program,
    );
    let titan_pda_input_ata = get_associated_token_address_with_program_id(
        &titan_pda,
        &request.input_mint,
        &input_token_program,
    );
    let titan_pda_output_ata = get_associated_token_address_with_program_id(
        &titan_pda,
        &request.output_mint,
        &output_token_program,
    );

    litesvm
        .set_account(input_ata, create_token_account(input_token, payer, u64::MAX))
        .unwrap();
    litesvm
        .set_account(output_ata, create_token_account(output_token, payer, 0))
        .unwrap();
    litesvm
        .set_account(
            titan_pda_input_ata,
            create_token_account(input_token, titan_pda, 0),
        )
        .unwrap();
    litesvm
        .set_account(
            titan_pda_output_ata,
            create_token_account(output_token, titan_pda, 0),
        )
        .unwrap();

    let latest_clock = cache.get_account(&clock::ID).await.unwrap();
    let latest_clock: Clock = latest_clock
        .as_ref()
        .ok_or(TradingVenueError::NoAccountFound(clock::ID.into()))
        .unwrap()
        .deserialize_data()
        .unwrap();
    litesvm.set_sysvar::<Clock>(&latest_clock);

    let mut accounts_to_load = vec![request.input_mint, request.output_mint];
    accounts_to_load.extend(route_ix.accounts.iter().map(|account| account.pubkey));
    accounts_to_load.extend(venue.get_required_pubkeys_for_update().unwrap());
    accounts_to_load.sort();
    accounts_to_load.dedup();

    let accounts = cache.get_accounts(&accounts_to_load).await.unwrap();
    for (pubkey, account) in accounts_to_load.into_iter().zip(accounts) {
        if [
            input_ata,
            output_ata,
            titan_pda_input_ata,
            titan_pda_output_ata,
            payer,
            titan_pda,
            byreal_titan_venue_program::ID,
            BYREAL_CLMM_PROGRAM_ID,
            spl_token::ID,
            spl_token_2022::ID,
            spl_associated_token_account::ID,
            spl_memo::ID,
            solana_sdk::system_program::id(),
        ]
        .contains(&pubkey)
        {
            continue;
        }
        if let Some(account) = account {
            if !account.executable {
                litesvm.set_account(pubkey, account).unwrap();
            }
        }
    }
}

pub async fn run_swap_route<V: RouteVenue>() {
    init_test_logger();

    let Some(rpc_url) = rpc_url_or_skip() else {
        return;
    };
    let Some(pool) = pool_or_skip() else {
        return;
    };

    let route_so = route_program_path();
    if !require_file_or_skip(&route_so, "missing built route program; run `make build-program`") {
        return;
    }
    let byreal_so = venue_program_path(BYREAL_CLMM_PROGRAM_ID);
    if !require_file_or_skip(
        &byreal_so,
        "missing Byreal program dump; run `make dump-programs`",
    ) {
        return;
    }

    let (venue, cache) = build_venue::<V>(rpc_url, pool).await;
    let (mut litesvm, payer) = setup_litesvm();
    litesvm
        .add_program_from_file(byreal_titan_venue_program::ID, route_so)
        .expect("failed to load route program");
    litesvm
        .add_program_from_file(BYREAL_CLMM_PROGRAM_ID, byreal_so)
        .expect("failed to load Byreal CLMM program");

    let (titan_pda, _) =
        Pubkey::find_program_address(&[TitanPda::SEED], &byreal_titan_venue_program::ID);
    initialize_titan_pda(&mut litesvm, &payer, titan_pda);

    for (input_index, output_index) in venue.directions_num() {
        let input_mint = venue.get_token(input_index as usize).unwrap().pubkey;
        let output_mint = venue.get_token(output_index as usize).unwrap().pubkey;
        let (lower, upper) = venue
            .bounds(input_index, output_index)
            .expect("failed to compute bounds");
        let amount = lower.max(1).min(upper);
        let request = exact_in(input_mint, output_mint, amount);
        let quote = venue.quote(request.clone()).expect("quote failed");
        if quote.not_enough_liquidity || quote.expected_output == 0 {
            continue;
        }

        let route_ix =
            match build_route_instruction(payer.pubkey(), titan_pda, &venue, &request) {
                Ok(ix) => ix,
                Err(TradingVenueError::UnsupportedVenue(reason)) => {
                    eprintln!(
                        "SKIP {}: route unsupported for {} -> {}: {reason}",
                        current_test(),
                        input_mint,
                        output_mint,
                    );
                    continue;
                }
                Err(err) => panic!("failed to build route instruction: {err:?}"),
            };
        load_route_accounts(
            &mut litesvm,
            &cache,
            &venue,
            payer.pubkey(),
            titan_pda,
            &request,
            &route_ix,
        )
        .await;

        let output_token = token_for_mint(&venue, request.output_mint);
        let output_ata = get_associated_token_address_with_program_id(
            &payer.pubkey(),
            &request.output_mint,
            &output_token.get_token_program(),
        );
        let before = token_amount(&litesvm, output_ata);
        send(&mut litesvm, &payer, route_ix);
        let after = token_amount(&litesvm, output_ata);
        let simulated = after
            .checked_sub(before)
            .expect("output account balance decreased");

        assert_eq!(
            simulated, quote.expected_output,
            "route simulation output mismatch for {input_mint} -> {output_mint} amount {amount}",
        );
    }
}
