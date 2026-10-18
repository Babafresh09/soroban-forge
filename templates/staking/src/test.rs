use super::*;
use soroban_sdk::testutils::{Address as _, Events as _};
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::{Address, Env};

/// What `setup` mints to each of Alice and Bob, so they can deposit.
const USER_FUNDS: i128 = 10_000_000_000;

fn setup(env: &Env) -> (StakingContractClient<'_>, TokenClient<'_>, Address, Address, Address) {
    env.mock_all_auths();
    let admin = Address::generate(env);
    let alice = Address::generate(env);
    let bob = Address::generate(env);

    let token_id = env.register_stellar_asset_contract(admin.clone());
    let token = TokenClient::new(env, &token_id);
    let sac = StellarAssetClient::new(env, &token_id);
    sac.mint(&admin, &10_000_000_000);
    sac.mint(&alice, &USER_FUNDS);
    sac.mint(&bob, &USER_FUNDS);

    let staking_id = env.register(StakingContract, (admin.clone(), token_id));
    let staking = StakingContractClient::new(env, &staking_id);

    (staking, token, admin, alice, bob)
}

#[test]
fn deposit_moves_tokens_into_contract() {
    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup(&env);

    assert_eq!(token.balance(&alice), USER_FUNDS);
    staking.deposit(&alice, &1000);
    assert_eq!(staking.get_staked(&alice), 1000);
    assert_eq!(token.balance(&alice), USER_FUNDS - 1000);
    assert_eq!(token.balance(&staking.address), 1000);
}

#[test]
fn withdraw_moves_tokens_back_to_user() {
    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup(&env);

    staking.deposit(&alice, &1000);
    staking.withdraw(&alice, &1000);
    assert_eq!(staking.get_staked(&alice), 0);
    assert_eq!(token.balance(&alice), USER_FUNDS);
    assert_eq!(token.balance(&staking.address), 0);
}

#[test]
fn rewards_only_split_among_deposited_tokens() {
    let env = Env::default();
    let (staking, token, _admin, alice, bob) = setup(&env);

    staking.deposit(&alice, &1000);
    staking.distribute(&500);

    // Bob never deposited, so he has nothing pending.
    assert_eq!(staking.get_pending_reward(&bob), 0);
    assert_eq!(token.balance(&bob), USER_FUNDS);

    assert_eq!(staking.claim(&alice), 500);
    assert_eq!(token.balance(&alice), USER_FUNDS - 1000 + 500);
    assert_eq!(token.balance(&staking.address), 1000);
}

#[test]
#[should_panic]
fn deposit_without_balance_fails() {
    let env = Env::default();
    let (staking, _token, _admin, _alice, _bob) = setup(&env);
    let broke = Address::generate(&env);
    staking.deposit(&broke, &1000);
}

#[test]
fn single_staker_distribution() {
    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup(&env);

    staking.deposit(&alice, &1000);
    assert_eq!(staking.get_staked(&alice), 1000);
    assert_eq!(staking.get_total_staked(), 1000);

    staking.distribute(&500);

    let claimed = staking.claim(&alice);
    assert_eq!(claimed, 500);
    assert_eq!(token.balance(&alice), USER_FUNDS - 1000 + 500);
    assert_eq!(token.balance(&staking.address), 1000);
}

#[test]
fn multi_staker_proportional_distribution() {
    let env = Env::default();
    let (staking, token, _admin, alice, bob) = setup(&env);

    staking.deposit(&alice, &1000);
    staking.deposit(&bob, &3000);
    assert_eq!(staking.get_total_staked(), 4000);

    staking.distribute(&400);

    let alice_claimed = staking.claim(&alice);
    let bob_claimed = staking.claim(&bob);

    // Alice: 1/4 of 400 = 100, Bob: 3/4 of 400 = 300
    assert_eq!(alice_claimed, 100);
    assert_eq!(bob_claimed, 300);
    assert_eq!(token.balance(&alice), USER_FUNDS - 1000 + 100);
    assert_eq!(token.balance(&bob), USER_FUNDS - 3000 + 300);
}

#[test]
fn distribute_with_no_stakers_rejects_without_side_effects() {
    let env = Env::default();
    let (staking, token, admin, alice, _bob) = setup(&env);
    let admin_balance = token.balance(&admin);
    let contract_balance = token.balance(&staking.address);
    let events = env.events().all();

    assert_eq!(staking.try_distribute(&500), Err(Ok(Error::NoStakers.into())));

    assert_eq!(env.events().all(), events);
    assert_eq!(token.balance(&admin), admin_balance);
    assert_eq!(token.balance(&staking.address), contract_balance);
    assert_eq!(staking.get_total_staked(), 0);
    assert_eq!(staking.get_acc_reward_per_share(), 0);
    assert_eq!(staking.get_pending_reward(&alice), 0);
}

