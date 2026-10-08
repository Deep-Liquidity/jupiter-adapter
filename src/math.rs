//! DeepSwap swap math, ported from `programs/deep-amm` (itself byte-identical to
//! raydium-cp-swap `programs/cp-swap/src/curve/*` at commit b3187ae5).
//!
//! Every function mirrors one on-chain function. The only intentional difference is that
//! places where the program would `unwrap()` / panic (or overflow under
//! `overflow-checks = true`) return `None` here, so a quote errors exactly where the
//! on-chain swap would fail. All arithmetic is integer (`u128`, checked); no floats.

/// `programs/deep-amm/src/curve/fees.rs` `FEE_RATE_DENOMINATOR_VALUE`.
pub const FEE_RATE_DENOMINATOR_VALUE: u64 = 1_000_000;

/// Mirrors `curve/fees.rs` `ceil_div` (private there): `(a * n + d - 1) / d`.
pub fn ceil_div(token_amount: u128, fee_numerator: u128, fee_denominator: u128) -> Option<u128> {
    if fee_denominator == 0 {
        return None;
    }
    token_amount
        .checked_mul(fee_numerator)?
        .checked_add(fee_denominator)?
        .checked_sub(1)?
        .checked_div(fee_denominator)
}

/// Mirrors `curve/fees.rs` `floor_div`: `a * n / d`.
pub fn floor_div(token_amount: u128, fee_numerator: u128, fee_denominator: u128) -> Option<u128> {
    if fee_denominator == 0 {
        return None;
    }
    token_amount
        .checked_mul(fee_numerator)?
        .checked_div(fee_denominator)
}

/// Mirrors `Fees::trading_fee` (rounds up, in the pool's favour).
pub fn trading_fee(amount: u128, trade_fee_rate: u64) -> Option<u128> {
    ceil_div(
        amount,
        u128::from(trade_fee_rate),
        u128::from(FEE_RATE_DENOMINATOR_VALUE),
    )
}

/// Mirrors `Fees::protocol_fee` (rounds down).
pub fn protocol_fee(amount: u128, protocol_fee_rate: u64) -> Option<u128> {
    floor_div(
        amount,
        u128::from(protocol_fee_rate),
        u128::from(FEE_RATE_DENOMINATOR_VALUE),
    )
}

/// Mirrors `Fees::fund_fee` (rounds down).
pub fn fund_fee(amount: u128, fund_fee_rate: u64) -> Option<u128> {
    floor_div(
        amount,
        u128::from(fund_fee_rate),
        u128::from(FEE_RATE_DENOMINATOR_VALUE),
    )
}

/// Mirrors `Fees::creator_fee` (rounds up).
pub fn creator_fee(amount: u128, creator_fee_rate: u64) -> Option<u128> {
    ceil_div(
        amount,
        u128::from(creator_fee_rate),
        u128::from(FEE_RATE_DENOMINATOR_VALUE),
    )
}

/// Mirrors `Fees::split_creator_fee`: `total_fee * creator / (trade + creator)`, floored.
/// On chain `trade_fee_rate + creator_fee_rate` is an unchecked `u64` add; with
/// `overflow-checks = true` an overflow panics, so it is `None` here.
pub fn split_creator_fee(
    total_fee: u128,
    trade_fee_rate: u64,
    creator_fee_rate: u64,
) -> Option<u128> {
    floor_div(
        total_fee,
        u128::from(creator_fee_rate),
        u128::from(trade_fee_rate.checked_add(creator_fee_rate)?),
    )
}

/// Mirrors `ConstantProductCurve::swap_base_input_without_fees`
/// (`curve/constant_product.rs`): `dy = dx * y / (x + dx)`, floored. The program
/// `unwrap()`s each step, so any failure is `None`.
pub fn swap_base_input_without_fees(
    input_amount: u128,
    input_vault_amount: u128,
    output_vault_amount: u128,
) -> Option<u128> {
    let numerator = input_amount.checked_mul(output_vault_amount)?;
    let denominator = input_vault_amount.checked_add(input_amount)?;
    numerator.checked_div(denominator)
}

/// Mirrors `curve/calculator.rs` `SwapResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapResult {
    pub new_input_vault_amount: u128,
    pub new_output_vault_amount: u128,
    pub input_amount: u128,
    pub output_amount: u128,
    pub trade_fee: u128,
    pub protocol_fee: u128,
    pub fund_fee: u128,
    pub creator_fee: u128,
}

