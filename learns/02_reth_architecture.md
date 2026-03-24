# 02: Reth Architecture Overview

Reth is designed from first principles with three primary goals: **Modularity**, **Performance**, and **Maintainability**.

Instead of a monolithic application, Reth is built as a collection of libraries (crates) that can be used independently. Under the hood, it uses an innovative approach to sync and process data.

## 1. Modular Design

If you look at typical blockchain node architectures, everything is tightly coupled. Reth breaks things apart:
- The database is abstracted via traits.
- The Execution Engine (the EVM) is abstracted out entirely (Reth uses a separate crate called `revm`).
- The P2P network, transaction pool, and RPC servers can be swapped or customized.

This means if you want to build a custom rollup or a Layer 2, you can import only the specific Reth crates you need without importing the entire node software.

## 2. Staged Sync (The Core Engine)

The most unique architectural decision in Reth is **Staged Sync**. 

Traditionally, when a node syncs from scratch, it downloads a block, executes all transactions in it, updates the state database, downloads the next block, executes, etc. This random read/write pattern on the database is a massive performance bottleneck.

Reth solves this by separating the syncing process into distinct **Stages** that process data in bulk:

1. **Headers Stage**: Download all block headers from peers.
2. **Bodies Stage**: Download all block bodies (transactions) that match those headers.
3. **Senders Stage**: Cryptographically recover the sender addresses for all transactions (CPU intensive, done in parallel).
4. **Execution Stage**: Execute all transactions sequentially through the EVM (`revm`). Instead of writing to the database immediately, changes are kept in memory and flushed in optimal batches.
5. **Hashing & Merkle Stages**: Compute the cryptographic State Root required by the protocol.

**Why does this matter?**
Doing one thing at a time across millions of blocks takes advantage of CPU caching, parallel processing, and sequential database writes (which are vastly faster than random writes).

## 3. The Database Layer (MDBX)

Ethereum state is massive (hundreds of gigabytes). How it is stored defines the node's performance. Reth uses **libmdbx**, a wildly fast, memory-mapped database.
- It operates as a Key-Value store.
- Reth groups data logically through heavily optimized tables (e.g., `Headers`, `Transactions`, `AccountChangeSet`).
- Because of Staged Sync, Reth can write to these tables sequentially, avoiding database fragmentation.

## 4. The Engine API

Because Reth is an Execution Client, it communicates with the Consensus Client via the **Engine API**. 
When the network produces a new block, the Consensus Client sends a JSON-RPC call over the Engine API `engine_newPayloadV1`. Reth executes it, guarantees it is valid, and reports back.

## Summary

Whenever you look at Reth's code, remember the architecture:
- **Data flows in** via P2P or Engine API.
- **It goes through Staged Sync** (Bulk Processing).
- **Execution** happens completely in `revm`.
- **State** persists in an MDBX database. 

In `03_core_modules_and_crates.md`, we will map these architectural concepts to the actual directories in the repository.
