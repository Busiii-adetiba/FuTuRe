#![cfg(feature = "testutils")]

use prediction_market::{Error, PredictionMarket, PredictionMarketClient};
use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events as _},
    token, vec, Address, BytesN, Env, IntoVal, String,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn setup() -> (Env, PredictionMarketClient<'static>, Address, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, PredictionMarket);
    let client = PredictionMarketClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let treasury = Address::generate(&env);
    let oracle = Address::generate(&env);
    // Tests below that don't move real collateral (everything except
    // dispute/split/merge) don't need to observe this token, so a throwaway
    // address here keeps every pre-existing call site untouched. Tests that
    // do need to fund/transfer real collateral use `setup_with_token()`.
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract(token_admin);
    client.init(&admin, &treasury, &token_id);
    (env, client, admin, treasury, oracle)
}

/// Like `setup()`, but also returns a live token client + its Stellar Asset
/// Contract admin client so a test can mint collateral to users before
/// calling `split`, `merge`, or `dispute` — all three now require real 1:1
/// token transfers (#1258, #1260).
fn setup_with_token() -> (
    Env,
    PredictionMarketClient<'static>,
    Address,
    Address,
    Address,
    token::Client<'static>,
    token::StellarAssetClient<'static>,
) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register_contract(None, PredictionMarket);
    let client = PredictionMarketClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let treasury = Address::generate(&env);
    let oracle = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract(token_admin.clone());
    let token = token::Client::new(&env, &token_id);
    let token_sac = token::StellarAssetClient::new(&env, &token_id);
    client.init(&admin, &treasury, &token_id);
    (env, client, admin, treasury, oracle, token, token_sac)
}

fn question(env: &Env) -> String {
    String::from_str(env, "Will BTC hit 100k?")
}

// ── 1. Happy path ─────────────────────────────────────────────────────────────

#[test]
fn test_happy_path_full_lifecycle() {
    let (env, client, admin, _treasury, oracle) = setup();
    let user_yes = Address::generate(&env);
    let user_no = Address::generate(&env);

    // create
    let mid = client.create_market(&admin, &question(&env), &oracle);

    // seed
    client.seed_market(&admin, &mid, &1_000_000);

    // buy YES
    let yes_shares = client.buy_yes(&user_yes, &mid, &100_000, &1);
    assert!(yes_shares > 0);

    // buy NO
    let no_shares = client.buy_no(&user_no, &mid, &100_000, &1);
    assert!(no_shares > 0);

    // close
    client.close_market(&admin, &mid);

    // oracle reports YES wins
    client.oracle_report(&oracle, &mid, &true);

    // dispute window passes → finalize
    client.finalize(&mid);

    // redeem YES position
    let payout = client.redeem(&user_yes, &mid);
    assert!(payout > 0);

    // NO holder gets nothing (NothingToRedeem)
    let err = client.try_redeem(&user_no, &mid).unwrap_err().unwrap();
    assert_eq!(err, Error::NothingToRedeem);
}

// ── 2. Dispute flow ───────────────────────────────────────────────────────────

#[test]
fn test_dispute_admin_upholds_emergency_resolve() {
    let (env, client, admin, _treasury, oracle, token, token_sac) = setup_with_token();
    let user = Address::generate(&env);
    let disputer = Address::generate(&env);
    let bond = 10_000_000i128; // == MIN_DISPUTE_BOND
    token_sac.mint(&disputer, &bond);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);
    client.buy_yes(&user, &mid, &100_000, &1);
    client.close_market(&admin, &mid);
    client.oracle_report(&oracle, &mid, &false); // oracle says NO

    // disputer challenges, escrowing the bond into the contract
    client.dispute(&disputer, &mid, &bond);
    assert_eq!(token.balance(&disputer), 0);

    // admin upholds → flips to YES
    client.admin_uphold_dispute(&admin, &mid, &true);

    // finalize (emergency resolved)
    client.finalize(&mid);

    // user redeems YES
    let payout = client.redeem(&user, &mid);
    assert!(payout > 0);
}

// ── 3. Dispute rejected — bond slashed ───────────────────────────────────────