/// Mirrors `CurveCalculator::swap_base_input` (`curve/calculator.rs`) line for line.
///
/// * creator fee on input: one fee of `trade_fee_rate + creator_fee_rate` is taken from the
///   input (ceil), then split: creator part = floor(total * creator / (trade + creator)),
///   trade part = the rest;
/// * creator fee on output: the trade fee is taken from the input (ceil), and the creator
///   fee is taken from the curve output (ceil);
/// * protocol and fund fees are floored shares of the trade fee (they stay in the vault
///   but are excluded from the reserves).
#[allow(clippy::too_many_arguments)]
pub fn swap_base_input(
    input_amount: u128,
    input_vault_amount: u128,
    output_vault_amount: u128,
    trade_fee_rate: u64,
    creator_fee_rate: u64,
    protocol_fee_rate: u64,
    fund_fee_rate: u64,
    is_creator_fee_on_input: bool,
) -> Option<SwapResult> {
    let mut creator_fee_amount = 0u128;
    let trade_fee_amount: u128;

    let input_amount_less_fees = if is_creator_fee_on_input {
        let total_fee = trading_fee(input_amount, trade_fee_rate.checked_add(creator_fee_rate)?)?;
        creator_fee_amount = split_creator_fee(total_fee, trade_fee_rate, creator_fee_rate)?;
        trade_fee_amount = total_fee.checked_sub(creator_fee_amount)?;
        input_amount.checked_sub(total_fee)?
    } else {
        trade_fee_amount = trading_fee(input_amount, trade_fee_rate)?;
        input_amount.checked_sub(trade_fee_amount)?
    };
    let protocol_fee_amount = protocol_fee(trade_fee_amount, protocol_fee_rate)?;
    let fund_fee_amount = fund_fee(trade_fee_amount, fund_fee_rate)?;

    let output_amount_swapped = swap_base_input_without_fees(
        input_amount_less_fees,
        input_vault_amount,
        output_vault_amount,
    )?;

    let output_amount = if is_creator_fee_on_input {
        output_amount_swapped
    } else {
        creator_fee_amount = creator_fee(output_amount_swapped, creator_fee_rate)?;
        output_amount_swapped.checked_sub(creator_fee_amount)?
    };

    Some(SwapResult {
        new_input_vault_amount: input_vault_amount.checked_add(input_amount_less_fees)?,
        new_output_vault_amount: output_vault_amount.checked_sub(output_amount_swapped)?,
        input_amount,
        output_amount,
        trade_fee: trade_fee_amount,
        protocol_fee: protocol_fee_amount,
        fund_fee: fund_fee_amount,
        creator_fee: creator_fee_amount,
    })
}

// ---------------------------------------------------------------------------------------
// DEEP V1 side-dependent fees. Ported line for line from DEEP's own
// `programs/deep-amm/src/curve/v1.rs` (not from upstream cp-swap). Legacy pools
// (`fee_model == 0`) never reach this code; everything above is untouched.
//
// A V1 pool has a QUOTE token (SOL on TOKEN/SOL) and every fee is taken in it:
//   * buy  (quote in):  the fee comes off the input before the curve;
//   * sell (quote out): the fee comes off the curve's output before the trader is paid.
// The fee is one total at `lp + protocol + reward` (rates per 1e6). The total rounds UP;
// the LP and reward parts are floored and the protocol (DEEP) part takes the remainder, so
// `lp + protocol + reward == total` to the unit. Any overflow, division by zero or
// impossible trade is `None`.
// ---------------------------------------------------------------------------------------

/// `curve/v1.rs` `MAX_TOTAL_FEE_RATE` (`deep_keys::MAX_TOTAL_FEE_RATE`): THE cap, the
/// largest total rate (LP + protocol + reward) the math accepts: 10% per side, per 1e6.
pub const MAX_TOTAL_FEE_RATE: u64 = 100_000;
/// `curve/v1.rs` `MAX_REWARD_RATE` (`deep_keys::MAX_REWARD_RATE`): the ceiling of a pool's
/// reward rate, 5%. Checked by `state::PoolState::v1_rates` (and on chain where a pool is
/// created), not by the math; it also limits LP + protocol to
/// `MAX_TOTAL_FEE_RATE - MAX_REWARD_RATE`.
pub const MAX_REWARD_RATE: u64 = 50_000;

/// Mirrors `curve/v1.rs` `FeeParts`: one fee total and its three parts;
/// `lp + protocol + reward == total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeParts {
    pub total: u128,
    pub lp: u128,
    pub protocol: u128,
    pub reward: u128,
}

/// Mirrors `curve/v1.rs` `total_rate`: `lp_rate + protocol_rate + reward_rate`, or `None`
/// above the hard cap: the one check of the 10% total per side.
pub fn total_rate(lp_rate: u64, protocol_rate: u64, reward_rate: u64) -> Option<u64> {
    let total = lp_rate
        .checked_add(protocol_rate)?
        .checked_add(reward_rate)?;
    if total > MAX_TOTAL_FEE_RATE {
        return None;
    }
    Some(total)
}

