use std::collections::HashMap;

mod core;

use async_trait::async_trait;
use self::core::{
    ByrealClmmVenue as CoreByrealClmmVenue, SwapBuildRequest,
};
use solana_account::Account;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use solana_sysvar::clock::{self, Clock};

use crate::{
    account_caching::AccountsCache,
    trading_venue::{
        FromAccount, QuoteRequest, QuoteResult, SwapType, TradingVenue,
        error::{ErrorInfo, TradingVenueError},
        protocol::PoolProtocol,
        token_info::TokenInfo,
        venue_creation::{ParsedInstruction, PoolCreation},
    },
};

pub const BYREAL_CLMM_MAINNET_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2");
pub const BYREAL_CLMM_PROGRAM_ID: Pubkey = BYREAL_CLMM_MAINNET_PROGRAM_ID;

pub const CREATE_POOL_DISCRIMINATOR: [u8; 8] = [233, 146, 209, 142, 207, 104, 64, 188];
pub const CREATE_POOL_DECAY_FEE_DISCRIMINATOR: [u8; 8] =
    [252, 154, 210, 191, 22, 217, 136, 252];
pub const SWAP_V3_DYN_DISCRIMINATOR: [u8; 8] = [229, 46, 213, 132, 105, 40, 40, 228];

const CREATE_POOL_POOL_INDEX: usize = 4;
const CREATE_POOL_TOKEN_MINT_0_INDEX: usize = 6;
const CREATE_POOL_TOKEN_MINT_1_INDEX: usize = 7;

fn push_unique(keys: &mut Vec<Pubkey>, key: Pubkey) {
    if !keys.contains(&key) {
        keys.push(key);
    }
}

fn starts_with_discriminator(data: &[u8], discriminator: [u8; 8]) -> bool {
    data.len() >= discriminator.len() && data[..discriminator.len()] == discriminator
}

pub fn parse_pool_creations(instructions: &[ParsedInstruction]) -> Vec<PoolCreation> {
    instructions
        .iter()
        .filter_map(|instruction| {
            if instruction.program_id != BYREAL_CLMM_PROGRAM_ID {
                return None;
            }

            let is_create_pool =
                starts_with_discriminator(&instruction.data, CREATE_POOL_DISCRIMINATOR)
                    || starts_with_discriminator(
                        &instruction.data,
                        CREATE_POOL_DECAY_FEE_DISCRIMINATOR,
                    );
            if !is_create_pool || instruction.accounts.len() <= CREATE_POOL_TOKEN_MINT_1_INDEX {
                return None;
            }

            let token_mint_0 = instruction.accounts[CREATE_POOL_TOKEN_MINT_0_INDEX];
            let token_mint_1 = instruction.accounts[CREATE_POOL_TOKEN_MINT_1_INDEX];

            Some(PoolCreation {
                protocol: PoolProtocol::ByrealClmm,
                pool: instruction.accounts[CREATE_POOL_POOL_INDEX],
                mints: vec![token_mint_0, token_mint_1],
            })
        })
        .collect()
}

#[derive(Clone)]
pub struct ByrealClmmVenue {
    core: CoreByrealClmmVenue,
    token_info: Vec<TokenInfo>,
    initialized: bool,
    current_epoch: u64,
}

impl ByrealClmmVenue {
    fn ensure_exact_in(request: &QuoteRequest) -> Result<(), TradingVenueError> {
        if request.swap_type != SwapType::ExactIn {
            return Err(TradingVenueError::ExactOutNotSupported);
        }
        Ok(())
    }

    fn base_token_info(core: &CoreByrealClmmVenue) -> Vec<TokenInfo> {
        let mints = core.token_mints();
        let decimals = core.mint_decimals();
        mints
            .into_iter()
            .zip(decimals)
            .map(|(mint, decimals)| TokenInfo {
                pubkey: mint,
                decimals: i32::from(decimals),
                is_token_2022: false,
                transfer_fee: None,
                maximum_fee: None,
            })
            .collect()
    }

