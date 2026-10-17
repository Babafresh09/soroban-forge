use super:*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::{Address, Env};

// Setup with a single token used for both staking and rewards.
fn setup_same_token(env: &Env) -> (StakingContractClient<'_>, TokenClient<'_>, Address, Address, Address) {
    env.mock_all_auths();
    let admin = Address::generate(env);
    let alice = Address::generate(env);
    let bob = Address::generate(env);

    let token_id = env.register_stellar_asset_contract(admin.clone());
    let token = TokenClient::new(env, &token_id);
    let sac = StellarAssetClient::new(env, &token_id);
    sac.mint(&admin, &10_000_000_000);
    sac.mint(&alice, &10_000_000_000);
    sac.mint(&bob, &10_000_000_000);

    let staking_id = env.register(StakingContract, (admin.clone(), token_id.clone(), token_id));
    let staking = StakingContractClient::new(env, &staking_id);

    (staking, token, admin, alice, bob)
}

// Setup with separate stake and reward tokens.
fn setup_two_tokens(
    env: &Env,
) -> (
    StakingContractClient<'_>,
    TokenClient<'_>,
    TokenClient<'_>,
    Address,
    Address,
    Address,
) {
    env.mock_all_auths();
    let admin = Address::generate(env);
    let alice = Address::generate(env);
    let bob = Address::generate(env);

    let stake_id = env.register_stellar_asset_contract(admin.clone());
    let stake_token = TokenClient::new(env, &stake_id);
    let stake_sac = StellarAssetClient::new(env, &stake_id);
    stake_sac.mint(&alice, &10_000_000_000);
    stake_sac.mint(&bob, &10_000_000_000);

    let reward_id = env.register_stellar_asset_contract(admin.clone());
    let reward_token = TokenClient::new(env, &reward_id);
    let reward_sac = StellarAssetClient::new(env, &reward_id);
    reward_sac.mint(&admin, &10_000_000_000);

    let staking_id = env.register(StakingContract, (admin.clone(), stake_id, reward_id));
    let staking = StakingContractClient::new(env, &staking_id);

    (staking, stake_token, reward_token, admin, alice, bob)
}

// ----------------------------------------------------------------------------
// Same-token setup tests
hr ----------------------------------------------------------------------------

#[test]
fn single_staker_distribution() {
    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup_same_token(&env);

    staking.deposit(&alice, &1000);
    assert_eq!(staking.get_staked(&alice), 1000);
    assert_eq!(staking.get_total_staked(), 1000);

    staking.distribute(&500);

    let claimed = staking.claim(&alice);
    assert_eq!(claimed, 500);
    assert_eq!(token.balance(&alice), 10_000_000_000 - 1000 + 500);
}

#[test]
fn multi_staker_proportional_distribution() {
    let env = Env::default();
    let (staking, token, _admin, alice, bob) = setup_same_token(&env);

    staking.deposit(&alice, &1000);
    staking.deposit(&bob, &3000);
    assert_eq!(staking.get_total_staked(), 4000);

    staking.distribute(&400);

    let alice_claimed = staking.claim(&alice);
    let bob_claimed = staking.claim(&bob);

    // Alice: 1/4 of 400 = 100, Bob: 3/4 of 400 = 300
    assert_eq!(alice_claimed, 100);
    assert_eq!(bob_claimed, 300);
    assert_eq!(token.balance(&alice), 10_000_000_000 - 1000 + 100);
    assert_eq!(token.balance(&bob), 10_000_000_000 - 3000 + 300);
}

#[test]
fn distribute_with_no_stakers_is_noop() {
    let env = Env::default();
    let (staking, _token, _admin, _alice, _bob) = setup_same_token(&env);

    staking.distribute(&500);
    assert_eq!(staking.get_acc_reward_per_share(), 0);
}

#[test]
fn claim_after_multiple_distributions() {
    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup_same_token(&env);

    staking.deposit(&alice, &1000);
    staking.distribute(&200);
    staking.distribute(&300);

    let claimed = staking.claim(&alice);
    assert_eq!(claimed, 500);
    assert_eq!(token.balance(&alice), 10_000_000_000 - 1000 + 500);
}

#[test]
fn withdraw_claims_pending_first() {
    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup_same_token(&env);

    staking.deposit(&alice, &1000);
    staking.distribute(&500);
    staking.withdraw(&alice, &500);

    // withdraw triggered internal claim of 500, then withdrew 500
    assert_eq!(staking.get_staked(&alice), 500);
    assert_eq!(staking.get_total_staked(), 500);
    assert_eq!(token.balance(&alice), 10_000_000_000 - 1000 + 500);
}

#[test]
fn rounding_handling() {
    let env = Env::default();
    let (staking, token, _admin, alice, bob) = setup_same_token(&env);

    // Alice 1/3, Bob 2/3 — 10 rewards
    staking.deposit(&alice, &100);
    staking.deposit(&bob, &200);
    staking.distribute(&10);

    let alice_claimed = staking.claim(&alice);
    let bob_claimed = staking.claim(&bob);

    // floor(10 * 100 / 300) = 3, floor(10 * 200 / 300) = 6
    // some dust may be lost to rounding
    assert_eq!(alice_claimed, 3);
    assert_eq!(bob_claimed, 6);
    assert_eq!(token.balance(&alice), 10_000_000_000 - 100 + 3);
    assert_eq!(token.balance(&bob), 10_000_000_000 - 200 + 6);
    // 1 unit lost to integer rounding
    assert_eq!(alice_claimed + bob_claimed, 9);
}

