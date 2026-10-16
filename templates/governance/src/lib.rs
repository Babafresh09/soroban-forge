#![no_std]

//! DAO-style governance: members open proposals and vote on them; a proposal
//! executes once its voting period ends with quorum and a majority in favour.
//!
//! **Voting power.** Every registered member has exactly one vote per
//! proposal. Callers cannot choose their weight: `cast_vote` takes no weight
//! argument and always counts `VOTE_WEIGHT` (1), so a single voter can never
//! outvote everyone else, and negative or inflated votes are impossible by
//! construction.
//!
//! Members are registered by the admin. Eligibility is snapshotted per
//! proposal: only members registered at or before the ledger a proposal was
//! created in may vote on it, so adding members mid-vote cannot swing an open
//! proposal. Removing a member revokes their ability to vote from then on.
//!
//! To weight votes by token holdings instead, replace `voting_power` with a
//! balance read through `token::Client` — and snapshot balances when the
//! proposal is created, or the same tokens can be moved between accounts and
//! voted with repeatedly.

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env};

/// Votes counted for each eligible voter.
pub const VOTE_WEIGHT: i128 = 1;

/// A governance proposal.
#[contracttype]
#[derive(Clone)]
pub struct Proposal {
    pub id: u64,
    pub proposer: Address,
    /// Ledger sequence the proposal was created in; only members registered
    /// at or before it may vote (membership snapshot).
    pub created_ledger: u32,
    /// Absolute ledger sequence at which voting closes.
    pub voting_deadline: u32,
    /// Number of members who voted in favour (one vote each).
    pub votes_for: i128,
    /// Number of members who voted against.
    pub votes_against: i128,
    /// Whether the proposal has been executed.
    pub executed: bool,
}

#[contracttype]
pub enum DataKey {
    Proposal(u64),
    /// Set of (proposal_id, voter) pairs that have already voted.
    Voted(u64, Address),
    NextId,
    /// Minimum quorum (total votes required to execute).
    Quorum,
    /// Duration in ledgers for the voting period.
    VotingPeriod,
    Admin,
    /// Ledger sequence at which an address became a voting member.
    MemberSince(Address),
    /// Number of registered members.
    MemberCount,
}

#[contract]
pub struct GovernanceContract;

fn admin(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .expect("not initialized")
}

/// Voting power of `voter` on `proposal`: `VOTE_WEIGHT` for a member who was
/// registered when the proposal was created, otherwise none. Never derived
/// from anything the voter passes in.
fn voting_power(env: &Env, voter: &Address, proposal: &Proposal) -> i128 {
    match env
        .storage()
        .instance()
        .get::<_, u32>(&DataKey::MemberSince(voter.clone()))
    {
        Some(since) if since <= proposal.created_ledger => VOTE_WEIGHT,
        _ => 0,
    }
}

#[contractimpl]
impl GovernanceContract {
    /// Initialize the contract.
    ///
    /// * `admin`         — address allowed to manage members
    /// * `quorum`        — minimum total votes (for + against) required to execute;
    ///   with one vote per member this is a number of members
    /// * `voting_period` — voting window in ledgers (~5 s per ledger on mainnet)
    pub fn initialize(env: Env, admin: Address, quorum: i128, voting_period: u32) {
        admin.require_auth();
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        if quorum <= 0 {
            panic!("quorum must be positive");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Quorum, &quorum);
        env.storage()
            .instance()
            .set(&DataKey::VotingPeriod, &voting_period);
        env.storage().instance().set(&DataKey::NextId, &0u64);
        env.storage().instance().set(&DataKey::MemberCount, &0u32);
    }

    /// Register `member` as a voter (admin only). Members may vote on
    /// proposals created from this ledger onwards.
    pub fn add_member(env: Env, member: Address) {
        admin(&env).require_auth();
        let key = DataKey::MemberSince(member);
        if env.storage().instance().has(&key) {
            panic!("already a member");
        }
        env.storage()
            .instance()
            .set(&key, &env.ledger().sequence());
        let count: u32 = Self::member_count(env.clone());
        env.storage()
            .instance()
            .set(&DataKey::MemberCount, &(count + 1));
    }