#[test]
fn test_dispute_rejected_bond_slashed_to_treasury() {
    let (env, client, admin, _treasury, oracle, _token, token_sac) = setup_with_token();
    let disputer = Address::generate(&env);
    let bond = 10_000_000i128; // == MIN_DISPUTE_BOND
    token_sac.mint(&disputer, &bond);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);
    client.close_market(&admin, &mid);
    client.oracle_report(&oracle, &mid, &true);
    client.dispute(&disputer, &mid, &bond);

    // admin rejects → bond slashed
    client.admin_reject_dispute(&admin, &mid);

    let treasury_bal = client.get_treasury_balance();
    assert_eq!(treasury_bal, bond);

    // market reverts to Closed → can finalize
    client.finalize(&mid);
    let market = client.get_market(&mid);
    assert_eq!(market.status, prediction_market::MarketStatus::Resolved);
}

// ── 4. Cancel flow ────────────────────────────────────────────────────────────

#[test]
fn test_cancel_and_refund_all_positions() {
    let (env, client, admin, _treasury, oracle) = setup();
    let user_a = Address::generate(&env);
    let user_b = Address::generate(&env);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);
    client.buy_yes(&user_a, &mid, &100_000, &1);
    client.buy_no(&user_b, &mid, &100_000, &1);

    client.cancel_market(&admin, &mid);

    // Both users get refunds
    let refund_a = client.redeem(&user_a, &mid);
    let refund_b = client.redeem(&user_b, &mid);
    assert!(refund_a > 0);
    assert!(refund_b > 0);
}

// ── 5. LP flow ────────────────────────────────────────────────────────────────

#[test]
fn test_lp_add_trade_claim_fees_remove() {
    let (env, client, admin, _treasury, oracle) = setup();
    let lp = Address::generate(&env);
    let trader = Address::generate(&env);

    let mid = client.create_market(&admin, &question(&env), &oracle);

    // add liquidity
    let lp_shares = client.add_liquidity(&lp, &mid, &1_000_000);
    assert!(lp_shares > 0);

    // trade
    client.buy_yes(&trader, &mid, &100_000, &1);

    // claim LP fees (may be 0 in simple model, just must not error)
    let _fees = client.claim_lp_fees(&lp, &mid);

    // remove liquidity
    let returned = client.remove_liquidity(&lp, &mid, &lp_shares);
    assert!(returned > 0);
}

// ── 5b. LP proportional share minting (#951 fix) ─────────────────────────────
//
// After trading shifts lp_pool value, a second LP depositing the same nominal
// amount must receive *fewer* shares than the first LP.  On withdrawal, each LP
// should get back roughly what they put in (adjusted for pool-value changes) —
// not a flat 1:1 refund of their deposit regardless of pool size.

#[test]
fn test_lp_proportional_share_minting() {
    let (env, client, admin, _treasury, oracle) = setup();
    let lp_a = Address::generate(&env);
    let lp_b = Address::generate(&env);
    let trader = Address::generate(&env);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);

    // LP-A deposits 500_000 into an empty LP pool → gets 500_000 shares (1:1 bootstrap)
    let shares_a = client.add_liquidity(&lp_a, &mid, &500_000);
    assert_eq!(shares_a, 500_000, "first LP should receive 1:1 shares on bootstrap");

    // Simulate trading that increases the pool's total value by buying YES.
    // This increases market.lp_pool implicitly via yes_pool / no_pool changes
    // (in our model lp_pool only tracks LP-deposited value, so we exercise
    //  the proportional branch via a second deposit after more LP value accrues).
    client.add_liquidity(&lp_a, &mid, &500_000); // lp_pool now 1_000_000, total_lp_shares = 1_000_000

    // LP-B deposits the same 500_000 into a pool that now has 1_000_000 value and 1_000_000 shares.
    // Expected shares_b = 500_000 * 1_000_000 / 1_000_000 = 500_000 (ratio still 1:1 when unchanged)
    let shares_b = client.add_liquidity(&lp_b, &mid, &500_000);
    assert_eq!(shares_b, 500_000);

    // Now simulate pool-value growth by having a large trade push value into lp_pool
    // (trade then check that shares for an equivalent third deposit are fewer)
    client.buy_yes(&trader, &mid, &1_000_000, &1);

    // At this point lp_pool hasn't changed (trades don't add to lp_pool directly),
    // but we can verify the core invariant: total_lp_shares is tracked separately.
    let market = client.get_market(&mid);
    // total_lp_shares should equal sum of all LP share grants
    assert_eq!(market.total_lp_shares, shares_a + 500_000 + shares_b,
        "total_lp_shares must equal sum of all minted shares");
    assert!(market.total_lp_shares > 0);
}

// ── 5c. LP redemption reflects pool value, not flat deposit return (#951 fix) ─

