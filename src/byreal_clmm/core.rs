use {
  anchor_lang::{
    prelude::Pubkey,
    solana_program::{clock, sysvar},
    Discriminator, InstructionData,
  },
  anyhow::anyhow,
  byreal_clmm::{
    libraries::{
      dynamic_fee_math::{
        calculate_dynamic_fee_rate, normalize_trade_size, price_from_sqrt_price_x64,
        quote_amount_from_base, DynamicFeeInputs,
      },
      liquidity_math, swap_math, tick_math, MAX_SQRT_PRICE_X64, MIN_SQRT_PRICE_X64,
    },
    states::{
      AmmConfig, PoolState, PoolStatusBitIndex, TickArrayBitmapExtension, TickArrayState,
      TickState, TickUtils, TICK_ARRAY_SEED,
    },
    util::pyth::calculate_price_index,
  },
  crate::trading_venue::{
    error::{ErrorInfo, TradingVenueError},
    protocol::PoolProtocol,
    QuoteRequest,
  },
  pyth_solana_receiver_sdk::price_update::{Price, PriceUpdateV2},
  solana_account::Account,
  solana_instruction::{AccountMeta, Instruction},
  spl_token::solana_program::program_pack::Pack,
  spl_token_2022::{extension::StateWithExtensions, state::Account as Token2022Account},
  std::{
    collections::{BTreeSet, HashMap},
    mem::size_of,
  },
};

const PYTH_RECEIVER_PROGRAM_ID: Pubkey = pyth_solana_receiver_sdk::ID;
const PYTH_PRICE_FEED_PROGRAM_ID: Pubkey = pyth_solana_receiver_sdk::PYTH_PUSH_ORACLE_ID;
const POOL_TICK_ARRAY_BITMAP_SEED: &str = "pool_tick_array_bitmap_extension";
const DYNAMIC_MAX_PYTH_AGE_SECONDS: i64 = 3600;
const ZERO_PUBKEY: Pubkey = Pubkey::new_from_array([0u8; 32]);

#[derive(Debug, Clone)]
pub(super) struct CoreQuoteResult {
  pub in_amount: u64,
  pub out_amount: u64,
  pub not_enough_liquidity: bool,
}

pub(super) struct SwapBuildRequest {
  pub user_authority: Pubkey,
  pub source_mint: Pubkey,
  pub destination_mint: Pubkey,
  pub source_token_account: Pubkey,
  pub destination_token_account: Pubkey,
  pub amount: u64,
  pub other_amount_threshold: u64,
}

#[derive(Clone)]
pub enum DynamicTickArrayState {
  Dynamic(Box<(byreal_clmm::states::DynTickArrayState, Vec<TickState>)>),
  Fixed(Box<TickArrayState>),
}

impl DynamicTickArrayState {
  fn decode_dyn_tick_array(
    data: &[u8],
  ) -> Option<(byreal_clmm::states::DynTickArrayState, Vec<TickState>)> {
    if data.len() < 8 {
      return None;
    }
    if &data[0..8] != byreal_clmm::states::DynTickArrayState::DISCRIMINATOR {
      return None;
    }
    if data.len() < byreal_clmm::states::DynTickArrayState::HEADER_LEN {
      return None;
    }

    // The on-chain account bytes may not be aligned to the Rust struct's alignment,
    // so avoid `bytemuck::from_bytes` / `try_cast_slice` (which assume alignment).
    let header_bytes = &data[8..(byreal_clmm::states::DynTickArrayState::HEADER_LEN)];
    let mut header_buf = vec![0u8; header_bytes.len()];
    header_buf.copy_from_slice(header_bytes);
    let header: byreal_clmm::states::DynTickArrayState = bytemuck::pod_read_unaligned(&header_buf);

    let ticks_bytes = &data[byreal_clmm::states::DynTickArrayState::HEADER_LEN..];
    let tick_size = size_of::<TickState>();
    if tick_size == 0 || !ticks_bytes.len().is_multiple_of(tick_size) {
      return None;
    }
    let mut ticks = Vec::with_capacity(ticks_bytes.len() / tick_size);
    for chunk in ticks_bytes.chunks_exact(tick_size) {
      ticks.push(bytemuck::pod_read_unaligned::<TickState>(chunk));
    }

    Some((header, ticks))
  }

  fn decode_fixed_tick_array(data: &[u8]) -> Option<TickArrayState> {
    let mut slice: &[u8] = data;
    <TickArrayState as anchor_lang::AccountDeserialize>::try_deserialize(&mut slice).ok()
  }

  pub fn from_account_data(data: &[u8]) -> Option<Self> {
    if data.len() < 8 {
      return None;
    }

    let discriminator = &data[0..8];
    if discriminator == byreal_clmm::states::DynTickArrayState::DISCRIMINATOR {
      Self::decode_dyn_tick_array(data)
        .map(|(header, ticks)| Self::Dynamic(Box::new((header, ticks))))
    } else if discriminator == TickArrayState::DISCRIMINATOR {
      Self::decode_fixed_tick_array(data).map(|ta| Self::Fixed(Box::new(ta)))
    } else {
      None
    }
  }

  pub fn next_initialized_tick(
    &self,
    cur_tick: i32,
    spacing: u16,
    zero_for_one: bool,
  ) -> Option<i32> {
    match self {
      DynamicTickArrayState::Dynamic(inner) => {
        let (header, ticks) = inner.as_ref();
        if let Ok(Some(local_idx)) =
          header.next_initialized_tick_index(ticks, cur_tick, spacing, zero_for_one)
        {
          Some(ticks[local_idx as usize].tick)
        } else {
          None
        }
      }
      DynamicTickArrayState::Fixed(ta) => {
        let mut ta = ta.clone();
        ta.next_initialized_tick(cur_tick, spacing, zero_for_one).ok().flatten().map(|ts| ts.tick)
      }
    }
  }

  pub fn first_initialized_tick(&self, zero_for_one: bool) -> Option<i32> {
    match self {
      DynamicTickArrayState::Dynamic(inner) => {
        let (header, ticks) = inner.as_ref();
        header
          .first_initialized_tick_index(ticks, zero_for_one)
          .ok()
          .map(|local_idx| ticks[local_idx as usize].tick)
      }
      DynamicTickArrayState::Fixed(ta) => {
        let mut ta = ta.clone();
        ta.first_initialized_tick(zero_for_one).ok().map(|ts| ts.tick)
      }
    }
  }