#[test]
fn distribute_after_last_withdrawal_rejects_without_side_effects() {
    let env = Env::default();
    let (staking, token, admin, alice, _bob) = setup(&env);
    staking.deposit(&alice, &1000);
    staking.distribute(&500);
    staking.withdraw(&alice, &1000);
    let accumulator = staking.get_acc_reward_per_share();
    let admin_balance = token.balance(&admin);
    let contract_balance = token.balance(&staking.address);
    let events = env.events().all();

    assert_eq!(staking.try_distribute(&500), Err(Ok(Error::NoStakers.into())));

    assert_eq!(env.events().all(), events);
    assert_eq!(token.balance(&admin), admin_balance);
    assert_eq!(token.balance(&staking.address), contract_balance);
    assert_eq!(staking.get_total_staked(), 0);
    assert_eq!(staking.get_acc_reward_per_share(), accumulator);
    assert_eq!(staking.get_pending_reward(&alice), 0);
}

#[test]
fn claim_after_multiple_distributions() {
    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup(&env);

    staking.deposit(&alice, &1000);
    staking.distribute(&200);
    staking.distribute(&300);

    let claimed = staking.claim(&alice);
    assert_eq!(claimed, 500);
    assert_eq!(token.balance(&alice), USER_FUNDS - 1000 + 500);
}

#[test]
fn withdraw_claims_pending_first() {
    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup(&env);

    staking.deposit(&alice, &1000);
    staking.distribute(&500);
    staking.withdraw(&alice, &500);

    // withdraw triggered internal claim of 500, then returned 500 of stake
    assert_eq!(staking.get_staked(&alice), 500);
    assert_eq!(staking.get_total_staked(), 500);
    assert_eq!(token.balance(&alice), USER_FUNDS - 1000 + 500 + 500);
    assert_eq!(token.balance(&staking.address), 500);
}

#[test]
fn rounding_handling() {
    let env = Env::default();
    let (staking, token, _admin, alice, bob) = setup(&env);

    // Alice 1/3, Bob 2/3 â€” 10 rewards
    staking.deposit(&alice, &100);
    staking.deposit(&bob, &200);
    staking.distribute(&10);

    let alice_claimed = staking.claim(&alice);
    let bob_claimed = staking.claim(&bob);

    // floor(10 * 100 / 300) = 3, floor(10 * 200 / 300) = 6
    // some dust may be lost to rounding
    assert_eq!(alice_claimed, 3);
    assert_eq!(bob_claimed, 6);
    assert_eq!(token.balance(&alice), USER_FUNDS - 100 + 3);
    assert_eq!(token.balance(&bob), USER_FUNDS - 200 + 6);
    // 1 unit lost to integer rounding
    assert_eq!(alice_claimed + bob_claimed, 9);
}

#[test]
fn multiple_users_can_deposit_before_rewards() {
    let env = Env::default();
    let (staking, token, _admin, alice, bob) = setup(&env);

    staking.deposit(&alice, &2000);
    staking.deposit(&bob, &2000);
    staking.distribute(&1000);

    let a = staking.claim(&alice);
    let b = staking.claim(&bob);
    assert_eq!(a, 500);
    assert_eq!(b, 500);
    assert_eq!(token.balance(&alice), USER_FUNDS - 2000 + 500);
    assert_eq!(token.balance(&bob), USER_FUNDS - 2000 + 500);
}

#[test]
fn accumulator_tracks_multiple_distributions_after_withdraw() {
    let env = Env::default();
    let (staking, _token, _admin, alice, bob) = setup(&env);

    staking.deposit(&alice, &500);
    staking.deposit(&bob, &500);
    staking.distribute(&200);

    staking.withdraw(&bob, &500);
    staking.distribute(&200);

    let alice_claimed = staking.claim(&alice);
    // First distribution: 100, second: 200 (stake unchanged)
    assert_eq!(alice_claimed, 300);
}

#[test]
#[should_panic]
fn deposit_zero_is_rejected() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup(&env);
    staking.deposit(&alice, &0);
}

#[test]
#[should_panic]
fn deposit_negative_is_rejected() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup(&env);
    staking.deposit(&alice, &-1);
}

#[test]
#[should_panic]
fn withdraw_zero_is_rejected() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup(&env);
    staking.deposit(&alice, &1000);
    staking.withdraw(&alice, &0);
}

#[test]
#[should_panic]
fn withdraw_negative_is_rejected() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup(&env);
    staking.deposit(&alice, &1000);
    staking.withdraw(&alice, &-1);
}

#[test]
fn get_pending_reward_before_claim() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup(&env);

    staking.deposit(&alice, &1000);
    assert_eq!(staking.get_pending_reward(&alice), 0);

    staking.distribute(&300);
    assert_eq!(staking.get_pending_reward(&alice), 300);
}

#[test]
fn get_pending_reward_after_claim_is_zero() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup(&env);

    staking.deposit(&alice, &1000);
    staking.distribute(&300);
    staking.claim(&alice);
    assert_eq!(staking.get_pending_reward(&alice), 0);
}

// ---------------------------------------------------------------------------
// Near-limit arithmetic tests
// ---------------------------------------------------------------------------

