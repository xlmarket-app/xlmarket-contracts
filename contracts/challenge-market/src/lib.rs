//! XLMarket — challenge-market contract
//!
//! A pari-mutuel style prediction market where the "outcome" is real Stellar
//! network data (ledger close time, tx counts, fee spikes, etc.) rather than
//! game logic made up for the app.
//!
//! Design notes (read this before extending):
//!
//! - Two resolution paths are supported on purpose:
//!     1. `resolve_native`  — fully trustless. Used for anything the Soroban
//!        host environment already exposes (ledger sequence / timestamp).
//!        No oracle, no trusted party, anyone can call it once the target
//!        ledger has closed.
//!     2. `resolve_via_oracle` — used for metrics that live in Horizon but
//!        are NOT exposed to the contract host (tx counts, fee stats,
//!        anything aggregated across ledgers). Restricted to a single
//!        trusted relayer address for the MVP. The interface is written so
//!        a quorum-of-relayers scheme can replace the single address later
//!        without touching the pool/staking logic below.
//! - Pool logic (stake / claim) is intentionally separate from resolution,
//!   so it's easy to audit and easy to swap resolution mechanisms later.
//! - Payout is pro-rata parimutuel: winners split the *losing* pool
//!   proportional to their stake in the winning pool, plus get their own
//!   stake back. No AMM, no share tokens — deliberately simple for v1.

#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env, String,
    Symbol,
};

// ---------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------

const EVENT_CREATED: Symbol = symbol_short!("created");
const EVENT_STAKED: Symbol = symbol_short!("staked");
const EVENT_RESOLVED: Symbol = symbol_short!("resolved");
const EVENT_CLAIMED: Symbol = symbol_short!("claimed");
const EVENT_CANCELLED: Symbol = symbol_short!("cancelled");
const EVENT_REFUNDED: Symbol = symbol_short!("refunded");

// ---------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------

