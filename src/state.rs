//! Raw-byte decoders for the deep-amm accounts the adapter reads.
//!
//! Layouts mirror `programs/deep-amm/src/states/pool.rs` (`PoolState`,
//! `#[account(zero_copy(unsafe))] #[repr(C, packed)]`, 637 bytes) and
//! `programs/deep-amm/src/states/config.rs` (`AmmConfig`, Borsh `#[account]`, 236 bytes).
//! Offsets below include the 8-byte Anchor discriminator. The discriminators are
//! `sha256("account:PoolState")[..8]` and `sha256("account:AmmConfig")[..8]`; the
//! `#[program]` module rename (raydium_cp_swap -> deep_amm) does not change them.

use solana_pubkey::Pubkey;

pub const POOL_STATE_DISCRIMINATOR: [u8; 8] = [0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46];
pub const AMM_CONFIG_DISCRIMINATOR: [u8; 8] = [0xda, 0xf4, 0x21, 0x68, 0xcb, 0xcb, 0x2b, 0x6f];

/// `PoolState::LEN`.
pub const POOL_STATE_LEN: usize = 637;
/// `AmmConfig::LEN`.
pub const AMM_CONFIG_LEN: usize = 236;

/// `PoolStatusBitIndex::Swap` (bit 2: 1 = swap disabled).
pub const STATUS_SWAP_DISABLED_BIT: u8 = 1 << 2;

/// `AmmConfig::fee_model` / `PoolState::fee_model` values (`states/config.rs`): 0 = legacy
/// (upstream cp-swap fee semantics), 1 = DEEP V1 (side-dependent fees in the quote token).
pub const FEE_MODEL_LEGACY: u8 = 0;
pub const FEE_MODEL_V1: u8 = 1;

/// `PoolState::reward_model` values (`states/pool.rs`).
pub const REWARD_MODEL_STANDARD: u8 = 0;
pub const REWARD_MODEL_CREATOR: u8 = 1;
pub const REWARD_MODEL_HOLDER: u8 = 2;

/// Byte offsets (including the discriminator) of the DEEP V1 fields, the same constants as
/// `states/pool.rs` and `states/config.rs`. They are carved out of upstream's padding, so
/// an account written before V1 reads as legacy with every V1 value 0.
pub const POOL_STATE_FEE_MODEL_OFFSET: usize = 413;
pub const POOL_STATE_REWARD_MODEL_OFFSET: usize = 414;
pub const POOL_STATE_REWARD_RATE_SNAPSHOT_OFFSET: usize = 421;
pub const AMM_CONFIG_FEE_MODEL_OFFSET: usize = 124;
pub const AMM_CONFIG_BUY_LP_FEE_RATE_OFFSET: usize = 132;
pub const AMM_CONFIG_BUY_PROTOCOL_FEE_RATE_OFFSET: usize = 140;
pub const AMM_CONFIG_SELL_LP_FEE_RATE_OFFSET: usize = 148;
pub const AMM_CONFIG_SELL_PROTOCOL_FEE_RATE_OFFSET: usize = 156;
/// `AmmConfig::max_reward_rate`: the admin's current maximum for NEW pools. Swaps never
/// read it; a pool's reward rate is its own (`PoolState::reward_rate_snapshot`).
pub const AMM_CONFIG_MAX_REWARD_RATE_OFFSET: usize = 164;

/// Mirrors `states/pool.rs` `V1Rates`: the per-1e6 rates one V1 swap is charged at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V1Rates {
    pub lp: u64,
    pub protocol: u64,
    pub reward: u64,
}

