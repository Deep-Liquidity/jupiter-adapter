//! Jupiter AMM adapter for DeepSwap (`deep-amm`), DEEP's constant-product AMM: an
//! Apache-2.0 fork of Raydium cp-swap deployed at
//! `HCrCy6bzHhZ1b6bXwQAucEFkKXyzYMh3hgAR8UPrYSEP`.
//!
//! The adapter is pure: no network calls, all decoding happens in [`Amm::update`], and
//! quotes are integer-exact ports of the on-chain `swap_base_input` handler
//! (`programs/deep-amm/src/instructions/swap_base_input.rs`). ExactIn only.
//!
//! Two fee models, chosen per pool by `PoolState::fee_model` (which must equal its
//! `AmmConfig::fee_model`): 0 = legacy, upstream cp-swap's fees ([`math::swap_base_input`]);
//! 1 = DEEP V1, side-dependent fees taken in the pool's quote token
//! ([`math::swap_base_input_v1`]).

pub mod math;
pub mod state;
pub mod token;

use jupiter_amm_interface::{
    AccountProvider, Amm, AmmContext, AmmError, AmmLabel, ClockRef, KeyedAccount, Quote,
    QuoteParams, SingleProgramAmm, Swap, SwapAndAccountMetas, SwapMode, SwapParams,
    single_program_amm,
};
use rust_decimal::Decimal;
use solana_account::ReadableAccount;
use solana_instruction::AccountMeta;
use solana_pubkey::{Pubkey, pubkey};
use std::sync::atomic::Ordering;

use crate::state::{
    AmmConfig, FEE_MODEL_V1, PoolState, REWARD_MODEL_STANDARD, token_account_amount,
};
use crate::token::{MintSupport, TOKEN_2022_PROGRAM_ID, mint_support};

/// The deep-amm program id (`programs/deep-amm/src/lib.rs` `declare_id!`).
pub const DEEP_AMM_PROGRAM_ID: Pubkey = pubkey!("HCrCy6bzHhZ1b6bXwQAucEFkKXyzYMh3hgAR8UPrYSEP");
/// `AUTH_SEED` in `programs/deep-amm/src/lib.rs`.
pub const AUTH_SEED: &[u8] = b"vault_and_lp_mint_auth_seed";
/// `sha256("global:swap_base_input")[..8]`.
pub const SWAP_BASE_INPUT_DISCRIMINATOR: [u8; 8] = [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];

/// `Swap::Placeholder` data, byte 0: the native instruction (only `swap_base_input`).
pub const OP_SWAP_BASE_INPUT: u8 = 0;
/// `Swap::Placeholder` data, byte 1: direction. 0 = token_0 -> token_1, 1 = token_1 -> token_0.
pub const DIR_ZERO_FOR_ONE: u8 = 0;
pub const DIR_ONE_FOR_ZERO: u8 = 1;

/// Native `swap_base_input` instruction data: discriminator + `amount_in` (u64 LE) +
/// `minimum_amount_out` (u64 LE).
pub fn encode_swap_base_input(amount_in: u64, minimum_amount_out: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&SWAP_BASE_INPUT_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());
    data
}

/// A quote with every fee component, for callers that want more than [`Quote`].
///
/// Legacy pool (`fee_model == 0`): upstream's meanings. `trade_fee` is taken from the
/// input; `protocol_fee` and `fund_fee` are parts of it; `creator_fee` is taken from the
/// input or the output (`creator_fee_on_input`).
///
/// V1 pool (`fee_model == 1`): every fee is in the pool's QUOTE token, the input of a buy
/// and the output of a sell. `trade_fee = lp_fee + protocol_fee`, `fund_fee = 0`,
/// `creator_fee = reward_fee`, `creator_fee_on_input = is_buy`; the whole fee is
/// `lp_fee + protocol_fee + reward_fee`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetailedQuote {
    pub zero_for_one: bool,
    pub amount_in: u64,
    pub amount_out: u64,
    pub trade_fee: u64,
    pub protocol_fee: u64,
    pub fund_fee: u64,
    pub creator_fee: u64,
    pub creator_fee_on_input: bool,
    /// `state::FEE_MODEL_LEGACY` (0) or `state::FEE_MODEL_V1` (1).
    pub fee_model: u8,
    /// V1: the input is the quote token. Always `false` on a legacy pool, which has no
    /// buy/sell notion.
    pub is_buy: bool,
    /// The part of the fee that stays in the pool as reserve: `trade_fee - protocol_fee -
    /// fund_fee` (the fund fee is 0 on a V1 pool).
    pub lp_fee: u64,
    /// V1: the reward part (equal to `creator_fee`). Always 0 on a legacy pool.
    pub reward_fee: u64,
    /// The total fee rate of this swap, per 1e6. V1: `lp + protocol + reward` of the swap's
    /// side. Legacy: `trade_fee_rate + effective creator_fee_rate`.
    pub total_fee_rate: u64,
}

#[derive(Clone)]
pub struct DeepSwapAmm {
    key: Pubkey,
    authority: Pubkey,
    pool: PoolState,
    config: Option<AmmConfig>,
    vault_amounts: Option<[u64; 2]>,
    /// `None` until `update`; SPL Token mints are `Supported` without being read.
    mint_support: [Option<MintSupport>; 2],
    clock_ref: ClockRef,
}

single_program_amm!(DeepSwapAmm, DEEP_AMM_PROGRAM_ID, "DeepSwap");

fn err(msg: impl Into<String>) -> AmmError {
    AmmError::Custom(msg.into())
}

impl DeepSwapAmm {
    pub fn pool_state(&self) -> &PoolState {
        &self.pool
    }

    fn is_token_2022(&self, i: usize) -> bool {
        let program = if i == 0 {
            self.pool.token_0_program
        } else {
            self.pool.token_1_program
        };
        program == TOKEN_2022_PROGRAM_ID
    }

    fn unix_timestamp(&self) -> i64 {
        self.clock_ref.unix_timestamp.load(Ordering::Relaxed)
    }

    /// Why a swap would currently be rejected regardless of amount, if it would.
    fn inactive_reason(&self) -> Option<String> {
        if !self.pool.swap_enabled() {
            return Some("pool swaps are disabled (status bit 2)".into());
        }
        // swap_base_input: `block_timestamp < pool_state.open_time` -> NotApproved, where
        // block_timestamp is `unix_timestamp as u64`.
        if (self.unix_timestamp() as u64) < self.pool.open_time {
            return Some(format!("pool opens at {}", self.pool.open_time));
        }
        for (i, s) in self.mint_support.iter().enumerate() {
            match s {
                Some(MintSupport::Supported) => {}
                Some(MintSupport::Unsupported(why)) => return Some(format!("token_{i}: {why}")),
                None => return Some("pool not updated yet".into()),
            }
        }
        if self.config.is_none() || self.vault_amounts.is_none() {
            return Some("pool not updated yet".into());
        }
        None
    }

