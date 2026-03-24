# 01: Blockchain and Ethereum Basics

Welcome to the journey of learning `reth`. Before diving into the codebase, it is crucial to understand the fundamental concepts of Blockchain, Ethereum, and the specific role of a node.

## What is a Blockchain?

At its core, a blockchain is a distributed ledger. Imagine a global database where anyone can participate, but no single entity is in control. 
- **Blocks**: Data is stored in batches called blocks.
- **Chain**: Each block cryptographically references the previous one, forming an unbreakable chain. If you alter past data, all subsequent blocks become invalid.
- **Consensus**: The network nodes agree on the true state of the ledger using a consensus mechanism (e.g., Proof of Stake).

## What is Ethereum?

Ethereum is more than just a ledger of balances (like Bitcoin). It is a "World Computer."
- **State**: Ethereum maintains a global state, which includes account balances and smart contract data.
- **Smart Contracts**: Programs deployed to the blockchain. They execute deterministically on the Ethereum Virtual Machine (EVM).
- **Transactions**: User-initiated actions that change the state. A transaction could be transferring Ether (ETH) or calling a function on a smart contract.
- **Gas**: Every computation in the EVM costs "Gas," which prevents infinite loops and allocates network resources efficiently.

## Nodes and Clients

To interact with or help secure Ethereum, you run a **Node**. A node runs software called a **Client**. After "The Merge" (Ethereum's transition to Proof of Stake), an Ethereum node requires two clients running in tandem:

1. **Consensus Client (CL)**: Handles Proof of Stake, validator duties, and agrees on the sequence of blocks. (Examples: Lighthouse, Prysm).
2. **Execution Client (EL)**: Handles smart contract execution, maintains the Ethereum state tree, validates transaction logic, and exposes the JSON-RPC API to users. 

**Reth is an Execution Client.**

## Where Does Reth Fit In?

Reth (Rust Ethereum) does *not* do Proof of Stake. Instead, it expects a Consensus Client to tell it: *"Here is the latest block, execute it and tell me if it is valid."*

Reth's responsibilities:
1. **P2P Networking**: Connect with other execution clients to broadcast and receive pending transactions.
2. **Transaction Pool (TxPool)**: Keep track of pending transactions before they are included in a block.
3. **Execution Engine (REVM)**: Run the EVM to process transactions and compute the new state.
4. **State Storage**: Store historical and current state data efficiently on a hard drive using a database (MDBX).
5. **RPC API**: Serve user requests (e.g., MetaMask asking for a balance or submitting a transaction).

## Summary

Keep this mental model: **Reth is the engine that actually computes and stores Ethereum data.** It listens to a Consensus Client to know *which* data is finalized. With this foundation, you are ready to explore how Reth is architected in `02_reth_architecture.md`.
