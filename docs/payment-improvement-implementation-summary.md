# Payment Lane Implementation Summary

This document summarizes the `reth-payment-lane` crate — the production implementation of the payment throughput and finality improvement system. For the problem analysis, reference study, and design rationale, see [payment-improvement-proposal.md](payment-improvement-proposal.md).

---

## 1. Crate Overview

| | |
|---|---|
| **Crate** | `reth-payment-lane` |
| **Location** | `crates/payment-lane/` |
| **Tests** | 81 unit tests, all passing |
| **Quality** | `cargo check`, `cargo fmt`, `cargo clippy` — clean |

### Module Map

| Module | File | Phase | Tests | Description |
|--------|------|-------|-------|-------------|
| Configuration | `config.rs` | 1–5 | 8 | Central config, gas budget computation |
| Classifier | `classifier.rs` | 1 | 12 | Stateless tx classification (payment / general) |
| Ordering | `ordering.rs` | 1 | 7 | Payment-aware priority for the txpool |
| Payload builder | `payload.rs` | 1 | 7 | Dual-budget `PayloadBuilder` |
| Consensus | `consensus.rs` | 2 | 10 | Post-execution lane gas enforcement |
| 2D nonce | `nonce.rs` | 3 | 13 | Parallel nonce keys, validity windows |
| Fast path | `fastpath.rs` | 4 | 9 | Conflict detection, batch partitioning |
| BFT finality | `finality.rs` | 5 | 15 | Vote collection, finality certificates |
| Metrics | `metrics.rs` | 1–5 | — | 14 Prometheus counters |
| Re-exports | `lib.rs` | — | — | Module declarations and public API surface |

---

## 2. Configuration (`config.rs`)

### `PaymentLaneConfig`

Central configuration shared across all modules.

```rust
pub struct PaymentLaneConfig {
    pub payment_gas_fraction: f64,          // default 0.3
    pub general_gas_limit: Option<u64>,     // derived when None
    pub payment_address_prefixes: Vec<PaymentPrefix>,  // default [0x20, 0xc0]
    pub payment_allowlist: Vec<Address>,
    pub consensus_enforced: bool,           // default false
    pub payment_priority_boost: u64,        // default 10
    pub max_nonce_keys: u16,                // default 256
    pub nonce_expiry_window: u64,           // default 30s
}
```

### Key Methods

| Method | Purpose |
|--------|---------|
| `compute_general_gas_limit(block_gas_limit)` | Returns `general_gas_limit` or derives it as `block_gas_limit - reserved` |
| `compute_payment_reserved_gas(block_gas_limit)` | Returns the payment-reserved portion |

### `PaymentPrefix`

Matches a `to` address against a byte prefix (e.g. `[0x20, 0xc0]` for TIP-20).

---

## 3. Classifier (`classifier.rs`)

### `TxLane`

```rust
pub enum TxLane { Payment, General }
```

### `PaymentClassifier`

Stateless, no chain state. Classification requires **both** conditions:

1. `to` address matches a prefix **or** appears in the allowlist
2. Calldata is exactly `transfer(address,uint256)` — selector `0xa9059cbb`, 68 bytes total

Design choice: `transferFrom` is excluded (its allowance writes make the read/write set non-trivial).

```rust
pub fn classify(&self, to: Option<&Address>, input: &[u8]) -> TxLane
```

---

## 4. Ordering (`ordering.rs`)

### `PaymentPriority`

Composite priority: first compare boosted tip, then use lane as tie-breaker.

```rust
pub struct PaymentPriority { pub lane: TxLane, pub tip: u128 }
// Ord: tip first, then lane (Payment > General at equal tip)
```

### `PaymentAwareOrdering<T>`

Implements `TransactionOrdering` from `reth-transaction-pool`.

- Payment tip = `base_tip * boost_factor`
- General tip = `base_tip`
- High-tip general transactions still beat low-tip payment transactions

---

## 5. Payload Builder (`payload.rs`)

### `PaymentLanePayloadBuilder<Pool, Client, EvmConfig>`

Implements the `PayloadBuilder` trait with dual gas budgets.

### `LaneGasBudget` (internal)

Tracks two gas ceilings during block construction:

```rust
struct LaneGasBudget {
    total_gas_limit: u64,
    general_gas_limit: u64,
    cumulative_gas_used: u64,
    general_gas_used: u64,
}
```