    async fn fetch_account_map(
        cache: &dyn AccountsCache,
        keys: &[Pubkey],
    ) -> Result<HashMap<Pubkey, Account>, TradingVenueError> {
        let accounts = cache.get_accounts(keys).await?;
        let mut map = HashMap::with_capacity(accounts.len());
        for (key, account) in keys.iter().copied().zip(accounts) {
            if let Some(account) = account {
                map.insert(key, account);
            }
        }
        Ok(map)
    }

    fn clock_from_account_map(
        account_map: &HashMap<Pubkey, Account>,
    ) -> Result<Clock, TradingVenueError> {
        let clock_key = clock::ID;
        let clock_account = account_map
            .get(&clock_key)
            .ok_or_else(|| TradingVenueError::MissingState("sysvar clock".into()))?;
        bincode::deserialize(&clock_account.data).map_err(|e| {
            TradingVenueError::DeserializationFailed(ErrorInfo::String(format!(
                "decode sysvar clock failed: {e}"
            )))
        })
    }

    fn refresh_token_info(
        &mut self,
        account_map: &HashMap<Pubkey, Account>,
        epoch: u64,
    ) -> Result<(), TradingVenueError> {
        let mints = self.core.token_mints();
        let decimals = self.core.mint_decimals();
        let mut token_info = Vec::with_capacity(mints.len());

        for (index, mint_key) in mints.into_iter().enumerate() {
            let mint = mint_key;
            let Some(account_data) = account_map.get(&mint_key) else {
                return Err(TradingVenueError::MissingState(mint.into()));
            };

            let info = TokenInfo::new(&mint, account_data, epoch)
                .map(|mut info| {
                    if info.decimals < 0 {
                        info.decimals = i32::from(decimals[index]);
                    }
                    info
                })
                .map_err(|_| {
                    TradingVenueError::DeserializationFailed(ErrorInfo::String(format!(
                        "failed to decode token mint {mint}"
                    )))
                })?;
            token_info.push(info);
        }

        self.token_info = token_info;
        Ok(())
    }

    fn quote_output_for_price(&self, request: QuoteRequest) -> Result<u64, TradingVenueError> {
        Self::ensure_exact_in(&request)?;
        let quote = self.core.quote(request)?;
        if quote.not_enough_liquidity {
            return Err(TradingVenueError::NoQuotableValue(
                "not enough liquidity for price probe".into(),
            ));
        }
        Ok(quote.out_amount)
    }

    fn marginal_price(
        &self,
        request: QuoteRequest,
        quoted_output: u64,
    ) -> Result<f64, TradingVenueError> {
        if request.amount == 0 {
            let mut step = 1u64;
            for _ in 0..32 {
                let probe = QuoteRequest {
                    amount: step,
                    ..request.clone()
                };
                if let Ok(output) = self.quote_output_for_price(probe)
                    && output > 0
                {
                    return Ok(output as f64 / step as f64);
                }
                step = step.saturating_mul(2);
            }
            return Err(TradingVenueError::NoQuotableValue(
                "zero-input spot price probe produced no output".into(),
            ));
        }

        let step = (request.amount / 10_000).max(1);
        if let Some(next_amount) = request.amount.checked_add(step) {
            let probe = QuoteRequest {
                amount: next_amount,
                ..request.clone()
            };
            if let Ok(next_output) = self.quote_output_for_price(probe)
                && next_output > quoted_output
            {
                return Ok((next_output - quoted_output) as f64 / step as f64);
            }
        }

        let previous_amount = request.amount.saturating_sub(step);
        if previous_amount < request.amount {
            let probe = QuoteRequest {
                amount: previous_amount,
                ..request.clone()
            };
            if let Ok(previous_output) = self.quote_output_for_price(probe)
                && quoted_output > previous_output
            {
                return Ok((quoted_output - previous_output) as f64
                    / (request.amount - previous_amount) as f64);
            }
        }

        if quoted_output > 0 {
            return Ok(quoted_output as f64 / request.amount as f64);
        }

        Err(TradingVenueError::NoQuotableValue(
            "price probe produced no positive rate".into(),
        ))
    }
}

