use super::*;
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::{Address, Env};

const YES: bool = true;
const NO: bool = false;

/// Betting closes at this ledger timestamp. `Env::default()` starts at 0, so
/// this is always in the future when the market is deployed.
const CLOSE_TIME: u64 = 1_000;

/// Generated `try_*` methods report contract errors as `soroban_sdk::Error`,
/// so convert ours before comparing.
fn err(e: Error) -> soroban_sdk::Error {
    soroban_sdk::Error::from(e)
}
struct Setup<'a> {
    market: PredictionMarketContractClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    oracle: Address,
    token_address: Address,
}

fn setup(env: &Env) -> Setup<'_> {
    setup_with_close_time(env, CLOSE_TIME)
}

fn setup_with_close_time(env: &Env, close_time: u64) -> Setup<'_> {
    env.mock_all_auths();

    let oracle = Address::generate(env);
    let token_issuer = Address::generate(env);
    let sac = env.register_stellar_asset_contract_v2(token_issuer);
    let token_address = sac.address();

    let market_id = env.register(
        PredictionMarketContract,
        (oracle.clone(), token_address.clone(), close_time),
    );

    Setup {
        market: PredictionMarketContractClient::new(env, &market_id),
        token: TokenClient::new(env, &token_address),
        token_admin: StellarAssetClient::new(env, &token_address),
        oracle,
        token_address,
    }
}

fn set_time(env: &Env, timestamp: u64) {
    env.ledger().set_timestamp(timestamp);
}

/// Move the ledger to the close instant, so betting is closed and resolving
/// is open.
fn close_betting(env: &Env) {
    set_time(env, CLOSE_TIME);
}

/// Mint `amount` to a fresh address and stake it all on `outcome`.
fn staker(env: &Env, s: &Setup, outcome: bool, amount: i128) -> Address {
    let who = Address::generate(env);
    s.token_admin.mint(&who, &amount);
    s.market.stake(&who, &outcome, &amount);
    who
}

// --- constructor ---

#[test]
fn constructor_stores_oracle_token_and_close_time() {
    let env = Env::default();
    let s = setup(&env);

    assert_eq!(s.market.get_oracle(), s.oracle);
    assert_eq!(s.market.get_token(), s.token_address);
    assert_eq!(s.market.get_close_time(), CLOSE_TIME);
    assert_eq!(s.market.get_outcome(), None);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn constructor_rejects_close_time_in_the_past() {
    let env = Env::default();
    set_time(&env, 500);
    setup_with_close_time(&env, 499);
}

#[test]
#[should_panic(expected = "Error(Contract, #7)")]
fn constructor_rejects_close_time_equal_to_now() {
    let env = Env::default();
    set_time(&env, 500);
    setup_with_close_time(&env, 500);
}

// --- staking window ---

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
fn stake_succeeds_one_second_before_close() {
    let env = Env::default();
    let s = setup(&env);
    set_time(&env, CLOSE_TIME - 1);

    let alice = staker(&env, &s, YES, 100);

    assert_eq!(s.market.get_stake(&alice, &YES), 100);
    assert_eq!(s.market.get_total_pool(), 100);
}

#[test]
fn stake_fails_at_the_close_instant() {
    let env = Env::default();
    let s = setup(&env);
    set_time(&env, CLOSE_TIME);

    let late = Address::generate(&env);
    s.token_admin.mint(&late, &100);

    assert_eq!(
        s.market.try_stake(&late, &YES, &100),
        Err(Ok(err(Error::BettingClosed)))
    );
    // The rejected stake moved no tokens and recorded nothing.
    assert_eq!(s.token.balance(&late), 100);
    assert_eq!(s.market.get_total_pool(), 0);
}

#[test]
fn stake_fails_after_the_close_time() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);
    set_time(&env, CLOSE_TIME + 3_600);

    let late = Address::generate(&env);
    s.token_admin.mint(&late, &100);

    assert_eq!(
        s.market.try_stake(&late, &NO, &100),
        Err(Ok(err(Error::BettingClosed)))
    );
    assert_eq!(s.market.get_pool(&NO), 0);
    assert_eq!(s.market.get_total_pool(), 100);
}