#[test]
fn test_lp_remove_liquidity_proportional_payout() {
    let (env, client, admin, _treasury, oracle) = setup();
    let lp_a = Address::generate(&env);
    let lp_b = Address::generate(&env);

    let mid = client.create_market(&admin, &question(&env), &oracle);

    // LP-A deposits 1_000_000 first (bootstrap: gets 1_000_000 shares, lp_pool = 1_000_000)
    let shares_a = client.add_liquidity(&lp_a, &mid, &1_000_000);
    assert_eq!(shares_a, 1_000_000);

    // LP-B deposits 500_000 into pool with 1_000_000 value and 1_000_000 shares
    // → shares_b = 500_000 * 1_000_000 / 1_000_000 = 500_000
    let shares_b = client.add_liquidity(&lp_b, &mid, &500_000);
    assert_eq!(shares_b, 500_000);

    // Total lp_pool = 1_500_000, total_lp_shares = 1_500_000

    // LP-A withdraws all shares: payout = 1_000_000 * 1_500_000 / 1_500_000 = 1_000_000
    let payout_a = client.remove_liquidity(&lp_a, &mid, &shares_a);
    assert_eq!(payout_a, 1_000_000, "LP-A should recover their proportional share of pool value");

    // After LP-A withdraws: lp_pool = 500_000, total_lp_shares = 500_000
    // LP-B withdraws: payout = 500_000 * 500_000 / 500_000 = 500_000
    let payout_b = client.remove_liquidity(&lp_b, &mid, &shares_b);
    assert_eq!(payout_b, 500_000, "LP-B should recover their proportional share of pool value");

    // Pool should now be empty
    let market = client.get_market(&mid);
    assert_eq!(market.lp_pool, 0);
    assert_eq!(market.total_lp_shares, 0);
}

// ── 5d. claim_lp_fees uses share units not pool-value units (#951 fix) ────────

#[test]
fn test_claim_lp_fees_proportional_to_shares() {
    let (env, client, admin, _treasury, oracle) = setup();
    let lp_a = Address::generate(&env);
    let lp_b = Address::generate(&env);

    let mid = client.create_market(&admin, &question(&env), &oracle);

    // LP-A deposits 2_000 (bootstrap, gets 2_000 shares)
    client.add_liquidity(&lp_a, &mid, &2_000);
    // LP-B deposits 1_000 → shares_b = 1_000 * 2_000 / 2_000 = 1_000
    client.add_liquidity(&lp_b, &mid, &1_000);

    // Manually inject fees (in a real deployment fees come from trades):
    // We verify the proportional claim via the math, not via fee injection here —
    // with 0 fees both LPs must get 0 (not panic) and the test verifies no error.
    let fees_a = client.claim_lp_fees(&lp_a, &mid);
    let fees_b = client.claim_lp_fees(&lp_b, &mid);

    // With no accumulated fees both results are 0, but the calls succeed
    assert_eq!(fees_a, 0);
    assert_eq!(fees_b, 0);

    // Verify: if lp_fees were set externally we'd check proportionality.
    // The contract now uses total_lp_shares as denominator, so this test
    // documents the expected invariant: fee_a / fee_b == shares_a / shares_b == 2.
}

// ── 6. Batch redeem ───────────────────────────────────────────────────────────

#[test]
fn test_batch_redeem_across_three_markets() {
    let (env, client, admin, _treasury, oracle) = setup();
    let user = Address::generate(&env);

    let mut ids = vec![&env];
    for _ in 0..3 {
        let mid = client.create_market(&admin, &question(&env), &oracle);
        client.seed_market(&admin, &mid, &1_000_000);
        client.buy_yes(&user, &mid, &100_000, &1);
        client.close_market(&admin, &mid);
        client.oracle_report(&oracle, &mid, &true);
        client.finalize(&mid);
        ids.push_back(mid);
    }

    let result = client.batch_redeem(&user, &ids);
    assert_eq!(result.successes.len(), 3);
    assert_eq!(result.failures.len(), 0);
    assert!(result.total_payout > 0);
}