/// Mirrors `curve/v1.rs` `fee_on`: `ceil(amount * total_rate / 1e6)`, the fee on a gross
/// amount.
pub fn fee_on(amount: u128, total_rate: u64) -> Option<u128> {
    let d = u128::from(FEE_RATE_DENOMINATOR_VALUE);
    amount
        .checked_mul(u128::from(total_rate))?
        .checked_add(d - 1)?
        .checked_div(d)
}

/// Mirrors `curve/v1.rs` `gross_up`: `ceil(net * 1e6 / (1e6 - total_rate))`, the smallest
/// gross amount that leaves at least `net` after the fee.
pub fn gross_up(net: u128, total_rate: u64) -> Option<u128> {
    if total_rate == 0 {
        return Some(net);
    }
    let d = u128::from(FEE_RATE_DENOMINATOR_VALUE);
    let keep = d.checked_sub(u128::from(total_rate))?;
    if keep == 0 {
        return None;
    }
    net.checked_mul(d)?.checked_add(keep - 1)?.checked_div(keep)
}

/// Mirrors `curve/v1.rs` `split_fee`: LP and reward parts floored pro rata, the protocol
/// part takes the remainder.
pub fn split_fee(
    total: u128,
    lp_rate: u64,
    protocol_rate: u64,
    reward_rate: u64,
) -> Option<FeeParts> {
    let rate = u128::from(total_rate(lp_rate, protocol_rate, reward_rate)?);
    if rate == 0 {
        // no rate, no fee
        if total != 0 {
            return None;
        }
        return Some(FeeParts {
            total: 0,
            lp: 0,
            protocol: 0,
            reward: 0,
        });
    }
    let lp = total.checked_mul(u128::from(lp_rate))?.checked_div(rate)?;
    let reward = total
        .checked_mul(u128::from(reward_rate))?
        .checked_div(rate)?;
    let protocol = total.checked_sub(lp)?.checked_sub(reward)?;
    Some(FeeParts {
        total,
        lp,
        protocol,
        reward,
    })
}

/// `curve/v1.rs` `curve_out`: `floor(input * out_reserve / (in_reserve + input))`.
fn curve_out(input: u128, in_reserve: u128, out_reserve: u128) -> Option<u128> {
    input
        .checked_mul(out_reserve)?
        .checked_div(in_reserve.checked_add(input)?)
}

/// `curve/v1.rs` `curve_in`: `ceil(in_reserve * output / (out_reserve - output))`; `None`
/// unless `output < out_reserve`.
fn curve_in(output: u128, in_reserve: u128, out_reserve: u128) -> Option<u128> {
    let left = out_reserve.checked_sub(output)?;
    if left == 0 {
        return None;
    }
    in_reserve
        .checked_mul(output)?
        .checked_add(left - 1)?
        .checked_div(left)
}

/// `curve/v1.rs` `result`. `SwapResult` keeps upstream's field meanings as far as they go:
/// `trade_fee` is the pool fee (LP + protocol), `protocol_fee` DEEP's part of it,
/// `creator_fee` the reward part, `fund_fee` always 0.
fn v1_result(
    new_input_vault_amount: u128,
    new_output_vault_amount: u128,
    input_amount: u128,
    output_amount: u128,
    fee: FeeParts,
) -> Option<SwapResult> {
    Some(SwapResult {
        new_input_vault_amount,
        new_output_vault_amount,
        input_amount,
        output_amount,
        trade_fee: fee.lp.checked_add(fee.protocol)?,
        protocol_fee: fee.protocol,
        fund_fee: 0,
        creator_fee: fee.reward,
    })
}

/// Mirrors `curve/v1.rs` `swap_base_input_v1`. `is_buy`: the input is the quote token.
///
/// * buy:  `total = ceil(in * T / 1e6)`, `net = in - total`,
///   `out = floor(net * out_reserve / (in_reserve + net))`;
/// * sell: `gross = floor(in * out_reserve / (in_reserve + in))`,
///   `total = ceil(gross * T / 1e6)`, `out = gross - total`.
///
/// `new_*_vault_amount` are the reserves the invariant check uses: they count no part of
/// the fee.
pub fn swap_base_input_v1(
    input_amount: u128,
    input_vault_amount: u128,
    output_vault_amount: u128,
    lp_rate: u64,
    protocol_rate: u64,
    reward_rate: u64,
    is_buy: bool,
) -> Option<SwapResult> {
    let rate = total_rate(lp_rate, protocol_rate, reward_rate)?;
    if is_buy {
        let total = fee_on(input_amount, rate)?;
        let fee = split_fee(total, lp_rate, protocol_rate, reward_rate)?;
        let net = input_amount.checked_sub(total)?;
        let out = curve_out(net, input_vault_amount, output_vault_amount)?;
        v1_result(
            input_vault_amount.checked_add(net)?,
            output_vault_amount.checked_sub(out)?,
            input_amount,
            out,
            fee,
        )
    } else {
        let gross = curve_out(input_amount, input_vault_amount, output_vault_amount)?;
        let total = fee_on(gross, rate)?;
        let fee = split_fee(total, lp_rate, protocol_rate, reward_rate)?;
        v1_result(
            input_vault_amount.checked_add(input_amount)?,
            output_vault_amount.checked_sub(gross)?,
            input_amount,
            gross.checked_sub(total)?,
            fee,
        )
    }
}

