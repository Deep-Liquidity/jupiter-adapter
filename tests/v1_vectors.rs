//! Cross-language vectors for the DEEP V1 swap math: `tests/fixtures/deepswap-v1-vectors.json`
//! pins this crate's `math::swap_base_input_v1`,
//! `math::swap_base_output_v1` and `state::PoolState::v1_rates` to the on-chain program
//! (`programs/deep-amm/src/curve/v1.rs`, `states/pool.rs`) and to the TypeScript mirror
//! (`packages/curve-math`). Every amount and rate in the file is a decimal string.
//!
//!   cargo test --test v1_vectors

use deepswap_jupiter::math::{self, SwapResult};
use deepswap_jupiter::state::{AmmConfig, FEE_MODEL_V1, PoolState, V1Rates};
use serde_json::Value;

const VECTORS: &str = include_str!("fixtures/deepswap-v1-vectors.json");

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("deepswap-v1-vectors.json is valid JSON")
}

fn list<'a>(root: &'a Value, key: &str) -> &'a Vec<Value> {
    root[key]
        .as_array()
        .unwrap_or_else(|| panic!("`{key}` must be an array"))
}

fn name(v: &Value) -> &str {
    v["name"].as_str().unwrap_or("<unnamed>")
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{}: `{key}` must be a decimal string", name(v)))
}

fn amount(v: &Value, key: &str) -> u128 {
    text(v, key)
        .parse()
        .unwrap_or_else(|e| panic!("{}: `{key}` is not a u128: {e}", name(v)))
}

fn rate(v: &Value, key: &str) -> u64 {
    text(v, key)
        .parse()
        .unwrap_or_else(|e| panic!("{}: `{key}` is not a u64: {e}", name(v)))
}

fn flag(v: &Value, key: &str) -> bool {
    v[key]
        .as_bool()
        .unwrap_or_else(|| panic!("{}: `{key}` must be a boolean", name(v)))
}

/// The result fields shared by `exactIn` and `exactOut` vectors.
fn assert_result(v: &Value, r: &SwapResult, reserve_in: u128, reserve_out: u128, is_buy: bool) {
    let n = name(v);
    let lp_fee = r.trade_fee - r.protocol_fee;
    assert_eq!(r.input_amount, amount(v, "amountIn"), "{n}: amountIn");
    assert_eq!(r.output_amount, amount(v, "amountOut"), "{n}: amountOut");
    assert_eq!(
        lp_fee + r.protocol_fee + r.creator_fee,
        amount(v, "totalFee"),
        "{n}: totalFee"
    );
    assert_eq!(lp_fee, amount(v, "lpFee"), "{n}: lpFee");
    assert_eq!(r.protocol_fee, amount(v, "protocolFee"), "{n}: protocolFee");
    assert_eq!(r.creator_fee, amount(v, "rewardFee"), "{n}: rewardFee");
    assert_eq!(r.fund_fee, 0, "{n}: fund fee");
    assert_eq!(
        r.new_input_vault_amount,
        amount(v, "newInputVault"),
        "{n}: newInputVault"
    );
    assert_eq!(
        r.new_output_vault_amount,
        amount(v, "newOutputVault"),
        "{n}: newOutputVault"
    );
    // The reserves after the swap: `new_*_vault_amount` count no part of the fee, and the
    // LP part stays in the pool on the quote side (the input of a buy, the output of a
    // sell). Equivalently: reserve + amount in/out, minus the protocol and reward parts on
    // the quote side.
    let (reserve_in_after, reserve_out_after) = if is_buy {
        (r.new_input_vault_amount + lp_fee, r.new_output_vault_amount)
    } else {
        (r.new_input_vault_amount, r.new_output_vault_amount + lp_fee)
    };
    assert_eq!(
        reserve_in_after,
        amount(v, "reserveInAfter"),
        "{n}: reserveInAfter"
    );
    assert_eq!(
        reserve_out_after,
        amount(v, "reserveOutAfter"),
        "{n}: reserveOutAfter"
    );
    let quote_side_fees = r.protocol_fee + r.creator_fee;
    if is_buy {
        assert_eq!(
            reserve_in_after,
            reserve_in + r.input_amount - quote_side_fees
        );
        assert_eq!(reserve_out_after, reserve_out - r.output_amount);
    } else {
        assert_eq!(reserve_in_after, reserve_in + r.input_amount);
        assert_eq!(
            reserve_out_after,
            reserve_out - r.output_amount - quote_side_fees
        );
    }
}

#[test]
fn constants_match_the_vectors() {
    let root = vectors();
    assert_eq!(
        rate(&root, "feeRateDenominator"),
        math::FEE_RATE_DENOMINATOR_VALUE
    );
    // one total cap (10%), and the ceiling of a pool's reward rate (5%)
    assert_eq!(rate(&root, "maxTotalFeeRate"), math::MAX_TOTAL_FEE_RATE);
    assert_eq!(rate(&root, "maxRewardRate"), math::MAX_REWARD_RATE);

    // The schedule is the AmmConfig's side of the fee only: four LP / protocol rates. A
    // reward rate is the pool's own (`PoolState::reward_rate_snapshot`), never a config
    // value, so the schedule holds none.
    let schedule = root["schedule"]
        .as_object()
        .expect("`schedule` must be an object");
    let mut keys: Vec<&str> = schedule.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "buyLpFeeRate",
            "buyProtocolFeeRate",
            "sellLpFeeRate",
            "sellProtocolFeeRate"
        ]
    );
    let s = &root["schedule"];
    let config = AmmConfig {
        fee_model: FEE_MODEL_V1,
        buy_lp_fee_rate: rate(s, "buyLpFeeRate"),
        buy_protocol_fee_rate: rate(s, "buyProtocolFeeRate"),
        sell_lp_fee_rate: rate(s, "sellLpFeeRate"),
        sell_protocol_fee_rate: rate(s, "sellProtocolFeeRate"),
        ..Default::default()
    };
    // the schedule is a valid config, and prices a pool at the reward ceiling on both sides
    // within the total cap
    let pool = PoolState {
        fee_model: FEE_MODEL_V1,
        reward_model: 1,
        reward_rate_snapshot: math::MAX_REWARD_RATE,
        ..Default::default()
    };
    for is_buy in [true, false] {
        let rates = pool
            .v1_rates(&config, is_buy)
            .expect("the schedule is within the caps");
        assert!(rates.lp + rates.protocol <= math::MAX_TOTAL_FEE_RATE - math::MAX_REWARD_RATE);
        assert!(rates.total() <= math::MAX_TOTAL_FEE_RATE);
        assert_eq!(
            math::total_rate(rates.lp, rates.protocol, rates.reward),
            Some(rates.total())
        );
    }
}

