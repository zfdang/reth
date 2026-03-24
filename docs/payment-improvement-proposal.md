# Payment Throughput and Finality Improvement - Design and Implementation

> This document combines a deep study of [Sui fast path](https://move-book.com/object/fast-path-and-consensus/), [Tempo](https://docs.tempo.xyz/protocol/blockspace/overview), and the completed `crates/payment-lane/` implementation on `reth` into a single design-and-implementation writeup.
>
> The original analysis draft is preserved in `payment-improvement-proposal.md.bak`.

---

## 1. Background and Motivation

`reth` already decomposes the txpool, payload builder, execution engine, RPC/Engine API, and consensus into replaceable components. That means we do not need to replace the execution engine to build a payment-first roadmap on top of this repository.

### 1.1 Core References

| Reference | Core Idea | Implication for `reth` |
|------|---------|---------------|
| **Sui fast path** | When a transaction only touches owned or immutable objects, it does not need global ordering | This cannot be copied directly because the EVM uses shared state, but the conflict-detection idea can be reused for a *constrained payment path* |
| **Tempo** | Payments are first-class workloads: payment lanes reserve blockspace, 2D nonces solve hot-account contention, and BFT finality shortens settlement time | This is the most valuable architectural reference for `reth` |

### 1.2 Four Independent Bottlenecks

| # | Bottleneck | Description |
|---|------|------|
| 1 | **Blockspace contention** | Payments are squeezed out by DeFi, MEV, and complex contract calls during congestion |
| 2 | **Hot account serialization** | A single sender is limited by sequential nonces |
| 3 | **Settlement latency** | Finality still requires waiting for multiple confirmations |
| 4 | **Execution-path overhead** | Even simple payments still go through the full generic EVM execution path |

---

## 2. Practical Constraints in the Current Repo

### 2.1 The txpool and payload builder have no payment concept

- Txpool ordering uses `CoinbaseTipOrdering` (`crates/transaction-pool/src/ordering.rs`)
- The pending, queued, basefee, and blob subpools have no payment-aware classification
- The default payload builder only understands one global gas budget (`crates/ethereum/payload/src/lib.rs`)

**Conclusion**: under heavy load, payment transactions compete with ordinary contract calls for the same block budget.

### 2.2 Finality comes from external forkchoice, not an internal BFT layer

- An external CL provides `newPayload` and `forkchoiceUpdated`
- The EL only executes, validates, and tracks safe/finalized blocks

**Conclusion**: changing only the txpool or payload builder cannot provide deterministic finality.

### 2.3 Arbitrary ERC-20 transfers cannot be treated as a Sui-style fast path

In the EVM, an ERC-20 transfer still touches shared contract state such as balance slots, allowance state, and optional hooks. That means it cannot bypass ordering the way Sui transactions can under an object-ownership model.

**Conclusion**: fast path logic must be restricted to a *constrained payment transaction type whose read/write set can be determined statically*.

---

## 3. Overall Plan

Two tracks progress in parallel across five phases:

```text
Main track:       Phase 1 (Soft Lane) -> Phase 2 (Consensus-Enforced Lane) -> Phase 5 (BFT Finality)
Enhancement track: Phase 3 (2D Nonce) -> Phase 4 (Fast Path)
```

**Key design decisions**:

- Build the payment lane first because it is the least invasive change with the highest immediate payoff
- Restrict fast path to a constrained payment path, not a generic EVM fast path
- Start BFT finality as an external sidecar first, then consider in-process integration later

---

## 4. Phase 1: Soft Payment Lane [Implemented]

This corresponds to Tempo's payment lane idea, but implemented first as a builder policy without changing the consensus format of blocks or headers.

### 4.1 Transaction Classifier (`classifier.rs`)

**Design principle**: the classifier is purely stateless. It depends only on transaction payload and never reads chain state.

Classification rules use AND logic: the transaction must satisfy both target matching and call matching.

| Rule | Meaning |
|------|------|
| **Payment address prefix** | The `to` address starts with `0x20c0` in the TIP-20 style |
| **Payment allowlist** | The `to` address appears in a configured allowlist |
| **Supported selector** | The call is `transfer(address,uint256)` and calldata length is exactly 68 bytes |

Anything that does not satisfy these conditions is classified as `General`. This intentionally favors conservative false negatives over cross-client inconsistency.

**Key types**:

```rust
pub enum TxLane { Payment, General }

pub struct PaymentClassifier {
    config: PaymentLaneConfig,
}
```

**Difference from the original draft**: the original proposal suggested recognizing both `transfer` and `transferFrom`. The implementation narrows this to `transfer` only, because `transferFrom` depends on allowance state and is better deferred to a protocol-enforced payment path later.

### 4.2 Payment-Aware Ordering (`ordering.rs`)

The implementation provides a `TransactionOrdering` that boosts payment transactions:

```rust
pub struct PaymentAwareOrdering<T> { ... }

impl<T: PoolTransaction> TransactionOrdering for PaymentAwareOrdering<T> {
    type PriorityValue = PaymentPriority;
    // payment tip = base_tip * boost_factor (default 10x)
    // payment lane wins only as a tie-breaker at the same effective tip
}
```

**Ordering logic**:

1. First sort by boosted effective tip
2. If effective tips are equal, prefer the payment lane as a tie-breaker

This is more reasonable than the earlier "payment always beats general" model. A very high-tip general transaction should not be unconditionally displaced by a low-tip payment transaction.

### 4.3 Dual-Budget Payload Builder (`payload.rs`)

A payment-aware implementation of the `PayloadBuilder` trait is provided:

```rust
pub struct PaymentLanePayloadBuilder<Pool, Client, EvmConfig> { ... }
```

**Packing strategy**:

1. Classify each candidate transaction as `Payment` or `General`
2. Limit general transactions with `general_gas_limit` (default: `block_gas_limit * 0.7`)
3. Allow payment transactions to use any remaining gas up to `block_gas_limit`
4. Keep general transactions on a strict `general_gas_limit`; unused payment capacity is not reassigned to the general lane in the current implementation

**Configuration** (`PaymentLaneConfig`):

| Parameter | Default | Meaning |
|------|--------|------|
| `payment_gas_fraction` | `0.3` | Fraction of block gas reserved for payment traffic |
| `general_gas_limit` | `None` | Hard cap for non-payment transactions, derived automatically when unset |
| `payment_priority_boost` | `10` | Priority multiplier for payment transactions |
| `consensus_enforced` | `false` | Whether consensus-level lane validation is enabled |

### 4.4 Metrics (`metrics.rs`)

The implementation exposes 14 counters covering classification, payload construction, and consensus validation:

- `payment_tx_classified` / `general_tx_classified`
- `payment_tx_included` / `general_tx_included`
- `general_tx_skipped_lane_full`
- `blocks_built`
- `consensus_lane_valid` / `consensus_lane_invalid`
- and the remaining counters for fast path and nonce tracking

### 4.5 Limits of Phase 1

- This is still proposer-local policy, not a protocol guarantee
- Finality does not change; only throughput and inclusion latency improve

---

## 5. Phase 2: Consensus-Enforced Lane [Implemented]

This upgrades the payment lane from builder policy to consensus rule so that payment throughput becomes a protocol property rather than a local scheduling choice.

### 5.1 Implementation: `PaymentLaneValidator<C>` (`consensus.rs`)

The implementation wraps an arbitrary `Consensus` instance and injects lane validation in `validate_block_post_execution`, using the actual gas consumed by receipts:

```rust
pub struct PaymentLaneValidator<C> {
    inner: C,                      // original consensus
    classifier: PaymentClassifier, // classifier
    config: PaymentLaneConfig,     // lane configuration
    metrics: PaymentLaneMetrics,   // metrics
}
```

**Consensus rules** when `consensus_enforced = true`:

1. Standard Ethereum rule: `gas_used <= gas_limit` (delegated to the inner validator)
2. Additional lane rule: cumulative executed gas of non-payment transactions must remain `<= general_gas_limit`

**Trait behavior**:

- `HeaderValidator<H>` -> directly delegated to the inner validator
- `Consensus<B>` -> directly delegates pre-execution validation
- `FullConsensus<N>` -> calls the inner validator first, then enforces post-execution lane accounting

This is more robust than the earlier gas-limit-based approximation because reverted transactions and transactions that do not consume all declared gas are not over-counted.

### 5.2 Comparison with Tempo

| Tempo | This implementation |
|-------|--------|
| Headers explicitly carry `general_gas_limit` and `shared_gas_limit` | Header format is unchanged; `general_gas_limit` is derived from local config |
| Sub-block and validator-subblock structure | Not needed in the current version |
| Classification must be deterministic | Yes, the classifier is fully stateless and deterministic |

### 5.3 Optional Future Evolution

- Encode `general_gas_limit` explicitly in the header by extending engine primitives
- Add a Tempo-style shared lane so payment traffic can overflow into general capacity under explicit protocol rules

---

## 6. Phase 3: 2D Nonce System [Implemented]

This solves the hot-sender serialization problem by borrowing Tempo's 2D nonce, nonce-key, and expiring-nonce ideas.

### 6.1 Core Types (`nonce.rs`)

```rust
// 2D nonce: (key, value)
pub struct NonceKey {
    pub key: u64,   // 0 = protocol nonce, 1+ = user-defined parallel lanes
    pub value: u64,
}

// Validity window
pub struct ValidityWindow {
    pub valid_after: u64,  // 0 = no lower bound
    pub valid_before: u64, // 0 = no upper bound
}

// Payment transaction metadata
pub struct PaymentTxMeta {
    pub nonce_key: NonceKey,
    pub validity: ValidityWindow,
    pub memo: Option<B256>,
}
```

### 6.2 Nonce Model

| Key | Behavior |
|-----|------|
| `key = 0` | Protocol nonce with standard Ethereum-style sequential progression |
| `key > 0` | User nonce key with an independent sequence, allowing parallel submission by the same sender |

**Sender state tracking**: `SenderNonceState` tracks the current value of every nonce key and enforces:

1. `value < current` -> `NonceTooLow`
2. `value > current` -> `NonceTooHigh`
3. A new key beyond `max_nonce_keys` (default `256`) -> `TooManyNonceKeys`

### 6.3 Expiring Nonces

`ValidityWindow` supports automatic expiration:

- `valid_before > 0 && timestamp >= valid_before` -> the transaction is expired
- `valid_after > 0 && timestamp < valid_after` -> the transaction is not yet valid

**Benefit**: unused nonce keys do not need to remain permanently live forever, reducing state bloat.

### 6.4 Comparison with Tempo

| Tempo | This implementation |
|-------|--------|
| `nonce_key` (u16) + `nonce_value` (u64) | `key` (u64) + `value` (u64) |
| Protocol nonce is key `0` | Yes |
| `valid_after` / `valid_before` | Yes |
| Maximum key count | Yes, `max_nonce_keys = 256` |
| Memo field | Yes, `Option<B256>` |

---

## 7. Phase 4: Constrained Fast Path [Implemented]

This borrows the conflict-detection intuition from Sui fast path, but only for payment operations whose read/write sets can be derived statically.

The current implementation provides the core primitives, such as `PaymentIntent`, `BatchPaymentIntent`, and `ConflictDetector`. Actual integration into a protocol-enforced payment path still belongs to the future integration work in Section 12.

### 7.1 Design Constraints

**What is allowed**:

- Fixed-format transfers with `PaymentIntent`
- Fixed-format batch transfers with `BatchPaymentIntent`
- A read/write set of `{(asset, sender), (asset, receiver)}` derived directly from transaction fields
- Parallel execution of non-conflicting payment operations

**What is explicitly not allowed**:

- Letting arbitrary ERC-20 `transfer` calls use the fast path
- Dynamically analyzing arbitrary EVM bytecode at runtime to infer read/write sets
- Claiming a Sui-style fast path without a protocol-enforced payment contract or system path

### 7.2 Core Types (`fastpath.rs`)

```rust
pub struct PaymentIntent {
    pub from: Address,
    pub to: Address,
    pub asset: Address,
    pub amount: U256,
    pub nonce_key: u64,
    pub nonce_value: u64,
}

pub struct ConflictKey {
    pub asset: Address,
    pub account: Address,
}
```

### 7.3 Conflict Detection and Partitioning

`ConflictDetector` uses greedy graph coloring to divide a set of payments into non-conflicting batches:

1. Build a conflict index: `ConflictKey -> {payment indices}`
2. Construct a conflict graph
3. Apply greedy coloring to partition execution batches

**Examples**:

- `A -> B` and `C -> D` -> same batch
- `A -> B` and `A -> C` -> two batches because they conflict on `A`
- `A -> B`, `B -> C`, and `C -> A` -> three batches in the cyclic case

### 7.4 Batch Payments

```rust
pub struct BatchPaymentIntent {
    pub from: Address,
    pub transfers: Vec<TransferTarget>,
    pub asset: Address, // one asset per batch
}
```

Conflict keys are:

`{(asset, sender)} U {(asset, receiver_i) for each transfer}`

---

## 8. Phase 5: BFT Finality Sidecar [Implemented as a Framework]

The real way to reduce settlement latency is deterministic finality, not simply shorter block times.

The current implementation covers the BFT finality state machine and the core vote/certificate logic. Actual sidecar-to-Engine-API integration remains future node integration work.

### 8.1 Architecture

```text
+---------------------+
|  BFT Consensus      |
|  (Simplex-style)    |
|  Proposer election  |
|  Block voting       |
|  Finalization       |
+---------+-----------+
          | Engine API
          v
+---------------------+
|  Reth EL            |
|  newPayload         |
|  forkchoiceUpdated  |
+---------------------+
```

### 8.2 Core Types (`finality.rs`)

```rust
pub struct BftConfig {
    pub block_interval: Duration,     // 600ms
    pub validator_count: u32,         // 4
    pub fault_tolerance: u32,         // 1 (= (4-1)/3)
    pub proposal_timeout: Duration,   // 400ms
    pub vote_timeout: Duration,       // 200ms
}

pub struct Vote { validator_index, block_hash, block_number, signature }

pub struct FinalityCertificate { block_hash, block_number, votes, total_fees }

pub struct PaymentPreconfirmation { tx_hash, target_block, proposer_index, signature }

pub struct FinalityTracker { latest_finalized, pending_blocks, ... }
```

### 8.3 Finality Guarantees

| Property | Guarantee |
|------|------|
| **Safety** | No conflicting block is finalized while Byzantine validators remain below one third |
| **Liveness** | Finalization continues while honest validators remain at least two thirds |
| **Supermajority** | Finalization requires `ceil(validators * 2 / 3)` votes |
| **No reorgs** | Finalized blocks are irreversible |

### 8.4 Preconfirmation

Preconfirmation gives a low-latency signal before full block finality:

- The proposer signs that a transaction has been accepted
- Finalized blocks remain the source of strong settlement
- This is especially useful for API, MCP, and machine-payment workflows

### 8.5 `FinalityTracker` State Machine

```text
Proposed -> Voting(n) -> Finalized
                     -> Rejected
```

- `propose_block()` -> enter `Proposed`
- `add_vote()` -> accumulate votes and finalize at the supermajority threshold
- Old finalized blocks are pruned automatically

---

## 9. Comparison Summary: Sui and Tempo

### 9.1 Sui Fast Path

| Sui Feature | `reth` counterpart | Status |
|----------|----------|------|
| Object ownership gives natural conflict isolation | Not applicable to EVM shared state | N/A |
| Owned-object transactions can skip global ordering | `ConflictDetector` performs conflict analysis for constrained `PaymentIntent`s | Implemented in Phase 4 |
| Fast certificates | `FinalityCertificate` | Implemented in Phase 5 |

**Key difference**: Sui can apply fast-path logic to any transaction that satisfies the object rules. In contrast, this repo can only use fast-path logic for a protocol-enforced payment path because the EVM cannot statically determine read/write sets for arbitrary contract calls.

### 9.2 Tempo

| Tempo Feature | `reth` counterpart | Status |
|-----------|----------|------|
| Payment lane blockspace guarantee | `PaymentLanePayloadBuilder` + `PaymentLaneValidator` | Implemented in Phases 1 and 2 |
| `general_gas_limit` / `shared_gas_limit` | `PaymentLaneConfig.payment_gas_fraction` used to derive the limit | Partially implemented |
| TIP-20 prefix `0x20c0` | `PaymentPrefix` matching | Implemented |
| 2D nonce (`nonce_key` + `nonce_value`) | `NonceKey` | Implemented in Phase 3 |
| Expiring nonce | `ValidityWindow` | Implemented in Phase 3 |
| Simplex-style BFT consensus (~600ms) | `BftConfig` + `FinalityTracker` | Implemented in Phase 5 |
| Fee sponsorship | Not implemented | Future work |
| Sub-block structure | Not implemented because it is not required yet | Future work |

---

## 10. Implementation Status

### 10.1 Code Location

All code lives in `crates/payment-lane/` (crate name: `reth-payment-lane`).

| Module | File | Phase | Tests |
|------|------|-------|------|
| Configuration | `config.rs` | 1-5 | 8 tests |
| Classifier | `classifier.rs` | 1 | 12 tests |
| Ordering | `ordering.rs` | 1 | 7 tests |
| Payload builder | `payload.rs` | 1 | 7 tests |
| Consensus validation | `consensus.rs` | 2 | 10 tests |
| 2D nonce | `nonce.rs` | 3 | 13 tests |
| Fast path | `fastpath.rs` | 4 | 9 tests |
| BFT finality | `finality.rs` | 5 | 15 tests |
| Metrics | `metrics.rs` | 1-5 | structural only |

**Total**: 81 unit tests, all passing.

### 10.2 Quality Status

| Check | Status |
|--------|------|
| `cargo check -p reth-payment-lane` | Pass |
| `cargo fmt -p reth-payment-lane -- --check` | Pass |
| `cargo clippy -p reth-payment-lane --all-targets -- -D warnings` | Pass |
| `cargo test -p reth-payment-lane` | Pass |

---

## 11. Routes Explicitly Not Recommended

### 11.1 Claiming a payment lane guarantee after changing only the txpool

That is only local policy, not a protocol guarantee. Phase 1 alone is not enough; Phase 2 is what turns it into consensus behavior.

### 11.2 Forcing Sui-style fast path onto arbitrary ERC-20 or arbitrary contract calls

That introduces severe correctness risk under the EVM shared-state model. Fast path must stay constrained to a protocol-defined payment path.

### 11.3 Trying to solve finality only by reducing block time

Faster block production means faster visibility, not stronger settlement. Deterministic BFT finality is still required.

---

## 12. Future Work

### 12.1 Near Term: integrate into the `reth` node

- [ ] Inject `PaymentAwareOrdering` and `PaymentLanePayloadBuilder` in `crates/ethereum/node/src/node.rs`
- [ ] Inject `PaymentLaneValidator` as a consensus wrapper in `crates/ethereum/node/src/node.rs`
- [ ] Expose `PaymentLaneMetrics` through Prometheus
- [ ] Add RPC methods such as `reth_getPaymentLaneStats`

### 12.2 Mid Term: protocol evolution

- [ ] Encode `general_gas_limit` explicitly in the header by extending engine primitives
- [ ] Implement a payment transaction envelope that carries 2D nonce and validity-window semantics
- [ ] Add native 2D nonce ordering and eviction in the txpool
- [ ] Deploy a protocol-enforced payment precompile or system contract

### 12.3 Longer Term: production BFT

- [ ] Implement a BFT consensus sidecar as a separate binary that drives `reth` through the Engine API
- [ ] Add payment preconfirmation endpoints
- [ ] Consider embedding BFT into the node launch path
- [ ] Add fee sponsorship

---

## 13. Success Criteria

| Metric | Goal |
|------|------|
| Payment TPS under mixed load | Meaningfully higher than baseline |
| Payment p99 inclusion latency during DeFi congestion | Stable within 1-2 blocks |
| Concurrency for a single hot sender | Roughly linear in the number of nonce keys |
| Finalized payment latency | <= 1 second after BFT rollout |
| Reorgs after finalization | 0 |

---

## 14. Conclusion

For this repo, the recommended path is:

1. **First build the payment lane** -> implemented in Phases 1 and 2
2. **Then add parallel nonce and payment-specific transaction semantics** -> implemented in Phase 3
3. **Then add the constrained fast path** -> implemented in Phase 4
4. **Finally add BFT finality** -> framework implemented in Phase 5
5. **Next**: integrate into the `reth` node, run real workload testing, and evolve the protocol surface

The core thesis is simple: **Tempo-style lane plus BFT finality** should be the main track, while **Sui-style fast path** should remain a later enhancement after the payment path has been made sufficiently constrained.