    /// Exact port of `instructions/swap_base_input.rs` `swap_base_input` up to the token
    /// transfers. Errors wherever the program would fail.
    pub fn quote_exact_in(
        &self,
        input_mint: &Pubkey,
        output_mint: &Pubkey,
        amount_in: u64,
    ) -> Result<DetailedQuote, AmmError> {
        let zero_for_one = if *input_mint == self.pool.token_0_mint
            && *output_mint == self.pool.token_1_mint
        {
            true
        } else if *input_mint == self.pool.token_1_mint && *output_mint == self.pool.token_0_mint {
            false
        } else {
            return Err(err("mints do not match this pool"));
        };
        if let Some(reason) = self.inactive_reason() {
            return Err(err(reason));
        }
        let config = self.config.expect("checked by inactive_reason");
        let [vault_0, vault_1] = self.vault_amounts.expect("checked by inactive_reason");

        // Both mints are SPL Token or fee-less Token-2022 (checked above): transfer fee 0.
        let actual_amount_in = amount_in;
        if actual_amount_in == 0 {
            return Err(err("amount_in must be > 0"));
        }

        // PoolState::get_swap_params -> vault_amount_without_fee + token_price_x32 (the
        // latter divides by each fee-less reserve, so an empty side fails on chain).
        let (total_0, total_1) = self
            .pool
            .vault_amount_without_fee(vault_0, vault_1)
            .ok_or_else(|| err("vault holds less than the accrued fees (InsufficientVault)"))?;
        if total_0 == 0 || total_1 == 0 {
            return Err(err("empty reserve"));
        }
        let (total_in, total_out) = if zero_for_one {
            (total_0, total_1)
        } else {
            (total_1, total_0)
        };
        let is_creator_fee_on_input = self
            .pool
            .is_creator_fee_on_input(zero_for_one)
            .ok_or_else(|| err("invalid creator_fee_on (InvalidFeeModel)"))?;
        let constant_before = u128::from(total_in) * u128::from(total_out);

        // DEEP V1 (`swap_fee_model`): a pool is priced with the fee model it was created
        // under, and only by a config of that same model. On a V1 pool `creator_fee_on` is
        // the quote side, so "creator fee on input" reads "the input is the quote token":
        // a buy. (`creator_fee_on` = 0 cannot occur on a V1 pool; the program would treat
        // every swap as a buy, and so does this port.)
        let fee_model = self
            .pool
            .swap_fee_model(&config)
            .ok_or_else(|| err("pool and config fee models differ (FeeModelMismatch)"))?;
        let is_v1 = fee_model == FEE_MODEL_V1;
        let is_buy = is_v1 && is_creator_fee_on_input;
        let (r, total_fee_rate) = if is_v1 {
            let rates = self
                .pool
                .v1_rates(&config, is_buy)
                .ok_or_else(|| err("V1 rates above a cap (FeeRateAboveCap / InvalidRewardRate)"))?;
            (
                math::swap_base_input_v1(
                    u128::from(actual_amount_in),
                    u128::from(total_in),
                    u128::from(total_out),
                    rates.lp,
                    rates.protocol,
                    rates.reward,
                    is_buy,
                ),
                rates.total(),
            )
        } else {
            let creator_fee_rate = self.pool.adjust_creator_fee_rate(config.creator_fee_rate);
            (
                math::swap_base_input(
                    u128::from(actual_amount_in),
                    u128::from(total_in),
                    u128::from(total_out),
                    config.trade_fee_rate,
                    creator_fee_rate,
                    config.protocol_fee_rate,
                    config.fund_fee_rate,
                    is_creator_fee_on_input,
                ),
                config.trade_fee_rate.saturating_add(creator_fee_rate),
            )
        };
        let r = r.ok_or_else(|| err("swap math failed (ZeroTradingTokens)"))?;

        let constant_after = r
            .new_input_vault_amount
            .checked_mul(r.new_output_vault_amount)
            .ok_or_else(|| err("invariant overflow"))?;
        let to_u64 = |v: u128| u64::try_from(v).map_err(|_| err("amount exceeds u64"));
        let amount_out = to_u64(r.output_amount)?;
        // Output transfer fee is 0 (see above); `require_gt!(amount_received, 0)`.
        if amount_out == 0 {
            return Err(err("output amount is zero"));
        }
        let (protocol_fee, fund_fee, creator_fee) = (
            to_u64(r.protocol_fee)?,
            to_u64(r.fund_fee)?,
            to_u64(r.creator_fee)?,
        );

        if is_v1 {
            // PoolState::update_fees_v1: the protocol AND the reward part are in the quote
            // token (the input of a buy, the output of a sell), so both are added to that
            // side's accumulators whatever the direction (checked adds). A Standard pool
            // must not produce a reward (`require_eq!(reward_fee, 0)`).
            if self.pool.reward_model == REWARD_MODEL_STANDARD && creator_fee != 0 {
                return Err(err("reward fee on a Standard pool"));
            }
            let quote_is_token_0 = zero_for_one == is_buy;
            let (p_quote, c_quote) = if quote_is_token_0 {
                (
                    self.pool.protocol_fees_token_0,
                    self.pool.creator_fees_token_0,
                )
            } else {
                (
                    self.pool.protocol_fees_token_1,
                    self.pool.creator_fees_token_1,
                )
            };
            if p_quote.checked_add(protocol_fee).is_none()
                || c_quote.checked_add(creator_fee).is_none()
            {
                return Err(err("fee accumulator overflow"));
            }
        } else {
            // PoolState::update_fees: u64 accumulators are `checked_add(..).unwrap()`.
            let (p_in, f_in, c_in, c_out) = if zero_for_one {
                (
                    self.pool.protocol_fees_token_0,
                    self.pool.fund_fees_token_0,
                    self.pool.creator_fees_token_0,
                    self.pool.creator_fees_token_1,
                )
            } else {
                (
                    self.pool.protocol_fees_token_1,
                    self.pool.fund_fees_token_1,
                    self.pool.creator_fees_token_1,
                    self.pool.creator_fees_token_0,
                )
            };
            let c_acc = if is_creator_fee_on_input { c_in } else { c_out };
            if p_in.checked_add(protocol_fee).is_none()
                || f_in.checked_add(fund_fee).is_none()
                || c_acc.checked_add(creator_fee).is_none()
            {
                return Err(err("fee accumulator overflow"));
            }
        }
        if constant_after < constant_before {
            return Err(err("invariant decreased"));
        }

        let trade_fee = to_u64(r.trade_fee)?;
        // What stays in the pool as reserve. V1: `trade_fee = lp + protocol` exactly.
        // Legacy: protocol and fund fees are floored shares of the trade fee; with rates
        // that sum above 100% the program still swaps, so this is saturating, not checked.
        let lp_fee = trade_fee
            .saturating_sub(protocol_fee)
            .saturating_sub(fund_fee);
        Ok(DetailedQuote {
            zero_for_one,
            amount_in,
            amount_out,
            trade_fee,
            protocol_fee,
            fund_fee,
            creator_fee,
            creator_fee_on_input: is_creator_fee_on_input,
            fee_model,
            is_buy,
            lp_fee,
            reward_fee: if is_v1 { creator_fee } else { 0 },
            total_fee_rate,
        })
    }
}

