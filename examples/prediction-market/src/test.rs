use super::*;
use soroban_sdk::testutils::Address as _;
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::{Address, Env};

const YES: bool = true;
const NO: bool = false;

struct Setup<'a> {
    market: PredictionMarketContractClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    oracle: Address,
}

fn setup(env: &Env) -> Setup<'_> {
    env.mock_all_auths();

    let oracle = Address::generate(env);
    let token_issuer = Address::generate(env);
    let sac = env.register_stellar_asset_contract_v2(token_issuer);
    let token_address = sac.address();

    let market_id = env.register(
        PredictionMarketContract,
        (oracle.clone(), token_address.clone()),
    );

    Setup {
        market: PredictionMarketContractClient::new(env, &market_id),
        token: TokenClient::new(env, &token_address),
        token_admin: StellarAssetClient::new(env, &token_address),
        oracle,
    }
}

/// Mint `amount` to a fresh address and stake it all on `outcome`.
fn staker(env: &Env, s: &Setup, outcome: bool, amount: i128) -> Address {
    let who = Address::generate(env);
    s.token_admin.mint(&who, &amount);
    s.market.stake(&who, &outcome, &amount);
    who
}

#[test]
fn stake_escrows_tokens_and_records_position() {
    let env = Env::default();
    let s = setup(&env);

    let alice = staker(&env, &s, YES, 600);
    let bob = staker(&env, &s, NO, 400);

    // The stake left the staker and is held by the market.
    assert_eq!(s.token.balance(&alice), 0);
    assert_eq!(s.token.balance(&bob), 0);
    assert_eq!(s.token.balance(&s.market.address), 1_000);

    // Positions and pools track each side separately.
    assert_eq!(s.market.get_stake(&alice, &YES), 600);
    assert_eq!(s.market.get_stake(&alice, &NO), 0);
    assert_eq!(s.market.get_pool(&YES), 600);
    assert_eq!(s.market.get_pool(&NO), 400);
    assert_eq!(s.market.get_total_pool(), 1_000);
}

#[test]
fn repeated_stakes_accumulate() {
    let env = Env::default();
    let s = setup(&env);

    let alice = Address::generate(&env);
    s.token_admin.mint(&alice, &500);
    s.market.stake(&alice, &YES, &200);
    s.market.stake(&alice, &YES, &300);

    assert_eq!(s.market.get_stake(&alice, &YES), 500);
    assert_eq!(s.market.get_pool(&YES), 500);
}

#[test]
fn oracle_resolves_and_outcome_is_readable() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);

    assert_eq!(s.market.get_outcome(), None);
    s.market.resolve(&s.oracle, &YES);
    assert_eq!(s.market.get_outcome(), Some(YES));
}

#[test]
#[should_panic]
fn only_the_oracle_can_resolve() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);

    // A perfectly ordinary address — authorized, but not the designated
    // oracle — must not be able to report an outcome.
    let impostor = Address::generate(&env);
    s.market.resolve(&impostor, &YES);
}

#[test]
fn winners_split_the_pool_proportionally() {
    let env = Env::default();
    let s = setup(&env);

    let alice = staker(&env, &s, YES, 600);
    let bob = staker(&env, &s, YES, 400);
    let carol = staker(&env, &s, NO, 500);
    assert_eq!(s.market.get_total_pool(), 1_500);

    s.market.resolve(&s.oracle, &YES);

    // Each winner takes stake * total_pool / winning_pool:
    //   alice 600 * 1500 / 1000 = 900
    //   bob   400 * 1500 / 1000 = 600
    assert_eq!(s.market.claim(&alice), 900);
    assert_eq!(s.market.claim(&bob), 600);

    assert_eq!(s.token.balance(&alice), 900);
    assert_eq!(s.token.balance(&bob), 600);
    assert_eq!(s.token.balance(&carol), 0);

    // The losing side's stakes are exactly what funded the winners' profit,
    // and the market is left empty.
    assert_eq!(s.token.balance(&s.market.address), 0);
    assert!(s.market.has_claimed(&alice));
    assert!(s.market.has_claimed(&bob));
}

#[test]
#[should_panic]
fn double_claim_panics() {
    let env = Env::default();
    let s = setup(&env);

    let alice = staker(&env, &s, YES, 600);
    staker(&env, &s, NO, 400);
    s.market.resolve(&s.oracle, &YES);

    s.market.claim(&alice);
    s.market.claim(&alice);
}

#[test]
#[should_panic]
fn loser_cannot_claim() {
    let env = Env::default();
    let s = setup(&env);

    staker(&env, &s, YES, 600);
    let bob = staker(&env, &s, NO, 400);
    s.market.resolve(&s.oracle, &YES);

    s.market.claim(&bob);
}

#[test]
#[should_panic]
fn claim_before_resolution_panics() {
    let env = Env::default();
    let s = setup(&env);
    let alice = staker(&env, &s, YES, 600);

    s.market.claim(&alice);
}

#[test]
#[should_panic]
fn resolve_twice_panics() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);

    s.market.resolve(&s.oracle, &YES);
    s.market.resolve(&s.oracle, &NO);
}

#[test]
#[should_panic]
fn stake_after_resolution_panics() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);
    s.market.resolve(&s.oracle, &YES);

    staker(&env, &s, NO, 100);
}

#[test]
#[should_panic]
fn zero_amount_stake_panics() {
    let env = Env::default();
    let s = setup(&env);

    let alice = Address::generate(&env);
    s.token_admin.mint(&alice, &100);
    s.market.stake(&alice, &YES, &0);
}
