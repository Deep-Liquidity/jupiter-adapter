//! Quote parity against the real deep-amm program, using Jupiter's test kit: each swap
//! is quoted with `DeepSwapAmm::quote`, then executed as a native `swap_base_input` in
//! LiteSVM against the deep-amm release binary deployed on mainnet (`tests/fixtures/deepswap.so`),
//! and the realized output must equal the quote exactly.
//!
//!   cargo test --test parity                                    # offline, from fixtures
//!   RPC=https://api.devnet.solana.com cargo test --test parity  # snapshot missing fixtures
//!   REFRESH=1 RPC=... cargo test --test parity devnet_wsol_pool  # re-snapshot
//!
//! Fixture: devnet pool HxRCxiLSTaT7dsP5DLBH1s8hbGa815Q8JuUhZVmcLw5S (WSOL / BgDhdv...,
//! 0.30% trade fee, 1/3 of it protocol fee, creator fee disabled).
//!
//! The `synthetic_v1_*` tests (DEEP V1 fee model) run against the same binary; see the
//! section at the end of this file.

use std::path::{Path, PathBuf};

use deepswap_jupiter::{DeepSwapAmm, OP_SWAP_BASE_INPUT, encode_swap_base_input};
use jupiter_amm_interface::Swap;
use jupiter_amm_test_kit::{PoolTest, assert_pool_parity};
use solana_pubkey::{Pubkey, pubkey};

const POOL: Pubkey = pubkey!("HxRCxiLSTaT7dsP5DLBH1s8hbGa815Q8JuUhZVmcLw5S");
const AMM_CONFIG: Pubkey = pubkey!("8UY49GiUH4jR9RkUYTCHPpuKu5WQsFaf6KCTeUxQojoS");
const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const TOKEN: Pubkey = pubkey!("BgDhdvnYVwSDuSZFmbEE8B48ygtEghgq9yoHmtV2DxUY");

/// Native `swap_base_input` data from the adapter's `Swap::Placeholder` decision bits.
/// `minimum_amount_out` is 0: asserting the exact output is the kit's job.
fn encode_swap(swap: &Swap, in_amount: u64) -> Vec<u8> {
    match swap {
        Swap::Placeholder { data: Some(d) } if d.len() == 2 && d[0] == OP_SWAP_BASE_INPUT => {
            encode_swap_base_input(in_amount, 0)
        }
        other => panic!("unexpected swap variant: {other:?}"),
    }
}

/// Snapshot reserves: 4.95 SOL (4_950_000_000) and 158_400_000 tokens (6 decimals).
fn swaps(test: PoolTest) -> PoolTest {
    test
        // WSOL -> token
        .add_swap(WSOL, TOKEN, 1_000) // dust
        .add_swap(WSOL, TOKEN, 10_000_000) // 0.01 SOL (~0.2% of reserve)
        .add_swap(WSOL, TOKEN, 500_000_000) // 0.5 SOL (~10%)
        .add_swap(WSOL, TOKEN, 20_000_000_000) // 20 SOL (4x reserve: drains ~80% of tokens)
        // token -> WSOL
        .add_swap(TOKEN, WSOL, 1_000_000) // 1 token
        .add_swap(TOKEN, WSOL, 10_000_000_000) // 10k tokens
        .add_swap(TOKEN, WSOL, 16_000_000_000_000) // 16M tokens (~10%)
        .add_swap(TOKEN, WSOL, 500_000_000_000_000) // 500M tokens (~3x reserve)
}

#[test]
fn devnet_wsol_pool() {
    assert_pool_parity::<DeepSwapAmm>(&swaps(PoolTest::new(POOL)), encode_swap);
}

// ---------------------------------------------------------------------------------------
// Fee-path variants. The live config has creator fees disabled and no fund fee, so these
// tests take the committed devnet snapshot, PATCH the pool and config account bytes in a
// temporary copy (never in tests/fixtures) and run the same parity check against the
// same real program binary. They are synthetic states, not chain data.
// ---------------------------------------------------------------------------------------

/// `bincode(Account)`: lamports u64, then `data` as u64 length + bytes.
const DATA_OFFSET: usize = 16;

fn patch(file: &Path, expected_len: usize, edits: &[(usize, &[u8])]) {
    let mut b = std::fs::read(file).unwrap();
    let len = u64::from_le_bytes(b[8..16].try_into().unwrap()) as usize;
    assert_eq!(
        len, expected_len,
        "{file:?}: unexpected account data length"
    );
    for (off, bytes) in edits {
        b[DATA_OFFSET + off..DATA_OFFSET + off + bytes.len()].copy_from_slice(bytes);
    }
    std::fs::write(file, b).unwrap();
}