impl Amm for DeepSwapAmm {
    fn from_keyed_account(
        keyed_account: &KeyedAccount,
        amm_context: &AmmContext,
    ) -> Result<Self, AmmError> {
        if keyed_account.account.owner() != &Self::PROGRAM_ID {
            return Err(err("account is not owned by deep-amm"));
        }
        let pool =
            PoolState::decode(keyed_account.account.data()).map_err(|e| err(e.to_string()))?;
        let authority =
            Pubkey::create_program_address(&[AUTH_SEED, &[pool.auth_bump]], &Self::PROGRAM_ID)
                .map_err(|e| err(format!("authority PDA: {e}")))?;
        let mut amm = Self {
            key: keyed_account.key,
            authority,
            pool,
            config: None,
            vault_amounts: None,
            mint_support: [None, None],
            clock_ref: amm_context.clock_ref.clone(),
        };
        for i in 0..2 {
            if !amm.is_token_2022(i) {
                amm.mint_support[i] = Some(mint_support(
                    &if i == 0 {
                        amm.pool.token_0_program
                    } else {
                        amm.pool.token_1_program
                    },
                    &[],
                ));
            }
        }
        Ok(amm)
    }

    fn label(&self) -> AmmLabel {
        Self::LABEL
    }

    fn program_id(&self) -> Pubkey {
        Self::PROGRAM_ID
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![self.pool.token_0_mint, self.pool.token_1_mint]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        let mut v = vec![
            self.key,
            self.pool.amm_config,
            self.pool.token_0_vault,
            self.pool.token_1_vault,
        ];
        // Token-2022 mints are re-read to catch a transfer-fee / other extension.
        if self.is_token_2022(0) {
            v.push(self.pool.token_0_mint);
        }
        if self.is_token_2022(1) {
            v.push(self.pool.token_1_mint);
        }
        v
    }