  pub fn get_tick_liquidity_net(&self, tick_index: i32, spacing: u16) -> Option<i128> {
    match self {
      DynamicTickArrayState::Dynamic(inner) => {
        let (header, ticks) = inner.as_ref();
        header
          .get_tick_index_in_array(tick_index, spacing)
          .ok()
          .map(|i| ticks[i as usize].liquidity_net)
      }
      DynamicTickArrayState::Fixed(ta) => ta
        .get_tick_offset_in_array(tick_index, spacing)
        .ok()
        .map(|offset| ta.ticks[offset].liquidity_net),
    }
  }
}

#[derive(Debug)]
struct TickNavState {
  is_match_pool_current_tick_array: bool,
  current_valid_tick_array_start_index: i32,
}

#[derive(Debug)]
struct SwapState {
  amount_specified_remaining: u64,
  amount_calculated: u64,
  sqrt_price_x64: u128,
  tick: i32,
  liquidity: u128,
  fee_amount: u64,
}

#[derive(Debug)]
struct SwapResult {
  amount_in: u64,
  amount_out: u64,
}

#[derive(Clone)]
pub struct ByrealClmmVenue {
  market_id: Pubkey,
  program_id: Pubkey,
  reserve_mints: [Pubkey; 2],

  pool_state: PoolState,
  amm_config: Option<AmmConfig>,
  bitmap_extension: Option<TickArrayBitmapExtension>,
  dynamic_tick_arrays: HashMap<Pubkey, DynamicTickArrayState>,
  token0_vault_amount: u64,
  token1_vault_amount: u64,
  token0_program_id: Pubkey,
  token1_program_id: Pubkey,
  token0_pyth_oracle_data: Option<Price>,
  token1_pyth_oracle_data: Option<Price>,

  max_one_side_tick_arrays: usize,
  current_unix_timestamp: i64,
}

impl ByrealClmmVenue {
  pub fn from_pool_state_account(
    market_id: Pubkey,
    owner: Pubkey,
    pool_state_data: &[u8],
  ) -> Result<Self, TradingVenueError> {
    let mut slice: &[u8] = pool_state_data;
    let pool_state = <PoolState as anchor_lang::AccountDeserialize>::try_deserialize(&mut slice)
      .map_err(|e| {
        TradingVenueError::DeserializationFailed(ErrorInfo::String(format!(
          "decode PoolState failed: {e}"
        )))
      })?;

    Ok(Self {
      market_id,
      program_id: owner,
      reserve_mints: [pool_state.token_mint_0, pool_state.token_mint_1],
      pool_state,
      amm_config: None,
      bitmap_extension: None,
      dynamic_tick_arrays: HashMap::new(),
      token0_vault_amount: 0,
      token1_vault_amount: 0,
      token0_program_id: ZERO_PUBKEY,
      token1_program_id: ZERO_PUBKEY,
      token0_pyth_oracle_data: None,
      token1_pyth_oracle_data: None,
      max_one_side_tick_arrays: 6,
      current_unix_timestamp: 0,
    })
  }

  fn program_pubkey(&self) -> Pubkey {
    self.program_id
  }

  fn market_pubkey(&self) -> Pubkey {
    self.market_id
  }

  fn load_clock(&mut self, accounts: &HashMap<Pubkey, Account>) -> Result<(), TradingVenueError> {
    let key = sysvar::clock::ID;
    let Some(acc) = accounts.get(&key) else {
      return Err(TradingVenueError::MissingState("missing sysvar clock".into()));
    };
    let clock: clock::Clock = bincode::deserialize(&acc.data).map_err(|e| {
      TradingVenueError::DeserializationFailed(ErrorInfo::String(format!(
        "decode sysvar clock failed: {e}"
      )))
    })?;
    self.current_unix_timestamp = clock.unix_timestamp;
    Ok(())
  }

  fn decode_vault_amount(
    vault_owner: Pubkey,
    vault_data: &[u8],
    vault_name: &str,
  ) -> Result<u64, TradingVenueError> {
    if vault_owner == spl_token::ID {
      let account = spl_token::state::Account::unpack(vault_data).map_err(|e| {
        TradingVenueError::DeserializationFailed(ErrorInfo::String(format!(
          "decode {vault_name} failed: {e}"
        )))
      })?;
      return Ok(account.amount);
    }

    if vault_owner == spl_token_2022::ID {
      let account = StateWithExtensions::<Token2022Account>::unpack(vault_data).map_err(|e| {
        TradingVenueError::DeserializationFailed(ErrorInfo::String(format!(
          "decode {vault_name} failed: {e}"
        )))
      })?;
      return Ok(account.base.amount);
    }

    Err(TradingVenueError::DeserializationFailed(ErrorInfo::String(
      format!(
        "decode {vault_name} failed: unsupported token program owner {}",
        vault_owner
      ),
    )))
  }

  fn decode_pyth_price(
    account_data: &[u8],
    expected_feed_id: &[u8; 32],
  ) -> Result<Price, TradingVenueError> {
    let mut slice: &[u8] = account_data;
    let update =
      <PriceUpdateV2 as anchor_lang::AccountDeserialize>::try_deserialize(&mut slice).map_err(
        |e| {
          TradingVenueError::DeserializationFailed(ErrorInfo::String(format!(
            "decode pyth oracle failed: {e}"
          )))
        },
      )?;
    update
      .get_price_unchecked(expected_feed_id)
      .map_err(|_| TradingVenueError::MissingState("pyth feed id mismatch".into()))
  }

  fn tick_array_bitmap_extension_key(&self) -> Pubkey {
    Pubkey::find_program_address(
      &[POOL_TICK_ARRAY_BITMAP_SEED.as_bytes(), self.market_pubkey().as_ref()],
      &self.program_pubkey(),
    )
    .0
  }

  pub fn token_mints(&self) -> [Pubkey; 2] {
    [self.pool_state.token_mint_0, self.pool_state.token_mint_1]
  }

  pub fn mint_decimals(&self) -> [u8; 2] {
    [
      self.pool_state.mint_decimals_0,
      self.pool_state.mint_decimals_1,
    ]
  }

  fn get_price_feed_account_address(shard_id: u16, price_feed_id: &[u8; 32]) -> Pubkey {
    let shard_bytes = shard_id.to_le_bytes();
    Pubkey::find_program_address(&[&shard_bytes, price_feed_id], &PYTH_PRICE_FEED_PROGRAM_ID).0
  }

  fn token0_pyth_oracle_pubkey(&self) -> Option<Pubkey> {
    (self.pool_state.token0_pyth_feed_id != [0u8; 32])
      .then_some(Self::get_price_feed_account_address(0, &self.pool_state.token0_pyth_feed_id))
  }