    /// Remove `member` (admin only). They can no longer vote; votes they
    /// already cast stay counted.
    pub fn remove_member(env: Env, member: Address) {
        admin(&env).require_auth();
        let key = DataKey::MemberSince(member);
        if !env.storage().instance().has(&key) {
            panic!("not a member");
        }
        env.storage().instance().remove(&key);
        let count: u32 = Self::member_count(env.clone());
        env.storage()
            .instance()
            .set(&DataKey::MemberCount, &(count - 1));
    }

    /// Whether `address` is currently a registered member.
    pub fn is_member(env: Env, address: Address) -> bool {
        env.storage()
            .instance()
            .has(&DataKey::MemberSince(address))
    }

    /// Number of registered members.
    pub fn member_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::MemberCount)
            .unwrap_or(0)
    }

    /// Create a new governance proposal (members only).
    ///
    /// Returns the proposal id.
    pub fn create_proposal(env: Env, proposer: Address) -> u64 {
        proposer.require_auth();
        if !Self::is_member(env.clone(), proposer.clone()) {
            panic!("only members can create proposals");
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextId)
            .unwrap_or(0);
        let voting_period: u32 = env
            .storage()
            .instance()
            .get(&DataKey::VotingPeriod)
            .unwrap_or(1000);
        let created_ledger = env.ledger().sequence();
        let deadline = created_ledger + voting_period;

        let proposal = Proposal {
            id,
            proposer,
            created_ledger,
            voting_deadline: deadline,
            votes_for: 0,
            votes_against: 0,
            executed: false,
        };
        env.storage()
            .instance()
            .set(&DataKey::Proposal(id), &proposal);
        env.storage()
            .instance()
            .set(&DataKey::NextId, &(id + 1));
        id
    }

    /// Cast a vote on a proposal.
    ///
    /// * `in_favour` — `true` to vote for, `false` to vote against
    ///
    /// The vote counts exactly `VOTE_WEIGHT` (1). Voting power comes from
    /// membership at the time the proposal was created, never from the
    /// caller, so it cannot be inflated or negative.
    pub fn cast_vote(env: Env, voter: Address, proposal_id: u64, in_favour: bool) {
        voter.require_auth();

        // Prevent double-voting.
        let voted_key = DataKey::Voted(proposal_id, voter.clone());
        if env.storage().instance().has(&voted_key) {
            panic!("already voted on this proposal");
        }

        let mut proposal: Proposal = env
            .storage()
            .instance()
            .get(&DataKey::Proposal(proposal_id))
            .expect("proposal not found");

        if env.ledger().sequence() > proposal.voting_deadline {
            panic!("voting period has ended");
        }

        let power = voting_power(&env, &voter, &proposal);
        if power <= 0 {
            panic!("not eligible to vote on this proposal");
        }

        if in_favour {
            proposal.votes_for += power;
        } else {
            proposal.votes_against += power;
        }

        env.storage()
            .instance()
            .set(&DataKey::Proposal(proposal_id), &proposal);
        env.storage().instance().set(&voted_key, &true);
    }

    /// Execute a proposal after its voting period has ended.
    ///
    /// Succeeds only when:
    /// 1. The voting deadline has passed.
    /// 2. Total votes meet or exceed the configured quorum.
    /// 3. Votes in favour exceed votes against.
    pub fn execute_proposal(env: Env, proposal_id: u64) {
        let mut proposal: Proposal = env
            .storage()
            .instance()
            .get(&DataKey::Proposal(proposal_id))
            .expect("proposal not found");

        if env.ledger().sequence() <= proposal.voting_deadline {
            panic!("voting period has not ended yet");
        }
        if proposal.executed {
            panic!("proposal already executed");
        }

        let quorum: i128 = env
            .storage()
            .instance()
            .get(&DataKey::Quorum)
            .unwrap_or(1);
        let total = proposal.votes_for + proposal.votes_against;
        if total < quorum {
            panic!("quorum not reached");
        }
        if proposal.votes_for <= proposal.votes_against {
            panic!("proposal did not pass");
        }

        proposal.executed = true;
        env.storage()
            .instance()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        // TODO: trigger on-chain action here (e.g. call another contract).
    }

    /// Return the current state of a proposal.
    pub fn get_proposal(env: Env, proposal_id: u64) -> Proposal {
        env.storage()
            .instance()
            .get(&DataKey::Proposal(proposal_id))
            .expect("proposal not found")
    }
}

#[cfg(test)]
mod test;