| Method | Purpose |
|--------|---------|
| `can_fit_in_block(gas)` | Total capacity check |
| `can_fit_in_lane(lane, gas)` | Lane-specific check (General is capped; Payment is not) |
| `record_execution(lane, gas)` | Update counters after execution |

### Packing Loop

```
for each best_tx from pool:
    classify(tx) -> lane
    if General and general budget exhausted -> skip (don't mark sender invalid)
    if can_fit_in_block(tx.gas_limit) -> execute
    record_execution(lane, gas_used)
    update metrics
```

Payment transactions can use both the reserved budget and any unused general capacity. When the general budget is exhausted, only payment transactions continue.

### `PaymentLaneBuilderConfig`

```rust
pub struct PaymentLaneBuilderConfig {
    pub desired_gas_limit: u64,
    pub await_payload_on_missing: bool,
    pub max_blobs_per_block: Option<u64>,
    pub extra_data: Bytes,
    pub payment_config: PaymentLaneConfig,
}
```

---

## 6. Consensus Validator (`consensus.rs`)

### `PaymentLaneValidator<C>`

Wraps any `C: Consensus + FullConsensus` implementation.

**Trait delegation**:

| Trait | Behavior |
|-------|----------|
| `HeaderValidator<H>` | Fully delegated to `inner` |
| `Consensus<B>` | Fully delegated to `inner` |
| `FullConsensus<N>` | Calls `inner.validate_block_post_execution()`, then enforces lane gas |

**Lane gas rule** (when `consensus_enforced = true`):

1. Compute `general_gas_limit` from config
2. Walk `(transactions, receipts)` pairs; compute per-tx gas from cumulative receipt values
3. Sum gas of `General`-classified transactions
4. Reject if `general_gas_used > general_gas_limit`

Uses executed gas (from receipts), not declared gas limits. This avoids over-counting from reverted or partially-used transactions.

---

## 7. 2D Nonce System (`nonce.rs`)

### Core Types

| Type | Purpose |
|------|---------|
| `NonceKey { key, value }` | 2D nonce — key 0 is protocol, 1+ are parallel |
| `ValidityWindow { valid_after, valid_before }` | Expiration bounds (0 = unbounded) |
| `PaymentTxMeta` | Combines nonce key + validity + optional memo |
| `SenderNonceState` | Per-sender nonce tracking across all keys |

### `SenderNonceState`

```rust
pub fn validate_and_advance(&mut self, sender: Address, nonce_key: &NonceKey)
    -> Result<(), NonceError>
```

Checks:
- `NonceTooLow` — replay attempt
- `NonceTooHigh` — gap
- `TooManyNonceKeys` — key count exceeds `max_nonce_keys`

### `validate_payment_meta()`

Top-level validation function: checks validity window first, then nonce.

### `NonceError`

```rust
pub enum NonceError {
    NonceTooLow { key, expected, got },
    NonceTooHigh { key, expected, got },
    TooManyNonceKeys { max, sender },
    Expired { valid_before, timestamp },
    NotYetValid { valid_after, timestamp },
}
```

---

## 8. Fast Path (`fastpath.rs`)

### `PaymentIntent`

A constrained payment whose read/write set is statically known:

```rust
pub struct PaymentIntent {
    pub from: Address,
    pub to: Address,
    pub asset: Address,   // Address::ZERO for native ETH
    pub amount: U256,
    pub nonce_key: u64,
    pub nonce_value: u64,
}
```

`conflict_keys()` -> `[(asset, from), (asset, to)]`

### `BatchPaymentIntent`

```rust
pub struct BatchPaymentIntent {
    pub from: Address,
    pub transfers: Vec<TransferTarget>,
    pub asset: Address,
    pub nonce_key: u64,
    pub nonce_value: u64,
}
```

`conflict_keys()` -> `[(asset, from)] ∪ {(asset, to_i) for each transfer}`

### `ConflictDetector`

Partitions a set of `PaymentIntent`s into non-conflicting batches using greedy graph coloring:

```rust
pub fn partition(&mut self, payments: &[PaymentIntent]) -> Vec<Vec<usize>>
```

1. Build conflict index: `ConflictKey -> {payment indices}`
2. Construct an adjacency-list conflict graph
3. Greedy color assignment → each color = one parallel batch