  fn token1_pyth_oracle_pubkey(&self) -> Option<Pubkey> {
    (self.pool_state.token1_pyth_feed_id != [0u8; 32])
      .then_some(Self::get_price_feed_account_address(0, &self.pool_state.token1_pyth_feed_id))
  }

  fn is_swap_dynamic_fee_enabled(&self) -> bool {
    self.pool_state.is_swap_dynamic_fee_enabled()
  }

  fn init_tick_nav_state(&self, zero_for_one: bool) -> Result<TickNavState, anyhow::Error> {
    let (is_match, first_start) =
      self.pool_state.get_first_initialized_tick_array(&self.bitmap_extension, zero_for_one)?;
    Ok(TickNavState {
      is_match_pool_current_tick_array: is_match,
      current_valid_tick_array_start_index: first_start,
    })
  }

  fn get_tick_array_address(&self, start_index: i32) -> Pubkey {
    Pubkey::find_program_address(
      &[TICK_ARRAY_SEED.as_bytes(), self.market_pubkey().as_ref(), &start_index.to_be_bytes()],
      &self.program_pubkey(),
    )
    .0
  }

  fn get_all_tick_array_addresses(&self) -> Vec<Pubkey> {
    let mut start_indexes: BTreeSet<i32> = BTreeSet::new();

    let mut collect_dir = |zero_for_one: bool, limit: usize| {
      if limit == 0 {
        return;
      }
      if let Ok((_, mut start)) =
        self.pool_state.get_first_initialized_tick_array(&self.bitmap_extension, zero_for_one)
      {
        start_indexes.insert(start);
        for _ in 1..limit {
          match self.pool_state.next_initialized_tick_array_start_index(
            &self.bitmap_extension,
            start,
            zero_for_one,
          ) {
            Ok(Some(next)) => {
              start_indexes.insert(next);
              start = next;
            }
            _ => break,
          }
        }
      }
    };

    let overflow_default =
      self.pool_state.is_overflow_default_tickarray_bitmap(vec![self.pool_state.tick_current]);
    let can_use_bitmap_helpers = self.bitmap_extension.is_some() || !overflow_default;
    if can_use_bitmap_helpers {
      collect_dir(true, self.max_one_side_tick_arrays);
      collect_dir(false, self.max_one_side_tick_arrays);
    }

    if start_indexes.is_empty() {
      let tick_spacing = self.pool_state.tick_spacing;
      let current_tick = self.pool_state.tick_current;
      let current_start_index = TickUtils::get_array_start_index(current_tick, tick_spacing);
      start_indexes.insert(current_start_index);
      for i in 1..self.max_one_side_tick_arrays {
        let offset = (byreal_clmm::states::TICK_ARRAY_SIZE * i as i32) * i32::from(tick_spacing);
        start_indexes.insert(current_start_index.saturating_sub(offset));
      }
    }

    start_indexes.into_iter().map(|s| self.get_tick_array_address(s)).collect()
  }

  fn get_swap_tick_arrays(&self, zero_for_one: bool) -> Vec<Pubkey> {
    let mut addrs = Vec::new();

    if let Ok((_, first_start)) =
      self.pool_state.get_first_initialized_tick_array(&self.bitmap_extension, zero_for_one)
    {
      addrs.push(self.get_tick_array_address(first_start));
      let mut current = first_start;
      for _ in 1..self.max_one_side_tick_arrays {
        match self.pool_state.next_initialized_tick_array_start_index(
          &self.bitmap_extension,
          current,
          zero_for_one,
        ) {
          Ok(Some(next)) => {
            addrs.push(self.get_tick_array_address(next));
            current = next;
          }
          _ => break,
        }
      }
      return addrs;
    }

    let tick_spacing = self.pool_state.tick_spacing;
    let current_tick = self.pool_state.tick_current;
    let current_start_index = TickUtils::get_array_start_index(current_tick, tick_spacing);
    addrs.push(self.get_tick_array_address(current_start_index));
    for i in 1..self.max_one_side_tick_arrays {
      let offset = (byreal_clmm::states::TICK_ARRAY_SIZE * i as i32) * i32::from(tick_spacing);
      let start_index = if zero_for_one {
        current_start_index.saturating_sub(offset)
      } else {
        current_start_index.saturating_add(offset)
      };
      addrs.push(self.get_tick_array_address(start_index));
    }
    addrs
  }

  fn live_directional_tick_arrays(
    &self,
    zero_for_one: bool,
  ) -> Result<Vec<Pubkey>, TradingVenueError> {
    let mut live = Vec::new();
    for tick_array in self.get_swap_tick_arrays(zero_for_one) {
      if self.dynamic_tick_arrays.contains_key(&tick_array) {
        live.push(tick_array);
      } else if live.is_empty() {
        return Err(TradingVenueError::MissingState(
          format!("directional first tick array account missing for swap: {tick_array}").into(),
        ));
      }
    }

    if live.is_empty() {
      return Err(TradingVenueError::MissingState(
        "no directional tick array accounts available; call update_state() with tick arrays".into(),
      ));
    }
    Ok(live)
  }

  fn find_next_initialized_tick_with_nav(
    &self,
    current_tick: i32,
    zero_for_one: bool,
    nav: &mut TickNavState,
  ) -> Result<i32, anyhow::Error> {
    let spacing = self.pool_state.tick_spacing;

    loop {
      let start_index = nav.current_valid_tick_array_start_index;
      let addr = self.get_tick_array_address(start_index);
      let tick_array = self
        .dynamic_tick_arrays
        .get(&addr)
        .ok_or_else(|| anyhow!("Missing tick array data for start_index {}", start_index))?;

      if let Some(t) = tick_array.next_initialized_tick(current_tick, spacing, zero_for_one) {
        return Ok(t);
      }

      if !nav.is_match_pool_current_tick_array {
        nav.is_match_pool_current_tick_array = true;
        if let Some(t) = tick_array.first_initialized_tick(zero_for_one) {
          return Ok(t);
        }
      }

      let next_arr = self.pool_state.next_initialized_tick_array_start_index(
        &self.bitmap_extension,
        nav.current_valid_tick_array_start_index,
        zero_for_one,
      )?;
      let Some(next_start) = next_arr else {
        return Err(anyhow!("Liquidity insufficient: no further initialized tick arrays"));
      };
      nav.current_valid_tick_array_start_index = next_start;

      let next_addr = self.get_tick_array_address(next_start);
      let next_tick_array = self.dynamic_tick_arrays.get(&next_addr).ok_or_else(|| {
        anyhow!("Missing tick array data for advanced start_index {}", next_start)
      })?;
      if let Some(t) = next_tick_array.first_initialized_tick(zero_for_one) {
        return Ok(t);
      }
    }
  }