fn patched_snapshot(name: &str, creator_fee_on: u8) -> PathBuf {
    let src = PathBuf::from("tests/fixtures/accounts").join(POOL.to_string());
    assert!(
        src.exists(),
        "run `cargo test --test parity devnet_wsol_pool` first"
    );
    let dst = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dst);
    std::fs::create_dir_all(&dst).unwrap();
    for e in std::fs::read_dir(&src).unwrap() {
        let p = e.unwrap().path();
        std::fs::copy(&p, dst.join(p.file_name().unwrap())).unwrap();
    }
    // PoolState (offsets incl. discriminator): accrued protocol/fund/creator fees on both
    // sides, creator_fee_on, enable_creator_fee.
    patch(
        &dst.join(format!("{POOL}.bin")),
        637,
        &[
            (341, &12_345_678u64.to_le_bytes()),    // protocol_fees_token_0
            (349, &9_876_543_210u64.to_le_bytes()), // protocol_fees_token_1
            (357, &1_111_111u64.to_le_bytes()),     // fund_fees_token_0
            (365, &2_222_222_222u64.to_le_bytes()), // fund_fees_token_1
            (389, &[creator_fee_on, 1]),            // creator_fee_on, enable_creator_fee
            (397, &3_333_333u64.to_le_bytes()),     // creator_fees_token_0
            (405, &4_444_444_444u64.to_le_bytes()), // creator_fees_token_1
        ],
    );
    // AmmConfig: fund fee 5% of the trade fee, creator fee 1%.
    patch(
        &dst.join(format!("{AMM_CONFIG}.bin")),
        236,
        &[
            (28, &50_000u64.to_le_bytes()),
            (108, &10_000u64.to_le_bytes()),
        ],
    );
    dst
}

fn assert_variant(name: &str, creator_fee_on: u8) {
    let mut test = swaps(PoolTest::new(POOL));
    test.fixtures_dir = patched_snapshot(name, creator_fee_on);
    assert_pool_parity::<DeepSwapAmm>(&test, encode_swap);
}

#[test]
fn synthetic_creator_fee_on_both_tokens() {
    assert_variant("creator_fee_both", 0);
}

#[test]
fn synthetic_creator_fee_only_token_0() {
    assert_variant("creator_fee_token_0", 1);
}

#[test]
fn synthetic_creator_fee_only_token_1() {
    assert_variant("creator_fee_token_1", 2);
}

/// A DEEP graduation pool at the policy rates, as a synthetic state of the same snapshot:
/// creator fee enabled and taken in WSOL only (token 0 of this pool), AmmConfig creator
/// fee rate 3000 (0.30%) on top of the 0.30% trade fee, nothing accrued yet. Checked
/// against the same real program binary, in both directions.
#[test]
fn synthetic_graduation_pool_policy_rates() {
    let src = PathBuf::from("tests/fixtures/accounts").join(POOL.to_string());
    assert!(
        src.exists(),
        "run `cargo test --test parity devnet_wsol_pool` first"
    );
    let dst = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("graduation_policy");
    let _ = std::fs::remove_dir_all(&dst);
    std::fs::create_dir_all(&dst).unwrap();
    for e in std::fs::read_dir(&src).unwrap() {
        let p = e.unwrap().path();
        std::fs::copy(&p, dst.join(p.file_name().unwrap())).unwrap();
    }
    // creator_fee_on = 1 (OnlyToken0 = WSOL here), enable_creator_fee = true
    patch(&dst.join(format!("{POOL}.bin")), 637, &[(389, &[1, 1])]);
    // AmmConfig.creator_fee_rate = 3000, creator_fee_share_rate = 500_000 (the share only
    // matters at collection; it is set so the state matches the live policy)
    patch(
        &dst.join(format!("{AMM_CONFIG}.bin")),
        236,
        &[
            (108, &3_000u64.to_le_bytes()),
            (116, &500_000u64.to_le_bytes()),
        ],
    );
    let mut test = swaps(PoolTest::new(POOL));
    test.fixtures_dir = dst;
    assert_pool_parity::<DeepSwapAmm>(&test, encode_swap);
}

// ---------------------------------------------------------------------------------------
// DEEP V1 pools (side-dependent fees in the quote token). Synthetic states of the same
// snapshot, patched in a temporary copy like the variants above: the pool becomes a V1
// pool whose quote side is token 0 (WSOL), under a V1 AmmConfig at the owner's schedule.
//
// They need a program binary that contains the V1 code: the committed
// tests/fixtures/deepswap.so is the mainnet release build, which does. The legacy tests
// above run in the same pass against it and show the legacy path is untouched.
// ---------------------------------------------------------------------------------------

