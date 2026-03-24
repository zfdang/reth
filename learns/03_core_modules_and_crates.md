# 03: Core Modules and Crates Guide

Reth is a Rust workspace composed of dozens of `crates` (Rust packages). Understanding this structure is the fastest way to navigate the code. All core logic lives inside the `crates/` directory.

Here is a functional breakdown of the most critical crates.

## Fundamental Data Structures
- **`crates/primitives`**: The absolute foundation. It defines `Block`, `Transaction`, `Header`, `Receipt`, and aliases for common Ethereum hashes and addresses. If an object is universally used across the node, it lives here.

## Storage and Database
- **`crates/db`**: Contains the abstractions and the actual `libmdbx` implementation. It defines schemas, tables, and cursors.
- **`crates/provider`**: This is how the rest of the node queries data. Instead of interacting with the DB directly, higher-level modules call the Provider API (e.g., `provider.block_by_number(100)`). It orchestrates reading from the database and the in-memory cache.

## Execution and Sync
- **`crates/evm` & `crates/revm`**: Integration with `revm` (Rust Ethereum Virtual Machine). This is where EVM opcodes are processed.
- **`crates/stages`**: Implementation of the Staged Sync pipeline discussed in Chapter 02. Each stage (Headers, Bodies, Execution) is an isolated module here.
- **`crates/payload` & `crates/payload-builder`**: Responsible for creating *new* blocks. When a validator is selected to propose the next block, the payload builder pulls transactions from the pool and packs them efficiently.

## Network and P2P
- **`crates/network`**: Implements the `devp2p` Ethereum networking protocol. It handles peering, discovery (discv4), and broadcasting transactions and blocks to other nodes globally.

## User Interface (API)
- **`crates/rpc`**: Implements the JSON-RPC server (e.g., `eth_call`, `eth_sendRawTransaction`). This module translates HTTP/WebSocket requests into internal node queries.
- **`crates/transaction-pool`**: An in-memory cache of valid transactions submitted by users that are waiting to be mined into a block.

## Node Wiring
- **`crates/node` & `crates/node-builder`**: The glue that ties all the crates together into an actual running application.
- **`bin/reth`**: The executable entry point (where `main()` lives). It handles CLI arguments, logging, and spinning up the node builder.

## Specialized / Custom Logic (e.g., Payment Lane)
- **`crates/payment-lane`**: A specialized module that introduces priority blockspace for high-speed payment transactions. This demonstrates Reth's modularity—custom logic like 2D nonces, fast-path conflict detection, and separate payload packaging can be implemented as a distinct crate and injected into the Node Builder.

## How to navigate?
If you want to know how a block is executed, start at `crates/stages` and `crates/evm`. 
If you want to know how user balances are fetched, look at `crates/rpc` tracing down to `crates/provider`.
If you want to see how the software boots up, look at `bin/reth/src/main.rs`.