  fn get_tick_liquidity_net(&self, tick_index: i32) -> Option<i128> {
    let spacing = self.pool_state.tick_spacing;
    let start = TickUtils::get_array_start_index(tick_index, spacing);
    let addr = self.get_tick_array_address(start);
    self.dynamic_tick_arrays.get(&addr)?.get_tick_liquidity_net(tick_index, spacing)
  }

  fn load_dynamic_pyth_prices(&self) -> Result<(Price, Price), anyhow::Error> {
    let token0_price = *self
      .token0_pyth_oracle_data
      .as_ref()
      .ok_or_else(|| anyhow!("dynamic fee pyth oracle accounts missing"))?;
    let token1_price = *self
      .token1_pyth_oracle_data
      .as_ref()
      .ok_or_else(|| anyhow!("dynamic fee pyth oracle accounts missing"))?;

    if token0_price.price <= 0 || token1_price.price <= 0 {
      return Err(anyhow!("pyth price is non-positive"));
    }

    let oldest_allowed = self.current_unix_timestamp - DYNAMIC_MAX_PYTH_AGE_SECONDS;
    if token0_price.publish_time < oldest_allowed || token1_price.publish_time < oldest_allowed {
      return Err(anyhow!("pyth price is stale"));
    }

    Ok((token0_price, token1_price))
  }

  fn compute_trade_fee_rate(
    &self,
    zero_for_one: bool,
    amount_specified: u64,
    current_timestamp: i64,
  ) -> Result<u32, anyhow::Error> {
    self.dynamic_fee_rate(zero_for_one, amount_specified, current_timestamp)
  }

  fn dynamic_fee_rate(
    &self,
    zero_for_one: bool,
    amount_specified: u64,
    current_timestamp: i64,
  ) -> Result<u32, anyhow::Error> {
    let amm_config = self.amm_config.as_ref().ok_or_else(|| anyhow!("AMM config not loaded"))?;
    let fee_rate =
      self.pool_state.calculate_base_trade_fee_rate(amm_config, zero_for_one, current_timestamp as u64)?;

    if !self.is_swap_dynamic_fee_enabled() {
      return Ok(fee_rate);
    }

    if self.token0_program_id == ZERO_PUBKEY || self.token1_program_id == ZERO_PUBKEY {
      return Err(anyhow!("dynamic fee vault accounts missing"));
    }

    let (token0_price, token1_price) = self.load_dynamic_pyth_prices()?;
    let p_index = calculate_price_index(
      &token0_price,
      &token1_price,
      self.pool_state.mint_decimals_0,
      self.pool_state.mint_decimals_1,
    )?;
    let p_0 = price_from_sqrt_price_x64(self.pool_state.sqrt_price_x64)?;

    let token1_as_quote = self.pool_state.is_token1_quote();
    let input_is_quote = if token1_as_quote { !zero_for_one } else { zero_for_one };
    let is_buying_base = input_is_quote;

    let quote_amount = if input_is_quote {
      amount_specified as u128
    } else {
      quote_amount_from_base(amount_specified as u128, p_0, token1_as_quote)?
    };

    let quote_decimals = if token1_as_quote {
      self.pool_state.mint_decimals_1
    } else {
      self.pool_state.mint_decimals_0
    };
    let trade_size = normalize_trade_size(quote_amount, quote_decimals)?;

    let token0_vault_amount = self.token0_vault_amount as u128;
    let token1_vault_amount = self.token1_vault_amount as u128;
    let (quote_value_of_base, quote_balance) = if token1_as_quote {
      let base_amount = token0_vault_amount;
      let quote_balance = token1_vault_amount;
      (quote_amount_from_base(base_amount, p_0, true)?, quote_balance)
    } else {
      let base_amount = token1_vault_amount;
      let quote_balance = token0_vault_amount;
      (quote_amount_from_base(base_amount, p_0, false)?, quote_balance)
    };

    let dynamic_fee_inputs = DynamicFeeInputs {
      p_0,
      p_index,
      trade_size,
      quote_value_of_base,
      quote_balance,
      is_buying_base,
      fee_base: fee_rate,
      arbitrage_fee_buffer_ppm: self.pool_state.arbitrage_fee_buffer_ppm,
      trade_slippage_fee_base_milli_bp: self.pool_state.trade_slippage_fee_base_milli_bp,
      trade_slippage_fee_trade_size_threshold: self
        .pool_state
        .trade_slippage_fee_trade_size_threshold,
      imbalance_fee_base_tenths_of_bp: self.pool_state.imbalance_fee_base_tenths_of_bp,
      imbalance_fee_x: self.pool_state.imbalance_fee_x,
    };

    let fee_result = calculate_dynamic_fee_rate(&dynamic_fee_inputs).map_err(|e| anyhow!(e))?;

    Ok(fee_result.total_fee_rate)
  }

  fn compute_swap(
    &self,
    zero_for_one: bool,
    amount_specified: u64,
    sqrt_price_limit_x64: Option<u128>,
    current_timestamp: i64,
  ) -> Result<SwapResult, anyhow::Error> {
    let sqrt_price_limit = sqrt_price_limit_x64.unwrap_or({
      if zero_for_one {
        MIN_SQRT_PRICE_X64 + 1
      } else {
        MAX_SQRT_PRICE_X64 - 1
      }
    });

    let (state, _) = self.run_swap(
      zero_for_one,
      amount_specified,
      sqrt_price_limit,
      current_timestamp,
    )?;

    let amount_in = amount_specified
      .checked_sub(state.amount_specified_remaining)
      .ok_or_else(|| anyhow!("compute_swap: raw input underflow"))?;
    if amount_in == 0 || state.amount_calculated == 0 {
      return Err(anyhow!("swap produced zero amount; chain would reject TooSmallInputOrOutputAmount"));
    }

    Ok(SwapResult { amount_in, amount_out: state.amount_calculated })
  }

  /// Resolve the trade fee rate and run the exact-in swap-step loop.
  fn run_swap(
    &self,
    zero_for_one: bool,
    amount_specified: u64,
    sqrt_price_limit: u128,
    current_timestamp: i64,
  ) -> Result<(SwapState, u32), anyhow::Error> {
    let final_fee = self.compute_trade_fee_rate(zero_for_one, amount_specified, current_timestamp)?;

    let state = self.simulate_swap_steps(
      final_fee,
      amount_specified,
      sqrt_price_limit,
      zero_for_one,
      current_timestamp,
    )?;
    Ok((state, final_fee))
  }