#[test]
fn exact_in_vectors() {
    let root = vectors();
    let cases = list(&root, "exactIn");
    assert!(!cases.is_empty(), "no exactIn vectors");
    let (mut ok, mut rejected) = (0usize, 0usize);
    for v in cases {
        let n = name(v);
        let is_buy = flag(v, "isBuy");
        let (reserve_in, reserve_out) = (amount(v, "reserveIn"), amount(v, "reserveOut"));
        let r = math::swap_base_input_v1(
            amount(v, "amountIn"),
            reserve_in,
            reserve_out,
            rate(v, "lpRate"),
            rate(v, "protocolRate"),
            rate(v, "rewardRate"),
            is_buy,
        );
        if flag(v, "ok") {
            let r = r.unwrap_or_else(|| panic!("{n}: expected a result, got None"));
            assert_result(v, &r, reserve_in, reserve_out, is_buy);
            ok += 1;
        } else {
            assert_eq!(r, None, "{n}: expected None");
            rejected += 1;
        }
    }
    assert!(ok > 0, "no passing exactIn vector");
    assert!(rejected > 0, "no rejected exactIn vector");
}

#[test]
fn exact_out_vectors() {
    let root = vectors();
    let cases = list(&root, "exactOut");
    assert!(!cases.is_empty(), "no exactOut vectors");
    let (mut ok, mut rejected) = (0usize, 0usize);
    for v in cases {
        let n = name(v);
        let is_buy = flag(v, "isBuy");
        let (reserve_in, reserve_out) = (amount(v, "reserveIn"), amount(v, "reserveOut"));
        let r = math::swap_base_output_v1(
            amount(v, "amountOut"),
            reserve_in,
            reserve_out,
            rate(v, "lpRate"),
            rate(v, "protocolRate"),
            rate(v, "rewardRate"),
            is_buy,
        );
        if flag(v, "ok") {
            let r = r.unwrap_or_else(|| panic!("{n}: expected a result, got None"));
            assert_result(v, &r, reserve_in, reserve_out, is_buy);
            ok += 1;
        } else {
            assert_eq!(r, None, "{n}: expected None");
            rejected += 1;
        }
    }
    assert!(ok > 0, "no passing exactOut vector");
    assert!(rejected > 0, "no rejected exactOut vector");
}

#[test]
fn pool_rates_vectors() {
    let root = vectors();
    let cases = list(&root, "poolRates");
    assert!(!cases.is_empty(), "no poolRates vectors");
    let (mut ok, mut rejected) = (0usize, 0usize);
    for v in cases {
        let n = name(v);
        let reward_model = v["rewardModel"]
            .as_u64()
            .and_then(|m| u8::try_from(m).ok())
            .unwrap_or_else(|| panic!("{n}: `rewardModel` must be a number 0..=255"));
        let pool = PoolState {
            fee_model: FEE_MODEL_V1,
            reward_model,
            reward_rate_snapshot: rate(v, "rewardRateSnapshot"),
            ..Default::default()
        };
        let config = AmmConfig {
            fee_model: FEE_MODEL_V1,
            buy_lp_fee_rate: rate(v, "buyLpFeeRate"),
            buy_protocol_fee_rate: rate(v, "buyProtocolFeeRate"),
            sell_lp_fee_rate: rate(v, "sellLpFeeRate"),
            sell_protocol_fee_rate: rate(v, "sellProtocolFeeRate"),
            ..Default::default()
        };
        let rates = pool.v1_rates(&config, flag(v, "isBuy"));
        if flag(v, "ok") {
            let expected = V1Rates {
                lp: rate(v, "lpRate"),
                protocol: rate(v, "protocolRate"),
                reward: rate(v, "rewardRate"),
            };
            assert_eq!(rates, Some(expected), "{n}");
            // LP + protocol within their limit, the reward within its ceiling, and so the
            // total within the math's one cap
            assert!(
                expected.lp + expected.protocol <= math::MAX_TOTAL_FEE_RATE - math::MAX_REWARD_RATE,
                "{n}: lp + protocol"
            );
            assert!(expected.reward <= math::MAX_REWARD_RATE, "{n}: reward");
            assert_eq!(
                math::total_rate(expected.lp, expected.protocol, expected.reward),
                Some(expected.total()),
                "{n}: total"
            );
            ok += 1;
        } else {
            assert_eq!(rates, None, "{n}: expected None");
            rejected += 1;
        }
    }
    assert!(ok > 0, "no passing poolRates vector");
    assert!(rejected > 0, "no rejected poolRates vector");
}