const REWARD_MODEL_STANDARD: u8 = 0;
const REWARD_MODEL_CREATOR: u8 = 1;
const REWARD_MODEL_HOLDER: u8 = 2;

/// The owner's V1 schedule (the AmmConfig's side of the fee), per 1e6: LP 0.10% on both
/// sides, DEEP 0.25% on buys and 0.65% on sells. LP + DEEP is limited to 5% per side.
const V1_BUY_LP: u64 = 1_000;
const V1_BUY_PROTOCOL: u64 = 2_500;
const V1_SELL_LP: u64 = 1_000;
const V1_SELL_PROTOCOL: u64 = 6_500;
/// The reward rate is the pool's own (the token's rate, chosen by its creator and stored
/// in the PoolState at creation), not a config value. Its ceiling: 5% per side, so the
/// total of a side never exceeds 10%.
const MAX_REWARD_RATE: u64 = 50_000;

struct V1Variant {
    name: &'static str,
    reward_model: u8,
    /// `PoolState::reward_rate_snapshot`: the pool's own reward rate, per 1e6 (0 for
    /// Standard, as `set_v1` stores it).
    reward_rate_snapshot: u64,
    /// `AmmConfig::sell_protocol_fee_rate`.
    sell_protocol_rate: u64,
    /// `AmmConfig::max_reward_rate`: the admin's current maximum for NEW pools. Swaps never
    /// read it.
    config_max_reward_rate: u64,
    /// Patch non-zero accrued protocol / fund / creator fees on both sides.
    accrued_fees: bool,
}

fn v1_snapshot(v: &V1Variant) -> PathBuf {
    let src = PathBuf::from("tests/fixtures/accounts").join(POOL.to_string());
    assert!(
        src.exists(),
        "run `cargo test --test parity devnet_wsol_pool` first"
    );
    let dst = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(v.name);
    let _ = std::fs::remove_dir_all(&dst);
    std::fs::create_dir_all(&dst).unwrap();
    for e in std::fs::read_dir(&src).unwrap() {
        let p = e.unwrap().path();
        std::fs::copy(&p, dst.join(p.file_name().unwrap())).unwrap();
    }
    let pool_file = dst.join(format!("{POOL}.bin"));
    // PoolState, what `set_v1` writes: creator_fee_on = 1 (the quote side is token 0 =
    // WSOL), enable_creator_fee = "not Standard", fee_model = 1, reward_model, zeroed
    // padding2, reward_rate_snapshot.
    let enable_creator_fee = (v.reward_model != REWARD_MODEL_STANDARD) as u8;
    patch(
        &pool_file,
        637,
        &[
            (389, &[1, enable_creator_fee]),
            (413, &[1, v.reward_model, 0, 0, 0, 0, 0, 0]),
            (421, &v.reward_rate_snapshot.to_le_bytes()),
        ],
    );
    if v.accrued_fees {
        // The same amounts as `patched_snapshot`. On a V1 pool only the WSOL-side protocol
        // and creator accumulators grow, but the reserves exclude all six, as on chain.
        patch(
            &pool_file,
            637,
            &[
                (341, &12_345_678u64.to_le_bytes()),    // protocol_fees_token_0
                (349, &9_876_543_210u64.to_le_bytes()), // protocol_fees_token_1
                (357, &1_111_111u64.to_le_bytes()),     // fund_fees_token_0
                (365, &2_222_222_222u64.to_le_bytes()), // fund_fees_token_1
                (397, &3_333_333u64.to_le_bytes()),     // creator_fees_token_0
                (405, &4_444_444_444u64.to_le_bytes()), // creator_fees_token_1
            ],
        );
    }
    // AmmConfig: fee_model = 1 (124), the four LP / protocol rates (132..164) and
    // max_reward_rate (164); 172..180 is zeroed (padding). Plus the legacy mirror a V1
    // config carries (`sync_legacy_mirror`: trade = buy LP + buy protocol, the protocol's
    // share of it, no legacy creator fee). No V1 code path reads the mirror.
    let legacy_trade = V1_BUY_LP + V1_BUY_PROTOCOL;
    let legacy_protocol_share = V1_BUY_PROTOCOL * 1_000_000 / legacy_trade;
    let mut v1_rates = Vec::with_capacity(40);
    for rate in [
        V1_BUY_LP,
        V1_BUY_PROTOCOL,
        V1_SELL_LP,
        v.sell_protocol_rate,
        v.config_max_reward_rate,
    ] {
        v1_rates.extend_from_slice(&rate.to_le_bytes());
    }
    patch(
        &dst.join(format!("{AMM_CONFIG}.bin")),
        236,
        &[
            (12, &legacy_trade.to_le_bytes()),
            (20, &legacy_protocol_share.to_le_bytes()),
            (108, &0u64.to_le_bytes()),       // creator_fee_rate
            (116, &0u64.to_le_bytes()),       // creator_fee_share_rate
            (124, &[1, 0, 0, 0, 0, 0, 0, 0]), // fee_model, padding0
            (132, v1_rates.as_slice()),
            (172, &[0u8; 8]), // padding
        ],
    );
    dst
}