#[test]
fn multiple_users_can_deposit_before_rewards() {
    let env = Env::default();
    let (staking, token, _admin, alice, bob) = setup_same_token(&env);

    staking.deposit(&alice, &2000);
    staking.deposit(&bob, &2000);
    staking.distribute(&1000);

    let a = staking.claim(&alice);
    let b = staking.claim(&bob);
    assert_eq!(a, 500);
    assert_eq!(b, 500);
    assert_eq!(token.balance(&alice), 10_000_000_000 - 2000 + 500);
    assert_eq!(token.balance(&bob), 10_000_000_000 - 2000 + 500);
}

#[test]
fn accumulator_tracks_multiple_distributions_after_withdraw() {
    let env = Env::default();
    let (staking, _token, _admin, alice, bob) = setup_same_token(&env);

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
    let (staking, _token, _admin, alice, _bob) = setup_same_token(&env);
    staking.deposit(&alice, &0);
}

#[test]
#[should_panic]
fn deposit_negative_is_rejected() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup_same_token(&env);
    staking.deposit(&alice, &-1);
}

#[test]
#[should_panic]
fn withdraw_zero_is_rejected() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup_same_token(&env);
    staking.deposit(&alice, &1000);
    staking.withdraw(&alice, &0);
}

#[test]
#[should_panic]
fn withdraw_negative_is_rejected() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup_same_token(&env);
    staking.deposit(&alice, &1000);
    staking.withdraw(&alice, &-1);
}

#[test]
fn get_pending_reward_before_claim() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup_same_token(&env);

    staking.deposit(&alice, &1000);
    assert_eq!(staking.get_pending_reward(&alice), 0);

    staking.distribute(&300);
    assert_eq!(staking.get_pending_reward(&alice), 300);
}

#[test]
fn get_pending_reward_after_claim_is_zero() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup_same_token(&env);

    staking.deposit(&alice, &1000);
    staking.distribute(&300);
    staking.claim(&alice);
    assert_eq!(staking.get_pending_reward(&alice), 0);
}

// ----------------------------------------------------------------------------
// Two-token setup tests
hr ----------------------------------------------------------------------------

#[test]
fn two_tokens_deposit_uses_stake_token() {
    let env = Env::default();
    let (staking, stake_token, reward_token, _admin, alice, _bob) =
        setup_two_tokens(&env);

    staking.deposit(&alice, &1000);
    assert_eq!(staking.get_staked(&alice), 1000);
    // Stake token moved into contract
    assert_eq!(stake_token.balance(&alice), 10_000_000_000 - 1000);
    // Reward token untouched
    assert_eq!(reward_token.balance(&alice), 0);
}

#[test]
fn two_tokens_distribute_and_claim_use_reward_token() {
    let env = Env::default();
    let (staking, stake_token, reward_token, _admin, alice, _bob) =
        setup_two_tokens(&env);

    staking.deposit(&alice, &1000);
    staking.distribute(&500);

    let claimed = staking.claim(&alice);
    assert_eq!(claimed, 500);

    // Rewards paid in reward token, stake unchanged
    assert_eq!(reward_token.balance(&alice), 500);
    assert_eq!(stake_token.balance(&alice), 10_000_000_000 - 1000);
    assert_eq!(staking.get_staked(&alice), 1000);
}

#[test]
fn two_tokens_withdraw_returns_stake_token() {
    let env = Env::default();
    let (staking, stake_token, reward_token, _admin, alice, _bob) =
        setup_two_tokens(&env);

    staking.deposit(&alice, &1000);
    staking.distribute(&500);
    staking.withdraw(&alice, &400);

    // Internal claim paid 500 reward tokens, withdraw returned 400 stake tokens
    assert_eq!(reward_token.balance(&alice), 500);
    assert_eq!(stake_token.balance(&alice), 10_000_000_000 - 1000 + 400);
    assert_eq!(staking.get_staked(&alice), 600);
    assert_eq!(staking.get_total_staked(), 600);
}

#[test]
fn two_tokens_multi_staker_proportional() {
    let env = Env::default();
    let (staking, _stake_token, reward_token, _admin, alice, bob) =
        setup_two_tokens(&env);

    staking.deposit(&alice, &1000);
    staking.deposit(&bob, &3000);
    staking.distribute(&400);

    let a = staking.claim(&alice);
    let b = staking.claim(&boj);
    assert_eq!(a, 100);
    assert_eq!(b, 300);
    assert_eq!(reward_token.balance(&alice), 100);
    assert_eq!(reward_token.balance(&bob), 300);
}

#[test]
fn two_tokens_getters_return_configured_tokens() {
    let env = Env::default();
    let (staking, stake_token, reward_token, _admin, _alice, _bob) =
        setup_two_tokens(&env);

    assert_eq!(staking.get_stake_token(), stake_token.address);
    assert_eq!(staking.get_reward_token(), reward_token.address);
}

#[test]
fn same_token_getters_return_same_token() {
    let env = Env::default();
    let (staking, token, _admin, _alice, _bob) = setup_same_token(&env);

    assert_eq!(staking.get_stake_token(), token.address);
    assert_eq!(staking.get_reward_token(), token.address);
}
