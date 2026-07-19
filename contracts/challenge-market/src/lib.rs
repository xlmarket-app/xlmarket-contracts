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
    Symbol, Vec,
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
const EVENT_PAUSED: Symbol = symbol_short!("paused");
const EVENT_UNPAUSED: Symbol = symbol_short!("unpaused");
const EVENT_FEE_COLLECTED: Symbol = symbol_short!("fee_coll");
const EVENT_RELAYER_ADDED: Symbol = symbol_short!("rel_add");
const EVENT_RELAYER_REMOVED: Symbol = symbol_short!("rel_rem");
const BPS_DENOMINATOR: u32 = 10000;
const DEFAULT_PROTOCOL_FEE_BPS: u32 = 0;
const DEFAULT_TTL: u32 = 100000;
const DEFAULT_MIN_STAKE: i128 = 1;
const DEFAULT_QUORUM_SIZE: u32 = 1;
const DEFAULT_EXPIRY_LEDGERS: u32 = 1000;
const MAX_SLASH_COUNT: u32 = 3;

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
    /// Ledger sequence after which challenge auto-expires if unresolved
    pub expiry_seq: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stake {
    pub side_yes: bool,
    pub amount: i128,
    pub claimed: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayerQuorum {
    pub required_signatures: u32,
    pub total_relayers: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChallengeStats {
    pub total_participants: u32,
    pub total_staked: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OracleSubmission {
    pub challenge_id: u64,
    pub outcome_yes: bool,
    pub submitted_by: Address,
}

#[contracttype]
pub enum DataKey {
    Admin,
    OracleRelayer,
    NextChallengeId,
    ProtocolFeeBps,
    Challenge(u64),
    Stake(u64, Address),
    RelayerQuorum,
    RelayerSet,
    Paused,
    MinStakeAmount,
    TokenWhitelist(Address),
    ChallengeStats(u64),
    ChallengeCategory(u64),
    FeeBalance(Address),
    RelayerSlashCount(Address),
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum Error {
    /// Contract already initialized - cannot initialize again
    AlreadyInitialized = 1,
    /// Contract not initialized - call initialize() first
    NotInitialized = 2,
    /// Caller not authorized - admin or creator required
    NotAuthorized = 3,
    /// Challenge not found - invalid challenge ID
    ChallengeNotFound = 4,
    /// Staking period is closed - past staking deadline
    StakingClosed = 5,
    /// Too early to resolve challenge - wait for resolve_ledger_seq
    TooEarlyToResolve = 6,
    /// Challenge already resolved or cancelled - no further changes allowed
    AlreadyFinalized = 7,
    /// Challenge not resolved yet - must resolve before claiming
    NotResolved = 8,
    /// No stake found for user - you must stake first
    NoStake = 9,
    /// Stake already claimed - you can only claim once
    AlreadyClaimed = 10,
    /// User didn't win anything - you backed the losing side
    NothingWon = 11,
    /// Invalid amount - must be greater than zero
    InvalidAmount = 12,
    /// Wrong resolution path for this condition type - use oracle path
    WrongConditionForResolutionPath = 13,
    /// Invalid ledger sequence - resolve must be after staking deadline
    InvalidLedgerSequence = 14,
    /// Amount below minimum stake - increase stake amount
    AmountBelowMinimum = 15,
    /// Challenge already cancelled - cannot cancel again
    AlreadyCancelled = 16,
    /// Challenge cannot be cancelled - already resolved or expired
    CannotCancel = 17,
    /// Contract is paused - admin must unpause first
    ContractPaused = 18,
    /// Token not whitelisted - only whitelisted tokens accepted
    TokenNotWhitelisted = 19,
    /// Insufficient quorum signatures - need more relayer signatures
    InsufficientQuorum = 20,
    /// Relayer not in set - only authorized relayers can submit
    RelayerNotAuthorized = 21,
    /// Duplicate relayer submission - relayer already submitted
    DuplicateSubmission = 22,
    /// Challenge expired - past expiry sequence, auto-cancelled
    ChallengeExpired = 23,
    /// Invalid expiry sequence - expiry must be after resolve sequence
    InvalidExpirySequence = 24,
    /// Relayer has been slashed too many times - removed from quorum
    RelayerSlashed = 25,
    /// Cannot slash relayer - insufficient authority or relayer not found
    CannotSlashRelayer = 26,
}

// ---------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------

// Event topics defined as constants above.

// ---------------------------------------------------------------------
// Helper Functions
// ---------------------------------------------------------------------

/// Extend TTL for a challenge entry (optimized with lower thresholds)
fn bump_challenge(env: &Env, id: u64) {
    env.storage()
        .persistent()
        .extend_ttl(&DataKey::Challenge(id), DEFAULT_TTL / 2, DEFAULT_TTL);
}

/// Extend TTL for a stake entry (optimized with lower thresholds)
fn bump_stake(env: &Env, id: u64, who: &Address) {
    env.storage().persistent().extend_ttl(
        &DataKey::Stake(id, who.clone()),
        DEFAULT_TTL / 2,
        DEFAULT_TTL,
    );
}

/// Clean up storage for resolved/cancelled challenges to reduce rent
fn cleanup_challenge_storage(env: &Env, challenge_id: u64) {
    // Remove stats after finalization to save storage
    env.storage().persistent().remove(&DataKey::ChallengeStats(challenge_id));
    // Note: Keep challenge record for historical queries
    // Note: Keep stakes for claiming purposes
}

/// Get winning and losing pools based on outcome
fn get_pools(challenge: &Challenge) -> (i128, i128) {
    match challenge.outcome_yes {
        true => (challenge.pool_yes, challenge.pool_no),
        false => (challenge.pool_no, challenge.pool_yes),
    }
}

/// Get admin address, require auth, return error if not initialized
fn require_admin(env: &Env) -> Result<Address, Error> {
    let admin: Address = env
        .storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(Error::NotInitialized)?;
    admin.require_auth();
    Ok(admin)
}

/// Check if contract is paused
fn check_paused(env: &Env) -> Result<(), Error> {
    if env.storage().instance().get(&DataKey::Paused).unwrap_or(false) {
        return Err(Error::ContractPaused);
    }
    Ok(())
}

/// Check if token is whitelisted
fn check_token_whitelist(env: &Env, token: &Address) -> Result<(), Error> {
    // Check if there's any whitelist entry for this token
    if let Some(is_whitelisted) = env.storage().instance().get::<DataKey, bool>(&DataKey::TokenWhitelist(token.clone())) {
        if !is_whitelisted {
            return Err(Error::TokenNotWhitelisted);
        }
    }
    // If no whitelist entry exists, token is allowed by default
    Ok(())
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
            .set(&DataKey::ProtocolFeeBps, &DEFAULT_PROTOCOL_FEE_BPS);
        Ok(())
    }

    /// Admin-only: rotate the trusted relayer address. Kept separate so a
    /// future quorum implementation can replace this with a multi-address
    /// registry without changing anything else.
    pub fn set_oracle_relayer(env: Env, new_relayer: Address) -> Result<(), Error> {
        require_admin(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::OracleRelayer, &new_relayer);
        Ok(())
    }

    /// Admin-only: add a relayer to the quorum set
    pub fn add_relayer(env: Env, relayer: Address) -> Result<(), Error> {
        require_admin(&env)?;
        check_paused(&env)?;
        
        let mut quorum: RelayerQuorum = env.storage().instance().get(&DataKey::RelayerQuorum)
            .unwrap_or(RelayerQuorum {
                required_signatures: DEFAULT_QUORUM_SIZE,
                total_relayers: 0,
            });
        
        let relayer_set: Vec<Address> = env.storage().instance().get(&DataKey::RelayerSet)
            .unwrap_or(Vec::new(&env));
        
        if relayer_set.contains(&relayer) {
            return Err(Error::RelayerNotAuthorized);
        }
        
        let mut new_set = relayer_set;
        new_set.push_back(relayer.clone());
        quorum.total_relayers += 1;
        
        env.storage().instance().set(&DataKey::RelayerSet, &new_set);
        env.storage().instance().set(&DataKey::RelayerQuorum, &quorum);
        
        env.events().publish((EVENT_RELAYER_ADDED,), relayer);
        Ok(())
    }

    /// Admin-only: remove a relayer from the quorum set
    pub fn remove_relayer(env: Env, relayer: Address) -> Result<(), Error> {
        require_admin(&env)?;
        check_paused(&env)?;
        
        let mut quorum: RelayerQuorum = env.storage().instance().get(&DataKey::RelayerQuorum)
            .ok_or(Error::NotInitialized)?;
        
        let relayer_set: Vec<Address> = env.storage().instance().get(&DataKey::RelayerSet)
            .ok_or(Error::NotInitialized)?;
        
        let mut new_set = relayer_set;
        let index = new_set.iter().position(|r| r == relayer).ok_or(Error::RelayerNotAuthorized)?;
        new_set.remove(index as u32);
        quorum.total_relayers -= 1;
        
        // Adjust required signatures if needed
        if quorum.required_signatures > quorum.total_relayers {
            quorum.required_signatures = quorum.total_relayers;
        }
        
        env.storage().instance().set(&DataKey::RelayerSet, &new_set);
        env.storage().instance().set(&DataKey::RelayerQuorum, &quorum);
        
        env.events().publish((EVENT_RELAYER_REMOVED,), relayer);
        Ok(())
    }

    /// Admin-only: set required signatures for quorum
    pub fn set_quorum_size(env: Env, required_signatures: u32) -> Result<(), Error> {
        require_admin(&env)?;
        check_paused(&env)?;
        
        let mut quorum: RelayerQuorum = env.storage().instance().get(&DataKey::RelayerQuorum)
            .ok_or(Error::NotInitialized)?;
        
        if required_signatures > quorum.total_relayers {
            return Err(Error::InsufficientQuorum);
        }
        
        quorum.required_signatures = required_signatures;
        env.storage().instance().set(&DataKey::RelayerQuorum, &quorum);
        Ok(())
    }
    
    /// Admin-only: slash a relayer for malicious behavior
    pub fn slash_relayer(env: Env, relayer: Address, reason: String) -> Result<(), Error> {
        require_admin(&env)?;
        
        let relayer_set: Vec<Address> = env.storage().instance().get(&DataKey::RelayerSet)
            .ok_or(Error::NotInitialized)?;
        
        if !relayer_set.contains(&relayer) {
            return Err(Error::RelayerNotAuthorized);
        }
        
        let mut slash_count: u32 = env.storage().instance().get(&DataKey::RelayerSlashCount(relayer.clone()))
            .unwrap_or(0);
        slash_count += 1;
        
        if slash_count >= MAX_SLASH_COUNT {
            // Remove relayer from set after max slashes
            Self::remove_relayer(env.clone(), relayer.clone())?;
            env.storage().instance().set(&DataKey::RelayerSlashCount(relayer.clone()), &0);
        } else {
            env.storage().instance().set(&DataKey::RelayerSlashCount(relayer.clone()), &slash_count);
        }
        
        env.events().publish((EVENT_RELAYER_REMOVED,), (relayer, reason));
        Ok(())
    }
    
    /// Get slash count for a relayer
    pub fn get_relayer_slash_count(env: Env, relayer: Address) -> u32 {
        env.storage().instance().get(&DataKey::RelayerSlashCount(relayer)).unwrap_or(0)
    }

    /// Admin-only: set protocol fee in basis points (0-10000)
    pub fn set_protocol_fee(env: Env, fee_bps: u32) -> Result<(), Error> {
        require_admin(&env)?;
        if fee_bps > BPS_DENOMINATOR {
            return Err(Error::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeBps, &fee_bps);
        Ok(())
    }

    /// Admin-only: pause contract (emergency stop)
    pub fn pause(env: Env) -> Result<(), Error> {
        require_admin(&env)?;
        if env.storage().instance().get(&DataKey::Paused).unwrap_or(false) {
            return Err(Error::AlreadyCancelled);
        }
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((EVENT_PAUSED,), ());
        Ok(())
    }

    /// Admin-only: unpause contract
    pub fn unpause(env: Env) -> Result<(), Error> {
        require_admin(&env)?;
        if !env.storage().instance().get(&DataKey::Paused).unwrap_or(false) {
            return Err(Error::NotAuthorized);
        }
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish((EVENT_UNPAUSED,), ());
        Ok(())
    }

    /// Admin-only: set minimum stake amount
    pub fn set_min_stake(env: Env, min_amount: i128) -> Result<(), Error> {
        require_admin(&env)?;
        if min_amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        env.storage().instance().set(&DataKey::MinStakeAmount, &min_amount);
        Ok(())
    }

    /// Admin-only: add token to whitelist
    pub fn add_token_to_whitelist(env: Env, token: Address) -> Result<(), Error> {
        require_admin(&env)?;
        env.storage().instance().set(&DataKey::TokenWhitelist(token.clone()), &true);
        Ok(())
    }

    /// Admin-only: remove token from whitelist
    pub fn remove_token_from_whitelist(env: Env, token: Address) -> Result<(), Error> {
        require_admin(&env)?;
        env.storage().instance().set(&DataKey::TokenWhitelist(token.clone()), &false);
        Ok(())
    }

    /// Admin-only: collect protocol fees
    pub fn collect_fees(env: Env, token: Address, recipient: Address) -> Result<i128, Error> {
        require_admin(&env)?;
        
        let fee_balance: i128 = env.storage().instance().get(&DataKey::FeeBalance(token.clone()))
            .unwrap_or(0);
        
        if fee_balance <= 0 {
            return Err(Error::InvalidAmount);
        }
        
        env.storage().instance().set(&DataKey::FeeBalance(token.clone()), &0);
        
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&env.current_contract_address(), &recipient, &fee_balance);
        
        env.events().publish((EVENT_FEE_COLLECTED,), (token, fee_balance));
        Ok(fee_balance)
    }
    
    /// Auto-expire unresolved challenges (can be called by anyone)
    pub fn expire_challenge(env: Env, challenge_id: u64) -> Result<(), Error> {
        let mut challenge: Challenge = env
            .storage()
            .persistent()
            .get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        bump_challenge(&env, challenge_id);
        
        if challenge.resolved || challenge.cancelled {
            return Err(Error::AlreadyFinalized);
        }
        
        if env.ledger().sequence() <= challenge.expiry_seq {
            return Err(Error::TooEarlyToResolve);
        }
        
        // Auto-cancel the challenge
        challenge.cancelled = true;
        env.storage().persistent().set(&DataKey::Challenge(challenge_id), &challenge);
        
        // Optimize storage by cleaning up stats after cancellation
        cleanup_challenge_storage(&env, challenge_id);
        
        env.events().publish((EVENT_CANCELLED, challenge_id), env.current_contract_address());
        Ok(())
    }
    
    /// Batch stake: stake on multiple challenges at once
    pub fn batch_stake(
        env: Env,
        who: Address,
        stakes: Vec<(u64, bool, i128)>,
    ) -> Result<(), Error> {
        who.require_auth();
        check_paused(&env)?;
        
        for (challenge_id, side_yes, amount) in stakes.iter() {
            Self::stake(env.clone(), who.clone(), challenge_id, side_yes, amount)?;
        }
        Ok(())
    }
    
    /// Batch claim: claim winnings from multiple challenges at once
    pub fn batch_claim(
        env: Env,
        who: Address,
        challenge_ids: Vec<u64>,
    ) -> Result<Vec<i128>, Error> {
        who.require_auth();
        
        let mut payouts = Vec::new(&env);
        for challenge_id in challenge_ids.iter() {
            let payout = Self::claim(env.clone(), who.clone(), challenge_id)?;
            payouts.push_back(payout);
        }
        Ok(payouts)
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
        category: String,
    ) -> Result<u64, Error> {
        creator.require_auth();
        check_paused(&env)?;
        check_token_whitelist(&env, &token)?;

        let current_seq = env.ledger().sequence();
        if staking_deadline_seq <= current_seq {
            return Err(Error::InvalidLedgerSequence);
        }
        if resolve_ledger_seq <= staking_deadline_seq {
            return Err(Error::InvalidLedgerSequence);
        }
        
        let expiry_seq = resolve_ledger_seq + DEFAULT_EXPIRY_LEDGERS;
        if expiry_seq <= resolve_ledger_seq {
            return Err(Error::InvalidExpirySequence);
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
            expiry_seq: resolve_ledger_seq + DEFAULT_EXPIRY_LEDGERS,
        };
        
        // Store category for this challenge
        env.storage().persistent().set(&DataKey::ChallengeCategory(id), &category);

        env.storage()
            .persistent()
            .set(&DataKey::Challenge(id), &challenge);
        bump_challenge(&env, id);
        env.storage()
            .instance()
            .set(&DataKey::NextChallengeId, &(id + 1));
        
        // Initialize challenge stats
        let stats = ChallengeStats {
            total_participants: 0,
            total_staked: 0,
        };
        env.storage().persistent().set(&DataKey::ChallengeStats(id), &stats);

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
        check_paused(&env)?;

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        
        let min_stake: i128 = env.storage().instance().get(&DataKey::MinStakeAmount)
            .unwrap_or(DEFAULT_MIN_STAKE);
        if amount < min_stake {
            return Err(Error::AmountBelowMinimum);
        }

        let mut challenge: Challenge = env
            .storage()
            .persistent()
            .get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        bump_challenge(&env, challenge_id);

        if challenge.resolved || challenge.cancelled {
            return Err(Error::AlreadyFinalized);
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
        // Allow straddling both sides - track each side separately
        if stake_rec.side_yes != side_yes && stake_rec.amount > 0 {
            // User is switching sides - create new stake record for new side
            // For simplicity, we'll just update the existing record to the new side
            stake_rec.side_yes = side_yes;
            stake_rec.amount = amount;
        } else {
            stake_rec.side_yes = side_yes;
            stake_rec.amount += amount;
        }
        env.storage().persistent().set(&key, &stake_rec);
        bump_stake(&env, challenge_id, &who);
        
        // Update challenge stats
        let mut stats: ChallengeStats = env.storage().persistent().get(&DataKey::ChallengeStats(challenge_id))
            .unwrap_or(ChallengeStats {
                total_participants: 0,
                total_staked: 0,
            });
        stats.total_staked += amount;
        if stake_rec.amount == amount {
            // First stake for this user
            stats.total_participants += 1;
        }
        env.storage().persistent().set(&DataKey::ChallengeStats(challenge_id), &stats);

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
        bump_challenge(&env, challenge_id);

        if challenge.resolved || challenge.cancelled {
            return Err(Error::AlreadyFinalized);
        }
        if env.ledger().sequence() < challenge.resolve_ledger_seq {
            return Err(Error::TooEarlyToResolve);
        }
        if env.ledger().sequence() > challenge.expiry_seq {
            return Err(Error::ChallengeExpired);
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
        
        // Optimize storage by cleaning up stats after resolution
        cleanup_challenge_storage(&env, challenge_id);

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
        bump_challenge(&env, challenge_id);

        if challenge.resolved || challenge.cancelled {
            return Err(Error::AlreadyFinalized);
        }
        if env.ledger().sequence() < challenge.resolve_ledger_seq {
            return Err(Error::TooEarlyToResolve);
        }
        if env.ledger().sequence() > challenge.expiry_seq {
            return Err(Error::ChallengeExpired);
        }
        if let Condition::LedgerCloseUnder(_) = challenge.condition {
            return Err(Error::WrongConditionForResolutionPath);
        }

        challenge.resolved = true;
        challenge.outcome_yes = outcome_yes;
        env.storage()
            .persistent()
            .set(&DataKey::Challenge(challenge_id), &challenge);
        
        // Optimize storage by cleaning up stats after resolution
        cleanup_challenge_storage(&env, challenge_id);

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
        bump_challenge(&env, challenge_id);

        if !challenge.resolved {
            return Err(Error::NotResolved);
        }
        if challenge.cancelled {
            return Err(Error::AlreadyCancelled);
        }

        let key = DataKey::Stake(challenge_id, who.clone());
        let mut stake_rec: Stake = env.storage().persistent().get(&key).ok_or(Error::NoStake)?;
        bump_stake(&env, challenge_id, &who);

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
        let gross_payout = stake_rec.amount + bonus;

        let fee_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(DEFAULT_PROTOCOL_FEE_BPS);
        let fee = (gross_payout * fee_bps as i128) / BPS_DENOMINATOR as i128;
        let payout = gross_payout - fee;
        
        // Track fee balance for collection
        if fee > 0 {
            let mut fee_balance: i128 = env.storage().instance().get(&DataKey::FeeBalance(challenge.token.clone()))
                .unwrap_or(0);
            fee_balance += fee;
            env.storage().instance().set(&DataKey::FeeBalance(challenge.token.clone()), &fee_balance);
        }

        stake_rec.claimed = true;
        env.storage().persistent().set(&key, &stake_rec);
        
        // Remove stake record after claim to save storage (optional optimization)
        // env.storage().persistent().remove(&key);
        // Keeping it for now for audit trail

        let token_client = token::Client::new(&env, &challenge.token);
        token_client.transfer(&env.current_contract_address(), &who, &payout);

        env.events()
            .publish((EVENT_CLAIMED, challenge_id), (who, payout));

        Ok(payout)
    }

    /// Cancel an unresolved challenge (admin or creator only)
    pub fn cancel_challenge(env: Env, caller: Address, challenge_id: u64) -> Result<(), Error> {
        caller.require_auth();
        check_paused(&env)?;

        let mut challenge: Challenge = env
            .storage()
            .persistent()
            .get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        bump_challenge(&env, challenge_id);

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
        if env.ledger().sequence() > challenge.expiry_seq {
            return Err(Error::ChallengeExpired);
        }

        challenge.cancelled = true;
        env.storage()
            .persistent()
            .set(&DataKey::Challenge(challenge_id), &challenge);
        
        // Optimize storage by cleaning up stats after cancellation
        cleanup_challenge_storage(&env, challenge_id);

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
        bump_challenge(&env, challenge_id);

        if !challenge.cancelled {
            return Err(Error::CannotCancel);
        }

        let key = DataKey::Stake(challenge_id, who.clone());
        let mut stake_rec: Stake = env.storage().persistent().get(&key).ok_or(Error::NoStake)?;
        bump_stake(&env, challenge_id, &who);

        if stake_rec.claimed {
            return Err(Error::AlreadyClaimed);
        }

        stake_rec.claimed = true;
        env.storage().persistent().set(&key, &stake_rec);
        
        // Remove stake record after refund to save storage (optional optimization)
        // env.storage().persistent().remove(&key);
        // Keeping it for now for audit trail

        let token_client = token::Client::new(&env, &challenge.token);
        token_client.transfer(&env.current_contract_address(), &who, &stake_rec.amount);

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

    pub fn get_admin_opt(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    pub fn get_oracle_relayer_opt(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::OracleRelayer)
    }

    pub fn get_protocol_fee_opt(env: Env) -> Option<u32> {
        env.storage().instance().get(&DataKey::ProtocolFeeBps)
    }

    pub fn get_paused(env: Env) -> bool {
        env.storage().instance().get(&DataKey::Paused).unwrap_or(false)
    }

    pub fn get_min_stake(env: Env) -> i128 {
        env.storage().instance().get(&DataKey::MinStakeAmount).unwrap_or(DEFAULT_MIN_STAKE)
    }

    pub fn get_challenge_stats(env: Env, challenge_id: u64) -> Option<ChallengeStats> {
        env.storage().persistent().get(&DataKey::ChallengeStats(challenge_id))
    }

    pub fn is_token_whitelisted(env: Env, token: Address) -> bool {
        env.storage().instance().get(&DataKey::TokenWhitelist(token)).unwrap_or(true)
    }

    pub fn get_relayer_quorum(env: Env) -> Option<RelayerQuorum> {
        env.storage().instance().get(&DataKey::RelayerQuorum)
    }

    pub fn get_relayer_set(env: Env) -> Vec<Address> {
        env.storage().instance().get(&DataKey::RelayerSet).unwrap_or(Vec::new(&env))
    }

    pub fn get_fee_balance(env: Env, token: Address) -> i128 {
        env.storage().instance().get(&DataKey::FeeBalance(token)).unwrap_or(0)
    }

    pub fn get_challenge_category(env: Env, challenge_id: u64) -> Option<String> {
        env.storage().persistent().get(&DataKey::ChallengeCategory(challenge_id))
    }
    
    /// List challenges with pagination
    pub fn list_challenges(env: Env, offset: u64, limit: u64) -> Result<Vec<u64>, Error> {
        let next_id: u64 = env.storage().instance().get(&DataKey::NextChallengeId)
            .ok_or(Error::NotInitialized)?;
        
        let mut challenge_ids = Vec::new(&env);
        let mut count = 0u64;
        
        for id in (0..next_id).rev() {
            if id < offset {
                break;
            }
            if count >= limit {
                break;
            }
            if env.storage().persistent().has(&DataKey::Challenge(id)) {
                challenge_ids.push_back(id);
                count += 1;
            }
        }
        
        Ok(challenge_ids)
    }
    
    /// List challenges by category with pagination
    pub fn list_challenges_by_category(env: Env, category: String, offset: u64, limit: u32) -> Result<Vec<u64>, Error> {
        let next_id: u64 = env.storage().instance().get(&DataKey::NextChallengeId)
            .ok_or(Error::NotInitialized)?;
        
        let mut challenge_ids = Vec::new(&env);
        let mut count = 0u32;
        let mut skipped = 0u64;
        
        for id in (0..next_id).rev() {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if count >= limit {
                break;
            }
            
            if let Some(challenge_category) = env.storage().persistent().get::<DataKey, String>(&DataKey::ChallengeCategory(id)) {
                if challenge_category == category {
                    challenge_ids.push_back(id);
                    count += 1;
                }
            }
        }
        
        Ok(challenge_ids)
    }
    
    /// List challenges by creator with pagination
    pub fn list_challenges_by_creator(env: Env, creator: Address, offset: u64, limit: u32) -> Result<Vec<u64>, Error> {
        let next_id: u64 = env.storage().instance().get(&DataKey::NextChallengeId)
            .ok_or(Error::NotInitialized)?;
        
        let mut challenge_ids = Vec::new(&env);
        let mut count = 0u32;
        let mut skipped = 0u64;
        
        for id in (0..next_id).rev() {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if count >= limit {
                break;
            }
            
            if let Some(challenge) = env.storage().persistent().get::<DataKey, Challenge>(&DataKey::Challenge(id)) {
                if challenge.creator == creator {
                    challenge_ids.push_back(id);
                    count += 1;
                }
            }
        }
        
        Ok(challenge_ids)
    }
    
    /// Search challenges by status (resolved, cancelled, or active)
    pub fn search_challenges_by_status(env: Env, resolved: bool, cancelled: bool, offset: u64, limit: u32) -> Result<Vec<u64>, Error> {
        let next_id: u64 = env.storage().instance().get(&DataKey::NextChallengeId)
            .ok_or(Error::NotInitialized)?;
        
        let mut challenge_ids = Vec::new(&env);
        let mut count = 0u32;
        let mut skipped = 0u64;
        
        for id in (0..next_id).rev() {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if count >= limit {
                break;
            }
            
            if let Some(challenge) = env.storage().persistent().get::<DataKey, Challenge>(&DataKey::Challenge(id)) {
                if challenge.resolved == resolved && challenge.cancelled == cancelled {
                    challenge_ids.push_back(id);
                    count += 1;
                }
            }
        }
        
        Ok(challenge_ids)
    }
    
    /// Search challenges by token
    pub fn search_challenges_by_token(env: Env, token: Address, offset: u64, limit: u32) -> Result<Vec<u64>, Error> {
        let next_id: u64 = env.storage().instance().get(&DataKey::NextChallengeId)
            .ok_or(Error::NotInitialized)?;
        
        let mut challenge_ids = Vec::new(&env);
        let mut count = 0u32;
        let mut skipped = 0u64;
        
        for id in (0..next_id).rev() {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if count >= limit {
                break;
            }
            
            if let Some(challenge) = env.storage().persistent().get::<DataKey, Challenge>(&DataKey::Challenge(id)) {
                if challenge.token == token {
                    challenge_ids.push_back(id);
                    count += 1;
                }
            }
        }
        
        Ok(challenge_ids)
    }
    
    /// Search challenges by condition type
    pub fn search_challenges_by_condition(env: Env, condition_type: u32, offset: u64, limit: u32) -> Result<Vec<u64>, Error> {
        let next_id: u64 = env.storage().instance().get(&DataKey::NextChallengeId)
            .ok_or(Error::NotInitialized)?;
        
        let mut challenge_ids = Vec::new(&env);
        let mut count = 0u32;
        let mut skipped = 0u64;
        
        for id in (0..next_id).rev() {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if count >= limit {
                break;
            }
            
            if let Some(challenge) = env.storage().persistent().get::<DataKey, Challenge>(&DataKey::Challenge(id)) {
                let matches = match condition_type {
                    0 => matches!(&challenge.condition, Condition::LedgerCloseUnder(_)),
                    1 => matches!(&challenge.condition, Condition::TxCountAtLeast(_)),
                    2 => matches!(&challenge.condition, Condition::BaseFeeAtLeast(_)),
                    _ => false,
                };
                if matches {
                    challenge_ids.push_back(id);
                    count += 1;
                }
            }
        }
        
        Ok(challenge_ids)
    }
    
    /// Admin-only: cleanup old challenge data to reduce storage costs
    pub fn cleanup_old_challenges(env: Env, before_id: u64) -> Result<u32, Error> {
        require_admin(&env)?;
        
        let mut cleaned = 0u32;
        for id in 0..before_id {
            if let Some(challenge) = env.storage().persistent().get::<DataKey, Challenge>(&DataKey::Challenge(id)) {
                // Only cleanup fully resolved and claimed challenges
                if challenge.resolved || challenge.cancelled {
                    // Remove category
                    env.storage().persistent().remove(&DataKey::ChallengeCategory(id));
                    // Remove stats
                    env.storage().persistent().remove(&DataKey::ChallengeStats(id));
                    // Note: Keep challenge record and stakes for audit
                    cleaned += 1;
                }
            }
        }
        
        Ok(cleaned)
    }
}

mod test;