/// Mirrors `curve/v1.rs` `swap_base_output_v1`: `output_amount` is what the trader
/// receives. The adapter itself is ExactIn only; this port exists so the cross-language
/// vectors pin both directions.
///
/// * buy  (exact tokens out): `swapped = ceil(in_reserve * out / (out_reserve - out))`,
///   `in = ceil(swapped * 1e6 / (1e6 - T))`, `total = in - swapped`;
/// * sell (exact quote out):  `gross = ceil(out * 1e6 / (1e6 - T))`, `total = gross - out`,
///   `in = ceil(in_reserve * gross / (out_reserve - gross))`; `gross` must be less than the
///   output reserve.
pub fn swap_base_output_v1(
    output_amount: u128,
    input_vault_amount: u128,
    output_vault_amount: u128,
    lp_rate: u64,
    protocol_rate: u64,
    reward_rate: u64,
    is_buy: bool,
) -> Option<SwapResult> {
    let rate = total_rate(lp_rate, protocol_rate, reward_rate)?;
    if is_buy {
        let swapped = curve_in(output_amount, input_vault_amount, output_vault_amount)?;
        let input = gross_up(swapped, rate)?;
        let fee = split_fee(
            input.checked_sub(swapped)?,
            lp_rate,
            protocol_rate,
            reward_rate,
        )?;
        v1_result(
            input_vault_amount.checked_add(swapped)?,
            output_vault_amount.checked_sub(output_amount)?,
            input,
            output_amount,
            fee,
        )
    } else {
        let gross = gross_up(output_amount, rate)?;
        let fee = split_fee(
            gross.checked_sub(output_amount)?,
            lp_rate,
            protocol_rate,
            reward_rate,
        )?;
        let input = curve_in(gross, input_vault_amount, output_vault_amount)?;
        v1_result(
            input_vault_amount.checked_add(input)?,
            output_vault_amount.checked_sub(gross)?,
            input,
            output_amount,
            fee,
        )
    }
}

/// Mirrors the unit tests of `programs/deep-amm/src/curve/v1.rs` (the property tests there
/// use proptest; here a deterministic sweep checks the same exact-in properties).
#[cfg(test)]
mod v1_tests {
    use super::*;

    const SOL: u128 = 1_000_000_000;
    // The owner's V1 schedule (per 1e6).
    const LP: u64 = 1_000;
    const BUY: u64 = 2_500;
    const SELL: u64 = 6_500;
    const REWARD: u64 = 10_000;

    fn parts(r: &SwapResult) -> (u128, u128, u128) {
        (r.trade_fee - r.protocol_fee, r.protocol_fee, r.creator_fee)
    }

    /// docs/V1_FEES.md section 5, "Worked examples".
    #[test]
    fn worked_examples() {
        let (x, y) = (100 * SOL, 1_000_000_000_000_000u128);
        // buy, 1 SOL in, Standard: 3,500,000 = LP 1,000,000 + DEEP 2,500,000
        let r = swap_base_input_v1(SOL, x, y, LP, BUY, 0, true).unwrap();
        assert_eq!(parts(&r), (1_000_000, 2_500_000, 0));
        assert_eq!(r.new_input_vault_amount - x, 996_500_000);
        assert_eq!(r.output_amount, 996_500_000 * y / (x + 996_500_000));
        assert_eq!(r.fund_fee, 0);
        // buy, 1 SOL in, Creator: 13,500,000 = LP 1,000,000 + reward 10,000,000 + DEEP 2,500,000
        let r = swap_base_input_v1(SOL, x, y, LP, BUY, REWARD, true).unwrap();
        assert_eq!(parts(&r), (1_000_000, 2_500_000, 10_000_000));
        assert_eq!(r.new_input_vault_amount - x, 986_500_000);

        // sell with a gross output of exactly 1 SOL: tokens in t = y / 99 into (y, 100 SOL)
        // gives floor(t * 100 SOL / (y + t)) = 1 SOL when y is a multiple of 99.
        let y = 990_000_000_000_000u128;
        let t = y / 99;
        // Standard: 7,500,000 = LP 1,000,000 + DEEP 6,500,000; the trader receives 992,500,000
        let r = swap_base_input_v1(t, y, x, LP, SELL, 0, false).unwrap();
        assert_eq!(x - r.new_output_vault_amount, SOL);
        assert_eq!(parts(&r), (1_000_000, 6_500_000, 0));
        assert_eq!(r.output_amount, 992_500_000);
        // Holder: 17,500,000 = LP 1,000,000 + reward 10,000,000 + DEEP 6,500,000 -> 982,500,000
        let r = swap_base_input_v1(t, y, x, LP, SELL, REWARD, false).unwrap();
        assert_eq!(parts(&r), (1_000_000, 6_500_000, 10_000_000));
        assert_eq!(r.output_amount, 982_500_000);
    }