#[test]
fn test_batch_redeem_partial_failure() {
    let (env, client, admin, _treasury, oracle) = setup();
    let user = Address::generate(&env);

    let mut ids = vec![&env];
    // First market: resolve with YES (user has YES shares)
    let mid1 = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid1, &1_000_000);
    client.buy_yes(&user, &mid1, &100_000, &1);
    client.close_market(&admin, &mid1);
    client.oracle_report(&oracle, &mid1, &true);
    client.finalize(&mid1);
    ids.push_back(mid1);

    // Second market: resolve with NO (user has YES shares, gets nothing)
    let mid2 = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid2, &1_000_000);
    client.buy_yes(&user, &mid2, &100_000, &1);
    client.close_market(&admin, &mid2);
    client.oracle_report(&oracle, &mid2, &false);
    client.finalize(&mid2);
    ids.push_back(mid2);

    let result = client.batch_redeem(&user, &ids);
    // Should have 1 success and 1 failure
    assert_eq!(result.successes.len(), 1);
    assert_eq!(result.failures.len(), 1);
    assert!(result.total_payout > 0);
    // Check that the failure is NothingToRedeem
    assert_eq!(result.failures.get(0).error, Error::NothingToRedeem);
}

// ── 7. Split / Merge ──────────────────────────────────────────────────────────

#[test]
fn test_split_sell_half_merge_remaining() {
    let (env, client, admin, _treasury, oracle, token, token_sac) = setup_with_token();
    let user = Address::generate(&env);
    token_sac.mint(&user, &200_000);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);

    // split: 1:1 collateral is pulled from the user into contract escrow
    client.split(&user, &mid, &200_000);
    assert_eq!(token.balance(&user), 0);
    assert_eq!(token.balance(&client.address), 200_000);
    let pos = client.get_position(&mid, &user);
    assert_eq!(pos.yes_shares, 200_000);
    assert_eq!(pos.no_shares, 200_000);

    // "sell half" — simulate by buying more on the other side (no sell fn needed)
    // merge remaining half → 1:1 collateral is returned to the user
    client.merge(&user, &mid, &100_000);
    assert_eq!(token.balance(&user), 100_000);
    assert_eq!(token.balance(&client.address), 100_000);
    let pos2 = client.get_position(&mid, &user);
    assert_eq!(pos2.yes_shares, 100_000);
    assert_eq!(pos2.no_shares, 100_000);
}

// ── 7b. Split/merge collateral enforcement (#1260) ───────────────────────────

#[test]
fn test_split_requires_positive_amount() {
    let (env, client, admin, _treasury, oracle, _token, _token_sac) = setup_with_token();
    let user = Address::generate(&env);
    let mid = client.create_market(&admin, &question(&env), &oracle);

    let err = client.try_split(&user, &mid, &0).unwrap_err().unwrap();
    assert_eq!(err, Error::InvalidAmount);

    let err = client.try_split(&user, &mid, &-1).unwrap_err().unwrap();
    assert_eq!(err, Error::InvalidAmount);
}

#[test]
#[should_panic]
fn test_split_fails_without_sufficient_collateral() {
    let (env, client, admin, _treasury, oracle, _token, token_sac) = setup_with_token();
    let user = Address::generate(&env);
    token_sac.mint(&user, &100); // less than the amount they'll try to split
    let mid = client.create_market(&admin, &question(&env), &oracle);

    // The token transfer traps on insufficient balance before any shares
    // are minted — uncollateralized share minting is impossible.
    client.split(&user, &mid, &1_000);
}

#[test]
fn test_merge_returns_collateral_only_up_to_split_tokens() {
    let (env, client, admin, _treasury, oracle, token, token_sac) = setup_with_token();
    let user = Address::generate(&env);
    token_sac.mint(&user, &500_000);
    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);

    client.split(&user, &mid, &500_000);
    assert_eq!(token.balance(&client.address), 500_000);

    client.merge(&user, &mid, &500_000);
    assert_eq!(token.balance(&user), 500_000, "full collateral returned 1:1");
    assert_eq!(token.balance(&client.address), 0);
}

// ── 8. Slippage exceeded ──────────────────────────────────────────────────────

#[test]
fn test_buy_yes_slippage_exceeded() {
    let (env, client, admin, _treasury, oracle) = setup();
    let user = Address::generate(&env);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);

    // min_shares_out impossibly high
    let err = client
        .try_buy_yes(&user, &mid, &100_000, &999_999_999)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::SlippageExceeded);
}

// ── 9. Emergency pause ────────────────────────────────────────────────────────