impl V1Rates {
    /// `lp + protocol + reward`. For a value returned by [`PoolState::v1_rates`],
    /// `lp + protocol` is at most `MAX_TOTAL_FEE_RATE - MAX_REWARD_RATE` (5%) and `reward`
    /// at most `math::MAX_REWARD_RATE` (5%), so the total never exceeds
    /// `math::MAX_TOTAL_FEE_RATE` (10%).
    pub fn total(&self) -> u64 {
        self.lp + self.protocol + self.reward
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("{0}: account data too short ({1} bytes)")]
    TooShort(&'static str, usize),
    #[error("{0}: wrong anchor discriminator")]
    Discriminator(&'static str),
}

fn pubkey(d: &[u8], off: usize) -> Pubkey {
    Pubkey::new_from_array(d[off..off + 32].try_into().unwrap())
}
fn u64_at(d: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(d[off..off + 8].try_into().unwrap())
}

/// The `PoolState` fields the adapter needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PoolState {
    pub amm_config: Pubkey,
    pub pool_creator: Pubkey,
    pub token_0_vault: Pubkey,
    pub token_1_vault: Pubkey,
    pub lp_mint: Pubkey,
    pub token_0_mint: Pubkey,
    pub token_1_mint: Pubkey,
    pub token_0_program: Pubkey,
    pub token_1_program: Pubkey,
    pub observation_key: Pubkey,
    pub auth_bump: u8,
    pub status: u8,
    pub mint_0_decimals: u8,
    pub mint_1_decimals: u8,
    pub protocol_fees_token_0: u64,
    pub protocol_fees_token_1: u64,
    pub fund_fees_token_0: u64,
    pub fund_fees_token_1: u64,
    pub open_time: u64,
    /// 0 = both tokens (fee taken from whichever is the input), 1 = only token 0,
    /// 2 = only token 1. Any other value makes the program fail with `InvalidFeeModel`.
    pub creator_fee_on: u8,
    pub enable_creator_fee: bool,
    pub creator_fees_token_0: u64,
    pub creator_fees_token_1: u64,
    /// DEEP V1: [`FEE_MODEL_LEGACY`] or [`FEE_MODEL_V1`]. On a V1 pool `creator_fee_on`
    /// holds the QUOTE side (1 = token 0, 2 = token 1), so "creator fee on input" reads
    /// "the input is the quote token": a buy.
    pub fee_model: u8,
    /// DEEP V1: 0 Standard, 1 Creator, 2 Holder.
    pub reward_model: u8,
    /// DEEP V1: the pool's own reward rate (per 1e6, each side): the token's rate, chosen
    /// by its creator and stored when the pool is created. Not an AmmConfig value.
    pub reward_rate_snapshot: u64,
}

impl PoolState {
    pub fn decode(d: &[u8]) -> Result<Self, DecodeError> {
        if d.len() < POOL_STATE_LEN {
            return Err(DecodeError::TooShort("PoolState", d.len()));
        }
        if d[..8] != POOL_STATE_DISCRIMINATOR {
            return Err(DecodeError::Discriminator("PoolState"));
        }
        Ok(Self {
            amm_config: pubkey(d, 8),
            pool_creator: pubkey(d, 40),
            token_0_vault: pubkey(d, 72),
            token_1_vault: pubkey(d, 104),
            lp_mint: pubkey(d, 136),
            token_0_mint: pubkey(d, 168),
            token_1_mint: pubkey(d, 200),
            token_0_program: pubkey(d, 232),
            token_1_program: pubkey(d, 264),
            observation_key: pubkey(d, 296),
            auth_bump: d[328],
            status: d[329],
            // d[330] lp_mint_decimals
            mint_0_decimals: d[331],
            mint_1_decimals: d[332],
            // 333 lp_supply
            protocol_fees_token_0: u64_at(d, 341),
            protocol_fees_token_1: u64_at(d, 349),
            fund_fees_token_0: u64_at(d, 357),
            fund_fees_token_1: u64_at(d, 365),
            open_time: u64_at(d, 373),
            // 381 recent_epoch
            creator_fee_on: d[389],
            enable_creator_fee: d[390] != 0,
            // 391..397 padding1
            creator_fees_token_0: u64_at(d, 397),
            creator_fees_token_1: u64_at(d, 405),
            fee_model: d[POOL_STATE_FEE_MODEL_OFFSET],
            reward_model: d[POOL_STATE_REWARD_MODEL_OFFSET],
            // 415..421 padding2
            reward_rate_snapshot: u64_at(d, POOL_STATE_REWARD_RATE_SNAPSHOT_OFFSET),
        })
    }

