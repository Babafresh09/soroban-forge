use super::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::{Address, Env};

fn setup(env: &Env) -> (StakingContractClient<'_>, TokenClient<'_>, Address, Address, Address) {
    env.mock_all_auths();
    let admin = Address::generate(env);
    let alice = Address::generate(env);
    let bob = Address::generate(env);

    let token_id = env.register_stellar_asset_contract(admin.clone());
    let token = TokenClient::new(env, &token_id);
    let sac = StellarAssetClient::new(env, &token_id);
    sac.mint(&admin, &alice, &10_000_000_000);
    sac.mint(&admin, &bob, &10_000_000_000);

    let staking_id = env.register(StakingContract, (admin.clone(), token_id));
    let staking = StakingContractClient::new(env, &staking_id);

    (staking, token, admin, alice, bob)
}

#[test]
fn deposit_moves_tokens_into_contract() {
    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup(&env);

    assert_eq!(token.balance(&alice), 10_000_000_000);
    staking.deposit(&alice, &1000);
    assert_eq!staking.get_staked(&alice), 1000);
    assert_eq!(token.balance(&alice), 10_000_000_000 - 1000);
    assert_eq!(token.balance(&staking.address), 1000);
}

#[test]
fn withdraw_moves_tokens_back_to_user() {
    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup(&env);

    staking.deposit(&alice, &1000);
    assert_eq!(token.balance(&alice), 10_000_000_000 - 1000);
    assert_eq!(token.balance(&staking.address), 1000);

    staking.withdraw(&alice, &1000);
    assert_eq!(staking.get_staked(&alice), 0);
    assert_eq!(token.balance(&alice), 10_000_000_000);
    assert_eq!(token.balance(&staking.address), 0);
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
    assert_eq!(token.balance(&alice), 10_000_000_000 - 1000 + 500);
}

#[test]
fn multi_staker_proportional_distribution() {
    let env = Env::default();
    let (staking, token, _admin, alice, bob) = setup(&env);

    staking.deposit(&alice, &1000);
    staking.deposit(&bob, &3000);
    assert_eq!(staking.get_total_staked(), 4000);
    assert_eq!(token.balance(&staking.address), 4000);

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
    let (staking, token, admin, _alice, _bob) = setup(&env);

    let before = token.balance(&admin);
    staking.distribute(&500);
    assert_eq!(staking.get_acc_reward_per_share(), 0);
    assert_eq!(token.balance(&admin), before);
}

#[derive(Clone)]
struct SetupResult<'a> {
    staking: StakingContractClient<'a>,
    token: TokenClient<'a>,
    admin: Address,
    alice: Address,
    bob: Address,
}

#[allow(dead_code)]
fn _unused_setup_struct() {}

#[test]
fn claim_after_multiple_distributions() {
    let env = Env::default();
    let (staking, token, _admin, alice, _bob) = setup(&env);

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
    let (staking, token, _admin, alice, _bob) = setup(&env);

    staking.deposit(&alice, &1000);
    staking.distribute(&500);
    staking.withdraw(&alice, &500);

    // withdraw triggered internal claim of 500, then withdrew 500
    assert_eq!(staking.get_staked(&alice), 500);
    assert_eq!(staking.get_total_staked(), 500);
    assert_eq!(token.balance(&alice), 10_000_000_000 - 1000 + 500 + 500);
    assert_eq!(token.balance(&staking.address), 500);
}

#[test]
fn rounding_handling() {
    let env = Env::default();
    let (staking, token, _admin, alice, bob) = setup(&env);

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
    let (staking, token, _admin, alice, bob) = setup(&env);

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

#[derive(Clone)]
struct _Unused;

#[test]
fn rewards_only_split_among_deposited_tokens() {
    let env = Env::default();
    let (staking, token, _admin, alice, bob) = setup(&env);

    // Alice deposits 1000, Bob deposits nothing.
    staking.deposit(&alice, &1000);
    assert_eq!(staking.get_staked(&bob), 0);

    staking.distribute(&500);

    // Bob cannot claim anything because he never deposited.
    assert_eq!(staking.get_pending_reward(&bob), 0);
    assert_eq!(token.balance(&bob), 10_000_000_000);

    // Alice gets the full reward.
    let claimed = staking.claim(&alice);
    assert_eq!(claimed, 500);
    assert_eq!(token.balance(&alice), 10_000_000_000 - 1000 + 500);
    assert_eq!(token.balance(&staking.address), 1000);
}

#[test]
#[should_panic]
fn deposit_without_balance_fails() {
    let env = Env::default();
    let (staking, _token, _admin, alice, _bob) = setup(&env);
    // Alice has no funds in this custom setup.
    let admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract(admin.clone());
    let staking_id = env.register(StakingContract, (admin.clone(), token_id));
    let staking2 = StakingContractClient::new(&env, &staking_id);
    staking2.deposit(&alice, &1000);
    let _ = staking;
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
