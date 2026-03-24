# Payment Throughput and Finality Improvement — Design Document

This document covers the problem analysis, reference study, and overall design for improving payment throughput and finality in `reth`. For the corresponding implementation summary, see [payment-improvement-proposal.v1.md](payment-improvement-proposal.v1.md).

---

## 1. Background

`reth` already decomposes the txpool, payload builder, execution engine, RPC/Engine API, and consensus into replaceable components. That means we do not need to replace the existing execution engine to build a payment-first roadmap on top of this repository.

---

## 2. Reference Study

### 2.1 Sui Fast Path

**Source**: <https://move-book.com/object/fast-path-and-consensus/>

**Core insight**: When a transaction only touches owned or immutable objects, it does not need full global ordering. The object ownership model provides a natural conflict boundary — if two transactions touch disjoint sets of owned objects, they can be processed in parallel without going through consensus first.

**Why it cannot be copied directly into `reth`**:

The EVM uses a shared-state model. An ERC-20 `transfer` touches shared contract state:

- Sender and receiver balance slots in the same contract
- Allowance slots
- Total supply / hooks / fee logic
- Possible external calls or custom logic in the receive path

Since the EVM cannot statically determine the read/write set of an arbitrary contract call, Sui-style fast path can only be applied to a *constrained payment path* whose read/write set is deterministic and known before execution.

### 2.2 Tempo

**Sources**:

- <https://docs.tempo.xyz/protocol/blockspace/overview>
- <https://docs.tempo.xyz/protocol/blockspace/payment-lane-specification>
- <https://docs.tempo.xyz/protocol/blockspace/consensus>
- <https://github.com/tempoxyz/tempo>
- <https://github.com/zfdang/tempo-analysis/blob/main/docs/tempo-network-execution-layer.md>

**Core insight**: Payments are first-class workloads. The protocol reserves blockspace through a dedicated payment lane, solves hot-account contention through 2D nonces, and shortens settlement time through deterministic BFT finality.

**Key Tempo features**:

| Feature | Description |
|---------|-------------|
| **Payment lane** | Block header carries `general_gas_limit` and `shared_gas_limit`; payment transactions get reserved capacity |
| **TIP-20 tokens** | Token contracts with a `0x20c0` address prefix; recognized at the protocol level |
| **2D nonce** | `(nonce_key, nonce_value)` — key 0 is the standard protocol nonce, keys 1+ are independent parallel sequences |
| **Expiring nonce** | `valid_after` / `valid_before` timestamps provide automatic replay protection without permanent state bloat |
| **Simplex BFT** | ~600ms block interval with deterministic finality |
| **Fee sponsorship** | Third party can pay gas on behalf of the sender |
| **Batch calls** | Multiple transfers in a single atomic transaction |

### 2.3 Key Takeaways

For `reth`, the most valuable lesson is not to copy Sui directly. It is:

1. First build a **Tempo-style payment lane**
2. Then add **payment-specific transaction semantics** (2D nonce, validity windows)
3. Then add a **constrained fast path** (Sui-inspired, but only for protocol-enforced payment operations)
4. Finally add **deterministic BFT finality**

---

## 3. Problem Analysis

### 3.1 Four Independent Bottlenecks

| # | Bottleneck | Description |
|---|------------|-------------|
| 1 | **Blockspace contention** | Payments are squeezed out by DeFi, MEV, and complex contract calls during congestion |
| 2 | **Hot account serialization** | A single sender is limited by sequential nonces |
| 3 | **Settlement latency** | Finality still requires waiting for multiple confirmations |
| 4 | **Execution-path overhead** | Even simple payments go through the full generic EVM execution path |

### 3.2 Constraints in the Current Repo

**Txpool and payload builder have no payment concept**

- Txpool ordering uses `CoinbaseTipOrdering` (`crates/transaction-pool/src/ordering.rs`)
- The pending, queued, basefee, and blob subpools have no payment-aware classification
- The default payload builder only understands one global gas budget (`crates/ethereum/payload/src/lib.rs`)

Under heavy load, payment transactions compete with ordinary contract calls for the same block budget.

**Finality comes from external forkchoice, not an internal BFT layer**