  /// Run the swap-step loop for a GIVEN fee_rate, returning the final swap state.
  fn simulate_swap_steps(
    &self,
    fee_rate: u32,
    amount_specified: u64,
    sqrt_price_limit: u128,
    zero_for_one: bool,
    current_timestamp: i64,
  ) -> Result<SwapState, anyhow::Error> {
    let mut state = SwapState {
      amount_specified_remaining: amount_specified,
      amount_calculated: 0,
      sqrt_price_x64: self.pool_state.sqrt_price_x64,
      tick: self.pool_state.tick_current,
      liquidity: self.pool_state.liquidity,
      fee_amount: 0,
    };

    let mut nav = self.init_tick_nav_state(zero_for_one)?;
    while state.amount_specified_remaining != 0 && state.sqrt_price_x64 != sqrt_price_limit {
      let next_tick = self.find_next_initialized_tick_with_nav(state.tick, zero_for_one, &mut nav)?;
      let sqrt_price_next = tick_math::get_sqrt_price_at_tick(next_tick)
        .map_err(|e| anyhow!("Failed to get sqrt price at tick {}: {}", next_tick, e))?;

      let target_price = if (zero_for_one && sqrt_price_next < sqrt_price_limit)
        || (!zero_for_one && sqrt_price_next > sqrt_price_limit)
      {
        sqrt_price_limit
      } else {
        sqrt_price_next
      };

      let step = swap_math::compute_swap_step(
        state.sqrt_price_x64,
        target_price,
        state.liquidity,
        state.amount_specified_remaining,
        fee_rate,
        true,
        zero_for_one,
        current_timestamp as u32,
      )
      .map_err(|e| anyhow!("Swap step computation failed: {:?}", e))?;

      state.sqrt_price_x64 = step.sqrt_price_next_x64;
      state.fee_amount = state
        .fee_amount
        .checked_add(step.fee_amount)
        .ok_or_else(|| anyhow!("compute_swap: fee_amount overflow"))?;

      let step_amount_in_with_fee = step
        .amount_in
        .checked_add(step.fee_amount)
        .ok_or_else(|| anyhow!("compute_swap: step.amount_in + fee_amount overflow"))?;
      state.amount_specified_remaining = state
        .amount_specified_remaining
        .checked_sub(step_amount_in_with_fee)
        .ok_or_else(|| {
          anyhow!("compute_swap: step.amount_in + fee_amount exceeds remaining")
        })?;
      state.amount_calculated =
        state.amount_calculated.checked_add(step.amount_out).ok_or_else(|| {
          anyhow!("compute_swap: amount_calculated overflow when adding amount_out")
        })?;

      if state.sqrt_price_x64 == sqrt_price_next {
        let mut liq_net = self
          .get_tick_liquidity_net(next_tick)
          .ok_or_else(|| anyhow!("Missing tick array data for tick {}", next_tick))?;
        if zero_for_one {
          liq_net = -liq_net;
        }
        state.liquidity = liquidity_math::add_delta(state.liquidity, liq_net)
          .map_err(|e| anyhow!("Failed to adjust liquidity at tick {}: {:?}", next_tick, e))?;
        state.tick = if zero_for_one { next_tick - 1 } else { next_tick };
      } else {
        state.tick = tick_math::get_tick_at_sqrt_price(state.sqrt_price_x64)
          .map_err(|e| anyhow!("Failed to get tick at sqrt price: {:?}", e))?;
      }
    }

    Ok(state)
  }

  fn validate_mints(
    &self,
    input: Pubkey,
    output: Pubkey,
  ) -> Result<(bool, bool), TradingVenueError> {
    let input_is_0 = input == self.reserve_mints[0];
    let input_is_1 = input == self.reserve_mints[1];
    if !input_is_0 && !input_is_1 {
      return Err(TradingVenueError::InvalidMint("input mint not part of pool".into()));
    }
    let output_is_0 = output == self.reserve_mints[0];
    let output_is_1 = output == self.reserve_mints[1];
    if !output_is_0 && !output_is_1 {
      return Err(TradingVenueError::InvalidMint("output mint not part of pool".into()));
    }
    if input == output {
      return Err(TradingVenueError::InvalidMint("input/output mint must differ".into()));
    }
    Ok((input_is_0, output_is_0))
  }

  fn ensure_swap_status_enabled(&self) -> Result<(), TradingVenueError> {
    if self.pool_state.get_status_by_bit(PoolStatusBitIndex::Swap) {
      return Ok(());
    }

    Err(TradingVenueError::InactivePoolError(
      self.market_id,
      PoolProtocol::ByrealClmm,
    ))
  }
}

impl ByrealClmmVenue {
  pub fn program_id(&self) -> Pubkey {
    self.program_id
  }

  pub fn market_id(&self) -> Pubkey {
    self.market_id
  }

  pub fn accounts_to_update(&self) -> Vec<Pubkey> {
    let mut out = Vec::new();
    out.push(self.market_id);
    out.push(self.pool_state.amm_config);
    out.push(self.pool_state.token_mint_0);
    out.push(self.pool_state.token_mint_1);
    out.push(self.tick_array_bitmap_extension_key());
    out.extend(self.get_all_tick_array_addresses());
    if self.is_swap_dynamic_fee_enabled() {
      out.push(self.pool_state.token_vault_0);
      out.push(self.pool_state.token_vault_1);
      if let Some(pyth0) = self.token0_pyth_oracle_pubkey() {
        out.push(pyth0);
      }
      if let Some(pyth1) = self.token1_pyth_oracle_pubkey() {
        out.push(pyth1);
      }
    }
    out.push(sysvar::clock::ID);
    out
  }

  pub fn base_accounts_to_update(&self) -> Vec<Pubkey> {
    let mut out = vec![
      self.market_id,
      self.pool_state.amm_config,
      self.pool_state.token_mint_0,
      self.pool_state.token_mint_1,
      self.tick_array_bitmap_extension_key(),
      sysvar::clock::ID,
    ];
    if self.is_swap_dynamic_fee_enabled() {
      out.push(self.pool_state.token_vault_0);
      out.push(self.pool_state.token_vault_1);
      if let Some(pyth0) = self.token0_pyth_oracle_pubkey() {
        out.push(pyth0);
      }
      if let Some(pyth1) = self.token1_pyth_oracle_pubkey() {
        out.push(pyth1);
      }
    }
    out
  }

