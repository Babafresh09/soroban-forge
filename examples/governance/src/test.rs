#![cfg(test)]

extern crate std;

use super::*;
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::{Address, Env};

struct Dao<'a> {
    env: Env,
    client: GovernanceContractClient<'a>,
}

/// A DAO with quorum 3, a 100-ledger voting period and `members` registered.
fn setup<'a>(members: usize) -> (Dao<'a>, std::vec::Vec<Address>) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(GovernanceContract, ());
    let client = GovernanceContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    client.initialize(&admin, &3, &100);

    let mut addrs = std::vec::Vec::new();
    for _ in 0..members {
        let member = Address::generate(&env);
        client.add_member(&member);
        addrs.push(member);
    }
    (Dao { env, client }, addrs)
}

fn end_voting(env: &Env) {
    env.ledger().with_mut(|li| li.sequence_number += 101);
}

#[test]
fn create_and_vote_and_execute() {
    let (dao, m) = setup(4);
    let pid = dao.client.create_proposal(&m[0]);
    dao.client.cast_vote(&m[0], &pid, &true);
    dao.client.cast_vote(&m[1], &pid, &true);
    dao.client.cast_vote(&m[2], &pid, &false);

    end_voting(&dao.env);
    dao.client.execute_proposal(&pid);

    let proposal = dao.client.get_proposal(&pid);
    assert!(proposal.executed);
    assert_eq!(proposal.votes_for, 2);
    assert_eq!(proposal.votes_against, 1);
}

#[test]
fn every_vote_counts_exactly_one() {
    let (dao, m) = setup(3);
    let pid = dao.client.create_proposal(&m[0]);
    for member in &m {
        dao.client.cast_vote(member, &pid, &true);
    }
    assert_eq!(dao.client.get_proposal(&pid).votes_for, 3 * VOTE_WEIGHT);
}

#[test]
fn a_single_voter_cannot_outvote_everyone_else() {
    // The attacker is a member and votes first; four others vote against.
    let (dao, m) = setup(5);
    let attacker = &m[0];
    let pid = dao.client.create_proposal(attacker);

    dao.client.cast_vote(attacker, &pid, &true);
    for member in &m[1..] {
        dao.client.cast_vote(member, &pid, &false);
    }

    // A second vote (the only other lever) is refused...
    assert!(dao.client.try_cast_vote(attacker, &pid, &true).is_err());

    let proposal = dao.client.get_proposal(&pid);
    assert_eq!(proposal.votes_for, 1);
    assert_eq!(proposal.votes_against, 4);

    // ...so the proposal fails, as the majority decided.
    end_voting(&dao.env);
    assert!(dao.client.try_execute_proposal(&pid).is_err());
    assert!(!dao.client.get_proposal(&pid).executed);
}

#[test]
fn a_lone_voter_cannot_pass_a_proposal_below_quorum() {
    // Previously one caller could pass `weight = i128::MAX`; now a single
    // vote is always 1, which can never satisfy a quorum of 3.
    let (dao, m) = setup(5);
    let pid = dao.client.create_proposal(&m[0]);
    dao.client.cast_vote(&m[0], &pid, &true);

    end_voting(&dao.env);
    let result = dao.client.try_execute_proposal(&pid);
    assert!(result.is_err(), "quorum of 3 cannot be met by one vote");
}

#[test]
#[should_panic(expected = "not eligible to vote")]
fn non_members_cannot_vote() {
    let (dao, m) = setup(2);
    let pid = dao.client.create_proposal(&m[0]);
    let outsider = Address::generate(&dao.env);
    dao.client.cast_vote(&outsider, &pid, &true);
}

#[test]
fn many_fresh_addresses_cannot_flood_a_vote() {
    // Sybil attempt: generating new addresses gives them no voting power.
    let (dao, m) = setup(3);
    let pid = dao.client.create_proposal(&m[0]);
    for _ in 0..10 {
        let sybil = Address::generate(&dao.env);
        assert!(dao.client.try_cast_vote(&sybil, &pid, &true).is_err());
    }
    assert_eq!(dao.client.get_proposal(&pid).votes_for, 0);
}

#[test]
fn members_added_after_a_proposal_cannot_vote_on_it() {
    // Membership is snapshotted at proposal creation, so the admin cannot
    // add friendly voters to swing an open proposal.
    let (dao, m) = setup(3);
    let pid = dao.client.create_proposal(&m[0]);

    dao.env.ledger().with_mut(|li| li.sequence_number += 1);
    let late = Address::generate(&dao.env);
    dao.client.add_member(&late);

    assert!(dao.client.try_cast_vote(&late, &pid, &true).is_err());

    // They can vote on proposals created after they joined.
    let next = dao.client.create_proposal(&m[0]);
    dao.client.cast_vote(&late, &next, &true);
    assert_eq!(dao.client.get_proposal(&next).votes_for, 1);
}

#[test]
fn removed_members_cannot_vote() {
    let (dao, m) = setup(3);
    let pid = dao.client.create_proposal(&m[0]);
    dao.client.remove_member(&m[2]);

    assert!(!dao.client.is_member(&m[2]));
    assert_eq!(dao.client.member_count(), 2);
    assert!(dao.client.try_cast_vote(&m[2], &pid, &false).is_err());
}

#[test]
#[should_panic(expected = "only members can create proposals")]
fn non_members_cannot_create_proposals() {
    let (dao, _m) = setup(1);
    let outsider = Address::generate(&dao.env);
    dao.client.create_proposal(&outsider);
}

#[test]
#[should_panic(expected = "already voted")]
fn double_vote_is_rejected() {
    let (dao, m) = setup(3);
    let pid = dao.client.create_proposal(&m[0]);
    dao.client.cast_vote(&m[1], &pid, &true);
    dao.client.cast_vote(&m[1], &pid, &true); // should panic
}

#[test]
#[should_panic(expected = "quorum not reached")]
fn execute_fails_below_quorum() {
    let (dao, m) = setup(3);
    let pid = dao.client.create_proposal(&m[0]);
    dao.client.cast_vote(&m[0], &pid, &true);
    dao.client.cast_vote(&m[1], &pid, &true); // quorum is 3

    end_voting(&dao.env);
    dao.client.execute_proposal(&pid);
}

#[test]
#[should_panic(expected = "quorum must be positive")]
fn zero_quorum_is_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let client = GovernanceContractClient::new(&env, &env.register(GovernanceContract, ()));
    client.initialize(&Address::generate(&env), &0, &100);
}
