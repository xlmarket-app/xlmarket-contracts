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
const EVENT_VESTING_CONFIGURED: Symbol = symbol_short!("vest_cfg");
const EVENT_VESTING_CLAIMED: Symbol = symbol_short!("vest_clm");
const EVENT_ODDS_UPDATED: Symbol = symbol_short!("odds_upd");
const EVENT_DISPUTE_CREATED: Symbol = symbol_short!("disp_cre");
const EVENT_DISPUTE_RESOLVED: Symbol = symbol_short!("disp_res");
const EVENT_LIQUIDITY_ADDED: Symbol = symbol_short!("liq_add");
const EVENT_LIQUIDITY_REMOVED: Symbol = symbol_short!("liq_rem");
const EVENT_ORACLE_SUBMITTED: Symbol = symbol_short!("ora_sub");
const EVENT_ORACLE_AGGREGATED: Symbol = symbol_short!("ora_agg");
const EVENT_FLASH_LOAN_DETECTED: Symbol = symbol_short!("flash_det");
const EVENT_AUDIT_LOG: Symbol = symbol_short!("audit_log");
const BPS_DENOMINATOR: u32 = 10000;
const DEFAULT_PROTOCOL_FEE_BPS: u32 = 0;
const DEFAULT_TTL: u32 = 100000;
const DEFAULT_MIN_STAKE: i128 = 1;
const DEFAULT_QUORUM_SIZE: u32 = 1;
const DEFAULT_EXPIRY_LEDGERS: u32 = 1000;
const MAX_SLASH_COUNT: u32 = 3;
const DEFAULT_VESTING_DURATION: u64 = 0; // 0 means no vesting by default
const DISPUTE_WINDOW_LEDGERS: u32 = 100; // Time window to file a dispute after resolution
const DISPUTE_STAKE_THRESHOLD: i128 = 100; // Minimum stake to file a dispute
const RATE_LIMIT_WINDOW: u64 = 3600; // 1 hour in seconds
const MAX_CHALLENGES_PER_WINDOW: u32 = 5;
const MAX_STAKES_PER_WINDOW: u32 = 20;
const MAX_DISPUTES_PER_WINDOW: u32 = 3;
const LIQUIDITY_REWARD_BPS: u32 = 100; // 1% reward for liquidity providers
const ORACLE_AGGREGATION_WINDOW: u32 = 50; // Ledger window for oracle submissions
const ORACLE_QUORUM_THRESHOLD: u32 = 2; // Minimum oracles required for aggregation
const FLASH_LOAN_COOLDOWN: u64 = 10; // Minimum seconds between large withdrawals
const FLASH_LOAN_THRESHOLD: i128 = 10000; // Amount threshold for flash loan detection
const MAX_AUDIT_LOG_ENTRIES: u32 = 1000; // Maximum audit log entries per challenge

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
    /// Multi-token support: list of additional allowed tokens
    pub additional_tokens: Vec<Address>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stake {
    pub side_yes: bool,
    pub amount: i128,
    pub claimed: bool,
    pub vesting_start: u64,
    pub vesting_duration: u64,
    pub is_vested: bool,
    pub token: Address,
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregatedOracleResult {
    pub challenge_id: u64,
    pub outcome_yes: bool,
    pub yes_votes: u32,
    pub no_votes: u32,
    pub total_oracles: u32,
    pub aggregated_at: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditLogEntry {
    pub entry_id: u64,
    pub challenge_id: u64,
    pub action_type: String,
    pub actor: Address,
    pub timestamp: u64,
    pub details: String,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dispute {
    pub challenge_id: u64,
    pub disputer: Address,
    pub disputed_outcome: bool,
    pub evidence: String,
    pub stake: i128,
    pub resolved: bool,
    pub successful: bool,
    pub created_at: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiquidityPosition {
    pub provider: Address,
    pub amount: i128,
    pub shares: i128,
    pub reward_debt: i128,
    pub last_update: u64,
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
    UserVestingConfig(Address),
    Dispute(u64, Address), // (challenge_id, disputer)
    DisputeCount(u64), // Count of disputes per challenge
    RateLimit(Address, u64), // (user, window_timestamp)
    ChallengeCreationCount(Address, u64), // (user, window_timestamp)
    StakeCount(Address, u64), // (user, window_timestamp)
    DisputeFilingCount(Address, u64), // (user, window_timestamp)
    MultiTokenPool(u64, Address), // (challenge_id, token) - pool for additional tokens
    LiquidityPool(u64, Address), // (challenge_id, provider) - liquidity provider position
    TotalLiquidity(u64), // Total liquidity per challenge
    OracleSubmission(u64, Address), // (challenge_id, oracle) - oracle submission
    OracleAggregation(u64), // Aggregated oracle result per challenge
    UserLastWithdrawal(Address, u64), // (user, challenge_id) - last withdrawal timestamp
    FlashLoanFlag(Address), // Flag for potential flash loan activity
    AuditLog(u64, u64), // (challenge_id, entry_id) - audit log entry
    AuditLogCount(u64), // Count of audit log entries per challenge
    AuditLogIndex(u64), // Next audit log entry ID for challenge
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
    /// Stake still vesting - cannot claim yet
    StakeStillVesting = 27,
    /// Invalid vesting duration - must be reasonable
    InvalidVestingDuration = 28,
    /// Dispute window closed - too late to file dispute
    DisputeWindowClosed = 29,
    /// Insufficient stake for dispute - must meet threshold
    InsufficientDisputeStake = 30,
    /// Dispute already exists - cannot file duplicate
    DisputeAlreadyExists = 31,
    /// Challenge not disputed - no dispute found
    NotDisputed = 32,
    /// Dispute already resolved - cannot resolve again
    DisputeAlreadyResolved = 33,
    /// Rate limit exceeded - too many actions in time window
    RateLimitExceeded = 34,
    /// Token not supported for this challenge
    TokenNotSupported = 35,
    /// Insufficient liquidity for operation
    InsufficientLiquidity = 36,
    /// Invalid liquidity amount
    InvalidLiquidityAmount = 37,
    /// Oracle not authorized for aggregation
    OracleNotAuthorized = 38,
    /// Insufficient oracle submissions for aggregation
    InsufficientOracleSubmissions = 39,
    /// Oracle already submitted for this challenge
    OracleAlreadySubmitted = 40,
    /// Flash loan detected - operation blocked
    FlashLoanDetected = 41,
    /// Cooldown period not elapsed
    CooldownNotElapsed = 42,
    /// Audit log full - cannot add more entries
    AuditLogFull = 43,
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

/// Check and update rate limit for a user action
fn check_rate_limit(env: &Env, count_key: DataKey, max_count: u32) -> Result<(), Error> {
    let current_time = env.ledger().timestamp();
    let window_start = current_time - RATE_LIMIT_WINDOW;
    
    // Get or create rate limit entry
    let (last_window, count) = env.storage().instance().get(&count_key)
        .unwrap_or((window_start, 0u32));
    
    // Check if we're in a new window
    if last_window < window_start {
        // New window, reset count
        env.storage().instance().set(&count_key, &(current_time, 1u32));
        Ok(())
    } else {
        // Same window, check count
        if count >= max_count {
            return Err(Error::RateLimitExceeded);
        }
        // Increment count
        env.storage().instance().set(&count_key, &(last_window, count + 1));
        Ok(())
    }
}

/// Check for flash loan activity
fn check_flash_loan_protection(env: &Env, user: &Address, challenge_id: u64, amount: i128) -> Result<(), Error> {
    // Only check for large amounts
    if amount < FLASH_LOAN_THRESHOLD {
        return Ok(());
    }
    
    let current_time = env.ledger().timestamp();
    let withdrawal_key = DataKey::UserLastWithdrawal(user.clone(), challenge_id);
    
    // Get last withdrawal time
    if let Some(last_withdrawal) = env.storage().instance().get::<DataKey, u64>(&withdrawal_key) {
        let time_since_last = current_time - last_withdrawal;
        
        // Check cooldown period
        if time_since_last < FLASH_LOAN_COOLDOWN {
            // Flag potential flash loan activity
            env.storage().instance().set(&DataKey::FlashLoanFlag(user.clone()), &true);
            env.events().publish((EVENT_FLASH_LOAN_DETECTED,), (user, challenge_id, amount));
            return Err(Error::CooldownNotElapsed);
        }
    }
    
    // Update last withdrawal time
    env.storage().instance().set(&withdrawal_key, &current_time);
    
    Ok(())
}

/// Log an audit entry for transparency
fn log_audit_entry(env: &Env, challenge_id: u64, action_type: String, actor: Address, details: String) {
    let current_count: u32 = env.storage().persistent().get(&DataKey::AuditLogCount(challenge_id))
        .unwrap_or(0);
    
    if current_count >= MAX_AUDIT_LOG_ENTRIES {
        return; // Audit log full, skip entry
    }
    
    let entry_id: u64 = env.storage().persistent().get(&DataKey::AuditLogIndex(challenge_id))
        .unwrap_or(0);
    
    let entry = AuditLogEntry {
        entry_id,
        challenge_id,
        action_type: action_type.clone(),
        actor,
        timestamp: env.ledger().timestamp(),
        details,
    };
    
    env.storage().persistent().set(&DataKey::AuditLog(challenge_id, entry_id), &entry);
    env.storage().persistent().set(&DataKey::AuditLogIndex(challenge_id), &(entry_id + 1));
    env.storage().persistent().set(&DataKey::AuditLogCount(challenge_id), &(current_count + 1));
    
    env.events().publish((EVENT_AUDIT_LOG, challenge_id), (entry_id, action_type));
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

    /// Admin-only: configure vesting duration for a user (in seconds)
    pub fn set_user_vesting(env: Env, user: Address, vesting_duration: u64) -> Result<(), Error> {
        require_admin(&env)?;
        // Max vesting duration of 1 year (31536000 seconds) to prevent abuse
        if vesting_duration > 31536000 {
            return Err(Error::InvalidVestingDuration);
        }
        env.storage().instance().set(&DataKey::UserVestingConfig(user.clone()), &vesting_duration);
        env.events().publish((EVENT_VESTING_CONFIGURED,), (user, vesting_duration));
        Ok(())
    }

    /// User can opt-in to vesting for their stakes
    pub fn set_self_vesting(env: Env, user: Address, vesting_duration: u64) -> Result<(), Error> {
        user.require_auth();
        // Max vesting duration of 1 year
        if vesting_duration > 31536000 {
            return Err(Error::InvalidVestingDuration);
        }
        env.storage().instance().set(&DataKey::UserVestingConfig(user.clone()), &vesting_duration);
        env.events().publish((EVENT_VESTING_CONFIGURED,), (user, vesting_duration));
        Ok(())
    }
    
    /// Admin-only: add additional token to a challenge for multi-token support
    pub fn add_challenge_token(env: Env, challenge_id: u64, token: Address) -> Result<(), Error> {
        require_admin(&env)?;
        
        let mut challenge: Challenge = env.storage().persistent().get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        
        if challenge.resolved || challenge.cancelled {
            return Err(Error::AlreadyFinalized);
        }
        
        // Check if token already added
        for t in challenge.additional_tokens.iter() {
            if t == token {
                return Err(Error::TokenNotSupported); // Already exists
            }
        }
        
        // Don't allow duplicate of primary token
        if challenge.token == token {
            return Err(Error::TokenNotSupported);
        }
        
        challenge.additional_tokens.push_back(token.clone());
        env.storage().persistent().set(&DataKey::Challenge(challenge_id), &challenge);
        
        // Initialize pool for this token
        env.storage().persistent().set(&DataKey::MultiTokenPool(challenge_id, token), &(0i128, 0i128)); // (pool_yes, pool_no)
        
        Ok(())
    }
    
    /// Get multi-token pool balance for a specific token
    pub fn get_multi_token_pool(env: Env, challenge_id: u64, token: Address) -> Result<(i128, i128), Error> {
        let (pool_yes, pool_no) = env.storage().persistent().get(&DataKey::MultiTokenPool(challenge_id, token))
            .unwrap_or((0i128, 0i128));
        Ok((pool_yes, pool_no))
    }
    
    /// Add liquidity to a challenge pool
    pub fn add_liquidity(env: Env, provider: Address, challenge_id: u64, amount: i128) -> Result<i128, Error> {
        provider.require_auth();
        check_paused(&env)?;
        
        if amount <= 0 {
            return Err(Error::InvalidLiquidityAmount);
        }
        
        let challenge: Challenge = env.storage().persistent().get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        
        if challenge.resolved || challenge.cancelled {
            return Err(Error::AlreadyFinalized);
        }
        
        // Get current total liquidity
        let total_liquidity: i128 = env.storage().persistent().get(&DataKey::TotalLiquidity(challenge_id))
            .unwrap_or(0);
        
        // Calculate shares to mint
        let shares = if total_liquidity == 0 {
            amount // First provider gets 1:1 shares
        } else {
            (amount * 1000000) / total_liquidity // Simple share calculation
        };
        
        // Get or create liquidity position
        let key = DataKey::LiquidityPool(challenge_id, provider.clone());
        let mut position: LiquidityPosition = env.storage().persistent().get(&key).unwrap_or(LiquidityPosition {
            provider: provider.clone(),
            amount: 0,
            shares: 0,
            reward_debt: 0,
            last_update: env.ledger().timestamp(),
        });
        
        // Update position
        position.amount += amount;
        position.shares += shares;
        position.last_update = env.ledger().timestamp();
        
        env.storage().persistent().set(&key, &position);
        
        // Update total liquidity
        env.storage().persistent().set(&DataKey::TotalLiquidity(challenge_id), &(total_liquidity + amount));
        
        // Escrow liquidity tokens
        let token_client = token::Client::new(&env, &challenge.token);
        token_client.transfer(&provider, &env.current_contract_address(), &amount);
        
        env.events().publish((EVENT_LIQUIDITY_ADDED, challenge_id), (provider, amount, shares));
        
        Ok(shares)
    }
    
    /// Remove liquidity from a challenge pool
    pub fn remove_liquidity(env: Env, provider: Address, challenge_id: u64, shares: i128) -> Result<i128, Error> {
        provider.require_auth();
        
        if shares <= 0 {
            return Err(Error::InvalidLiquidityAmount);
        }
        
        let challenge: Challenge = env.storage().persistent().get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        
        let key = DataKey::LiquidityPool(challenge_id, provider.clone());
        let mut position: LiquidityPosition = env.storage().persistent().get(&key)
            .ok_or(Error::InsufficientLiquidity)?;
        
        if position.shares < shares {
            return Err(Error::InsufficientLiquidity);
        }
        
        let total_liquidity: i128 = env.storage().persistent().get(&DataKey::TotalLiquidity(challenge_id))
            .unwrap_or(0);
        
        // Calculate amount to return
        let amount_to_return = (shares * total_liquidity) / position.shares;
        
        if amount_to_return <= 0 {
            return Err(Error::InsufficientLiquidity);
        }
        
        // Check for flash loan protection before returning
        check_flash_loan_protection(&env, &provider, challenge_id, amount_to_return)?;
        
        // Update position
        position.shares -= shares;
        position.amount -= amount_to_return;
        position.last_update = env.ledger().timestamp();
        
        env.storage().persistent().set(&key, &position);
        
        // Update total liquidity
        env.storage().persistent().set(&DataKey::TotalLiquidity(challenge_id), &(total_liquidity - amount_to_return));
        
        // Return tokens
        let token_client = token::Client::new(&env, &challenge.token);
        token_client.transfer(&env.current_contract_address(), &provider, &amount_to_return);
        
        env.events().publish((EVENT_LIQUIDITY_REMOVED, challenge_id), (provider, amount_to_return, shares));
        
        Ok(amount_to_return)
    }
    
    /// Get liquidity position for a provider
    pub fn get_liquidity_position(env: Env, challenge_id: u64, provider: Address) -> Result<LiquidityPosition, Error> {
        let position: LiquidityPosition = env.storage().persistent().get(&DataKey::LiquidityPool(challenge_id, provider))
            .ok_or(Error::InsufficientLiquidity)?;
        Ok(position)
    }
    
    /// Oracle: submit outcome for a challenge (for aggregation)
    pub fn submit_oracle_outcome(env: Env, oracle: Address, challenge_id: u64, outcome_yes: bool) -> Result<(), Error> {
        oracle.require_auth();
        check_paused(&env)?;
        
        // Check if oracle is authorized (in relayer set)
        let relayer_set: Vec<Address> = env.storage().instance().get(&DataKey::RelayerSet)
            .unwrap_or(Vec::new(&env));
        
        let is_authorized = relayer_set.iter().any(|r| r == oracle);
        if !is_authorized {
            return Err(Error::OracleNotAuthorized);
        }
        
        let challenge: Challenge = env.storage().persistent().get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        
        if challenge.resolved || challenge.cancelled {
            return Err(Error::AlreadyFinalized);
        }
        
        if env.ledger().sequence() < challenge.resolve_ledger_seq {
            return Err(Error::TooEarlyToResolve);
        }
        
        // Check if oracle already submitted
        let submission_key = DataKey::OracleSubmission(challenge_id, oracle.clone());
        if env.storage().persistent().has(&submission_key) {
            return Err(Error::OracleAlreadySubmitted);
        }
        
        // Store submission
        let submission = OracleSubmission {
            challenge_id,
            outcome_yes,
            submitted_by: oracle.clone(),
        };
        env.storage().persistent().set(&submission_key, &submission);
        
        env.events().publish((EVENT_ORACLE_SUBMITTED, challenge_id), (oracle, outcome_yes));
        
        Ok(())
    }
    
    /// Aggregate oracle submissions and resolve challenge based on consensus
    pub fn aggregate_oracle_outcomes(env: Env, challenge_id: u64) -> Result<(), Error> {
        let challenge: Challenge = env.storage().persistent().get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        
        if challenge.resolved || challenge.cancelled {
            return Err(Error::AlreadyFinalized);
        }
        
        // Get all authorized oracles
        let relayer_set: Vec<Address> = env.storage().instance().get(&DataKey::RelayerSet)
            .unwrap_or(Vec::new(&env));
        
        let total_oracles = relayer_set.len() as u32;
        let quorum_size: u32 = env.storage().instance().get(&DataKey::RelayerQuorum)
            .unwrap_or(DEFAULT_QUORUM_SIZE);
        
        // Count submissions
        let mut yes_votes = 0u32;
        let mut no_votes = 0u32;
        
        for oracle in relayer_set.iter() {
            let submission_key = DataKey::OracleSubmission(challenge_id, oracle.clone());
            if let Some(submission) = env.storage().persistent().get::<DataKey, OracleSubmission>(&submission_key) {
                if submission.outcome_yes {
                    yes_votes += 1;
                } else {
                    no_votes += 1;
                }
            }
        }
        
        // Check if we have enough submissions
        let total_submissions = yes_votes + no_votes;
        if total_submissions < quorum_size {
            return Err(Error::InsufficientOracleSubmissions);
        }
        
        // Determine outcome based on majority
        let outcome_yes = yes_votes > no_votes;
        
        // Store aggregated result
        let aggregated = AggregatedOracleResult {
            challenge_id,
            outcome_yes,
            yes_votes,
            no_votes,
            total_oracles,
            aggregated_at: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&DataKey::OracleAggregation(challenge_id), &aggregated);
        
        // Resolve the challenge
        let mut challenge = challenge;
        challenge.resolved = true;
        challenge.outcome_yes = outcome_yes;
        env.storage().persistent().set(&DataKey::Challenge(challenge_id), &challenge);
        
        env.events().publish((EVENT_ORACLE_AGGREGATED, challenge_id), (yes_votes, no_votes, outcome_yes));
        
        Ok(())
    }
    
    /// Get aggregated oracle result for a challenge
    pub fn get_aggregated_oracle_result(env: Env, challenge_id: u64) -> Option<AggregatedOracleResult> {
        env.storage().persistent().get(&DataKey::OracleAggregation(challenge_id))
    }
    
    /// Check if user is flagged for flash loan activity
    pub fn is_flash_loan_flagged(env: Env, user: Address) -> bool {
        env.storage().instance().get(&DataKey::FlashLoanFlag(user)).unwrap_or(false)
    }
    
    /// Admin-only: clear flash loan flag for a user
    pub fn clear_flash_loan_flag(env: Env, user: Address) -> Result<(), Error> {
        require_admin(&env)?;
        env.storage().instance().set(&DataKey::FlashLoanFlag(user), &false);
        Ok(())
    }
    
    /// Get audit log entry for a challenge
    pub fn get_audit_log_entry(env: Env, challenge_id: u64, entry_id: u64) -> Option<AuditLogEntry> {
        env.storage().persistent().get(&DataKey::AuditLog(challenge_id, entry_id))
    }
    
    /// Get audit log count for a challenge
    pub fn get_audit_log_count(env: Env, challenge_id: u64) -> u32 {
        env.storage().persistent().get(&DataKey::AuditLogCount(challenge_id)).unwrap_or(0)
    }
    
    /// Get audit log entries for a challenge with pagination
    pub fn get_audit_log_entries(env: Env, challenge_id: u64, offset: u64, limit: u32) -> Result<Vec<AuditLogEntry>, Error> {
        let count: u32 = env.storage().persistent().get(&DataKey::AuditLogCount(challenge_id))
            .unwrap_or(0);
        
        let mut entries = Vec::new(&env);
        let mut collected = 0u32;
        
        for entry_id in (0..count as u64).rev() {
            if entry_id < offset {
                continue;
            }
            if collected >= limit {
                break;
            }
            
            if let Some(entry) = env.storage().persistent().get::<DataKey, AuditLogEntry>(&DataKey::AuditLog(challenge_id, entry_id)) {
                entries.push_back(entry);
                collected += 1;
            }
        }
        
        Ok(entries)
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
    
    /// File a dispute against an oracle resolution
    pub fn file_dispute(
        env: Env,
        disputer: Address,
        challenge_id: u64,
        disputed_outcome: bool,
        evidence: String,
        stake: i128,
    ) -> Result<(), Error> {
        disputer.require_auth();
        check_paused(&env)?;
        check_rate_limit(&env, DataKey::DisputeFilingCount(disputer.clone(), env.ledger().timestamp()), MAX_DISPUTES_PER_WINDOW)?;
        
        let challenge: Challenge = env.storage().persistent().get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        
        if !challenge.resolved {
            return Err(Error::NotResolved);
        }
        
        // Check dispute window
        let current_seq = env.ledger().sequence();
        let dispute_deadline = challenge.resolve_ledger_seq + DISPUTE_WINDOW_LEDGERS;
        if current_seq > dispute_deadline {
            return Err(Error::DisputeWindowClosed);
        }
        
        // Check minimum stake
        if stake < DISPUTE_STAKE_THRESHOLD {
            return Err(Error::InsufficientDisputeStake);
        }
        
        // Check if dispute already exists
        let dispute_key = DataKey::Dispute(challenge_id, disputer.clone());
        if env.storage().persistent().has(&dispute_key) {
            return Err(Error::DisputeAlreadyExists);
        }
        
        // Escrow dispute stake
        let token_client = token::Client::new(&env, &challenge.token);
        token_client.transfer(&disputer, &env.current_contract_address(), &stake);
        
        // Create dispute record
        let dispute = Dispute {
            challenge_id,
            disputer: disputer.clone(),
            disputed_outcome,
            evidence,
            stake,
            resolved: false,
            successful: false,
            created_at: env.ledger().timestamp(),
        };
        
        env.storage().persistent().set(&dispute_key, &dispute);
        
        // Increment dispute count
        let mut dispute_count: u32 = env.storage().persistent().get(&DataKey::DisputeCount(challenge_id))
            .unwrap_or(0);
        dispute_count += 1;
        env.storage().persistent().set(&DataKey::DisputeCount(challenge_id), &dispute_count);
        
        env.events().publish((EVENT_DISPUTE_CREATED, challenge_id), (disputer, disputed_outcome));
        
        Ok(())
    }
    
    /// Admin-only: resolve a dispute
    pub fn resolve_dispute(
        env: Env,
        challenge_id: u64,
        disputer: Address,
        successful: bool,
    ) -> Result<(), Error> {
        require_admin(&env)?;
        
        let dispute_key = DataKey::Dispute(challenge_id, disputer.clone());
        let mut dispute: Dispute = env.storage().persistent().get(&dispute_key)
            .ok_or(Error::NotDisputed)?;
        
        if dispute.resolved {
            return Err(Error::DisputeAlreadyResolved);
        }
        
        let mut challenge: Challenge = env.storage().persistent().get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        
        dispute.resolved = true;
        dispute.successful = successful;
        env.storage().persistent().set(&dispute_key, &dispute);
        
        let token_client = token::Client::new(&env, &challenge.token);
        
        if successful {
            // Return dispute stake with bonus
            let bonus = dispute.stake / 10; // 10% bonus for successful dispute
            let total_return = dispute.stake + bonus;
            token_client.transfer(&env.current_contract_address(), &disputer, &total_return);
            
            // If dispute was successful, flip the challenge outcome
            challenge.outcome_yes = dispute.disputed_outcome;
            challenge.resolved = true;
            env.storage().persistent().set(&DataKey::Challenge(challenge_id), &challenge);
        } else {
            // Burn the dispute stake (send to admin or contract)
            token_client.transfer(&env.current_contract_address(), &env.storage().instance().get(&DataKey::Admin).unwrap_or(env.current_contract_address()), &dispute.stake);
        }
        
        env.events().publish((EVENT_DISPUTE_RESOLVED, challenge_id), (disputer, successful));
        
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
        category: String,
    ) -> Result<u64, Error> {
        creator.require_auth();
        check_paused(&env)?;
        check_token_whitelist(&env, &token)?;
        check_rate_limit(&env, DataKey::ChallengeCreationCount(creator.clone(), env.ledger().timestamp()), MAX_CHALLENGES_PER_WINDOW)?;

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
            additional_tokens: Vec::new(&env),
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
        check_rate_limit(&env, DataKey::StakeCount(who.clone(), env.ledger().timestamp()), MAX_STAKES_PER_WINDOW)?;

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
        let vesting_config: u64 = env.storage().instance().get(&DataKey::UserVestingConfig(who.clone()))
            .unwrap_or(DEFAULT_VESTING_DURATION);
        let current_time = env.ledger().timestamp();
        
        let mut stake_rec: Stake = env.storage().persistent().get(&key).unwrap_or(Stake {
            side_yes,
            amount: 0,
            claimed: false,
            vesting_start: current_time,
            vesting_duration: vesting_config,
            is_vested: vesting_config == 0,
            token: challenge.token.clone(),
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
        
        // Emit odds update event for real-time odds calculation
        let (yes_odds, no_odds) = Self::calculate_odds(env.clone(), challenge_id)?;
        env.events().publish((EVENT_ODDS_UPDATED, challenge_id), (yes_odds, no_odds));

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
        check_flash_loan_protection(&env, &who, challenge_id, 0)?;

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
        
        // Check vesting - only allow claim if vested or vesting period has passed
        if !stake_rec.is_vested {
            let current_time = env.ledger().timestamp();
            let vested_time = stake_rec.vesting_start + stake_rec.vesting_duration;
            if current_time < vested_time {
                return Err(Error::StakeStillVesting);
            }
            stake_rec.is_vested = true;
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
    
    pub fn get_user_vesting(env: Env, user: Address) -> u64 {
        env.storage().instance().get(&DataKey::UserVestingConfig(user)).unwrap_or(DEFAULT_VESTING_DURATION)
    }
    
    pub fn get_stake_vesting_status(env: Env, challenge_id: u64, user: Address) -> Result<(bool, u64), Error> {
        let stake: Stake = env.storage().persistent().get(&DataKey::Stake(challenge_id, user))
            .ok_or(Error::NoStake)?;
        let current_time = env.ledger().timestamp();
        let vested_time = stake.vesting_start + stake.vesting_duration;
        let is_vested = stake.is_vested || current_time >= vested_time;
        let time_remaining = if is_vested { 0 } else { vested_time - current_time };
        Ok((is_vested, time_remaining))
    }
    
    /// Calculate current odds based on pool ratios
    /// Returns (yes_odds, no_odds) as percentages (0-10000 basis points)
    pub fn calculate_odds(env: Env, challenge_id: u64) -> Result<(u32, u32), Error> {
        let challenge: Challenge = env.storage().persistent().get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        
        let total_pool = challenge.pool_yes + challenge.pool_no;
        if total_pool == 0 {
            // Equal odds when no stakes
            return Ok((5000, 5000)); // 50% each
        }
        
        let yes_odds_bps = ((challenge.pool_yes * 10000) / total_pool) as u32;
        let no_odds_bps = 10000 - yes_odds_bps;
        
        Ok((yes_odds_bps, no_odds_bps))
    }
    
    /// Get potential payout for a given stake amount based on current odds
    pub fn calculate_potential_payout(env: Env, challenge_id: u64, side_yes: bool, amount: i128) -> Result<i128, Error> {
        let challenge: Challenge = env.storage().persistent().get(&DataKey::Challenge(challenge_id))
            .ok_or(Error::ChallengeNotFound)?;
        
        let (winning_pool, losing_pool) = if side_yes {
            (challenge.pool_yes, challenge.pool_no)
        } else {
            (challenge.pool_no, challenge.pool_yes)
        };
        
        if winning_pool == 0 {
            // No stakes on this side yet, return stake amount (no bonus)
            return Ok(amount);
        }
        
        let bonus = (amount * losing_pool) / winning_pool;
        Ok(amount + bonus)
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