    #[test]
    fn rounding_boundaries() {
        // the total rounds up: 1 unit at 0.35% still pays 1
        assert_eq!(fee_on(1, 3_500), Some(1));
        assert_eq!(fee_on(0, 3_500), Some(0));
        assert_eq!(fee_on(1_000_000, 3_500), Some(3_500));
        assert_eq!(fee_on(1_000_001, 3_500), Some(3_501));
        // parts are floored, DEEP takes the remainder
        let f = split_fee(1, LP, BUY, REWARD).unwrap();
        assert_eq!((f.lp, f.reward, f.protocol), (0, 0, 1));
        let f = split_fee(27, LP, BUY, REWARD).unwrap(); // 27 * 1000/13500 = 2, 27 * 10000/13500 = 20
        assert_eq!((f.lp, f.reward, f.protocol), (2, 20, 5));
        let f = split_fee(26, LP, BUY, REWARD).unwrap(); // 1.92 -> 1, 19.25 -> 19, rest 6
        assert_eq!((f.lp, f.reward, f.protocol), (1, 19, 6));
        // gross_up is the exact inverse bound of fee_on
        assert_eq!(gross_up(996_500, 3_500), Some(1_000_000));
        assert_eq!(gross_up(996_501, 3_500), Some(1_000_002));
        assert_eq!(gross_up(5, 0), Some(5));
        // zero rates: no fee at all
        let r = swap_base_input_v1(1_000, 1_000, 1_000, 0, 0, 0, true).unwrap();
        assert_eq!((r.output_amount, r.trade_fee, r.creator_fee), (500, 0, 0));
        let r = swap_base_output_v1(500, 1_000, 1_000, 0, 0, 0, false).unwrap();
        assert_eq!((r.input_amount, r.trade_fee, r.creator_fee), (1_000, 0, 0));
        assert_eq!(split_fee(1, 0, 0, 0), None);
    }

    #[test]
    fn dust_and_reserve_edges() {
        // 1 unit buy: the whole unit is fee, nothing is priced, nothing comes out
        let r = swap_base_input_v1(1, 1_000, 1_000, LP, BUY, REWARD, true).unwrap();
        assert_eq!((r.output_amount, r.protocol_fee, r.creator_fee), (0, 1, 0));
        // 1 unit sell that prices to 0: no fee, nothing out
        let r = swap_base_input_v1(1, 1_000_000, 1_000, LP, SELL, REWARD, false).unwrap();
        assert_eq!((r.output_amount, r.trade_fee, r.creator_fee), (0, 0, 0));
        // empty reserves: no trade
        assert_eq!(swap_base_input_v1(0, 0, 10, LP, BUY, 0, true), None);
        // asking for the whole output reserve (or more) is impossible in both directions
        assert_eq!(
            swap_base_output_v1(1_000, 1_000, 1_000, LP, BUY, 0, true),
            None
        );
        assert_eq!(
            swap_base_output_v1(1_001, 1_000, 1_000, LP, BUY, 0, true),
            None
        );
        // sell: the GROSS output must fit, so the largest net output is below the reserve
        assert_eq!(
            swap_base_output_v1(999, 1_000, 1_000, LP, SELL, REWARD, false),
            None
        );
        assert!(swap_base_output_v1(981, 1_000, 1_000, LP, SELL, REWARD, false).is_some());
        // a huge input never empties the output side
        let r = swap_base_input_v1(u64::MAX as u128, 5 * SOL, 1_000_000, LP, BUY, REWARD, true)
            .unwrap();
        assert!(r.new_output_vault_amount > 0);
    }

