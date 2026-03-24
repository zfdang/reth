# 05: The Lifecycle of a Transaction

To truly understand an Execution Engine, let's follow the journey of a single, simple transaction (e.g., Alice sending 1 ETH to Bob) from discovery to finality inside Reth.

## Step 1: Entry via P2P (Network) or RPC
A transaction can enter the node in two ways:
- **RPC**: You submit it directly locally via MetaMask (e.g., `eth_sendRawTransaction`).
- **P2P**: Reth connects to a peer node (`crates/network`) that broadcasts transactions its pool just received.

## Step 2: The Transaction Pool (`transaction-pool`)
The transaction doesn't execute immediately. It enters the **TxPool**. Before entering, the pool performs strict validation logic:
1. **Signature Verification**: Does the signature match the sender's address?
2. **Nonce Checking**: Each Ethereum account has a sequential nonce. Does this transaction's nonce match `sender.nonce`? (Note: Payment Lane's Custom 2D Nonce modifies this rule).
3. **Balance Checking**: Does Alice actually have enough ETH to pay the `gas_limit * max_fee_per_gas`?

If it passes, it sits in an in-memory queue waiting for a block.

## Step 3: Payload Building (`payload-builder`)
Every 12 seconds, a random validator is chosen on the Ethereum network to propose the next block. 
If our node is the proposer, the **Consensus Layer (CL)** tells Reth via the Engine API: *"Build me a new block payload!"*

1. The Payload Builder (`crates/payload`) grabs transactions from the TxPool.
2. It sorts them (typically by highest fee priority).
3. It packages them into an executable format and returns them to the CL.

*If our node is NOT the proposer, we skip this step and just wait for the winning node to broadcast the block.*

## Step 4: Staged Sync Execution (`stages` -> `evm`)
Once a new block is broadcast to the network and agreed upon by the CL, the CL tells Reth: *"This is the new payload, execute it."*

1. The **Bodies Stage** receives the block and transactions.
2. The **Execution Stage** takes over. For each transaction in the block, Reth hands it to `revm` (the execution engine).
    - `revm` loads Alice's balance from the database provider.
    - It subtracts 1 ETH + Fees from Alice.
    - It loads Bob's balance and adds 1 ETH.
    - If it was a smart contract instead of a simple transfer, `revm` would load the contract bytecodes and step through operations.
3. Once the entire block is executed, all of the state updates (Alice and Bob's new balances) are accumulated into an in-memory "Post-State" or `AccountChangeSet`.

## Step 5: Database Commit (`db`)
After execution completes without errors, Reth persists the changes to MDBX (`crates/db`).
- It saves the raw transaction bytes.
- It saves the execution receipts (proof the transaction succeeded).
- It updates the accounts tables.

Finally, Reth computes the new **State Root** through the Hashing/Merkle stages. If this hash matches the hashing root in the block header we received from the CL, the block is officially valid.

## Understanding Custom Paths (e.g., Payment Lane)
Normally, all transactions compete for the same Payload space and go through the exact same `revm` path. 

If you introduce custom solutions (like the `crates/payment-lane`), what you are doing is:
1. Creating a custom `TransactionPool` rule to prioritize certain prefixed transactions.
2. Creating a dual-budget `PayloadBuilder` to reserve 30% of standard block gas specifically for those payments.
3. Slipping in conflict-detection logic (Sui Fast Path style) to process non-conflicting payments concurrently *before* handing the rest over to typical EVM execution.