Multi-asset payments with different `asset` addresses do not conflict, even if `from` and `to` are the same.

---

## 9. BFT Finality (`finality.rs`)

### `BftConfig`

```rust
pub struct BftConfig {
    pub block_interval: Duration,     // 600ms
    pub validator_count: u32,         // 4
    pub fault_tolerance: u32,         // 1
    pub proposal_timeout: Duration,   // 400ms
    pub vote_timeout: Duration,       // 200ms
}
```

`supermajority()` = `ceil(validator_count * 2 / 3)` — required votes for finalization.

### State Machine: `FinalityTracker`

```text
propose_block(hash, number) → Proposed
add_vote(vote, config) → Voting { votes } | Finalized
prune(keep) → remove old finalized blocks
```

### Vote Collection

- Duplicate votes from the same validator are ignored
- Out-of-range validator indices are rejected
- Votes for already-finalized blocks return `Finalized` without error

### `FinalityCertificate`

Proves finality: must contain `>= supermajority()` distinct, matching votes.

Validity checks:
- All votes reference the same `(block_hash, block_number)`
- No duplicate `validator_index`
- Total unique voters >= supermajority

### `PaymentPreconfirmation`

A proposer-signed acknowledgment before full finality — useful for low-latency API/machine-payment workflows.

---

## 10. Metrics (`metrics.rs`)

14 Prometheus counters under the `payment_lane` scope:

| Counter | Tracks |
|---------|--------|
| `payment_tx_classified` | Transactions classified as payment |
| `general_tx_classified` | Transactions classified as general |
| `payment_tx_included` | Payment transactions included in built blocks |
| `general_tx_included` | General transactions included in built blocks |
| `general_tx_skipped_lane_full` | General transactions skipped due to budget exhaustion |
| `payment_gas_used_reserved` | Payment gas consumed from reserved budget |
| `payment_gas_used_overflow` | Payment gas consumed from overflow/shared capacity |
| `blocks_built` | Total blocks built with payment lane policy |
| `consensus_lane_valid` | Blocks passing consensus lane validation |
| `consensus_lane_invalid` | Blocks failing consensus lane validation |
| `fastpath_tx_processed` | Fast-path transactions processed |
| `fastpath_conflicts` | Fast-path conflicts detected |
| `parallel_nonce_validated` | Parallel nonce transactions validated |
| `expired_nonce_rejected` | Expired nonce transactions rejected |

---

## 11. Dependencies

Key `reth` crates consumed:

| Dependency | Used By |
|-----------|---------|
| `reth-transaction-pool` | Ordering (`TransactionOrdering`, `PoolTransaction`) |
| `reth-basic-payload-builder` | Payload (`PayloadBuilder`, `BuildArguments`) |
| `reth-consensus` | Consensus (`Consensus`, `FullConsensus`, `HeaderValidator`) |
| `reth-evm` / `reth-evm-ethereum` | Payload (EVM configuration, block building) |
| `reth-ethereum-primitives` | Transaction types, receipts |
| `reth-chainspec` | Chain spec provider for hardfork checks |
| `reth-storage-api` / `reth-revm` | State access during payload building |
| `reth-metrics` | Metrics derive macro |

External:
`alloy-primitives`, `alloy-consensus`, `alloy-rlp`, `revm`, `serde`, `thiserror`, `tracing`, `metrics`

---

## 12. What Is Implemented vs. Future Work

| Area | Status | Next Step |
|------|--------|-----------|
| Transaction classifier | **Done** | — |
| Payment-aware ordering | **Done** | Wire into `crates/ethereum/node/src/node.rs` |
| Dual-budget payload builder | **Done** | Wire into node builder |
| Consensus lane validation | **Done** | Wire as consensus wrapper, enable `consensus_enforced` |
| 2D nonce types & validation | **Done** | Integrate into txpool validation and a new tx envelope |
| Conflict detection & partitioning | **Done** | Connect to a protocol-enforced payment precompile |
| BFT finality state machine | **Done** | Build a sidecar binary that drives reth via Engine API |
| Metrics | **Done** (structural) | Expose through Prometheus after node integration |
| Payment preconfirmation RPC | Types only | Implement RPC endpoint |
| Fee sponsorship | Not started | — |
| Header-encoded lane limits | Not started | Extend engine primitives |