    #[test]
    fn rates_above_the_cap_are_rejected() {
        // one cap: the total of a side is at most 10%
        assert_eq!(total_rate(1_000, 49_000, 50_000), Some(100_000));
        assert_eq!(total_rate(1_000, 49_001, 50_000), None);
        assert_eq!(total_rate(1_000, 49_000, 50_001), None);
        assert_eq!(total_rate(1_000, 6_500, 50_000), Some(57_500));
        assert_eq!(total_rate(u64::MAX, 1, 0), None);
        assert_eq!(total_rate(0, 0, u64::MAX), None);
        // the math checks the total only: each part's own limit is enforced by
        // `PoolState::v1_rates`
        assert_eq!(total_rate(1_000, 89_000, 10_000), Some(100_000));
        assert_eq!(MAX_REWARD_RATE, 50_000);
        // exactly 10%, worked: 1_000_000 in, total 100_000 = LP 1_000 + reward 50_000 +
        // DEEP 49_000
        let r = swap_base_input_v1(1_000_000, SOL, SOL, 1_000, 49_000, 50_000, true).unwrap();
        assert_eq!(parts(&r), (1_000, 49_000, 50_000));
        assert_eq!(r.new_input_vault_amount - SOL, 900_000);
        for is_buy in [true, false] {
            assert!(swap_base_input_v1(SOL, SOL, SOL, 1_000, 49_000, 50_000, is_buy).is_some());
            assert!(swap_base_output_v1(1_000, SOL, SOL, 1_000, 49_000, 50_000, is_buy).is_some());
            assert_eq!(
                swap_base_input_v1(SOL, SOL, SOL, 1_000, 49_001, 50_000, is_buy),
                None
            );
            assert_eq!(
                swap_base_output_v1(1_000, SOL, SOL, 1_000, 49_000, 50_001, is_buy),
                None
            );
            // near 100% and above
            assert_eq!(
                swap_base_input_v1(SOL, SOL, SOL, 999_999, 0, 0, is_buy),
                None
            );
            assert_eq!(
                swap_base_output_v1(1_000, SOL, SOL, 0, 1_000_000, 0, is_buy),
                None
            );
            assert_eq!(
                swap_base_input_v1(SOL, SOL, SOL, 0, 0, u64::MAX, is_buy),
                None
            );
        }
        assert_eq!(gross_up(1, 1_000_000), None);
        assert_eq!(gross_up(1, 1_000_001), None);
    }