    /// Mirrors `instructions/swap_base_input.rs` `swap_fee_model`: the pool and its
    /// AmmConfig must carry the same fee model, and it must be legacy or V1. `None` where
    /// the program fails with `FeeModelMismatch`.
    pub fn swap_fee_model(&self, amm_config: &AmmConfig) -> Option<u8> {
        if self.fee_model != amm_config.fee_model {
            return None;
        }
        match self.fee_model {
            FEE_MODEL_LEGACY | FEE_MODEL_V1 => Some(self.fee_model),
            _ => None,
        }
    }

    /// Mirrors `PoolState::v1_rates`: the rates of one V1 swap. LP and protocol rates are
    /// the config's rates of that side (together at most
    /// `MAX_TOTAL_FEE_RATE - MAX_REWARD_RATE`, 5%); the reward rate is the pool's own,
    /// fixed at creation: 0 for a Standard pool, else `reward_rate_snapshot` as is (at most
    /// `MAX_REWARD_RATE`, 5%). Nothing is reduced to fit, and no config value (not even
    /// `max_reward_rate`, which only limits NEW pools) alters a pool's reward rate. `None`
    /// where the program fails: `lp + protocol` overflows or is above its limit
    /// (`FeeRateAboveCap`), or the reward is above its ceiling (`InvalidRewardRate`).
    pub fn v1_rates(&self, amm_config: &AmmConfig, is_buy: bool) -> Option<V1Rates> {
        let (lp, protocol) = if is_buy {
            (amm_config.buy_lp_fee_rate, amm_config.buy_protocol_fee_rate)
        } else {
            (
                amm_config.sell_lp_fee_rate,
                amm_config.sell_protocol_fee_rate,
            )
        };
        let cap = crate::math::MAX_TOTAL_FEE_RATE - crate::math::MAX_REWARD_RATE;
        let base = lp.checked_add(protocol)?;
        if base > cap {
            return None;
        }
        let reward = if self.reward_model == REWARD_MODEL_STANDARD {
            0
        } else {
            self.reward_rate_snapshot
        };
        if reward > crate::math::MAX_REWARD_RATE {
            return None;
        }
        Some(V1Rates {
            lp,
            protocol,
            reward,
        })
    }

    /// Mirrors `PoolState::get_status_by_bit(PoolStatusBitIndex::Swap)`.
    pub fn swap_enabled(&self) -> bool {
        self.status & STATUS_SWAP_DISABLED_BIT == 0
    }

    /// Mirrors `PoolState::vault_amount_without_fee`: vault balances minus the accrued
    /// protocol + fund + creator fees (checked; `InsufficientVault` on underflow).
    pub fn vault_amount_without_fee(&self, vault_0: u64, vault_1: u64) -> Option<(u64, u64)> {
        let fees_0 = self
            .protocol_fees_token_0
            .checked_add(self.fund_fees_token_0)?
            .checked_add(self.creator_fees_token_0)?;
        let fees_1 = self
            .protocol_fees_token_1
            .checked_add(self.fund_fees_token_1)?
            .checked_add(self.creator_fees_token_1)?;
        Some((vault_0.checked_sub(fees_0)?, vault_1.checked_sub(fees_1)?))
    }

    /// Mirrors `PoolState::is_creator_fee_on_input`. `None` = invalid `creator_fee_on`.
    pub fn is_creator_fee_on_input(&self, zero_for_one: bool) -> Option<bool> {
        match self.creator_fee_on {
            0 => Some(true),
            1 => Some(zero_for_one),
            2 => Some(!zero_for_one),
            _ => None,
        }
    }