  pub fn update_state(
    &mut self,
    accounts: &HashMap<Pubkey, Account>,
  ) -> Result<(), TradingVenueError> {
    self.load_clock(accounts)?;

    if let Some(pool_acc) = accounts.get(&self.market_id) {
      if pool_acc.owner != self.program_id {
        return Err(TradingVenueError::UnsupportedVenue(
          format!(
            "Byreal CLMM pool owner must remain {}, got {}",
            self.program_id, pool_acc.owner
          )
          .into(),
        ));
      }
      let mut slice: &[u8] = &pool_acc.data;
      self.pool_state = <PoolState as anchor_lang::AccountDeserialize>::try_deserialize(&mut slice)
        .map_err(|e| {
          TradingVenueError::DeserializationFailed(ErrorInfo::String(format!(
            "decode PoolState failed: {e}"
          )))
        })?;
      self.reserve_mints = [self.pool_state.token_mint_0, self.pool_state.token_mint_1];
    }

    let amm_key = self.pool_state.amm_config;
    if let Some(amm_acc) = accounts.get(&amm_key) {
      let mut slice: &[u8] = &amm_acc.data;
      let cfg = <AmmConfig as anchor_lang::AccountDeserialize>::try_deserialize(&mut slice)
        .map_err(|e| {
          TradingVenueError::DeserializationFailed(ErrorInfo::String(format!(
            "decode AmmConfig failed: {e}"
          )))
        })?;
      self.amm_config = Some(cfg);
    } else {
      self.amm_config = None;
    }

    let vault0_key = self.pool_state.token_vault_0;
    if let Some(vault0_acc) = accounts.get(&vault0_key) {
      self.token0_vault_amount =
        Self::decode_vault_amount(vault0_acc.owner, &vault0_acc.data, "token0 vault")?;
      self.token0_program_id = vault0_acc.owner;
    } else {
      self.token0_vault_amount = 0;
      self.token0_program_id = ZERO_PUBKEY;
    }

    let vault1_key = self.pool_state.token_vault_1;
    if let Some(vault1_acc) = accounts.get(&vault1_key) {
      self.token1_vault_amount =
        Self::decode_vault_amount(vault1_acc.owner, &vault1_acc.data, "token1 vault")?;
      self.token1_program_id = vault1_acc.owner;
    } else {
      self.token1_vault_amount = 0;
      self.token1_program_id = ZERO_PUBKEY;
    }

    if self.is_swap_dynamic_fee_enabled() {
      if self.token0_program_id == ZERO_PUBKEY || self.token1_program_id == ZERO_PUBKEY {
        return Err(TradingVenueError::MissingState(
          "missing dynamic fee vault accounts; call update_state() with required accounts".into(),
        ));
      }

      let Some(pyth0_key) = self.token0_pyth_oracle_pubkey() else {
        return Err(TradingVenueError::MissingState(
          "token0 pyth feed id is not configured".into(),
        ));
      };
      let Some(pyth1_key) = self.token1_pyth_oracle_pubkey() else {
        return Err(TradingVenueError::MissingState(
          "token1 pyth feed id is not configured".into(),
        ));
      };

      let Some(pyth0_acc) = accounts.get(&pyth0_key) else {
        return Err(TradingVenueError::MissingState(
          "missing token0 pyth oracle; call update_state() with required accounts".into(),
        ));
      };
      if pyth0_acc.owner != PYTH_RECEIVER_PROGRAM_ID {
        return Err(TradingVenueError::MissingState("token0 pyth oracle owner mismatch".into()));
      }
      let pyth0 = Self::decode_pyth_price(&pyth0_acc.data, &self.pool_state.token0_pyth_feed_id)?;
      self.token0_pyth_oracle_data = Some(pyth0);

      let Some(pyth1_acc) = accounts.get(&pyth1_key) else {
        return Err(TradingVenueError::MissingState(
          "missing token1 pyth oracle; call update_state() with required accounts".into(),
        ));
      };
      if pyth1_acc.owner != PYTH_RECEIVER_PROGRAM_ID {
        return Err(TradingVenueError::MissingState("token1 pyth oracle owner mismatch".into()));
      }
      let pyth1 = Self::decode_pyth_price(&pyth1_acc.data, &self.pool_state.token1_pyth_feed_id)?;
      self.token1_pyth_oracle_data = Some(pyth1);
    } else {
      self.token0_pyth_oracle_data = None;
      self.token1_pyth_oracle_data = None;
    }

    let bitmap_key = self.tick_array_bitmap_extension_key();
    self.bitmap_extension = accounts.get(&bitmap_key).and_then(|acc| {
      let mut slice: &[u8] = &acc.data;
      <TickArrayBitmapExtension as anchor_lang::AccountDeserialize>::try_deserialize(&mut slice)
        .ok()
    });

    self.dynamic_tick_arrays.clear();
    for tick_key in self.get_all_tick_array_addresses() {
      let Some(acc) = accounts.get(&tick_key) else {
        continue;
      };
      if let Some(parsed) = DynamicTickArrayState::from_account_data(&acc.data) {
        self.dynamic_tick_arrays.insert(tick_key, parsed);
      }
    }

    Ok(())
  }

  pub fn quote(&self, request: QuoteRequest) -> Result<CoreQuoteResult, TradingVenueError> {
    if request.amount == 0 {
      return Ok(CoreQuoteResult {
        in_amount: 0,
        out_amount: 0,
        not_enough_liquidity: false,
      });
    }

    let (input_is_0, _output_is_0) =
      self.validate_mints(request.input_mint, request.output_mint)?;
    let zero_for_one = input_is_0;

    if self.current_unix_timestamp <= 0 {
      return Err(TradingVenueError::MissingState(
        "missing clock; call update_state() with sysvar clock".into(),
      ));
    }
    if self.current_unix_timestamp as u64 <= self.pool_state.open_time {
      return Err(TradingVenueError::MissingState("pool is not open yet".into()));
    }
    self.ensure_swap_status_enabled()?;
    if self.amm_config.is_none() {
      return Err(TradingVenueError::MissingState(
        "amm config missing; call update_state()".into(),
      ));
    }
    if self.is_swap_dynamic_fee_enabled() {
      if self.token0_program_id == ZERO_PUBKEY || self.token1_program_id == ZERO_PUBKEY {
        return Err(TradingVenueError::MissingState(
          "dynamic fee vault accounts missing; call update_state() with required accounts".into(),
        ));
      }
      if let Err(e) = self.load_dynamic_pyth_prices() {
        return Err(TradingVenueError::MissingState(ErrorInfo::String(format!(
          "dynamic pyth state invalid: {e}"
        ))));
      }
    }
    if self.dynamic_tick_arrays.is_empty() {
      return Err(TradingVenueError::MissingState(
        "tick arrays missing; call update_state() with required tick accounts".into(),
      ));
    }

    let swap = match self.compute_swap(
      zero_for_one,
      request.amount,
      None,
      self.current_unix_timestamp,
    ) {
      Ok(v) => v,
      Err(e) => {
        let msg = e.to_string();
        if msg.contains("Liquidity insufficient") {
          return Ok(CoreQuoteResult {
            in_amount: 0,
            out_amount: 0,
            not_enough_liquidity: true,
          });
        }
        return Err(if msg.contains("dynamic fee") || msg.contains("pyth") || msg.contains("vault") {
          TradingVenueError::MissingState(ErrorInfo::String(format!("compute_swap failed: {e}")))
        } else {
          TradingVenueError::MathError(ErrorInfo::String(format!("compute_swap failed: {e}")))
        });
      }
    };

    Ok(CoreQuoteResult {
      in_amount: swap.amount_in,
      out_amount: swap.amount_out,
      not_enough_liquidity: false,
    })
  }