    /// The exact-in properties of the on-chain proptest, over a deterministic sweep of
    /// amounts, reserves, rate splits and both sides.
    #[test]
    fn exact_in_properties_sweep() {
        let amounts: [u128; 7] = [
            1,
            2,
            999,
            1_000_000,
            SOL,
            123_456_789_012_345,
            u64::MAX as u128,
        ];
        let reserves: [u128; 5] = [1, 1_000, 5 * SOL, 158_400_000_000_000, u64::MAX as u128];
        let rates: [(u64, u64, u64); 10] = [
            (0, 0, 0),
            (LP, BUY, 0),
            (LP, BUY, REWARD),
            (LP, SELL, 0),
            (LP, SELL, REWARD),
            (1_000, 94_000, 5_000),
            (1, 0, 99_999),
            (LP, BUY, 100), // a 1 bps reward
            (LP, BUY, MAX_REWARD_RATE),
            (1_000, 49_000, MAX_REWARD_RATE), // exactly the 10% cap
        ];
        for &a in &amounts {
            for &x in &reserves {
                for &y in &reserves {
                    for &(lp, protocol, reward) in &rates {
                        for is_buy in [true, false] {
                            let r =
                                swap_base_input_v1(a, x, y, lp, protocol, reward, is_buy).unwrap();
                            let t = u128::from(lp + protocol + reward);
                            let (lp_fee, protocol_fee, reward_fee) = parts(&r);
                            let total = lp_fee + protocol_fee + reward_fee;
                            // k never decreases, even without counting the LP part (a
                            // product beyond u128 is larger than any x * y of two u64s)
                            if let Some(k) = r
                                .new_input_vault_amount
                                .checked_mul(r.new_output_vault_amount)
                            {
                                assert!(k >= x * y);
                            }
                            assert_eq!(r.input_amount, a);
                            assert_eq!(r.fund_fee, 0);
                            // the parts are floored pro rata, DEEP holds the remainder
                            match (total * u128::from(lp)).checked_div(t) {
                                Some(expected_lp) => {
                                    assert_eq!(lp_fee, expected_lp);
                                    assert_eq!(
                                        Some(reward_fee),
                                        (total * u128::from(reward)).checked_div(t)
                                    );
                                }
                                None => assert_eq!(total, 0),
                            }
                            if is_buy {
                                assert_eq!(total, (a * t).div_ceil(1_000_000));
                                let net = a - total;
                                assert_eq!(r.new_input_vault_amount, x + net);
                                assert_eq!(r.output_amount, net * y / (x + net));
                                assert_eq!(r.new_output_vault_amount, y - r.output_amount);
                            } else {
                                let gross = a * y / (x + a);
                                assert_eq!(total, (gross * t).div_ceil(1_000_000));
                                assert_eq!(r.output_amount, gross - total);
                                assert_eq!(r.new_input_vault_amount, x + a);
                                assert_eq!(r.new_output_vault_amount, y - gross);
                            }
                            // the trader never gets the whole reserve
                            assert!(r.output_amount < y);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // DeepSwap's live config: 0.30% trade fee, 1/3 of it to the protocol, no fund fee.
    const TRADE: u64 = 3_000;
    const PROTOCOL: u64 = 333_333;

    #[test]
    fn ceil_and_floor_div() {
        assert_eq!(ceil_div(0, 3_000, 1_000_000), Some(0));
        assert_eq!(ceil_div(1, 3_000, 1_000_000), Some(1));
        assert_eq!(ceil_div(1_000_000, 3_000, 1_000_000), Some(3_000));
        assert_eq!(ceil_div(1_000_001, 3_000, 1_000_000), Some(3_001));
        assert_eq!(floor_div(999, 333_333, 1_000_000), Some(332));
        assert_eq!(ceil_div(1, 1, 0), None);
        assert_eq!(floor_div(1, 1, 0), None);
        assert_eq!(ceil_div(u128::MAX, 2, 1), None);
    }

    #[test]
    fn constant_product_upstream_vectors() {
        // The same vectors as upstream `constant_product_swap_rounding`.
        let v: &[(u128, u128, u128, u128)] = &[
            (10, 4_000_000, 70_000_000_000, 174_999),
            (20, 30_000 - 20, 10_000, 6),
            (19, 30_000 - 20, 10_000, 6),
            (18, 30_000 - 20, 10_000, 6),
            (10, 20_000, 30_000, 14),
            (10, 20_000 - 9, 30_000, 14),
            (10, 20_000 - 10, 30_000, 15),
            (100, 60_000, 30_000, 49),
            (99, 60_000, 30_000, 49),
            (98, 60_000, 30_000, 48),
        ];
        for &(dx, x, y, dy) in v {
            assert_eq!(
                swap_base_input_without_fees(dx, x, y),
                Some(dy),
                "{dx} {x} {y}"
            );
        }
        // 0 input on an empty-ish pool divides by zero on chain -> None.
        assert_eq!(swap_base_input_without_fees(0, 0, 10), None);
    }

    /// Hand-computed: 1 SOL into a 4.95 SOL / 158.4M token pool (the devnet pool's shape),
    /// creator fee disabled (rate 0) and mode "both tokens" (fee on input path).
    ///   total_fee = ceil(1e9 * 3000 / 1e6) = 3_000_000
    ///   creator   = floor(3_000_000 * 0 / 3000) = 0, trade = 3_000_000
    ///   protocol  = floor(3_000_000 * 333_333 / 1e6) = 999_999
    ///   in_less   = 997_000_000
    ///   out       = floor(997e6 * 158_400e9 / (4.95e9 + 997e6)) = 26_555_372_456_700
    #[test]
    fn hand_vector_sol_in() {
        let r = swap_base_input(
            1_000_000_000,
            4_950_000_000,
            158_400_000_000_000,
            TRADE,
            0,
            PROTOCOL,
            0,
            true,
        )
        .unwrap();
        assert_eq!(r.trade_fee, 3_000_000);
        assert_eq!(r.creator_fee, 0);
        assert_eq!(r.protocol_fee, 999_999);
        assert_eq!(r.fund_fee, 0);
        // 997_000_000 * 158_400_000_000_000 = 157_924_800_000_000_000_000_000
        // / 5_947_000_000 -> floor
        assert_eq!(
            r.output_amount,
            157_924_800_000_000_000_000_000u128 / 5_947_000_000u128
        );
        assert_eq!(r.output_amount, 26_555_372_456_700);
        assert_eq!(r.new_input_vault_amount, 5_947_000_000);
        assert_eq!(
            r.new_output_vault_amount,
            158_400_000_000_000 - 26_555_372_456_700
        );
    }

    /// Reverse direction, hand-computed:
    ///   1_000_000_000 token units in (1_000 tokens), same pool
    ///   fee = ceil(1e9 * 3000 / 1e6) = 3_000_000, in_less = 997_000_000
    ///   out = floor(997e6 * 4.95e9 / (158_400e9 + 997e6)) = floor(4.935150e18 / 158_400_997_000_000)
    #[test]
    fn hand_vector_token_in() {
        let r = swap_base_input(
            1_000_000_000,
            158_400_000_000_000,
            4_950_000_000,
            TRADE,
            0,
            PROTOCOL,
            0,
            true,
        )
        .unwrap();
        assert_eq!(r.trade_fee, 3_000_000);
        assert_eq!(
            r.output_amount,
            4_935_150_000_000_000_000u128 / 158_400_997_000_000u128
        );
        assert_eq!(r.output_amount, 31_156);
    }

    #[test]
    fn dust_inputs() {
        // 1 unit: fee ceil(1*3000/1e6) = 1 -> nothing left to swap -> 0 out.
        let r = swap_base_input(1, 1_000, 1_000, TRADE, 0, PROTOCOL, 0, true).unwrap();
        assert_eq!((r.trade_fee, r.output_amount), (1, 0));
        // 2 units: fee 1, 1 swapped: floor(1 * 1000 / 1001) = 0.
        let r = swap_base_input(2, 1_000, 1_000, TRADE, 0, PROTOCOL, 0, true).unwrap();
        assert_eq!((r.trade_fee, r.output_amount), (1, 0));
        // 0 input: no fee, no output (the handler then rejects amount 0 itself).
        let r = swap_base_input(0, 1_000, 1_000, TRADE, 0, PROTOCOL, 0, true).unwrap();
        assert_eq!((r.trade_fee, r.output_amount), (0, 0));
    }

    #[test]
    fn reserve_draining_input_never_empties_output() {
        let y = 158_400_000_000_000u128;
        let r = swap_base_input(
            u64::MAX as u128,
            4_950_000_000,
            y,
            TRADE,
            0,
            PROTOCOL,
            0,
            true,
        )
        .unwrap();
        assert!(r.output_amount < y);
        assert!(r.new_output_vault_amount > 0);
        // invariant never decreases
        assert!(r.new_input_vault_amount * r.new_output_vault_amount >= 4_950_000_000u128 * y);
    }

    #[test]
    fn creator_fee_on_input_split() {
        // trade 2500 + creator 1000 = 3500 ppm on 1_000_000:
        // total = 3500; creator = floor(3500*1000/3500) = 1000; trade = 2500
        let r = swap_base_input(
            1_000_000, 10_000_000, 10_000_000, 2_500, 1_000, 120_000, 40_000, true,
        )
        .unwrap();
        assert_eq!((r.trade_fee, r.creator_fee), (2_500, 1_000));
        assert_eq!(r.protocol_fee, 300); // floor(2500 * 0.12)
        assert_eq!(r.fund_fee, 100); // floor(2500 * 0.04)
        // in_less = 996_500; out = floor(996_500 * 1e7 / 10_996_500)
        assert_eq!(r.output_amount, 9_965_000_000_000u128 / 10_996_500);
        assert_eq!(r.output_amount, 906_197);

        // rounding of the split: total = ceil(333 * 3500 / 1e6) = 2;
        // creator = floor(2 * 1000 / 3500) = 0; trade = 2
        let r = swap_base_input(333, 10_000_000, 10_000_000, 2_500, 1_000, 0, 0, true).unwrap();
        assert_eq!((r.trade_fee, r.creator_fee), (2, 0));
    }

    #[test]
    fn creator_fee_on_output() {
        // trade = ceil(1e6 * 2500 / 1e6) = 2500; in_less = 997_500
        // swapped = floor(997_500 * 1e7 / 10_997_500) = 907_024
        // creator = ceil(907_024 * 1000 / 1e6) = 908; out = 906_116
        let r =
            swap_base_input(1_000_000, 10_000_000, 10_000_000, 2_500, 1_000, 0, 0, false).unwrap();
        assert_eq!(r.trade_fee, 2_500);
        assert_eq!(9_975_000_000_000u128 / 10_997_500, 907_024);
        assert_eq!(r.creator_fee, 908);
        assert_eq!(r.output_amount, 906_116);
        assert_eq!(r.new_output_vault_amount, 10_000_000 - 907_024);
    }

    #[test]
    fn zero_total_fee_rate_with_fee_on_input_fails_like_chain() {
        // split_creator_fee divides by (trade + creator) = 0 -> None on chain too.
        assert_eq!(swap_base_input(1_000, 1_000, 1_000, 0, 0, 0, 0, true), None);
        // with the fee on output, a zero rate is fine
        let r = swap_base_input(1_000, 1_000, 1_000, 0, 0, 0, 0, false).unwrap();
        assert_eq!(r.output_amount, 500);
    }

    #[test]
    fn overflowing_rates_fail() {
        assert_eq!(
            swap_base_input(1, 1, 1, u64::MAX, 1, 0, 0, true),
            None,
            "trade + creator rate overflow"
        );
        // a fee rate above 100% leaves a negative input -> None
        assert_eq!(swap_base_input(10, 10, 10, 2_000_000, 0, 0, 0, false), None);
    }
}