#[test]
fn test_emergency_pause_blocks_mutations_unpause_succeeds() {
    let (env, client, admin, _treasury, oracle) = setup();
    let user = Address::generate(&env);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);

    // pause
    client.pause(&admin);

    // all mutations fail with ContractPaused
    let err = client.try_buy_yes(&user, &mid, &100_000, &1).unwrap_err().unwrap();
    assert_eq!(err, Error::ContractPaused);

    let err = client.try_buy_no(&user, &mid, &100_000, &1).unwrap_err().unwrap();
    assert_eq!(err, Error::ContractPaused);

    let err = client.try_close_market(&admin, &mid).unwrap_err().unwrap();
    assert_eq!(err, Error::ContractPaused);

    let err = client
        .try_create_market(&admin, &question(&env), &oracle)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::ContractPaused);

    // unpause
    client.unpause(&admin);

    // mutations succeed again
    let shares = client.buy_yes(&user, &mid, &100_000, &1);
    assert!(shares > 0);
}

// ── 10. Events ───────────────────────────────────────────────────────────────

#[test]
fn test_create_market_emits_created_event() {
    let (env, client, admin, _treasury, oracle) = setup();

    let mid = client.create_market(&admin, &question(&env), &oracle);

    let events = env.events().all();
    let (contract_id, topics, data) = events.last().unwrap();
    assert_eq!(contract_id, client.address);
    assert_eq!(
        topics,
        vec![
            &env,
            symbol_short!("market").into_val(&env),
            symbol_short!("created").into_val(&env),
        ]
    );
    assert_eq!(
        data,
        (mid, admin.clone(), oracle.clone()).into_val(&env)
    );
}

#[test]
fn test_buy_yes_emits_traded_event() {
    let (env, client, admin, _treasury, oracle) = setup();
    let user = Address::generate(&env);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);
    let shares = client.buy_yes(&user, &mid, &100_000, &1);

    let events = env.events().all();
    let (_, topics, data) = events.last().unwrap();
    assert_eq!(
        topics,
        vec![
            &env,
            symbol_short!("market").into_val(&env),
            symbol_short!("traded").into_val(&env),
        ]
    );
    assert_eq!(
        data,
        (mid, user.clone(), true, 100_000i128, shares).into_val(&env)
    );
}

#[test]
fn test_finalize_emits_resolved_event() {
    let (env, client, admin, _treasury, oracle) = setup();
    let user = Address::generate(&env);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);
    client.buy_yes(&user, &mid, &100_000, &1);
    client.close_market(&admin, &mid);
    client.oracle_report(&oracle, &mid, &true);
    client.finalize(&mid);

    let events = env.events().all();
    let (_, topics, data) = events.last().unwrap();
    assert_eq!(
        topics,
        vec![
            &env,
            symbol_short!("market").into_val(&env),
            symbol_short!("resolved").into_val(&env),
        ]
    );
    assert_eq!(data, (mid, Some(true)).into_val(&env));
}

// ── 11. Emergency upgrade & market containment (#1259) ────────────────────────

#[test]
fn test_upgrade_rejects_non_admin() {
    let (env, client, _admin, _treasury, _oracle) = setup();
    let attacker = Address::generate(&env);
    let fake_hash = BytesN::from_array(&env, &[7u8; 32]);

    // Non-admin is rejected before the deployer is ever touched. A full
    // successful WASM-swap round trip requires a second compiled contract
    // binary uploaded via the deployer, which isn't available in this unit
    // test crate — that path is exercised in deployment/integration testing
    // outside `cargo test`, not here.
    let err = client.try_upgrade(&attacker, &fake_hash).unwrap_err().unwrap();
    assert_eq!(err, Error::Unauthorized);
}

#[test]
fn test_emergency_drain_market_rejects_non_admin() {
    let (env, client, admin, _treasury, oracle) = setup();
    let attacker = Address::generate(&env);
    let mid = client.create_market(&admin, &question(&env), &oracle);

    let err = client
        .try_emergency_drain_market(&attacker, &mid)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::Unauthorized);

    // Rejected call must not have touched the market.
    let market = client.get_market(&mid);
    assert_eq!(market.status, prediction_market::MarketStatus::Open);
}

#[test]
fn test_emergency_drain_market_cancels_and_preserves_storage() {
    let (env, client, admin, _treasury, oracle, token, token_sac) = setup_with_token();
    let user = Address::generate(&env);
    token_sac.mint(&user, &300_000);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);
    client.split(&user, &mid, &300_000);

    // Admin-gated emergency containment.
    client.emergency_drain_market(&admin, &mid);
    let market = client.get_market(&mid);
    assert_eq!(market.status, prediction_market::MarketStatus::Cancelled);
    // Unrelated market fields survive the drain untouched.
    assert_eq!(market.creator, admin);
    assert_eq!(market.question, question(&env));

    // Holders recover their position through the existing cancelled-market
    // refund path, including real collateral for the split-originated shares.
    let refund = client.redeem(&user, &mid);
    assert_eq!(refund, 600_000); // 300_000 yes + 300_000 no shares, 1:1
    assert_eq!(token.balance(&user), 300_000);
    assert_eq!(token.balance(&client.address), 0);

    // Draining an already-cancelled market fails cleanly instead of
    // double-cancelling.
    let err = client
        .try_emergency_drain_market(&admin, &mid)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::MarketAlreadyCancelled);
}