  pub fn build_swap_instruction(
    &self,
    request: SwapBuildRequest,
  ) -> Result<Instruction, TradingVenueError> {
    if self.current_unix_timestamp <= 0 {
      return Err(TradingVenueError::MissingState(
        "missing clock; call update_state() with sysvar clock".into(),
      ));
    }
    if self.current_unix_timestamp as u64 <= self.pool_state.open_time {
      return Err(TradingVenueError::MissingState("pool is not open yet".into()));
    }
    self.ensure_swap_status_enabled()?;

    let (input_is_0, _output_is_0) =
      self.validate_mints(request.source_mint, request.destination_mint)?;
    let zero_for_one = input_is_0;

    let (input_vault, output_vault, input_vault_mint, output_vault_mint) = if zero_for_one {
      (
        self.pool_state.token_vault_0,
        self.pool_state.token_vault_1,
        self.pool_state.token_mint_0,
        self.pool_state.token_mint_1,
      )
    } else {
      (
        self.pool_state.token_vault_1,
        self.pool_state.token_vault_0,
        self.pool_state.token_mint_1,
        self.pool_state.token_mint_0,
      )
    };

    let amount = request.amount;
    let other_amount_threshold = request.other_amount_threshold;
    let sqrt_price_limit_x64: u128 = 0;
    // Always emit swap_v3_dyn. On-chain will fallback to v2 behavior when
    // dynamic fee is not enabled for the pool.
    let use_swap_v3_dyn = true;
    let dynamic_fee_enabled = self.is_swap_dynamic_fee_enabled();
    let data = byreal_clmm::instruction::SwapV3Dyn {
      amount,
      other_amount_threshold,
      sqrt_price_limit_x64,
      is_base_input: true,
    }
    .data();

    let mut accounts = vec![
      AccountMeta::new_readonly(request.user_authority, true),
      AccountMeta::new_readonly(self.pool_state.amm_config, false),
      AccountMeta::new(self.market_id, false),
      AccountMeta::new(request.source_token_account, false),
      AccountMeta::new(request.destination_token_account, false),
      AccountMeta::new(input_vault, false),
      AccountMeta::new(output_vault, false),
      AccountMeta::new(self.pool_state.observation_key, false),
      AccountMeta::new_readonly(spl_token::ID, false),
      AccountMeta::new_readonly(spl_token_2022::ID, false),
      AccountMeta::new_readonly(spl_memo::ID, false),
      AccountMeta::new_readonly(input_vault_mint, false),
      AccountMeta::new_readonly(output_vault_mint, false),
    ];

    let bitmap_key = self.tick_array_bitmap_extension_key();
    accounts.push(AccountMeta::new_readonly(bitmap_key, false));

    let live = self.live_directional_tick_arrays(zero_for_one)?;

    for tick_array in live {
      accounts.push(AccountMeta::new(tick_array, false));
    }

    if use_swap_v3_dyn && dynamic_fee_enabled {
      let token0_pyth = self.token0_pyth_oracle_pubkey().ok_or_else(|| {
        TradingVenueError::MissingState("token0 pyth feed id is not configured".into())
      })?;
      let token1_pyth = self.token1_pyth_oracle_pubkey().ok_or_else(|| {
        TradingVenueError::MissingState("token1 pyth feed id is not configured".into())
      })?;
      accounts.push(AccountMeta::new_readonly(token0_pyth, false));
      accounts.push(AccountMeta::new_readonly(token1_pyth, false));
    }

    Ok(Instruction { program_id: self.program_id, accounts, data })
  }
}

#[cfg(test)]
mod tests {
  use {
    super::{ByrealClmmVenue, DynamicTickArrayState, SwapBuildRequest, ZERO_PUBKEY},
    anchor_lang::{
      prelude::Pubkey,
      solana_program::{clock, sysvar},
      Discriminator,
    },
    byreal_clmm::states::{PoolState, PoolStatusBitIndex, TickUtils},
    crate::trading_venue::{error::TradingVenueError, QuoteRequest, SwapType},
    solana_account::Account,
    std::collections::HashMap,
  };

  fn clock_account(unix_timestamp: i64) -> Account {
    let clock = clock::Clock {
      slot: 1,
      epoch_start_timestamp: 0,
      epoch: 0,
      leader_schedule_epoch: 0,
      unix_timestamp,
    };
    Account {
      lamports: 0,
      data: bincode::serialize(&clock).unwrap(),
      owner: ZERO_PUBKEY,
      executable: false,
      rent_epoch: 0,
    }
  }

  fn venue_for_pool_state(pool_state: PoolState) -> ByrealClmmVenue {
    ByrealClmmVenue {
      market_id: Pubkey::new_unique(),
      program_id: Pubkey::new_unique(),
      reserve_mints: [pool_state.token_mint_0, pool_state.token_mint_1],
      pool_state,
      amm_config: None,
      bitmap_extension: None,
      dynamic_tick_arrays: HashMap::new(),
      token0_vault_amount: 0,
      token1_vault_amount: 0,
      token0_program_id: ZERO_PUBKEY,
      token1_program_id: ZERO_PUBKEY,
      token0_pyth_oracle_data: None,
      token1_pyth_oracle_data: None,
      max_one_side_tick_arrays: 6,
      current_unix_timestamp: 0,
    }
  }

  #[test]
  fn dyn_tick_array_decode_unaligned_does_not_panic() {
    let mut data = vec![0u8; byreal_clmm::states::DynTickArrayState::HEADER_LEN];
    data[0..8].copy_from_slice(byreal_clmm::states::DynTickArrayState::DISCRIMINATOR);

    let res = std::panic::catch_unwind(|| DynamicTickArrayState::from_account_data(&data));
    assert!(res.is_ok());
  }

