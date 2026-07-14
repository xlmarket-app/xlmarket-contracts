# xlmarket-contracts

Soroban smart contracts for **XLMarket** — an on-chain prediction/challenge
game where the outcome data is real Stellar network activity (ledger close
times, tx counts, fee spikes), not made-up game logic.

## Layout

```
contracts/
  challenge-market/
    src/
      lib.rs    — the contract: challenges, staking, resolution, payouts
      test.rs   — unit tests covering both resolution paths
```

## The core idea

Every challenge is a pari-mutuel pool split between YES and NO:

- Anyone can `stake` XLM (or any SAC token) onto either side while staking
  is open.
- Once the target ledger is reached, the challenge is `resolve`d one of two
  ways:
  - **Native** (`resolve_native`) — fully trustless, uses the ledger's own
    timestamp exposed by the Soroban host. No oracle involved. This is the
    path for the flagship bet type: "next ledger closes under N seconds."
  - **Oracle** (`resolve_via_oracle`) — for Horizon-only metrics (tx counts,
    fee spikes) that the contract can't see natively. Restricted to a
    single trusted relayer address for now; the interface is written so a
    multi-relayer quorum can replace that single address later without
    touching the pool/payout logic.
- Winners `claim` their stake back plus a pro-rata share of the losing
  pool.

See the doc comment at the top of `lib.rs` for the full reasoning — it's
written to be read as design rationale, not just code.

## Building

You'll need Rust 1.84+ and the Stellar CLI (the successor to the old
`soroban-cli` name):

```bash
rustup target add wasm32v1-none
cargo install --locked stellar-cli
```

> **Note:** `Cargo.lock` is committed to this repo on purpose (unusual for
> a Rust library, normal for something people build and deploy). Soroban's
> dependency tree includes crypto crates (`ed25519-dalek`, `rand_core`)
> that Cargo can otherwise resolve to two incompatible major versions at
> once and fail with a `trait bound ... is not satisfied` error. Committing
> the lockfile means everyone resolves to the same known-working set. If
> you ever delete `Cargo.lock` and hit that error again, it means a fresh
> resolution picked incompatible versions — `cargo update -p ed25519-dalek --precise <version>` pinned lower, or bumping `soroban-sdk` further, is the fix.

Build (note: use `stellar contract build`, not raw `cargo build` — the
CLI applies the release settings Soroban requires):

```bash
cd contracts/challenge-market
stellar contract build
```

The compiled wasm lands at
`target/wasm32v1-none/release/challenge_market.wasm`.

Test (plain `cargo test` is fine for tests — they run against the SDK's
local host environment, not the wasm target):

```bash
cargo test
```

## Deploying to testnet (example)

```bash
stellar keys generate alice --network testnet --fund

stellar contract deploy \
  --wasm target/wasm32v1-none/release/challenge_market.wasm \
  --source-account alice \
  --network testnet \
  --alias challenge_market

stellar contract invoke \
  --id challenge_market \
  --source-account alice \
  --network testnet \
  -- initialize \
  --admin <admin-address> \
  --oracle_relayer <relayer-address>
```

## Known v1 simplifications (good first-contribution targets)

- A user can't straddle both sides of the same challenge with separate
  stakes — the second `stake` call overwrites their side. Worth deciding
  whether to support both sides per user or explicitly reject a side-switch.
- Single-address oracle trust for Horizon-derived conditions. A
  multi-relayer quorum (median-of-N or N-of-M signatures) would remove
  that trust assumption — `resolve_via_oracle` and `DataKey::OracleRelayer`
  are the places to start.
- No protocol fee / rake is taken anywhere. If XLMarket needs a revenue
  model, `claim` is where a cut would be deducted.
- No cancellation/refund path if a challenge never gets enough stake on
  one side.