impl FromAccount for ByrealClmmVenue {
    fn from_account(pubkey: &Pubkey, account: &Account) -> Result<Self, TradingVenueError> {
        if account.owner != BYREAL_CLMM_PROGRAM_ID {
            return Err(TradingVenueError::UnsupportedVenue(
                format!(
                    "Byreal CLMM pool owner must be production program {BYREAL_CLMM_PROGRAM_ID}, got {}",
                    account.owner
                )
                .into(),
            ));
        }

        let core =
            CoreByrealClmmVenue::from_pool_state_account(*pubkey, account.owner, &account.data)?;
        let token_info = Self::base_token_info(&core);
        Ok(Self {
            core,
            token_info,
            initialized: false,
            current_epoch: 0,
        })
    }
}

#[async_trait]
impl TradingVenue for ByrealClmmVenue {
    fn initialized(&self) -> bool {
        self.initialized
    }

    fn program_id(&self) -> Pubkey {
        self.core.program_id()
    }

    fn program_dependencies(&self) -> Vec<Pubkey> {
        vec![self.program_id()]
    }

    fn market_id(&self) -> Pubkey {
        self.core.market_id()
    }

    fn get_token_info(&self) -> &[TokenInfo] {
        &self.token_info
    }

    fn protocol(&self) -> PoolProtocol {
        PoolProtocol::ByrealClmm
    }

    fn get_required_pubkeys_for_update(&self) -> Result<Vec<Pubkey>, TradingVenueError> {
        let mut keys = self.core.accounts_to_update();
        for mint in self.core.token_mints() {
            push_unique(&mut keys, mint);
        }
        Ok(keys)
    }

    async fn update_state(&mut self, cache: &dyn AccountsCache) -> Result<(), TradingVenueError> {
        let mut base_keys = self.core.base_accounts_to_update();
        for mint in self.core.token_mints() {
            push_unique(&mut base_keys, mint);
        }

        let base_accounts = Self::fetch_account_map(cache, &base_keys).await?;
        self.core.update_state(&base_accounts)?;

        let mut update_keys = self.core.accounts_to_update();
        for mint in self.core.token_mints() {
            push_unique(&mut update_keys, mint);
        }

        let update_accounts = Self::fetch_account_map(cache, &update_keys).await?;
        self.core.update_state(&update_accounts)?;

        let clock = Self::clock_from_account_map(&update_accounts)?;
        self.current_epoch = clock.epoch;
        self.refresh_token_info(&update_accounts, clock.epoch)?;
        self.initialized = true;
        Ok(())
    }

    fn quote(&self, request: QuoteRequest) -> Result<QuoteResult, TradingVenueError> {
        if !self.initialized {
            return Err(TradingVenueError::NotInitialized(self.market_id().into()));
        }

        Self::ensure_exact_in(&request)?;
        let quote = self.core.quote(request.clone())?;
        let price = self.marginal_price(request.clone(), quote.out_amount)?;

        Ok(QuoteResult {
            input_mint: request.input_mint,
            output_mint: request.output_mint,
            amount: quote.in_amount,
            expected_output: quote.out_amount,
            not_enough_liquidity: quote.not_enough_liquidity,
            price,
        })
    }

    fn generate_swap_instruction(
        &self,
        request: QuoteRequest,
        user: Pubkey,
    ) -> Result<Instruction, TradingVenueError> {
        if !self.initialized {
            return Err(TradingVenueError::NotInitialized(self.market_id().into()));
        }
        if request.swap_type != SwapType::ExactIn {
            return Err(TradingVenueError::ExactOutNotSupported);
        }

        let input_token = self
            .token_info
            .iter()
            .find(|token| token.pubkey == request.input_mint)
            .ok_or_else(|| TradingVenueError::InvalidMint(request.input_mint.into()))?;
        let output_token = self
            .token_info
            .iter()
            .find(|token| token.pubkey == request.output_mint)
            .ok_or_else(|| TradingVenueError::InvalidMint(request.output_mint.into()))?;

        let instruction = self
            .core
            .build_swap_instruction(SwapBuildRequest {
                user_authority: user,
                source_mint: request.input_mint,
                destination_mint: request.output_mint,
                source_token_account: input_token.get_associated_token_address(&user),
                destination_token_account: output_token.get_associated_token_address(&user),
                amount: request.amount,
                other_amount_threshold: 0,
            })?;

        Ok(instruction)
    }
}
