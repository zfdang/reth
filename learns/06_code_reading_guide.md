# 06: How to Read the Code (A Beginner's Guide)

Staring at a massive, modular Rust repository is overwhelming. The best way to learn Reth is not to read top-to-bottom, but to trace execution paths or write tests.

Here is a recommended path for someone with basic Rust knowledge.

## Step 1: Follow the Node Boot Process
Open `bin/reth/src/main.rs`.
- Read how CLI arguments are parsed using `clap` (look for `NodeCommand`).
- Follow the logic into `crates/node/builder/src/`. This is the core "Node Builder" pattern.
- Notice how the database is opened, network P2P tasks are spawned, and the RPC servers are initialized in the background.

## Step 2: Look at Data Definitions
Open `crates/primitives/src/`.
- Look at `transaction/mod.rs` and `block/mod.rs`.
- Read how a Transaction is defined as a struct. Look at the traits it implements. Once you know what a Transaction looks like in memory, the rest of the node's code makes more sense.

## Step 3: Explore the TxPool
Navigate to `crates/transaction-pool/src/`.
- Look at `pool.rs`. Find the `add_transaction` method.
- Look at the `Validation` traits. Trace how a transaction is evaluated for base fees and signatures before it is put into the in-memory maps.

## Step 4: Write or Modify a Test
The absolute best way to learn Rust code is to mess heavily with the tests. You don't have to compile the whole node (which takes a long time). You can run tests for specific modules instantly.

Open a terminal at the project root and run:
```bash
cargo test -p reth-transaction-pool
```
Then, go into one of the `tests/` files in that crate. Add an `assert!(false);` to ensure your test fails. Look at the data structures the test constructs (like a mock transaction or mock provider), print them out using `println!("{:#?}", tx);`. This is infinitely better than reading passively.

## Step 5: Study a Custom Crate (E.g., Payment Lane)
Because Reth is modular, reviewing an extension crate is an incredibly good way to learn its APIs.

Look at `crates/payment-lane/src/`.
- Look at `classifier.rs`. It imports `reth_primitives::Transaction` and reads its properties.
- Look at `payload.rs`. It implements `PayloadBuilder` trait from Reth. This shows how you hook into the core node infrastructure.
- Run its specific tests: `cargo test -p reth-payment-lane`.

## Final Tips
1. **Use `rust-analyzer`**: Never read Rust without an IDE integration. Follow "Go to Definition" relentlessly. Whenever you see a Trait, look for "Implementations" to see how it's actually used.
2. **Ignore the Merkle Tree**: `crates/trie` contains extreme optimizations for Merkle Patricia Tries. From an architectural perspective, you don't need to understand *how* it's stored mathematically, only *that* it's stored.
3. **Rust Macros**: Don't let macros like `impl_` or attribute macros scare you. If you don't know what they do, read their generated documentation or treat them as black boxes that generate boilerplate.

Welcome to Reth! Happy hacking!