- An external CL provides `newPayload` and `forkchoiceUpdated`
- The EL only executes, validates, and tracks safe/finalized blocks

Changing only the txpool or payload builder cannot provide deterministic finality.

**Arbitrary ERC-20 transfers cannot be treated as a Sui-style fast path**

In the EVM, an ERC-20 transfer still touches shared contract state. Fast path logic must be restricted to a constrained payment transaction type whose read/write set can be determined statically.

---

## 4. Overall Design

Two tracks progress in parallel across five phases:

```text
Main track:        Phase 1 (Soft Lane) -> Phase 2 (Consensus-Enforced Lane) -> Phase 5 (BFT Finality)
Enhancement track: Phase 3 (2D Nonce)  -> Phase 4 (Fast Path)
```

**Key design decisions**:

- Build the payment lane first — least invasive, highest immediate payoff
- Restrict fast path to a constrained payment path, not a generic EVM fast path
- Start BFT finality as an external sidecar first, then consider in-process integration later

---

## 5. Phase 1: Soft Payment Lane

### Goal

Without changing the block/header consensus format, make the builder policy payment-aware.

### Design

**Transaction classifier**: Stateless classification based only on transaction payload. A transaction is classified as `Payment` only when both conditions are met:

1. The `to` address matches a known payment target (TIP-20 prefix `0x20c0` or configured allowlist)
2. The calldata is a supported payment selector (`transfer(address,uint256)`, exactly 68 bytes)

This conservative AND-logic approach favors false negatives over cross-client inconsistency.

**Note**: The original draft proposed recognizing both `transfer` and `transferFrom`. The final design narrows this to `transfer` only, because `transferFrom` involves allowance state and should be deferred to a protocol-enforced payment path.

**Payment-aware ordering**: Payment transactions receive a configurable priority boost (default 10x). Tip value still dominates — a very high-tip general transaction should not be unconditionally displaced by a low-tip payment transaction. Payment lane is only a tie-breaker at equal effective tip.

**Dual-budget payload builder**: Implements the `PayloadBuilder` trait with two gas budgets:

- General transactions are capped at `general_gas_limit` (default: 70% of block gas)
- Payment transactions can consume the reserved budget (default: 30% of block gas)
- Unused payment capacity is not reassigned to general transactions

### Limits

- Only proposer-local policy; no protocol guarantee
- Finality is unchanged; only throughput and inclusion latency improve

---

## 6. Phase 2: Consensus-Enforced Lane

### Goal

Upgrade the payment lane from builder policy to consensus rule.

### Design

Wrap the existing consensus implementation and inject lane gas validation in `validate_block_post_execution`:

- Standard Ethereum rule: `gas_used <= gas_limit` (delegated to inner validator)
- New lane rule: cumulative *executed* gas of non-payment transactions must remain `<= general_gas_limit`

The accounting uses actual gas consumed from receipts, not declared gas limits. This prevents over-counting from reverted transactions or transactions that do not consume all declared gas.

### Comparison with Tempo

| Tempo | This design |
|-------|-------------|
| Headers carry explicit `general_gas_limit` and `shared_gas_limit` | Header format is unchanged; `general_gas_limit` is derived from configuration |
| Sub-block / validator-subblock structure | Not needed in the initial version |
| Classification must be deterministic | Yes — purely stateless classifier |

### Future evolution

- Encode `general_gas_limit` explicitly in headers by extending engine primitives
- Add a Tempo-style shared lane for overflow capacity

---

## 7. Phase 3: 2D Nonce System

### Goal

Solve the hot-sender serialization problem.

### Design

**2D nonce model**: Each transaction carries a `(nonce_key, nonce_value)` pair:

- Key 0 is the standard protocol nonce (sequential Ethereum behavior)
- Keys 1+ are independent parallel sequences, allowing concurrent transaction submission from the same sender

**Validity window**: `valid_after` / `valid_before` timestamps provide automatic expiration. Unused nonce keys do not need to remain permanently live, reducing state bloat.

**Sender state tracking**: Per-sender state tracks current value of every nonce key and enforces:

- No replays (`NonceTooLow`)
- No gaps (`NonceTooHigh`)
- Bounded key count (default 256 keys per sender)

### Comparison with Tempo