    /// Mirrors `PoolState::adjust_creator_fee_rate`.
    pub fn adjust_creator_fee_rate(&self, creator_fee_rate: u64) -> u64 {
        if self.enable_creator_fee {
            creator_fee_rate
        } else {
            0
        }
    }
}

/// The `AmmConfig` fee rates (all in 1e-6 units).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AmmConfig {
    pub trade_fee_rate: u64,
    pub protocol_fee_rate: u64,
    pub fund_fee_rate: u64,
    pub creator_fee_rate: u64,
    /// DEEP V1: [`FEE_MODEL_LEGACY`] (the four rates above apply) or [`FEE_MODEL_V1`] (the
    /// rates below apply; the ones above are a mirror no V1 code path reads).
    pub fee_model: u8,
    /// DEEP V1, absolute rates per 1e6 (not shares of a trade fee). "Buy" = the input is
    /// the pool's quote token, "sell" = the output is.
    pub buy_lp_fee_rate: u64,
    pub buy_protocol_fee_rate: u64,
    pub sell_lp_fee_rate: u64,
    pub sell_protocol_fee_rate: u64,
    /// DEEP V1: the CURRENT maximum reward rate of a NEW pool. Decoded for completeness;
    /// it never influences a quote. A pool's reward rate is its own
    /// (`PoolState::reward_rate_snapshot`), fixed when the pool is created.
    pub max_reward_rate: u64,
}

impl AmmConfig {
    pub fn decode(d: &[u8]) -> Result<Self, DecodeError> {
        if d.len() < AMM_CONFIG_LEN {
            return Err(DecodeError::TooShort("AmmConfig", d.len()));
        }
        if d[..8] != AMM_CONFIG_DISCRIMINATOR {
            return Err(DecodeError::Discriminator("AmmConfig"));
        }
        // 8 bump, 9 disable_create_pool, 10 index (u16)
        Ok(Self {
            trade_fee_rate: u64_at(d, 12),
            protocol_fee_rate: u64_at(d, 20),
            fund_fee_rate: u64_at(d, 28),
            // 36 create_pool_fee, 44 protocol_owner, 76 fund_owner
            creator_fee_rate: u64_at(d, 108),
            // 116 creator_fee_share_rate: only applied when creator fees are settled
            // (moved into protocol_fees_*), never during a swap.
            fee_model: d[AMM_CONFIG_FEE_MODEL_OFFSET],
            // 125..132 padding0
            buy_lp_fee_rate: u64_at(d, AMM_CONFIG_BUY_LP_FEE_RATE_OFFSET),
            buy_protocol_fee_rate: u64_at(d, AMM_CONFIG_BUY_PROTOCOL_FEE_RATE_OFFSET),
            sell_lp_fee_rate: u64_at(d, AMM_CONFIG_SELL_LP_FEE_RATE_OFFSET),
            sell_protocol_fee_rate: u64_at(d, AMM_CONFIG_SELL_PROTOCOL_FEE_RATE_OFFSET),
            max_reward_rate: u64_at(d, AMM_CONFIG_MAX_REWARD_RATE_OFFSET),
            // 172..236 padding
        })
    }
}

