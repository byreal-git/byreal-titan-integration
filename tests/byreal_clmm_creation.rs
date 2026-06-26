use solana_pubkey::{Pubkey, pubkey};

use byreal_titan_integration::byreal_clmm::{
    BYREAL_CLMM_PROGRAM_ID, ByrealClmmVenue, CREATE_POOL_DECAY_FEE_DISCRIMINATOR,
    CREATE_POOL_DISCRIMINATOR, parse_pool_creations,
};
use byreal_titan_integration::trading_venue::FromAccount;
use byreal_titan_integration::trading_venue::error::TradingVenueError;
use byreal_titan_integration::trading_venue::protocol::PoolProtocol;
use byreal_titan_integration::trading_venue::venue_creation::{ParsedInstruction, PoolCreation};
use solana_account::Account;

const POOL: Pubkey = pubkey!("J4jiEPEu8c8nLdpkiMa7k1P8rL1HCJSNxCvzA5DsmYds");
const TOKEN_A_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const TOKEN_B_MINT: Pubkey = pubkey!("5W84C59fbWLnzhM6ZQpBSPes6cm6dZnQ6czax89bRB2Y");
const TEST_BYREAL_CLMM_PROGRAM_ID: Pubkey =
    pubkey!("45iBNkaENereLKMjLm2LHkF3hpDapf6mnvrM5HWFg9cY");

fn byreal_pool_creation(discriminator: [u8; 8]) -> ParsedInstruction {
    ParsedInstruction {
        program_id: BYREAL_CLMM_PROGRAM_ID,
        accounts: vec![
            pubkey!("11111111111111111111111111111111"),
            pubkey!("SysvarRent111111111111111111111111111111111"),
            pubkey!("4N8t4fQ3kQkYPZ6orFjU1MLFBp5y1wzLqfjM8w3B2m8M"),
            pubkey!("3PczJ2YQyQbXbgY7f9C5L6LJfmJ4L2t2Y5MG6c8z8CkN"),
            POOL,
            pubkey!("8EwH1yYdN1fC7f4GkqAo7QNKZYnJ5rU7VJ8kQgmZ9rgn"),
            TOKEN_A_MINT,
            TOKEN_B_MINT,
        ],
        data: discriminator.to_vec(),
    }
}

fn unrelated_instruction() -> ParsedInstruction {
    ParsedInstruction {
        program_id: BYREAL_CLMM_PROGRAM_ID,
        accounts: vec![],
        data: vec![],
    }
}

fn test_contract_pool_creation() -> ParsedInstruction {
    ParsedInstruction {
        program_id: TEST_BYREAL_CLMM_PROGRAM_ID,
        ..byreal_pool_creation(CREATE_POOL_DISCRIMINATOR)
    }
}

#[test]
fn parses_byreal_create_pool() {
    let creations = parse_pool_creations(&[byreal_pool_creation(CREATE_POOL_DISCRIMINATOR)]);

    assert_eq!(
        creations,
        vec![PoolCreation {
            protocol: PoolProtocol::ByrealClmm,
            pool: POOL,
            mints: vec![TOKEN_A_MINT, TOKEN_B_MINT],
        }],
    );
}

#[test]
fn parses_byreal_create_pool_decay_fee() {
    let creations =
        parse_pool_creations(&[byreal_pool_creation(CREATE_POOL_DECAY_FEE_DISCRIMINATOR)]);

    assert_eq!(
        creations,
        vec![PoolCreation {
            protocol: PoolProtocol::ByrealClmm,
            pool: POOL,
            mints: vec![TOKEN_A_MINT, TOKEN_B_MINT],
        }],
    );
}

#[test]
fn ignores_transactions_without_a_creation() {
    let creations = parse_pool_creations(&[unrelated_instruction()]);
    assert!(
        creations.is_empty(),
        "a transaction without a pool creation creates no pools, got {creations:?}"
    );
}

#[test]
fn ignores_test_contract_pool_creation() {
    let creations = parse_pool_creations(&[test_contract_pool_creation()]);
    assert!(
        creations.is_empty(),
        "production parser must not discover test-contract pools, got {creations:?}"
    );
}

#[test]
fn rejects_test_contract_pool_account_owner() {
    let account = Account {
        lamports: 0,
        data: vec![],
        owner: TEST_BYREAL_CLMM_PROGRAM_ID,
        executable: false,
        rent_epoch: 0,
    };

    let err = match ByrealClmmVenue::from_account(&POOL, &account) {
        Ok(_) => panic!("runtime venue construction must reject test-contract owners"),
        Err(err) => err,
    };
    assert!(matches!(err, TradingVenueError::UnsupportedVenue(_)));
}