#[test]
fn zero_amount_stake_is_rejected() {
    let env = Env::default();
    let s = setup(&env);

    let alice = Address::generate(&env);
    s.token_admin.mint(&alice, &100);

    assert_eq!(
        s.market.try_stake(&alice, &YES, &0),
        Err(Ok(err(Error::InvalidAmount)))
    );
}

// --- resolving window ---

#[test]
fn resolve_fails_before_close() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);

    // Right at deploy time and one second before the close: both too early.
    assert_eq!(
        s.market.try_resolve(&s.oracle, &YES),
        Err(Ok(err(Error::BettingStillOpen)))
    );
    set_time(&env, CLOSE_TIME - 1);
    assert_eq!(
        s.market.try_resolve(&s.oracle, &YES),
        Err(Ok(err(Error::BettingStillOpen)))
    );
    assert_eq!(s.market.get_outcome(), None);
}

#[test]
fn resolve_succeeds_at_the_close_instant() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);
    close_betting(&env);

    assert_eq!(s.market.get_outcome(), None);
    s.market.resolve(&s.oracle, &YES);
    assert_eq!(s.market.get_outcome(), Some(YES));
}

#[test]
fn resolve_succeeds_after_the_close_time() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, NO, 100);
    set_time(&env, CLOSE_TIME + 10_000);

    s.market.resolve(&s.oracle, &NO);
    assert_eq!(s.market.get_outcome(), Some(NO));
}

#[test]
fn only_the_oracle_can_resolve() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);
    close_betting(&env);

    // A perfectly ordinary address, authorized but not the designated
    // oracle, must not be able to report an outcome, even once resolving
    // is open.
    let impostor = Address::generate(&env);
    assert_eq!(
        s.market.try_resolve(&impostor, &YES),
        Err(Ok(err(Error::NotOracle)))
    );
    assert_eq!(s.market.get_outcome(), None);
}

#[test]
fn resolve_twice_is_rejected() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);
    close_betting(&env);

    s.market.resolve(&s.oracle, &YES);
    assert_eq!(
        s.market.try_resolve(&s.oracle, &NO),
        Err(Ok(err(Error::AlreadyResolved)))
    );
    assert_eq!(s.market.get_outcome(), Some(YES));
}

#[test]
fn stake_after_resolution_is_rejected() {
    let env = Env::default();
    let s = setup(&env);
    staker(&env, &s, YES, 100);
    close_betting(&env);
    s.market.resolve(&s.oracle, &YES);

    let late = Address::generate(&env);
    s.token_admin.mint(&late, &100);
    assert_eq!(
        s.market.try_stake(&late, &NO, &100),
        Err(Ok(err(Error::AlreadyResolved)))
    );
}

// --- claims ---

#[test]

fn winners_split_the_pool_proportionally() {
    let env = Env::default();
    let s = setup(&env);

    let alice = staker(&env, &s, YES, 600);
    let bob = staker(&env, &s, YES, 400);
    let carol = staker(&env, &s, NO, 500);
    assert_eq!(s.market.get_total_pool(), 1_500);

    close_betting(&env);
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
fn double_claim_is_rejected() {
    let env = Env::default();

    let s = setup(&env);

    let alice = staker(&env, &s, YES, 600);
    staker(&env, &s, NO, 400);
    close_betting(&env);
    s.market.resolve(&s.oracle, &YES);

    s.market.claim(&alice);
    assert_eq!(
        s.market.try_claim(&alice),
        Err(Ok(err(Error::AlreadyClaimed)))
    );
}

#[test]
fn loser_cannot_claim() {
    let env = Env::default();
    let s = setup(&env);

    staker(&env, &s, YES, 600);
    let bob = staker(&env, &s, NO, 400);
    close_betting(&env);
    s.market.resolve(&s.oracle, &YES);

    assert_eq!(
        s.market.try_claim(&bob),
        Err(Ok(err(Error::NothingToClaim)))
    );
}

#[test]
fn claim_before_resolution_is_rejected() {
    let env = Env::default();
    let s = setup(&env);
    let alice = staker(&env, &s, YES, 600);

    assert_eq!(s.market.try_claim(&alice), Err(Ok(err(Error::NotResolved))));

    // Still unresolved after the close: closing alone pays nobody.
    close_betting(&env);
    assert_eq!(s.market.try_claim(&alice), Err(Ok(err(Error::NotResolved))));
}