  #[test]
  fn clmm_vault_accounts_are_only_base_requirements_for_dynamic_fee() {
    let pool_state = PoolState {
      amm_config: Pubkey::new_unique(),
      token_vault_0: Pubkey::new_unique(),
      token_vault_1: Pubkey::new_unique(),
      tick_spacing: 60,
      ..Default::default()
    };
    let vault0 = pool_state.token_vault_0;
    let vault1 = pool_state.token_vault_1;

    let mut venue = ByrealClmmVenue {
      market_id: Pubkey::new_unique(),
      program_id: Pubkey::new_unique(),
      reserve_mints: [Pubkey::new_unique(), Pubkey::new_unique()],
      pool_state,
      amm_config: None,
      bitmap_extension: None,
      dynamic_tick_arrays: HashMap::new(),
      token0_vault_amount: 0,
      token1_vault_amount: 0,
      token0_program_id: ZERO_PUBKEY,
      token1_program_id: ZERO_PUBKEY,
      token0_pyth_oracle_data: None,
      token1_pyth_oracle_data: None,
      max_one_side_tick_arrays: 6,
      current_unix_timestamp: 0,
    };

    let non_dynamic_keys = venue.base_accounts_to_update();
    assert!(!non_dynamic_keys.contains(&vault0));
    assert!(!non_dynamic_keys.contains(&vault1));

    venue.pool_state.decay_fee_flag |= 1 << 4;
    let dynamic_keys = venue.base_accounts_to_update();
    assert!(dynamic_keys.contains(&vault0));
    assert!(dynamic_keys.contains(&vault1));
  }

  #[test]
  fn swap_tick_arrays_follow_directional_order() {
    let pool_state = PoolState {
      tick_spacing: 60,
      tick_current: 0,
      ..Default::default()
    };
    let mut venue = venue_for_pool_state(pool_state);
    venue.max_one_side_tick_arrays = 3;
    let current_start = TickUtils::get_array_start_index(
      venue.pool_state.tick_current,
      venue.pool_state.tick_spacing,
    );
    let offset = byreal_clmm::states::TICK_ARRAY_SIZE * i32::from(venue.pool_state.tick_spacing);

    let zero_for_one = venue.get_swap_tick_arrays(true);
    assert_eq!(zero_for_one[0], venue.get_tick_array_address(current_start));
    assert_eq!(
      zero_for_one[1],
      venue.get_tick_array_address(current_start.saturating_sub(offset))
    );

    let one_for_zero = venue.get_swap_tick_arrays(false);
    assert_eq!(one_for_zero[0], venue.get_tick_array_address(current_start));
    assert_eq!(
      one_for_zero[1],
      venue.get_tick_array_address(current_start.saturating_add(offset))
    );
  }

  #[test]
  fn dynamic_fee_update_state_requires_real_vault_accounts() {
    let pool_state = PoolState {
      amm_config: Pubkey::new_unique(),
      token_vault_0: Pubkey::new_unique(),
      token_vault_1: Pubkey::new_unique(),
      decay_fee_flag: 1 << 4,
      ..Default::default()
    };
    let mut venue = venue_for_pool_state(pool_state);
    let mut accounts = HashMap::new();
    accounts.insert(sysvar::clock::ID, clock_account(1_700_000_000));

    let err = venue.update_state(&accounts).expect_err("dynamic fee vaults are required");

    assert!(matches!(err, TradingVenueError::MissingState(_)));
    assert!(err.to_string().contains("missing dynamic fee vault accounts"));
  }

  #[test]
  fn quote_rejects_swap_disabled_pool() {
    let input_mint = Pubkey::new_unique();
    let output_mint = Pubkey::new_unique();
    let pool_state = PoolState {
      token_mint_0: input_mint,
      token_mint_1: output_mint,
      status: 1 << (PoolStatusBitIndex::Swap as u8),
      ..Default::default()
    };
    let mut venue = venue_for_pool_state(pool_state);
    venue.current_unix_timestamp = 1_700_000_000;

    let err = venue
      .quote(QuoteRequest {
        input_mint,
        output_mint,
        amount: 1,
        swap_type: SwapType::ExactIn,
      })
      .expect_err("swap-disabled pools must not quote");

    assert!(matches!(err, TradingVenueError::InactivePoolError(_, _)));
  }

  #[test]
  fn build_swap_instruction_rejects_swap_disabled_pool() {
    let input_mint = Pubkey::new_unique();
    let output_mint = Pubkey::new_unique();
    let pool_state = PoolState {
      token_mint_0: input_mint,
      token_mint_1: output_mint,
      status: 1 << (PoolStatusBitIndex::Swap as u8),
      ..Default::default()
    };
    let mut venue = venue_for_pool_state(pool_state);
    venue.current_unix_timestamp = 1_700_000_000;

    let err = venue
      .build_swap_instruction(SwapBuildRequest {
        user_authority: Pubkey::new_unique(),
        source_mint: input_mint,
        destination_mint: output_mint,
        source_token_account: Pubkey::new_unique(),
        destination_token_account: Pubkey::new_unique(),
        amount: 1,
        other_amount_threshold: 0,
      })
      .expect_err("swap-disabled pools must not build swap instructions");

    assert!(matches!(err, TradingVenueError::InactivePoolError(_, _)));
  }

  #[test]
  fn update_state_rejects_pool_owner_drift() {
    let pool_state = PoolState {
      amm_config: Pubkey::new_unique(),
      tick_spacing: 60,
      ..Default::default()
    };
    let mut venue = venue_for_pool_state(pool_state);
    let original_program_id = venue.program_id;
    let mut accounts = HashMap::new();
    accounts.insert(sysvar::clock::ID, clock_account(1_700_000_000));
    accounts.insert(
      venue.market_id,
      Account {
        lamports: 0,
        data: Vec::new(),
        owner: Pubkey::new_unique(),
        executable: false,
        rent_epoch: 0,
      },
    );

    let err = venue.update_state(&accounts).expect_err("pool owner drift must be rejected");

    assert!(matches!(err, TradingVenueError::UnsupportedVenue(_)));
    assert_eq!(venue.program_id, original_program_id);
  }

  #[test]
  fn non_dynamic_update_state_does_not_require_vault_accounts() {
    let pool_state = PoolState {
      amm_config: Pubkey::new_unique(),
      token_vault_0: Pubkey::new_unique(),
      token_vault_1: Pubkey::new_unique(),
      tick_spacing: 60,
      ..Default::default()
    };
    let mut venue = venue_for_pool_state(pool_state);
    let mut accounts = HashMap::new();
    accounts.insert(sysvar::clock::ID, clock_account(1_700_000_000));

    venue.update_state(&accounts).expect("non-dynamic pools do not require vault accounts");
  }

}