/// What kind of network condition this challenge resolves against.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Condition {
    /// Resolves natively. YES wins if the ledger at `resolve_ledger_seq`
    /// closed within the given number of seconds of the challenge's
    /// creation timestamp. No oracle required.
    LedgerCloseUnder(u32),

    /// Resolves via oracle. YES wins if the tx count observed by the
    /// relayer over the challenge window is >= the given threshold.
    TxCountAtLeast(u32),

    /// Resolves via oracle. YES wins if the relayer observes a base fee
    /// spike >= the given threshold, in stroops.
    BaseFeeAtLeast(i128),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Challenge {
    pub id: u64,
    pub creator: Address,
    pub description: String,
    pub condition: Condition,
    /// Ledger sequence at/after which this challenge can be resolved.
    pub resolve_ledger_seq: u32,
    /// Ledger timestamp (unix seconds) at creation — used by
    /// LedgerCloseUnder for the native resolution path.
    pub created_timestamp: u64,
    /// Deadline (ledger sequence) after which no new stakes are accepted.
    pub staking_deadline_seq: u32,
    pub token: Address,
    pub pool_yes: i128,
    pub pool_no: i128,
    pub resolved: bool,
    pub cancelled: bool,
    /// Only meaningful once `resolved == true`.
    pub outcome_yes: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stake {
    pub side_yes: bool,
    pub amount: i128,
    pub claimed: bool,
}

#[contracttype]
pub enum DataKey {
    Admin,
    OracleRelayer,
    NextChallengeId,
    ProtocolFeeBps,
    Challenge(u64),
    Stake(u64, Address),
}

// ---------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------

// Event topics defined as constants above.

// ---------------------------------------------------------------------
// Helper Functions
// ---------------------------------------------------------------------

fn bump_challenge(env: &Env, id: u64) {
    env.storage()
        .persistent()
        .extend_ttl(&DataKey::Challenge(id), 100000, 100000);
}

fn bump_stake(env: &Env, id: u64, who: &Address) {
    env.storage()
        .persistent()
        .extend_ttl(&DataKey::Stake(id, who.clone()), 100000, 100000);
}

fn get_pools(challenge: &Challenge) -> (i128, i128) {
    if challenge.outcome_yes {
        (challenge.pool_yes, challenge.pool_no)
    } else {
        (challenge.pool_no, challenge.pool_yes)
    }
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Error {
    /// Contract already initialized
    AlreadyInitialized = 1,
    /// Contract not initialized yet
    NotInitialized = 2,
    /// Caller not authorized
    NotAuthorized = 3,
    /// Challenge not found
    ChallengeNotFound = 4,
    /// Staking period is closed
    StakingClosed = 5,
    /// Too early to resolve challenge
    TooEarlyToResolve = 6,
    /// Challenge already resolved
    AlreadyResolved = 7,
    /// Challenge not resolved yet
    NotResolved = 8,
    /// No stake found for user
    NoStake = 9,
    /// Stake already claimed
    AlreadyClaimed = 10,
    /// User didn't win anything
    NothingWon = 11,
    /// Invalid amount (must be >0)
    InvalidAmount = 12,
    /// Wrong resolution path for this condition type
    WrongConditionForResolutionPath = 13,
    /// Invalid ledger sequence order
    InvalidLedgerSequence = 14,
    /// Amount below minimum stake
    AmountBelowMinimum = 15,
    /// Challenge already cancelled
    AlreadyCancelled = 16,
    /// Challenge cannot be cancelled
    CannotCancel = 17,
}

// ---------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------

#[contract]
pub struct ChallengeMarket;

#[contractimpl]
impl ChallengeMarket {
    /// One-time setup. `oracle_relayer` is the address trusted to submit
    /// Horizon-derived resolution data for oracle-path challenges.
    pub fn initialize(env: Env, admin: Address, oracle_relayer: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::OracleRelayer, &oracle_relayer);
        env.storage()
            .instance()
            .set(&DataKey::NextChallengeId, &0u64);
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &0u32);
        Ok(())
    }

    /// Admin-only: rotate the trusted relayer address. Kept separate so a
    /// future quorum implementation can replace this with a multi-address
    /// registry without changing anything else.
    pub fn set_oracle_relayer(env: Env, new_relayer: Address) -> Result<(), Error> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::OracleRelayer, &new_relayer);
        Ok(())
    }

    /// Admin-only: set protocol fee in basis points (0-10000)
    pub fn set_protocol_fee(env: Env, fee_bps: u32) -> Result<(), Error> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        admin.require_auth();
        if fee_bps > 10000 {
            return Err(Error::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &fee_bps);
        Ok(())
    }

    /// Create a new challenge. Anyone can create one — this is the
    /// "I bet the next ledger closes in under 6 seconds" entry point.
    pub fn create_challenge(
        env: Env,
        creator: Address,
        description: String,
        condition: Condition,
        resolve_ledger_seq: u32,
        staking_deadline_seq: u32,
        token: Address,
    ) -> Result<u64, Error> {
        creator.require_auth();

        let current_seq = env.ledger().sequence();
        if staking_deadline_seq <= current_seq {
            return Err(Error::InvalidLedgerSequence);
        }
        if resolve_ledger_seq <= staking_deadline_seq {
            return Err(Error::InvalidLedgerSequence);
        }

        let id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextChallengeId)
            .unwrap_or(0);

        let challenge = Challenge {
            id,
            creator: creator.clone(),
            description,
            condition,
            resolve_ledger_seq,
            created_timestamp: env.ledger().timestamp(),
            staking_deadline_seq,
            token,
            pool_yes: 0,
            pool_no: 0,
            resolved: false,
            cancelled: false,
            outcome_yes: false,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Challenge(id), &challenge);
        env.storage()
            .instance()
            .set(&DataKey::NextChallengeId, &(id + 1));

        env.events().publish((EVENT_CREATED, id), creator);

        Ok(id)
    }

    /// Stake `amount` of the challenge's token on YES or NO. Transfers the
    /// token from `who` into the contract immediately (escrow model).
    pub fn stake(
        env: Env,
        who: Address,
        challenge_id: u64,
        side_yes: bool,
        amount: i128,
    ) -> Result<(), Error> {
        who.require_auth();

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let mut challenge: Challenge = env
            .storage()
            .persistent()
            .get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;

        if challenge.resolved {
            return Err(Error::AlreadyResolved);
        }
        if env.ledger().sequence() > challenge.staking_deadline_seq {
            return Err(Error::StakingClosed);
        }

        // Escrow the stake.
        let token_client = token::Client::new(&env, &challenge.token);
        token_client.transfer(&who, &env.current_contract_address(), &amount);

        // Merge with any existing stake this user has on this challenge.
        let key = DataKey::Stake(challenge_id, who.clone());
        let mut stake_rec: Stake = env.storage().persistent().get(&key).unwrap_or(Stake {
            side_yes,
            amount: 0,
            claimed: false,
        });
        // Keep it simple for v1: don't allow straddling both sides.
        stake_rec.side_yes = side_yes;
        stake_rec.amount += amount;
        env.storage().persistent().set(&key, &stake_rec);

        if side_yes {
            challenge.pool_yes += amount;
        } else {
            challenge.pool_no += amount;
        }
        env.storage()
            .persistent()
            .set(&DataKey::Challenge(challenge_id), &challenge);

        env.events()
            .publish((EVENT_STAKED, challenge_id), (who, side_yes, amount));

        Ok(())
    }

    /// Trustless resolution path for `LedgerCloseUnder` challenges. Anyone
    /// can call this once `resolve_ledger_seq` has been reached — no
    /// oracle, no admin, just the ledger's own timestamp.
    pub fn resolve_native(env: Env, challenge_id: u64) -> Result<(), Error> {
        let mut challenge: Challenge = env
            .storage()
            .persistent()
            .get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;

        if challenge.resolved {
            return Err(Error::AlreadyResolved);
        }
        if env.ledger().sequence() < challenge.resolve_ledger_seq {
            return Err(Error::TooEarlyToResolve);
        }

        let max_close_seconds = match challenge.condition {
            Condition::LedgerCloseUnder(secs) => secs,
            _ => return Err(Error::WrongConditionForResolutionPath),
        };

        let elapsed = env
            .ledger()
            .timestamp()
            .saturating_sub(challenge.created_timestamp);
        let outcome_yes = elapsed <= max_close_seconds as u64;

        challenge.resolved = true;
        challenge.outcome_yes = outcome_yes;
        env.storage()
            .persistent()
            .set(&DataKey::Challenge(challenge_id), &challenge);

        env.events()
            .publish((EVENT_RESOLVED, challenge_id), outcome_yes);

        Ok(())
    }

    /// Oracle resolution path for Horizon-derived metrics (tx counts, fee
    /// spikes) that the contract host cannot see on its own. Restricted to
    /// the configured relayer address.
    pub fn resolve_via_oracle(env: Env, challenge_id: u64, outcome_yes: bool) -> Result<(), Error> {
        let relayer: Address = env
            .storage()
            .instance()
            .get(&DataKey::OracleRelayer)
            .ok_or(Error::NotInitialized)?;
        relayer.require_auth();

        let mut challenge: Challenge = env
            .storage()
            .persistent()
            .get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;

        if challenge.resolved {
            return Err(Error::AlreadyResolved);
        }
        if env.ledger().sequence() < challenge.resolve_ledger_seq {
            return Err(Error::TooEarlyToResolve);
        }
        if let Condition::LedgerCloseUnder(_) = challenge.condition {
            return Err(Error::WrongConditionForResolutionPath);
        }

        challenge.resolved = true;
        challenge.outcome_yes = outcome_yes;
        env.storage()
            .persistent()
            .set(&DataKey::Challenge(challenge_id), &challenge);

        env.events()
            .publish((EVENT_RESOLVED, challenge_id), outcome_yes);

        Ok(())
    }

    /// Claim pro-rata winnings. Winner's payout = their stake back, plus
    /// their share of the losing pool proportional to their share of the
    /// winning pool.
    pub fn claim(env: Env, who: Address, challenge_id: u64) -> Result<i128, Error> {
        who.require_auth();

        let challenge: Challenge = env
            .storage()
            .persistent()
            .get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;

        if !challenge.resolved {
            return Err(Error::NotResolved);
        }

        let key = DataKey::Stake(challenge_id, who.clone());
        let mut stake_rec: Stake = env.storage().persistent().get(&key).ok_or(Error::NoStake)?;

        if stake_rec.claimed {
            return Err(Error::AlreadyClaimed);
        }
        if stake_rec.side_yes != challenge.outcome_yes {
            return Err(Error::NothingWon);
        }

        let (winning_pool, losing_pool) = get_pools(&challenge);

        // payout = stake + stake * losing_pool / winning_pool
        let bonus = if winning_pool > 0 {
            (stake_rec.amount * losing_pool) / winning_pool
        } else {
            0
        };
        let payout = stake_rec.amount + bonus;

        stake_rec.claimed = true;
        env.storage().persistent().set(&key, &stake_rec);

        let token_client = token::Client::new(&env, &challenge.token);
        token_client.transfer(&env.current_contract_address(), &who, &payout);

        env.events()
            .publish((EVENT_CLAIMED, challenge_id), (who, payout));

        Ok(payout)
    }

    /// Cancel an unresolved challenge (admin or creator only)
    pub fn cancel_challenge(env: Env, caller: Address, challenge_id: u64) -> Result<(), Error> {
        caller.require_auth();

        let mut challenge: Challenge = env
            .storage()
            .persistent()
            .get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;

        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;

        if caller != admin && caller != challenge.creator {
            return Err(Error::NotAuthorized);
        }
        if challenge.resolved || challenge.cancelled {
            return Err(Error::AlreadyCancelled);
        }

        challenge.cancelled = true;
        env.storage()
            .persistent()
            .set(&DataKey::Challenge(challenge_id), &challenge);

        env.events()
            .publish((EVENT_CANCELLED, challenge_id), caller);

        Ok(())
    }

    /// Refund stake for a cancelled challenge
    pub fn refund(env: Env, who: Address, challenge_id: u64) -> Result<i128, Error> {
        who.require_auth();

        let challenge: Challenge = env
            .storage()
            .persistent()
            .get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;

        if !challenge.cancelled {
            return Err(Error::CannotCancel);
        }

        let key = DataKey::Stake(challenge_id, who.clone());
        let mut stake_rec: Stake = env.storage().persistent().get(&key).ok_or(Error::NoStake)?;

        if stake_rec.claimed {
            return Err(Error::AlreadyClaimed);
        }

        stake_rec.claimed = true;
        env.storage().persistent().set(&key, &stake_rec);

        let token_client = token::Client::new(&env, &challenge.token);
        token_client.transfer(&env.current_contract_address(), &who, stake_rec.amount);

        env.events()
            .publish((EVENT_REFUNDED, challenge_id), (who, stake_rec.amount));

        Ok(stake_rec.amount)
    }

    // -------------------------------------------------------------
    // Read-only helpers for the frontend
    // -------------------------------------------------------------

    pub fn get_challenge(env: Env, challenge_id: u64) -> Result<Challenge, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)
    }

    pub fn get_stake(env: Env, challenge_id: u64, who: Address) -> Option<Stake> {
        env.storage()
            .persistent()
            .get(&DataKey::Stake(challenge_id, who))
    }

    pub fn get_next_challenge_id(env: Env) -> Result<u64, Error> {
        env.storage()
            .instance()
            .get(&DataKey::NextChallengeId)
            .ok_or(Error::NotInitialized)
    }

    pub fn get_admin(env: Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)
    }

    pub fn get_oracle_relayer(env: Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::OracleRelayer)
            .ok_or(Error::NotInitialized)
    }

    pub fn get_protocol_fee(env: Env) -> Result<u32, Error> {
        env.storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .ok_or(Error::NotInitialized)
    }
}

mod test;
