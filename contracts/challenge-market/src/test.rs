#![cfg(test)]

use super::*;
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::{token, Env};

fn create_token<'a>(env: &Env, admin: &Address) -> (Address, token::StellarAssetClient<'a>, token::Client<'a>) {
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let address = sac.address();
    let admin_client = token::StellarAssetClient::new(env, &address);
    let client = token::Client::new(env, &address);
    (address, admin_client, client)
}

#[test]
fn test_ledger_close_under_happy_path() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let relayer = Address::generate(&env);
    let alice = Address::generate(&env); // bets YES (closes under 6s)
    let bob = Address::generate(&env); // bets NO

    let (token_addr, token_admin, token_client) = create_token(&env, &admin);
    token_admin.mint(&alice, &1_000_000);
    token_admin.mint(&bob, &1_000_000);

    let contract_id = env.register(ChallengeMarket, ());
    let client = ChallengeMarketClient::new(&env, &contract_id);
    client.initialize(&admin, &relayer);

    env.ledger().with_mut(|l| {
        l.timestamp = 1_000;
        l.sequence_number = 100;
    });

    let id = client.create_challenge(
        &alice,
        &String::from_str(&env, "Next ledger closes under 6s"),
        &Condition::LedgerCloseUnder(6),
        &101, // resolve_ledger_seq
        &105, // staking_deadline_seq
        &token_addr,
    );

    client.stake(&alice, &id, &true, &100);
    client.stake(&bob, &id, &false, &50);

    // Advance ledger: 4 seconds later, next sequence closed.
    env.ledger().with_mut(|l| {
        l.timestamp = 1_004;
        l.sequence_number = 101;
    });

    client.resolve_native(&id);

    let challenge = client.get_challenge(&id);
    assert!(challenge.resolved);
    assert!(challenge.outcome_yes);

    let payout = client.claim(&alice, &id);
    // stake back (100) + all of losing pool (50) since alice is the only YES staker
    assert_eq!(payout, 150);
    assert_eq!(token_client.balance(&alice), 1_000_000 - 100 + 150);

    // Bob lost, nothing to claim.
    let bob_result = client.try_claim(&bob, &id);
    assert!(bob_result.is_err());
}

#[test]
fn test_ledger_close_over_threshold_no_wins() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let relayer = Address::generate(&env);
    let alice = Address::generate(&env);
    let bob = Address::generate(&env);

    let (token_addr, token_admin, _token_client) = create_token(&env, &admin);
    token_admin.mint(&alice, &1_000_000);
    token_admin.mint(&bob, &1_000_000);

    let contract_id = env.register(ChallengeMarket, ());
    let client = ChallengeMarketClient::new(&env, &contract_id);
    client.initialize(&admin, &relayer);

    env.ledger().with_mut(|l| {
        l.timestamp = 2_000;
        l.sequence_number = 200;
    });

    let id = client.create_challenge(
        &alice,
        &String::from_str(&env, "Next ledger closes under 6s"),
        &Condition::LedgerCloseUnder(6),
        &201,
        &205,
        &token_addr,
    );

    client.stake(&alice, &id, &true, &100);
    client.stake(&bob, &id, &false, &50);

    // 10 seconds later — slower than the 6s threshold, so NO wins.
    env.ledger().with_mut(|l| {
        l.timestamp = 2_010;
        l.sequence_number = 201;
    });

    client.resolve_native(&id);
    let challenge = client.get_challenge(&id);
    assert!(!challenge.outcome_yes);

    let payout = client.claim(&bob, &id);
    assert_eq!(payout, 150); // bob's 50 back + alice's losing 100
}

#[test]
fn test_oracle_resolution_path() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let relayer = Address::generate(&env);
    let alice = Address::generate(&env);

    let (token_addr, token_admin, _token_client) = create_token(&env, &admin);
    token_admin.mint(&alice, &1_000_000);

    let contract_id = env.register(ChallengeMarket, ());
    let client = ChallengeMarketClient::new(&env, &contract_id);
    client.initialize(&admin, &relayer);

    env.ledger().with_mut(|l| {
        l.sequence_number = 300;
    });

    let id = client.create_challenge(
        &alice,
        &String::from_str(&env, "Tx count spikes above 5000 this window"),
        &Condition::TxCountAtLeast(5000),
        &301,
        &305,
        &token_addr,
    );

    client.stake(&alice, &id, &true, &200);

    env.ledger().with_mut(|l| {
        l.sequence_number = 301;
    });

    // Only the configured relayer can resolve this path.
    client.resolve_via_oracle(&id, &true);

    let challenge = client.get_challenge(&id);
    assert!(challenge.resolved);
    assert!(challenge.outcome_yes);
}

#[test]
fn test_staking_closed_after_deadline() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let relayer = Address::generate(&env);
    let alice = Address::generate(&env);

    let (token_addr, token_admin, _token_client) = create_token(&env, &admin);
    token_admin.mint(&alice, &1_000_000);

    let contract_id = env.register(ChallengeMarket, ());
    let client = ChallengeMarketClient::new(&env, &contract_id);
    client.initialize(&admin, &relayer);

    env.ledger().with_mut(|l| {
        l.sequence_number = 400;
    });

    let id = client.create_challenge(
        &alice,
        &String::from_str(&env, "Next ledger closes under 6s"),
        &Condition::LedgerCloseUnder(6),
        &401,
        &400, // staking deadline already passed
        &token_addr,
    );

    let result = client.try_stake(&alice, &id, &true, &100);
    assert!(result.is_err());
}