/// Build a staking environment whose admin has been minted `admin_balance`
/// tokens.  Used by the near-limit tests that need amounts far beyond what
/// the default `setup` helper mints.
fn setup_with_balance(
    env: &Env,
    admin_balance: i128,
) -> (StakingContractClient<'_>, TokenClient<'_>, Address, Address, Address) {
    env.mock_all_auths();
    let admin = Address::generate(env);
    let alice = Address::generate(env);
    let bob = Address::generate(env);

    let token_id = env.register_stellar_asset_contract(admin.clone());
    let token = TokenClient::new(env, &token_id);
    let sac = StellarAssetClient::new(env, &token_id);
    sac.mint(&admin, &admin_balance);

    let staking_id = env.register(StakingContract, (admin.clone(), token_id));
    let staking = StakingContractClient::new(env, &staking_id);

    (staking, token, admin, alice, bob)
}

/// The largest `amount` that can be passed to `distribute` without overflowing
/// `amount * SCALE` is `i128::MAX / SCALE`.
///
/// SCALE = 1_000_000_000_000_000_000 (1e18)
/// i128::MAX / SCALE = 170_141_183_460  (≈ 170 tokens at 18 decimals)
///
/// This test distributes exactly that amount and verifies the full reward
/// reaches the single staker — i.e., checked arithmetic does *not* fire here.
#[test]
fn distribute_near_scale_limit_succeeds() {
    // i128::MAX / SCALE — the largest safe distribution amount.
    let limit: i128 = i128::MAX / 1_000_000_000_000_000_000_i128; // 170_141_183_460

    let env = Env::default();
    // Mint twice the limit: once to distribute, once already held by the contract
    // (in practice the admin holds the tokens and transfers them in).
    let (staking, token, admin, alice, _bob) = setup_with_balance(&env, limit * 2);

    // Alice stakes 1 unit so total > 0.
    // Give Alice 1 token to deposit by minting directly to her.
    let token_id = token.address.clone();
    let sac = StellarAssetClient::new(&env, &token_id);
    sac.mint(&alice, &1);

    staking.deposit(&alice, &1);
    assert_eq!(staking.get_total_staked(), 1);

    // Distribute exactly `limit` — amount * SCALE = i128::MAX / SCALE * SCALE
    // which fits in i128 (it equals i128::MAX rounded down to a multiple of SCALE).
    staking.distribute(&limit);

    // Alice is the sole staker so she should receive the full `limit`.
    let claimed = staking.claim(&alice);
    assert_eq!(claimed, limit);
    // Alice's 1 minted token is still staked in the contract, so after claiming
    // `limit` rewards her balance is exactly `limit`.
    assert_eq!(token.balance(&alice), limit);
}

/// Distributing `i128::MAX / SCALE + 1` must panic with `Error::Overflow`
/// because `(i128::MAX / SCALE + 1) * SCALE` overflows i128.
#[test]
#[should_panic]
fn distribute_one_over_scale_limit_overflows() {
    let limit: i128 = i128::MAX / 1_000_000_000_000_000_000_i128;
    let over = limit + 1; // amount * SCALE overflows

    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup_with_balance(&env, over * 2);

    let token_id = token.address.clone();
    let sac = StellarAssetClient::new(&env, &token_id);
    sac.mint(&alice, &1);

    staking.deposit(&alice, &1);
    // This must panic with Error::Overflow.
    staking.distribute(&over);
}

/// Two successive near-limit distributions: each distribution uses half the safe limit
/// so `amount * SCALE` stays well within i128. The accumulator must correctly sum
/// both deltas.
#[test]
fn two_distributions_near_limit_accumulate_correctly() {
    // Use limit / 4 per distribution (quarter of the safe limit), distribute twice.
    // This keeps each `amount * SCALE` far from overflow while still exercising
    // large-value accumulator arithmetic.
    let limit: i128 = i128::MAX / 1_000_000_000_000_000_000_i128;
    let quarter = limit / 4;
    let two_quarters = quarter * 2; // exact, no rounding edge case

    let env = Env::default();
    // Admin needs at least 2 * quarter tokens to distribute.
    let (staking, token, _admin, alice, _bob) = setup_with_balance(&env, two_quarters + 1);

    let token_id = token.address.clone();
    let sac = StellarAssetClient::new(&env, &token_id);
    sac.mint(&alice, &1);

    staking.deposit(&alice, &1);
    staking.distribute(&quarter);
    staking.distribute(&quarter);

    // Both distributions accrue to Alice (sole staker with 1 unit, total = 1).
    // acc_per_share = 2 * quarter * SCALE / 1.
    // pending = 1 * (2 * quarter * SCALE) / SCALE = 2 * quarter.  No rounding.
    let claimed = staking.claim(&alice);
    assert_eq!(claimed, two_quarters);
    // Alice's 1 minted token is still staked in the contract.
    assert_eq!(token.balance(&alice), two_quarters);
}