    fn update(&mut self, account_provider: impl AccountProvider) -> Result<(), AmmError> {
        let pool_account = account_provider.try_get(&self.key)?;
        let pool = PoolState::decode(pool_account.data()).map_err(|e| err(e.to_string()))?;
        drop(pool_account);
        if pool.token_0_vault != self.pool.token_0_vault
            || pool.token_1_vault != self.pool.token_1_vault
            || pool.amm_config != self.pool.amm_config
        {
            return Err(err("pool immutable fields changed"));
        }
        let config = {
            let a = account_provider.try_get(&pool.amm_config)?;
            if a.owner() != &Self::PROGRAM_ID {
                return Err(err("amm_config not owned by deep-amm"));
            }
            AmmConfig::decode(a.data()).map_err(|e| err(e.to_string()))?
        };
        let amount = |vault: &Pubkey| -> Result<u64, AmmError> {
            let a = account_provider.try_get(vault)?;
            token_account_amount(a.data()).ok_or_else(|| err("malformed vault token account"))
        };
        let vaults = [amount(&pool.token_0_vault)?, amount(&pool.token_1_vault)?];
        let mut support = self.mint_support.clone();
        for (i, mint) in [pool.token_0_mint, pool.token_1_mint].iter().enumerate() {
            if self.is_token_2022(i) {
                let a = account_provider.try_get(mint)?;
                support[i] = Some(mint_support(a.owner(), a.data()));
            }
        }
        self.pool = pool;
        self.config = Some(config);
        self.vault_amounts = Some(vaults);
        self.mint_support = support;
        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote, AmmError> {
        if quote_params.swap_mode != SwapMode::ExactIn {
            return Err(err("DeepSwap adapter supports ExactIn only"));
        }
        let q = self.quote_exact_in(
            &quote_params.input_mint,
            &quote_params.output_mint,
            quote_params.amount,
        )?;
        if q.fee_model == FEE_MODEL_V1 {
            // V1: the whole fee (LP + protocol + reward) is in the quote token, which is
            // the input mint of a buy and the OUTPUT mint of a sell (there it is already
            // deducted from out_amount). fee_pct: that side's total rate as a fraction.
            return Ok(Quote {
                in_amount: q.amount_in,
                out_amount: q.amount_out,
                fee_amount: q.trade_fee.saturating_add(q.creator_fee),
                fee_mint: if q.is_buy {
                    quote_params.input_mint
                } else {
                    quote_params.output_mint
                },
                fee_pct: Decimal::from_i128_with_scale(i128::from(q.total_fee_rate), 6),
            });
        }
        let config = self.config.expect("quote_exact_in succeeded");
        let creator_rate = self.pool.adjust_creator_fee_rate(config.creator_fee_rate);
        // fee_amount: the fee taken from the input (trade fee, plus the creator fee when
        // it is charged on the input). A creator fee charged on the output is already
        // deducted from out_amount. fee_pct: the total fee rate as a fraction.
        let input_side_fee = if q.creator_fee_on_input {
            q.trade_fee.saturating_add(q.creator_fee)
        } else {
            q.trade_fee
        };
        let total_rate = config.trade_fee_rate.saturating_add(creator_rate);
        Ok(Quote {
            in_amount: q.amount_in,
            out_amount: q.amount_out,
            fee_amount: input_side_fee,
            fee_mint: quote_params.input_mint,
            fee_pct: Decimal::from_i128_with_scale(i128::from(total_rate), 6),
        })
    }

    fn get_swap_and_account_metas(
        &self,
        swap_params: &SwapParams,
    ) -> Result<SwapAndAccountMetas, AmmError> {
        let p = &self.pool;
        let zero_for_one = if swap_params.source_mint == p.token_0_mint
            && swap_params.destination_mint == p.token_1_mint
        {
            true
        } else if swap_params.source_mint == p.token_1_mint
            && swap_params.destination_mint == p.token_0_mint
        {
            false
        } else {
            return Err(err("mints do not match this pool"));
        };
        let (in_vault, out_vault, in_prog, out_prog, in_mint, out_mint) = if zero_for_one {
            (
                p.token_0_vault,
                p.token_1_vault,
                p.token_0_program,
                p.token_1_program,
                p.token_0_mint,
                p.token_1_mint,
            )
        } else {
            (
                p.token_1_vault,
                p.token_0_vault,
                p.token_1_program,
                p.token_0_program,
                p.token_1_mint,
                p.token_0_mint,
            )
        };
        // Exactly `instructions/swap_base_input.rs` `Swap` accounts, in order.
        let account_metas = vec![
            AccountMeta::new_readonly(swap_params.token_transfer_authority, true), // payer
            AccountMeta::new_readonly(self.authority, false),
            AccountMeta::new_readonly(p.amm_config, false),
            AccountMeta::new(self.key, false), // pool_state
            AccountMeta::new(swap_params.source_token_account, false),
            AccountMeta::new(swap_params.destination_token_account, false),
            AccountMeta::new(in_vault, false),
            AccountMeta::new(out_vault, false),
            AccountMeta::new_readonly(in_prog, false),
            AccountMeta::new_readonly(out_prog, false),
            AccountMeta::new_readonly(in_mint, false),
            AccountMeta::new_readonly(out_mint, false),
            AccountMeta::new(p.observation_key, false),
        ];
        Ok(SwapAndAccountMetas {
            swap: Swap::Placeholder {
                data: Some(vec![
                    OP_SWAP_BASE_INPUT,
                    if zero_for_one {
                        DIR_ZERO_FOR_ONE
                    } else {
                        DIR_ONE_FOR_ZERO
                    },
                ]),
            },
            account_metas,
        })
    }

    fn supports_exact_out(&self) -> bool {
        false
    }

    fn get_accounts_len(&self) -> usize {
        13
    }

    fn is_active(&self) -> bool {
        self.inactive_reason().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jupiter_amm_interface::FeeMode;
    use solana_account::Account;
    use solana_clock::Clock;
    use std::collections::HashMap;

    #[test]
    fn swap_discriminator_matches_anchor() {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(b"global:swap_base_input");
        assert_eq!(SWAP_BASE_INPUT_DISCRIMINATOR, h[..8]);
        let d = encode_swap_base_input(5, 7);
        assert_eq!(d.len(), 24);
        assert_eq!(&d[8..16], &5u64.to_le_bytes());
        assert_eq!(&d[16..24], &7u64.to_le_bytes());
    }

    #[test]
    fn authority_is_the_known_pda() {
        let (pda, bump) = Pubkey::find_program_address(&[AUTH_SEED], &DEEP_AMM_PROGRAM_ID);
        assert_eq!(pda, pubkey!("9Ed3EyFMgN3q2SbAJPcPNp6aDacgRGF5RyFT7smVSn8o"));
        assert_eq!(bump, 252);
    }

    // ---- a synthetic pool built in memory, exercising the adapter end to end ----

    const POOL: Pubkey = pubkey!("HxRCxiLSTaT7dsP5DLBH1s8hbGa815Q8JuUhZVmcLw5S");
    const CFG: Pubkey = pubkey!("8UY49GiUH4jR9RkUYTCHPpuKu5WQsFaf6KCTeUxQojoS");
    const V0: Pubkey = pubkey!("HTqxFoFPjMUarchjoNBhyPL8ENvdvpaDkXtnRmcAnvU");
    const V1: Pubkey = pubkey!("ArHJ5gaycdMj7pAPxXVxVroR9k5XULRwhQuQaLtxZQSx");
    const M0: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
    const M1: Pubkey = pubkey!("BgDhdvnYVwSDuSZFmbEE8B48ygtEghgq9yoHmtV2DxUY");

    struct Fx {
        status: u8,
        open_time: u64,
        fees0: [u64; 3], // protocol, fund, creator
        fees1: [u64; 3],
        creator_fee_on: u8,
        enable_creator_fee: bool,
        rates: [u64; 4], // trade, protocol, fund, creator
        vaults: [u64; 2],
        token_1_program: Pubkey,
        mint_1_data: Vec<u8>,
        // DEEP V1 (all zero = a legacy pool and config, as written before V1)
        pool_fee_model: u8,
        reward_model: u8,
        reward_rate_snapshot: u64,
        config_fee_model: u8,
        // AmmConfig: buy lp, buy protocol, sell lp, sell protocol (the reward rate is the
        // pool's own: `reward_rate_snapshot`)
        v1_rates: [u64; 4],
        // AmmConfig.max_reward_rate (limits NEW pools only; never read by a swap)
        config_max_reward_rate: u64,
    }

    impl Default for Fx {
        fn default() -> Self {
            Self {
                pool_fee_model: 0,
                reward_model: 0,
                reward_rate_snapshot: 0,
                config_fee_model: 0,
                v1_rates: [0; 4],
                config_max_reward_rate: 0,
                status: 0,
                open_time: 100,
                fees0: [0; 3],
                fees1: [0; 3],
                creator_fee_on: 0,
                enable_creator_fee: false,
                rates: [3_000, 333_333, 0, 0],
                vaults: [4_950_000_000, 158_400_000_000_000],
                token_1_program: token::TOKEN_PROGRAM_ID,
                mint_1_data: vec![0; 82],
            }
        }
    }

    fn acct(owner: Pubkey, data: Vec<u8>) -> Account {
        Account {
            lamports: 1,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        }
    }

    fn build(fx: &Fx, now: i64) -> DeepSwapAmm {
        let mut p = vec![0u8; state::POOL_STATE_LEN];
        p[..8].copy_from_slice(&state::POOL_STATE_DISCRIMINATOR);
        p[8..40].copy_from_slice(CFG.as_ref());
        p[72..104].copy_from_slice(V0.as_ref());
        p[104..136].copy_from_slice(V1.as_ref());
        p[168..200].copy_from_slice(M0.as_ref());
        p[200..232].copy_from_slice(M1.as_ref());
        p[232..264].copy_from_slice(token::TOKEN_PROGRAM_ID.as_ref());
        p[264..296].copy_from_slice(fx.token_1_program.as_ref());
        p[328] = 252;
        p[329] = fx.status;
        p[341..349].copy_from_slice(&fx.fees0[0].to_le_bytes());
        p[349..357].copy_from_slice(&fx.fees1[0].to_le_bytes());
        p[357..365].copy_from_slice(&fx.fees0[1].to_le_bytes());
        p[365..373].copy_from_slice(&fx.fees1[1].to_le_bytes());
        p[373..381].copy_from_slice(&fx.open_time.to_le_bytes());
        p[389] = fx.creator_fee_on;
        p[390] = fx.enable_creator_fee as u8;
        p[397..405].copy_from_slice(&fx.fees0[2].to_le_bytes());
        p[405..413].copy_from_slice(&fx.fees1[2].to_le_bytes());
        p[413] = fx.pool_fee_model;
        p[414] = fx.reward_model;
        p[421..429].copy_from_slice(&fx.reward_rate_snapshot.to_le_bytes());

        let mut c = vec![0u8; state::AMM_CONFIG_LEN];
        c[..8].copy_from_slice(&state::AMM_CONFIG_DISCRIMINATOR);
        c[12..20].copy_from_slice(&fx.rates[0].to_le_bytes());
        c[20..28].copy_from_slice(&fx.rates[1].to_le_bytes());
        c[28..36].copy_from_slice(&fx.rates[2].to_le_bytes());
        c[108..116].copy_from_slice(&fx.rates[3].to_le_bytes());
        c[124] = fx.config_fee_model;
        for (i, rate) in fx.v1_rates.iter().enumerate() {
            c[132 + 8 * i..140 + 8 * i].copy_from_slice(&rate.to_le_bytes());
        }
        c[164..172].copy_from_slice(&fx.config_max_reward_rate.to_le_bytes());

        let tok = |amount: u64| {
            let mut d = vec![0u8; 165];
            d[64..72].copy_from_slice(&amount.to_le_bytes());
            acct(token::TOKEN_PROGRAM_ID, d)
        };
        let mut accounts: HashMap<Pubkey, Account> = HashMap::new();
        accounts.insert(POOL, acct(DEEP_AMM_PROGRAM_ID, p));
        accounts.insert(CFG, acct(DEEP_AMM_PROGRAM_ID, c));
        accounts.insert(V0, tok(fx.vaults[0]));
        accounts.insert(V1, tok(fx.vaults[1]));
        accounts.insert(M1, acct(fx.token_1_program, fx.mint_1_data.clone()));

        let ctx = AmmContext {
            clock_ref: ClockRef::from(Clock {
                unix_timestamp: now,
                ..Clock::default()
            }),
        };
        let ka = KeyedAccount {
            key: POOL,
            account: accounts[&POOL].clone(),
            params: None,
        };
        let mut amm = DeepSwapAmm::from_keyed_account(&ka, &ctx).unwrap();
        let provider: HashMap<Pubkey, Box<Account>> = accounts
            .into_iter()
            .map(|(k, v)| (k, Box::new(v)))
            .collect();
        amm.update(&provider).unwrap();
        amm
    }

    fn q(amm: &DeepSwapAmm, i: Pubkey, o: Pubkey, amount: u64) -> Result<Quote, AmmError> {
        amm.quote(&QuoteParams {
            amount,
            input_mint: i,
            output_mint: o,
            swap_mode: SwapMode::ExactIn,
            fee_mode: FeeMode::Normal,
        })
    }

    #[test]
    fn quotes_match_hand_vectors_both_directions() {
        let amm = build(&Fx::default(), 200);
        assert!(amm.is_active());
        let a = q(&amm, M0, M1, 1_000_000_000).unwrap();
        assert_eq!(a.out_amount, 26_555_372_456_700);
        assert_eq!(a.fee_amount, 3_000_000);
        assert_eq!(a.fee_mint, M0);
        assert_eq!(a.fee_pct.to_string(), "0.003000");
        let b = q(&amm, M1, M0, 1_000_000_000).unwrap();
        assert_eq!(b.out_amount, 31_156);
    }

    #[test]
    fn reserves_exclude_accrued_fees() {
        // 50_000_000 lamports of accrued fees on token 0 -> reserve 4.9 SOL
        let fx = Fx {
            fees0: [30_000_000, 15_000_000, 5_000_000],
            ..Fx::default()
        };
        let amm = build(&fx, 200);
        let r = math::swap_base_input(
            1_000_000,
            158_400_000_000_000,
            4_900_000_000,
            3_000,
            0,
            333_333,
            0,
            true,
        )
        .unwrap();
        assert_eq!(
            q(&amm, M1, M0, 1_000_000).unwrap().out_amount,
            r.output_amount as u64
        );
    }

    #[test]
    fn creator_fee_modes() {
        // creator fee enabled, 0.1%, charged only on token 1
        let fx = Fx {
            enable_creator_fee: true,
            creator_fee_on: 2,
            rates: [2_500, 120_000, 40_000, 1_000],
            vaults: [10_000_000, 10_000_000],
            ..Fx::default()
        };
        let amm = build(&fx, 200);
        // token0 in: creator fee charged on the output (token 1)
        let d = amm.quote_exact_in(&M0, &M1, 1_000_000).unwrap();
        assert!(!d.creator_fee_on_input);
        assert_eq!(
            (d.trade_fee, d.creator_fee, d.amount_out),
            (2_500, 908, 906_116)
        );
        // token1 in: creator fee charged on the input
        let d = amm.quote_exact_in(&M1, &M0, 1_000_000).unwrap();
        assert!(d.creator_fee_on_input);
        assert_eq!(
            (d.trade_fee, d.creator_fee, d.amount_out),
            (2_500, 1_000, 906_197)
        );
        assert_eq!((d.protocol_fee, d.fund_fee), (300, 100));
        let qq = q(&amm, M1, M0, 1_000_000).unwrap();
        assert_eq!(qq.fee_amount, 3_500);
        assert_eq!(qq.fee_pct.to_string(), "0.003500");

        // the same config with creator fee disabled on the pool: rate forced to 0
        let amm = build(
            &Fx {
                enable_creator_fee: false,
                ..fx
            },
            200,
        );
        let d = amm.quote_exact_in(&M0, &M1, 1_000_000).unwrap();
        assert_eq!(d.creator_fee, 0);
    }

    /// A pool created by a DEEP graduation at the policy rates: trade fee 0.30%, creator
    /// fee 0.30% on top, taken in WSOL only (`creator_fee_on` = the WSOL index, here token
    /// 0). The same numbers are pinned in the DEEP SDK (packages/sdk/test/raydium-cpmm.test.ts).
    #[test]
    fn graduation_pool_creator_fee_in_sol() {
        let fx = Fx {
            enable_creator_fee: true,
            creator_fee_on: 1,
            rates: [3_000, 333_333, 0, 3_000],
            vaults: [84_150_000_000, 206_900_000_000_000],
            ..Fx::default()
        };
        let amm = build(&fx, 200);
        // buy 1 SOL: fee on the input. total = ceil(1e9 * 6000 / 1e6) = 6_000_000,
        // creator = floor(6_000_000 * 3000 / 6000) = 3_000_000, trade = 3_000_000
        let d = amm.quote_exact_in(&M0, &M1, 1_000_000_000).unwrap();
        assert!(d.creator_fee_on_input);
        assert_eq!(
            (d.trade_fee, d.creator_fee, d.amount_out),
            (3_000_000, 3_000_000, 2_415_420_933_947)
        );
        assert_eq!(d.protocol_fee, 999_999);
        let qq = q(&amm, M0, M1, 1_000_000_000).unwrap();
        assert_eq!(qq.fee_amount, 6_000_000);
        assert_eq!(qq.fee_pct.to_string(), "0.006000");

        // sell 1M tokens: trade fee ceil(1e12 * 3000 / 1e6) on the token input, then
        // creator = ceil(403_553_442 * 3000 / 1e6) = 1_210_661 lamports off the SOL output
        let d = amm.quote_exact_in(&M1, &M0, 1_000_000_000_000).unwrap();
        assert!(!d.creator_fee_on_input);
        assert_eq!(
            (d.trade_fee, d.creator_fee, d.amount_out),
            (3_000_000_000, 1_210_661, 402_342_781)
        );
        let qq = q(&amm, M1, M0, 1_000_000_000_000).unwrap();
        assert_eq!(qq.out_amount, 402_342_781);
        assert_eq!(qq.fee_amount, 3_000_000_000);
        assert_eq!(qq.fee_pct.to_string(), "0.006000");

        // a pool for the same pair from the permissionless `initialize`: 0.30% only
        let amm = build(
            &Fx {
                enable_creator_fee: false,
                creator_fee_on: 0,
                ..fx
            },
            200,
        );
        let d = amm.quote_exact_in(&M0, &M1, 1_000_000_000).unwrap();
        assert_eq!((d.trade_fee, d.creator_fee), (3_000_000, 0));
        assert_eq!(
            q(&amm, M0, M1, 1_000_000_000).unwrap().fee_pct.to_string(),
            "0.003000"
        );
    }

    // ---- DEEP V1 pools ----

    const SOL: u64 = 1_000_000_000;

    /// A V1 pool at the owner's schedule: 100 SOL (token 0 = the quote side, so
    /// `creator_fee_on` = 1) against 990M tokens; LP 0.10% both sides, DEEP 0.25% on buys
    /// and 0.65% on sells (the AmmConfig); the pool's own reward rate is 1% unless Standard.
    fn v1_fx(reward_model: u8) -> Fx {
        Fx {
            pool_fee_model: 1,
            config_fee_model: 1,
            reward_model,
            reward_rate_snapshot: if reward_model == 0 { 0 } else { 10_000 },
            creator_fee_on: 1,
            enable_creator_fee: reward_model != 0,
            // the legacy mirror of a V1 config (`sync_legacy_mirror`); not read for V1
            rates: [3_500, 714_285, 0, 0],
            v1_rates: [1_000, 2_500, 1_000, 6_500],
            config_max_reward_rate: 50_000,
            vaults: [100 * SOL, 990_000_000_000_000],
            ..Fx::default()
        }
    }

    /// docs/V1_FEES.md "Worked examples", through the adapter. Selling y / 99 tokens into
    /// (y, 100 SOL) prices to a gross output of exactly 1 SOL.
    #[test]
    fn v1_worked_examples_both_sides() {
        let (x, y) = (100 * u128::from(SOL), 990_000_000_000_000u128);
        let tokens_in = 10_000_000_000_000u64; // y / 99

        // Standard: buy 0.35% (LP 0.10% + DEEP 0.25%), sell 0.75% (LP 0.10% + DEEP 0.65%)
        let amm = build(&v1_fx(0), 200);
        let d = amm.quote_exact_in(&M0, &M1, SOL).unwrap();
        assert_eq!(
            (d.fee_model, d.is_buy, d.creator_fee_on_input),
            (1, true, true)
        );
        assert_eq!(
            (d.lp_fee, d.protocol_fee, d.reward_fee, d.fund_fee),
            (1_000_000, 2_500_000, 0, 0)
        );
        assert_eq!((d.trade_fee, d.creator_fee), (3_500_000, 0));
        assert_eq!(d.total_fee_rate, 3_500);
        assert_eq!(
            u128::from(d.amount_out),
            996_500_000 * y / (x + 996_500_000)
        );
        let qq = q(&amm, M0, M1, SOL).unwrap();
        assert_eq!(qq.out_amount, d.amount_out);
        assert_eq!((qq.fee_amount, qq.fee_mint), (3_500_000, M0));
        assert_eq!(qq.fee_pct.to_string(), "0.003500");

        let d = amm.quote_exact_in(&M1, &M0, tokens_in).unwrap();
        assert_eq!(
            (d.fee_model, d.is_buy, d.creator_fee_on_input),
            (1, false, false)
        );
        assert_eq!(
            (d.lp_fee, d.protocol_fee, d.reward_fee, d.amount_out),
            (1_000_000, 6_500_000, 0, 992_500_000)
        );
        // a sell's fee is in the OUTPUT mint (the quote token)
        let qq = q(&amm, M1, M0, tokens_in).unwrap();
        assert_eq!(qq.out_amount, 992_500_000);
        assert_eq!((qq.fee_amount, qq.fee_mint), (7_500_000, M0));
        assert_eq!(qq.fee_pct.to_string(), "0.007500");

        // Creator and Holder: the same plus the 1% reward on each side
        for reward_model in [1u8, 2] {
            let amm = build(&v1_fx(reward_model), 200);
            let d = amm.quote_exact_in(&M0, &M1, SOL).unwrap();
            assert_eq!(
                (d.lp_fee, d.protocol_fee, d.reward_fee),
                (1_000_000, 2_500_000, 10_000_000)
            );
            assert_eq!((d.trade_fee, d.creator_fee), (3_500_000, 10_000_000));
            assert_eq!(
                u128::from(d.amount_out),
                986_500_000 * y / (x + 986_500_000)
            );
            let qq = q(&amm, M0, M1, SOL).unwrap();
            assert_eq!((qq.fee_amount, qq.fee_mint), (13_500_000, M0));
            assert_eq!(qq.fee_pct.to_string(), "0.013500");

            let d = amm.quote_exact_in(&M1, &M0, tokens_in).unwrap();
            assert_eq!(
                (d.lp_fee, d.protocol_fee, d.reward_fee, d.amount_out),
                (1_000_000, 6_500_000, 10_000_000, 982_500_000)
            );
            let qq = q(&amm, M1, M0, tokens_in).unwrap();
            assert_eq!((qq.fee_amount, qq.fee_mint), (17_500_000, M0));
            assert_eq!(qq.fee_pct.to_string(), "0.017500");
        }
    }

    /// The quote side follows `creator_fee_on`, not the token order: with token 1 as the
    /// quote token, token1 -> token0 is the buy.
    #[test]
    fn v1_quote_side_can_be_token_1() {
        let fx = Fx {
            creator_fee_on: 2,
            vaults: [990_000_000_000_000, 100 * SOL],
            ..v1_fx(1)
        };
        let amm = build(&fx, 200);
        let d = amm.quote_exact_in(&M1, &M0, SOL).unwrap();
        assert!(d.is_buy && !d.zero_for_one);
        assert_eq!(
            (d.lp_fee, d.protocol_fee, d.reward_fee),
            (1_000_000, 2_500_000, 10_000_000)
        );
        assert_eq!(q(&amm, M1, M0, SOL).unwrap().fee_mint, M1);
        let d = amm.quote_exact_in(&M0, &M1, 10_000_000_000_000).unwrap();
        assert!(!d.is_buy && d.zero_for_one);
        assert_eq!(d.amount_out, 982_500_000);
        let qq = q(&amm, M0, M1, 10_000_000_000_000).unwrap();
        assert_eq!((qq.fee_amount, qq.fee_mint), (17_500_000, M1));
    }

    /// The pool's own reward rate is charged whatever the config's rates are, up to both
    /// limits: LP + protocol at most 5% (the config's side), the reward at most 5% (the
    /// pool's side), so a side never exceeds the 10% total cap. Nothing is reduced to fit.
    #[test]
    fn v1_pool_reward_rate_is_charged_whatever_the_config_rates_up_to_both_caps() {
        // each pool charges its own rate: 0.4% and 1 bps here, under the same config
        let fx = Fx {
            reward_rate_snapshot: 4_000,
            ..v1_fx(1)
        };
        let amm = build(&fx, 200);
        let d = amm.quote_exact_in(&M0, &M1, SOL).unwrap();
        assert_eq!((d.total_fee_rate, d.reward_fee), (7_500, 4_000_000));
        let fx = Fx {
            reward_rate_snapshot: 100,
            ..v1_fx(2)
        };
        let amm = build(&fx, 200);
        let d = amm.quote_exact_in(&M0, &M1, SOL).unwrap();
        assert_eq!(d.total_fee_rate, 3_600);
        assert_eq!(
            (d.lp_fee, d.protocol_fee, d.reward_fee),
            (1_000_000, 2_500_000, 100_000)
        );

        // the config's sell side at its limit (0.1% + 4.9%): the pool's 1% still comes on top
        let fx = Fx {
            v1_rates: [1_000, 2_500, 1_000, 49_000],
            ..v1_fx(1)
        };
        let amm = build(&fx, 200);
        let d = amm.quote_exact_in(&M1, &M0, 10_000_000_000_000).unwrap();
        assert_eq!(d.total_fee_rate, 60_000);
        assert_eq!(
            (d.lp_fee, d.protocol_fee, d.reward_fee, d.amount_out),
            (1_000_000, 49_000_000, 10_000_000, 940_000_000)
        );
        // the buy side of the same pool follows the buy rates
        assert_eq!(
            amm.quote_exact_in(&M0, &M1, SOL).unwrap().total_fee_rate,
            13_500
        );

        // both limits at once: exactly a 10% sell, and a 5.35% buy. The config's
        // `max_reward_rate` only limits NEW pools: whatever it holds (1 bps, 0, garbage), an
        // existing pool keeps charging its own rate.
        for config_max_reward_rate in [50_000, 100, 0, u64::MAX] {
            let fx = Fx {
                v1_rates: [1_000, 2_500, 1_000, 49_000],
                reward_rate_snapshot: 50_000,
                config_max_reward_rate,
                ..v1_fx(1)
            };
            let amm = build(&fx, 200);
            let d = amm.quote_exact_in(&M1, &M0, 10_000_000_000_000).unwrap();
            assert_eq!(d.total_fee_rate, 100_000);
            assert_eq!(
                (d.lp_fee, d.protocol_fee, d.reward_fee, d.amount_out),
                (1_000_000, 49_000_000, 50_000_000, 900_000_000)
            );
            let qq = q(&amm, M1, M0, 10_000_000_000_000).unwrap();
            assert_eq!((qq.fee_amount, qq.fee_mint), (100_000_000, M0));
            assert_eq!(qq.fee_pct.to_string(), "0.100000");
            let d = amm.quote_exact_in(&M0, &M1, SOL).unwrap();
            assert_eq!(d.total_fee_rate, 53_500);
            assert_eq!(
                (d.lp_fee, d.protocol_fee, d.reward_fee),
                (1_000_000, 2_500_000, 50_000_000)
            );
        }

        // LP + protocol above their 5% limit: the program refuses to price that side
        let fx = Fx {
            v1_rates: [1_000, 2_500, 1_000, 49_001],
            ..v1_fx(1)
        };
        let amm = build(&fx, 200);
        assert!(q(&amm, M1, M0, 10_000_000_000_000).is_err());
        assert!(q(&amm, M0, M1, SOL).is_ok());

        // a reward above its 5% ceiling (cannot be stored): refused on both sides...
        let fx = Fx {
            reward_rate_snapshot: 50_001,
            ..v1_fx(1)
        };
        let amm = build(&fx, 200);
        assert!(q(&amm, M1, M0, 10_000_000_000_000).is_err());
        assert!(q(&amm, M0, M1, SOL).is_err());
        // ...unless the pool is Standard, which never charges a reward whatever is stored
        let fx = Fx {
            reward_rate_snapshot: 50_001,
            ..v1_fx(0)
        };
        let amm = build(&fx, 200);
        let d = amm.quote_exact_in(&M0, &M1, SOL).unwrap();
        assert_eq!((d.total_fee_rate, d.reward_fee), (3_500, 0));
    }

    #[test]
    fn v1_rejects_what_the_program_rejects() {
        // the pool and its config must carry the same fee model, and it must be 0 or 1
        for (pool_fee_model, config_fee_model) in [(1, 0), (0, 1), (2, 2)] {
            let fx = Fx {
                pool_fee_model,
                config_fee_model,
                ..v1_fx(1)
            };
            let amm = build(&fx, 200);
            let e = q(&amm, M0, M1, SOL).unwrap_err().to_string();
            assert!(e.contains("FeeModelMismatch"), "{e}");
            assert!(q(&amm, M1, M0, 10_000_000_000_000).is_err());
        }

        let amm = build(&v1_fx(1), 200);
        assert!(q(&amm, M0, M1, 0).is_err(), "zero input");
        // dust buy: the whole unit is fee, nothing comes out
        assert!(q(&amm, M0, M1, 1).is_err(), "dust buy: zero output");
        // dust sell: 1 token unit prices to 0 lamports
        assert!(q(&amm, M1, M0, 1).is_err(), "dust sell: zero output");

        // Accumulators: DEEP's part and the reward are booked on the QUOTE side (token 0
        // here). A nearly full quote-side accumulator (reserve: 1_000 lamports) overflows
        // on a buy, for the protocol part (2_500_000) and for the reward part (10_000_000).
        for fees0 in [[u64::MAX - 1_000, 0, 0], [0, 0, u64::MAX - 1_000]] {
            let fx = Fx {
                fees0,
                vaults: [u64::MAX, 990_000_000_000_000],
                ..v1_fx(1)
            };
            let amm = build(&fx, 200);
            let e = q(&amm, M0, M1, SOL).unwrap_err().to_string();
            assert!(e.contains("accumulator"), "{e}");
        }
        // Nothing is ever added on the token side, in either direction: with its
        // accumulators nearly full (reserve: 1_000 token units) a sell, whose fee is far
        // above 1_000, still goes through because the fee is booked in SOL. (A legacy pool
        // books the protocol fee on the input side and would overflow here.)
        for fees1 in [[u64::MAX - 1_000, 0, 0], [0, 0, u64::MAX - 1_000]] {
            let fx = Fx {
                fees1,
                vaults: [100 * SOL, u64::MAX],
                ..v1_fx(1)
            };
            let amm = build(&fx, 200);
            let d = amm.quote_exact_in(&M1, &M0, 10_000_000_000_000).unwrap();
            assert!(!d.is_buy);
            assert!(d.protocol_fee > 1_000 && d.reward_fee > 1_000);
            assert!(amm.quote_exact_in(&M0, &M1, SOL).unwrap().is_buy);
        }
    }

    /// `creator_fee_on` = 0 cannot occur on a V1 pool (`set_v1` refuses it), but the program
    /// would then treat the input as the quote token in both directions; so does the port.
    #[test]
    fn v1_creator_fee_on_zero_makes_every_swap_a_buy() {
        let fx = Fx {
            creator_fee_on: 0,
            ..v1_fx(2)
        };
        let amm = build(&fx, 200);
        for (i, o) in [(M0, M1), (M1, M0)] {
            let d = amm.quote_exact_in(&i, &o, SOL).unwrap();
            assert!(d.is_buy);
            assert_eq!(
                (d.lp_fee, d.protocol_fee, d.reward_fee),
                (1_000_000, 2_500_000, 10_000_000)
            );
            let qq = q(&amm, i, o, SOL).unwrap();
            assert_eq!((qq.fee_amount, qq.fee_mint), (13_500_000, i));
        }
    }

    /// A legacy pool reports the new `DetailedQuote` fields without changing its numbers.
    #[test]
    fn legacy_quote_reports_the_new_fields() {
        let fx = Fx {
            enable_creator_fee: true,
            creator_fee_on: 2,
            rates: [2_500, 120_000, 40_000, 1_000],
            vaults: [10_000_000, 10_000_000],
            ..Fx::default()
        };
        let amm = build(&fx, 200);
        let d = amm.quote_exact_in(&M1, &M0, 1_000_000).unwrap();
        assert_eq!((d.fee_model, d.is_buy, d.reward_fee), (0, false, 0));
        // trade 2_500 = LP 2_100 + protocol 300 + fund 100
        assert_eq!(
            (d.trade_fee, d.lp_fee, d.total_fee_rate),
            (2_500, 2_100, 3_500)
        );
        assert_eq!((d.creator_fee, d.amount_out), (1_000, 906_197));
    }

    #[test]
    fn rejects_what_the_program_rejects() {
        let amm = build(&Fx::default(), 200);
        assert!(q(&amm, M0, M1, 0).is_err(), "zero input");
        assert!(q(&amm, M0, M1, 1).is_err(), "dust: zero output");
        assert!(q(&amm, M0, M0, 10).is_err(), "wrong mints");
        let exact_out = amm.quote(&QuoteParams {
            amount: 10,
            input_mint: M0,
            output_mint: M1,
            swap_mode: SwapMode::ExactOut,
            fee_mode: FeeMode::Normal,
        });
        assert!(exact_out.is_err());

        // not open yet (now < open_time) and swap-disabled pools are inactive
        let amm = build(
            &Fx {
                open_time: 1_000,
                ..Fx::default()
            },
            999,
        );
        assert!(!amm.is_active());
        assert!(q(&amm, M0, M1, 1_000_000).is_err());
        let amm = build(
            &Fx {
                open_time: 1_000,
                ..Fx::default()
            },
            1_000,
        );
        assert!(amm.is_active());
        let amm = build(
            &Fx {
                status: 4,
                ..Fx::default()
            },
            200,
        );
        assert!(!amm.is_active());
        assert!(q(&amm, M0, M1, 1_000_000).is_err());
        // deposit/withdraw disabled does not block swaps
        let amm = build(
            &Fx {
                status: 3,
                ..Fx::default()
            },
            200,
        );
        assert!(amm.is_active());

        // vault smaller than accrued fees
        let amm = build(
            &Fx {
                fees1: [158_400_000_000_001, 0, 0],
                ..Fx::default()
            },
            200,
        );
        assert!(q(&amm, M0, M1, 1_000_000).is_err());
        // invalid creator_fee_on
        let amm = build(
            &Fx {
                creator_fee_on: 7,
                ..Fx::default()
            },
            200,
        );
        assert!(q(&amm, M0, M1, 1_000_000).is_err());
    }

    #[test]
    fn draining_amounts_stay_below_reserve() {
        let amm = build(&Fx::default(), 200);
        let out = q(&amm, M1, M0, u64::MAX).unwrap().out_amount;
        assert!(out < 4_950_000_000 && out > 4_940_000_000);
        let out = q(&amm, M0, M1, u64::MAX).unwrap().out_amount;
        assert!(out < 158_400_000_000_000);
    }

    #[test]
    fn token_2022_transfer_fee_mint_is_inactive() {
        let mut mint = vec![0u8; 165];
        mint.push(1);
        mint.extend_from_slice(&1u16.to_le_bytes());
        mint.extend_from_slice(&108u16.to_le_bytes());
        mint.extend(std::iter::repeat_n(0u8, 108));
        let fx = Fx {
            token_1_program: TOKEN_2022_PROGRAM_ID,
            mint_1_data: mint,
            ..Fx::default()
        };
        let amm = build(&fx, 200);
        assert!(amm.get_accounts_to_update().contains(&M1));
        assert!(!amm.is_active());
        let e = q(&amm, M0, M1, 1_000_000).unwrap_err().to_string();
        assert!(e.contains("TransferFeeConfig"), "{e}");

        // a plain Token-2022 mint is fine
        let fx = Fx {
            token_1_program: TOKEN_2022_PROGRAM_ID,
            ..Fx::default()
        };
        let amm = build(&fx, 200);
        assert!(amm.is_active());
        assert!(q(&amm, M0, M1, 1_000_000).is_ok());
    }

    #[test]
    fn account_metas_follow_the_native_instruction() {
        let amm = build(&Fx::default(), 200);
        let user = Pubkey::new_unique();
        let (src, dst) = (Pubkey::new_unique(), Pubkey::new_unique());
        let jup = Pubkey::new_unique();
        let params = |s: Pubkey, d: Pubkey| SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: 1,
            out_amount: 1,
            source_mint: s,
            destination_mint: d,
            source_token_account: src,
            destination_token_account: dst,
            token_transfer_authority: user,
            user,
            payer: user,
            quote_mint_to_referrer: None,
            jupiter_program_id: &jup,
            missing_dynamic_accounts_as_default: false,
        };
        let r = amm.get_swap_and_account_metas(&params(M1, M0)).unwrap();
        assert_eq!(
            r.swap,
            Swap::Placeholder {
                data: Some(vec![0, 1])
            }
        );
        let m = &r.account_metas;
        assert_eq!(m.len(), amm.get_accounts_len());
        let keys: Vec<Pubkey> = m.iter().map(|a| a.pubkey).collect();
        assert_eq!(
            keys,
            vec![
                user,
                pubkey!("9Ed3EyFMgN3q2SbAJPcPNp6aDacgRGF5RyFT7smVSn8o"),
                CFG,
                POOL,
                src,
                dst,
                V1,
                V0,
                token::TOKEN_PROGRAM_ID,
                token::TOKEN_PROGRAM_ID,
                M1,
                M0,
                Pubkey::default(), // observation (zeroed in this synthetic pool)
            ]
        );
        let writable: Vec<bool> = m.iter().map(|a| a.is_writable).collect();
        assert_eq!(
            writable,
            vec![
                false, false, false, true, true, true, true, true, false, false, false, false, true
            ]
        );
        assert!(m[0].is_signer && m.iter().skip(1).all(|a| !a.is_signer));
        let r = amm.get_swap_and_account_metas(&params(M0, M1)).unwrap();
        assert_eq!(
            r.swap,
            Swap::Placeholder {
                data: Some(vec![0, 0])
            }
        );
        assert_eq!(r.account_metas[6].pubkey, V0);
        assert!(amm.get_swap_and_account_metas(&params(M0, M0)).is_err());
    }
}