fn assert_v1_variant(v: V1Variant) {
    let mut test = swaps(PoolTest::new(POOL));
    test.fixtures_dir = v1_snapshot(&v);
    assert_pool_parity::<DeepSwapAmm>(&test, encode_swap);
}

/// Standard: buys 0.35% (LP 0.10% + DEEP 0.25%), sells 0.75% (LP 0.10% + DEEP 0.65%).
#[test]
fn synthetic_v1_standard_pool() {
    assert_v1_variant(V1Variant {
        name: "v1_standard",
        reward_model: REWARD_MODEL_STANDARD,
        reward_rate_snapshot: 0,
        sell_protocol_rate: V1_SELL_PROTOCOL,
        config_max_reward_rate: MAX_REWARD_RATE,
        accrued_fees: false,
    });
}

/// Creator at a 1% reward rate: buys 1.35%, sells 1.75%.
#[test]
fn synthetic_v1_creator_pool() {
    assert_v1_variant(V1Variant {
        name: "v1_creator",
        reward_model: REWARD_MODEL_CREATOR,
        reward_rate_snapshot: 10_000,
        sell_protocol_rate: V1_SELL_PROTOCOL,
        config_max_reward_rate: MAX_REWARD_RATE,
        accrued_fees: false,
    });
}

/// Holder at a 1 bps reward rate (100 per 1e6): buys 0.36%, sells 0.76%. Priced like a
/// Creator pool; only the reward's recipient differs.
#[test]
fn synthetic_v1_holder_pool() {
    assert_v1_variant(V1Variant {
        name: "v1_holder",
        reward_model: REWARD_MODEL_HOLDER,
        reward_rate_snapshot: 100,
        sell_protocol_rate: V1_SELL_PROTOCOL,
        config_max_reward_rate: MAX_REWARD_RATE,
        accrued_fees: false,
    });
}

/// A pool at the reward ceiling (500 bps = 5%) under the normal schedule: buys 5.35%,
/// sells 5.75%.
#[test]
fn synthetic_v1_pool_at_the_reward_ceiling() {
    assert_v1_variant(V1Variant {
        name: "v1_reward_ceiling",
        reward_model: REWARD_MODEL_CREATOR,
        reward_rate_snapshot: MAX_REWARD_RATE,
        sell_protocol_rate: V1_SELL_PROTOCOL,
        config_max_reward_rate: MAX_REWARD_RATE,
        accrued_fees: false,
    });
}

/// A Creator pool (1%) with fees already accrued on both sides: the reserves exclude them.
#[test]
fn synthetic_v1_creator_pool_with_accrued_fees() {
    assert_v1_variant(V1Variant {
        name: "v1_creator_accrued",
        reward_model: REWARD_MODEL_CREATOR,
        reward_rate_snapshot: 10_000,
        sell_protocol_rate: V1_SELL_PROTOCOL,
        config_max_reward_rate: MAX_REWARD_RATE,
        accrued_fees: true,
    });
}

/// Exactly the 10% total cap on a sell: the config's sell side at its limit (LP 0.1% +
/// DEEP 4.9% = 5%) and the pool's reward rate at its ceiling (5%). The pool's reward rate
/// is charged in full whatever the config's rates are. Buys pay 5.35%. The config's
/// `max_reward_rate` is lowered to 1 bps here: it only limits NEW pools and must not
/// affect this existing one.
#[test]
fn synthetic_v1_ten_percent_sell_at_the_cap() {
    assert_v1_variant(V1Variant {
        name: "v1_ten_percent_sell",
        reward_model: REWARD_MODEL_HOLDER,
        reward_rate_snapshot: MAX_REWARD_RATE,
        sell_protocol_rate: 49_000,
        config_max_reward_rate: 100,
        accrued_fees: false,
    });
}