/// SPL Token / Token-2022 account `amount` (offset 64 in both programs).
pub fn token_account_amount(d: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(64..72)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha8(s: &str) -> [u8; 8] {
        use sha2::{Digest, Sha256};
        Sha256::digest(s.as_bytes())[..8].try_into().unwrap()
    }

    #[test]
    fn discriminators_match_anchor() {
        assert_eq!(POOL_STATE_DISCRIMINATOR, sha8("account:PoolState"));
        assert_eq!(AMM_CONFIG_DISCRIMINATOR, sha8("account:AmmConfig"));
    }

    #[test]
    fn rejects_wrong_discriminator_and_short_data() {
        let mut d = vec![0u8; POOL_STATE_LEN];
        assert_eq!(
            PoolState::decode(&d),
            Err(DecodeError::Discriminator("PoolState"))
        );
        d[..8].copy_from_slice(&POOL_STATE_DISCRIMINATOR);
        assert!(PoolState::decode(&d).is_ok());
        assert_eq!(
            PoolState::decode(&d[..100]),
            Err(DecodeError::TooShort("PoolState", 100))
        );
        let mut c = vec![0u8; AMM_CONFIG_LEN];
        c[..8].copy_from_slice(&POOL_STATE_DISCRIMINATOR);
        assert_eq!(
            AmmConfig::decode(&c),
            Err(DecodeError::Discriminator("AmmConfig"))
        );
    }

    #[test]
    fn decodes_fields_at_their_offsets() {
        let mut d = vec![0u8; POOL_STATE_LEN];
        d[..8].copy_from_slice(&POOL_STATE_DISCRIMINATOR);
        d[72..104].copy_from_slice(&[7u8; 32]);
        d[329] = STATUS_SWAP_DISABLED_BIT | 1;
        d[341..349].copy_from_slice(&11u64.to_le_bytes());
        d[365..373].copy_from_slice(&22u64.to_le_bytes());
        d[373..381].copy_from_slice(&33u64.to_le_bytes());
        d[389] = 2;
        d[390] = 1;
        d[405..413].copy_from_slice(&44u64.to_le_bytes());
        let p = PoolState::decode(&d).unwrap();
        assert_eq!(p.token_0_vault, Pubkey::new_from_array([7u8; 32]));
        assert!(!p.swap_enabled());
        assert_eq!(p.protocol_fees_token_0, 11);
        assert_eq!(p.fund_fees_token_1, 22);
        assert_eq!(p.open_time, 33);
        assert_eq!(p.creator_fee_on, 2);
        assert!(p.enable_creator_fee);
        assert_eq!(p.creator_fees_token_1, 44);
        // reserves exclude accrued fees: token0 -11, token1 -(22+44)
        assert_eq!(p.vault_amount_without_fee(100, 100), Some((89, 34)));
        assert_eq!(p.vault_amount_without_fee(10, 100), None);

        let mut c = vec![0u8; AMM_CONFIG_LEN];
        c[..8].copy_from_slice(&AMM_CONFIG_DISCRIMINATOR);
        c[12..20].copy_from_slice(&3_000u64.to_le_bytes());
        c[20..28].copy_from_slice(&333_333u64.to_le_bytes());
        c[28..36].copy_from_slice(&5u64.to_le_bytes());
        c[108..116].copy_from_slice(&1_000u64.to_le_bytes());
        let cfg = AmmConfig::decode(&c).unwrap();
        assert_eq!(
            cfg,
            AmmConfig {
                trade_fee_rate: 3_000,
                protocol_fee_rate: 333_333,
                fund_fee_rate: 5,
                creator_fee_rate: 1_000,
                // a pre-V1 account (zero padding) is a legacy config with every V1 rate 0
                ..Default::default()
            }
        );
        assert_eq!(cfg.fee_model, FEE_MODEL_LEGACY);
        assert_eq!(
            (p.fee_model, p.reward_model, p.reward_rate_snapshot),
            (FEE_MODEL_LEGACY, REWARD_MODEL_STANDARD, 0)
        );
    }

    /// The V1 fields sit at the offsets published by `states/pool.rs` and
    /// `states/config.rs`, between the bytes of their neighbours.
    #[test]
    fn decodes_v1_fields_at_their_offsets() {
        let mut d = vec![0u8; POOL_STATE_LEN];
        d[..8].copy_from_slice(&POOL_STATE_DISCRIMINATOR);
        d[405..413].copy_from_slice(&[0x11u8; 8]); // creator_fees_token_1
        d[413] = 0xA1;
        d[414] = 0xB2;
        d[415..421].copy_from_slice(&[0xEEu8; 6]); // padding2 is not read
        d[421..429].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        d[429..].fill(0xEE); // padding is not read
        let p = PoolState::decode(&d).unwrap();
        assert_eq!(p.creator_fees_token_1, 0x1111_1111_1111_1111);
        assert_eq!(p.fee_model, 0xA1);
        assert_eq!(p.reward_model, 0xB2);
        assert_eq!(p.reward_rate_snapshot, 0x0807_0605_0403_0201);
        assert_eq!(
            (
                POOL_STATE_FEE_MODEL_OFFSET,
                POOL_STATE_REWARD_MODEL_OFFSET,
                POOL_STATE_REWARD_RATE_SNAPSHOT_OFFSET
            ),
            (413, 414, 421)
        );

        let mut c = vec![0u8; AMM_CONFIG_LEN];
        c[..8].copy_from_slice(&AMM_CONFIG_DISCRIMINATOR);
        c[108..116].copy_from_slice(&10u64.to_le_bytes());
        c[116..124].copy_from_slice(&11u64.to_le_bytes()); // creator_fee_share_rate, not read
        c[124] = FEE_MODEL_V1;
        c[125..132].copy_from_slice(&[0xEEu8; 7]); // padding0 is not read
        for (i, off) in [132usize, 140, 148, 156, 164].into_iter().enumerate() {
            c[off..off + 8].copy_from_slice(&(12 + i as u64).to_le_bytes());
        }
        c[172..].fill(0xEE); // padding is not read
        let cfg = AmmConfig::decode(&c).unwrap();
        assert_eq!(
            cfg,
            AmmConfig {
                trade_fee_rate: 0,
                protocol_fee_rate: 0,
                fund_fee_rate: 0,
                creator_fee_rate: 10,
                fee_model: FEE_MODEL_V1,
                buy_lp_fee_rate: 12,
                buy_protocol_fee_rate: 13,
                sell_lp_fee_rate: 14,
                sell_protocol_fee_rate: 15,
                max_reward_rate: 16,
            }
        );
        assert_eq!(
            [
                AMM_CONFIG_FEE_MODEL_OFFSET,
                AMM_CONFIG_BUY_LP_FEE_RATE_OFFSET,
                AMM_CONFIG_BUY_PROTOCOL_FEE_RATE_OFFSET,
                AMM_CONFIG_SELL_LP_FEE_RATE_OFFSET,
                AMM_CONFIG_SELL_PROTOCOL_FEE_RATE_OFFSET,
                AMM_CONFIG_MAX_REWARD_RATE_OFFSET,
            ],
            [124, 132, 140, 148, 156, 164]
        );
    }

    fn v1_config() -> AmmConfig {
        AmmConfig {
            fee_model: FEE_MODEL_V1,
            buy_lp_fee_rate: 1_000,
            buy_protocol_fee_rate: 2_500,
            sell_lp_fee_rate: 1_000,
            sell_protocol_fee_rate: 6_500,
            max_reward_rate: 50_000,
            ..Default::default()
        }
    }

    /// Mirrors `states/pool.rs` `v1_rates_pick_the_side_and_never_exceed_the_cap`.
    #[test]
    fn v1_rates_pick_the_side_and_never_exceed_the_cap() {
        let cfg = v1_config();
        let pool = PoolState {
            fee_model: FEE_MODEL_V1,
            reward_model: REWARD_MODEL_CREATOR,
            reward_rate_snapshot: 10_000,
            creator_fee_on: 1,
            enable_creator_fee: true,
            ..Default::default()
        };
        let buy = pool.v1_rates(&cfg, true).unwrap();
        let sell = pool.v1_rates(&cfg, false).unwrap();
        assert_eq!((buy.lp, buy.protocol, buy.reward), (1_000, 2_500, 10_000));
        assert_eq!(
            (sell.lp, sell.protocol, sell.reward),
            (1_000, 6_500, 10_000)
        );
        assert_eq!((buy.total(), sell.total()), (13_500, 17_500));

        // the pool's own LP + DEEP side follows the config; the reward rate never does. With
        // the config at its 5% limit the pool's 1% still comes on top, unreduced
        let mut high = cfg;
        high.sell_protocol_fee_rate = 49_000;
        assert_eq!(
            pool.v1_rates(&high, false),
            Some(V1Rates {
                lp: 1_000,
                protocol: 49_000,
                reward: 10_000
            })
        );
        // the buy side of the same config is unaffected
        assert_eq!(pool.v1_rates(&high, true).unwrap().total(), 13_500);
        // config at its 5% limit and the pool at the 5% reward ceiling: exactly the 10% cap
        let max = PoolState {
            reward_model: REWARD_MODEL_HOLDER,
            reward_rate_snapshot: 50_000,
            ..pool
        };
        assert_eq!(
            max.v1_rates(&high, false).unwrap().total(),
            crate::math::MAX_TOTAL_FEE_RATE
        );
        // the config's `max_reward_rate` only limits NEW pools: it changes nothing here
        let mut lowered = high;
        lowered.max_reward_rate = 100;
        assert_eq!(max.v1_rates(&lowered, false), max.v1_rates(&high, false));
        lowered.max_reward_rate = u64::MAX;
        assert_eq!(max.v1_rates(&lowered, true), max.v1_rates(&high, true));
        // a config above its 5% limit (cannot be stored) is refused, not priced, even for
        // a Standard pool
        high.sell_protocol_fee_rate = 49_001;
        assert_eq!(pool.v1_rates(&high, false), None);
        let no_reward = PoolState {
            reward_model: REWARD_MODEL_STANDARD,
            ..pool
        };
        assert_eq!(no_reward.v1_rates(&high, false), None);
        high.sell_protocol_fee_rate = u64::MAX;
        assert_eq!(pool.v1_rates(&high, false), None);
        // a reward above its 5% ceiling (cannot be stored either) is refused on both sides
        let above = PoolState {
            reward_rate_snapshot: 50_001,
            ..max
        };
        assert_eq!(above.v1_rates(&cfg, true), None);
        assert_eq!(above.v1_rates(&cfg, false), None);

        // Standard: no reward, whatever the snapshot bytes say (even above the ceiling)
        let standard = PoolState {
            reward_model: REWARD_MODEL_STANDARD,
            ..pool
        };
        assert_eq!(standard.v1_rates(&cfg, false).unwrap().total(), 7_500);
        assert_eq!(standard.v1_rates(&cfg, true).unwrap().reward, 0);
        let standard = PoolState {
            reward_rate_snapshot: 50_001,
            ..standard
        };
        assert_eq!(standard.v1_rates(&cfg, true).unwrap().reward, 0);
        // Holder charges its own rate like Creator: 1 bps here
        let holder = PoolState {
            reward_model: REWARD_MODEL_HOLDER,
            reward_rate_snapshot: 100,
            ..pool
        };
        assert_eq!(holder.v1_rates(&cfg, true).unwrap().reward, 100);
        assert_eq!(holder.v1_rates(&cfg, false).unwrap().total(), 7_600);
    }

    /// Mirrors `swap_fee_model`: equal models only, and only 0 or 1.
    #[test]
    fn swap_fee_model_requires_agreement() {
        let pool = |m: u8| PoolState {
            fee_model: m,
            ..Default::default()
        };
        let cfg = |m: u8| AmmConfig {
            fee_model: m,
            ..Default::default()
        };
        assert_eq!(pool(0).swap_fee_model(&cfg(0)), Some(FEE_MODEL_LEGACY));
        assert_eq!(pool(1).swap_fee_model(&cfg(1)), Some(FEE_MODEL_V1));
        assert_eq!(pool(0).swap_fee_model(&cfg(1)), None);
        assert_eq!(pool(1).swap_fee_model(&cfg(0)), None);
        assert_eq!(pool(2).swap_fee_model(&cfg(2)), None);
    }

    #[test]
    fn creator_fee_side() {
        let mut p = PoolState {
            creator_fee_on: 0,
            ..Default::default()
        };
        assert_eq!(p.is_creator_fee_on_input(true), Some(true));
        assert_eq!(p.is_creator_fee_on_input(false), Some(true));
        p.creator_fee_on = 1;
        assert_eq!(p.is_creator_fee_on_input(true), Some(true));
        assert_eq!(p.is_creator_fee_on_input(false), Some(false));
        p.creator_fee_on = 2;
        assert_eq!(p.is_creator_fee_on_input(true), Some(false));
        assert_eq!(p.is_creator_fee_on_input(false), Some(true));
        p.creator_fee_on = 3;
        assert_eq!(p.is_creator_fee_on_input(true), None);
        assert_eq!(p.adjust_creator_fee_rate(1_000), 0);
        p.enable_creator_fee = true;
        assert_eq!(p.adjust_creator_fee_rate(1_000), 1_000);
    }
}