| Tempo | This design |
|-------|-------------|
| `nonce_key` (u16) + `nonce_value` (u64) | `key` (u64) + `value` (u64) |
| Protocol nonce = key 0 | Yes |
| `valid_after` / `valid_before` | Yes |
| Max key count | Yes, default 256 |
| Memo field | Yes, `Option<B256>` |

---

## 8. Phase 4: Constrained Fast Path

### Goal

Enable parallel execution of non-conflicting payment operations.

### Design

Borrow the conflict-detection intuition from Sui, but **only for payment operations whose read/write sets can be derived statically**.

A payment touches exactly two state slots: `(asset, sender)` and `(asset, receiver)`. Two payments conflict if they share any element in their read/write sets.

**What is allowed**:

- Fixed-format transfers (`PaymentIntent`)
- Fixed-format batch transfers (`BatchPaymentIntent`)
- Static read/write set derivation from transaction fields
- Greedy graph coloring for non-conflicting batch partitioning

**What is explicitly not allowed**:

- Arbitrary ERC-20 `transfer` calls on the fast path
- Runtime analysis of arbitrary EVM bytecode read/write sets
- Claiming Sui-style fast path without a protocol-enforced payment contract

---

## 9. Phase 5: BFT Finality

### Goal

Provide deterministic sub-second settlement.

### Design

**Architecture**: A BFT consensus sidecar drives the reth EL via the Engine API:

```text
BFT Consensus Sidecar -> Engine API -> Reth EL (newPayload, forkchoiceUpdated)
```

**Finality model**: Simplex-style BFT with ~600ms block interval:

- Safety as long as Byzantine validators < 1/3
- Liveness as long as honest validators >= 2/3
- Supermajority finalization: `ceil(validators * 2 / 3)` votes
- No reorgs after finalization

**Preconfirmation**: An optional fast signal before full block finality:

- Proposer signs acknowledgment that a transaction has been accepted
- Finalized blocks remain the source of strong settlement
- Useful for API, MCP, and machine-payment workflows

### Implementation approach

Start with an external BFT sidecar (Option A) to minimize intrusion into the codebase, then evaluate embedding BFT into the node launch path (Option B) later.

---

## 10. Routes Explicitly Not Recommended

### 10.1 Changing only the txpool and claiming a payment lane guarantee

That is only local policy, not a protocol guarantee. Phase 1 alone is not enough; Phase 2 is required to make it a consensus rule.

### 10.2 Forcing Sui-style fast path onto arbitrary ERC-20 or arbitrary contract calls

The EVM shared-state model makes this a severe correctness risk. Fast path must stay constrained to a protocol-defined payment path.

### 10.3 Solving finality only by reducing block time

Faster block production means faster visibility, not stronger settlement. Deterministic BFT finality is required for irreversible settlement.

---

## 11. Success Criteria

| Metric | Goal |
|--------|------|
| Payment TPS under mixed load | Meaningfully higher than baseline |
| Payment p99 inclusion latency during DeFi congestion | Stable within 1-2 blocks |
| Concurrency for a single hot sender | Roughly linear in the number of nonce keys |
| Finalized payment latency | <= 1 second after BFT rollout |
| Reorgs after finalization | 0 |

---

## 12. Future Integration Points in the Repo

### Minimally invasive (Phase 1)

- `crates/transaction-pool/*` — payment classifier, lane metadata, payment-aware ordering
- `crates/ethereum/payload/*` — dual-budget payload builder
- `crates/ethereum/node/src/node.rs` — inject custom pool builder / payload builder

### Protocolized (Phase 2)

- `crates/ethereum/engine-primitives/*` — new payload / header types
- `crates/consensus/*` + `crates/ethereum/consensus/*` — lane validity rule

### Finality (Phase 5)

- `crates/engine/primitives/src/forkchoice.rs` — faster finalized/safe advancement
- `crates/engine/tree/src/engine.rs` — connect to new consensus event source
- `crates/node/builder/src/launch/engine.rs` — integrate BFT sidecar or in-process consensus

### Observability

- `crates/rpc/rpc/src/reth.rs` or `examples/node-custom-rpc` — payment-specific RPC endpoints
- Metrics: payment/general backlog, inclusion p50/p95/p99, finalized latency, lane utilization