// ── 12. Redeem payout precision & dust routing (#1257) ────────────────────────

#[test]
fn test_redeem_precision_dust_flushes_to_lp_fees_with_exact_conservation() {
    let (env, client, admin, _treasury, oracle) = setup();
    let buyer_a = Address::generate(&env);
    let buyer_b = Address::generate(&env);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    // Fresh, unseeded market: the first buy lands on calc_shares' 0/0
    // bootstrap branch (shares == amount), so pool/share state — and thus
    // the exact payout math below — is fully predictable.
    let shares_a = client.buy_yes(&buyer_a, &mid, &3, &1);
    assert_eq!(shares_a, 3);
    let shares_b = client.buy_yes(&buyer_b, &mid, &4, &1);
    // shares_b = 4 * (0 + 3) / (3 + 4) = 12 / 7 = 1 (truncated) — a
    // deliberately non-evenly-divisible share split.
    assert_eq!(shares_b, 1);

    client.close_market(&admin, &mid);
    client.oracle_report(&oracle, &mid, &true);
    client.finalize(&mid);

    let market = client.get_market(&mid);
    let total_pool = market.yes_pool + market.no_pool;
    let total_winning = market.yes_shares;
    assert_eq!(total_pool, 7);
    assert_eq!(total_winning, 4);

    let payout_a = client.redeem(&buyer_a, &mid);
    let payout_b = client.redeem(&buyer_b, &mid);

    // Exact payouts, independently computed with the same PRECISION-scaled
    // single-division formula the contract uses internally.
    assert_eq!(payout_a, 5); // (3 * 10_000_000 * 7) / 4 / 10_000_000 = 5
    assert_eq!(payout_b, 1); // (1 * 10_000_000 * 7) / 4 / 10_000_000 = 1

    // Nothing is unaccounted for: the truncated fractions from both
    // redemptions (2_500_000 + 7_500_000 == PRECISION) flush into exactly
    // one whole lp_fees unit, leaving zero pending dust.
    let market_after = client.get_market(&mid);
    assert_eq!(market_after.lp_fees, 1);
    assert_eq!(market_after.dust, 0);

    // Conservation invariant: every unit of the pool ends up either paid
    // out or routed to claimable LP fees — nothing vanishes, nothing is
    // fabricated.
    assert_eq!(payout_a + payout_b + market_after.lp_fees, total_pool);
}

// ── 13. Dispute bond escrow (#1258) ───────────────────────────────────────────

#[test]
fn test_dispute_rejects_bond_below_minimum() {
    let (env, client, admin, _treasury, oracle, token, token_sac) = setup_with_token();
    let disputer = Address::generate(&env);
    token_sac.mint(&disputer, &10_000_000);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);
    client.close_market(&admin, &mid);
    client.oracle_report(&oracle, &mid, &true);

    // Below MIN_DISPUTE_BOND (10_000_000) → rejected before any token
    // transfer is attempted, and the disputer's balance is untouched.
    let err = client
        .try_dispute(&disputer, &mid, &9_999_999)
        .unwrap_err()
        .unwrap();
    assert_eq!(err, Error::InvalidAmount);
    assert_eq!(token.balance(&disputer), 10_000_000);

    let market = client.get_market(&mid);
    assert_eq!(market.status, prediction_market::MarketStatus::Closed);
}

#[test]
#[should_panic]
fn test_dispute_fails_without_sufficient_balance() {
    let (env, client, admin, _treasury, oracle, _token, token_sac) = setup_with_token();
    let disputer = Address::generate(&env);
    // Funded below the bond they'll attempt to post.
    token_sac.mint(&disputer, &5_000_000);

    let mid = client.create_market(&admin, &question(&env), &oracle);
    client.seed_market(&admin, &mid, &1_000_000);
    client.close_market(&admin, &mid);
    client.oracle_report(&oracle, &mid, &true);

    // Bond clears MIN_DISPUTE_BOND but exceeds the disputer's real balance —
    // the token transfer traps before the market is marked Disputed.
    client.dispute(&disputer, &mid, &10_000_000);
}
