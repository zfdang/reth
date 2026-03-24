//! # Constrained Fast Path (Phase 4)
//!
//! Implements a protocol-enforced payment fast path inspired by Sui's fast path,
//! but constrained to only handle transactions whose read/write sets can be
//! statically determined from the transaction payload.
//!
//! ## Design Constraints
//!
//! - Only supports protocol-enforced payment operations (not arbitrary EVM calls)
//! - Read/write sets derived statically from transaction fields
//! - Non-conflicting payments can execute in parallel
//! - Conflicting payments are serialized by conflict key
//!
//! ## Conflict Model
//!
//! A payment touches the read/write set: `{(asset, sender), (asset, receiver)}`.
//! Two payments conflict if they share any element in their read/write sets.

use alloy_primitives::{Address, U256};
use std::collections::{HashMap, HashSet};

/// A constrained payment intent that can be statically analyzed.
///
/// Unlike arbitrary EVM transactions, a `PaymentIntent` has a fully deterministic
/// read/write set derived from its fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaymentIntent {
    /// Sender of the payment.
    pub from: Address,
    /// Receiver of the payment.
    pub to: Address,
    /// Asset contract address (or `Address::ZERO` for native ETH).
    pub asset: Address,
    /// Amount to transfer.
    pub amount: U256,
    /// Nonce key for parallel execution.
    pub nonce_key: u64,
    /// Nonce value.
    pub nonce_value: u64,
}

/// A conflict key identifies a state slot that a payment touches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConflictKey {
    /// The asset contract address.
    pub asset: Address,
    /// The account whose balance is affected.
    pub account: Address,
}

impl PaymentIntent {
    /// Returns the read/write set for this payment intent.
    ///
    /// A payment touches exactly two state slots:
    /// - `(asset, sender)` — balance decreases
    /// - `(asset, receiver)` — balance increases
    pub const fn conflict_keys(&self) -> [ConflictKey; 2] {
        [
            ConflictKey { asset: self.asset, account: self.from },
            ConflictKey { asset: self.asset, account: self.to },
        ]
    }

    /// Returns whether this payment intent is valid on the surface.
    ///
    /// Basic checks: non-zero amount, sender != receiver, etc.
    pub fn is_valid(&self) -> bool {
        self.from != self.to && self.amount > U256::ZERO
    }
}

/// Batch payment intent for atomic multi-transfer operations.
#[derive(Debug, Clone)]
pub struct BatchPaymentIntent {
    /// The sender (payer) for all transfers in this batch.
    pub from: Address,
    /// Individual transfer targets and amounts.
    pub transfers: Vec<TransferTarget>,
    /// Asset contract address (single asset per batch).
    pub asset: Address,
    /// Nonce key for parallel execution.
    pub nonce_key: u64,
    /// Nonce value.
    pub nonce_value: u64,
}

/// A single transfer target in a batch payment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransferTarget {
    /// Recipient address.
    pub to: Address,
    /// Amount to transfer.
    pub amount: U256,
}

impl BatchPaymentIntent {
    /// Returns all conflict keys for this batch payment.
    pub fn conflict_keys(&self) -> Vec<ConflictKey> {
        let mut keys = Vec::with_capacity(1 + self.transfers.len());
        // Sender's balance is always touched
        keys.push(ConflictKey { asset: self.asset, account: self.from });
        // Each receiver's balance is touched
        for transfer in &self.transfers {
            keys.push(ConflictKey { asset: self.asset, account: transfer.to });
        }
        keys
    }

    /// Returns true if the batch is valid: non-empty, non-self-transfers, non-zero amounts.
    pub fn is_valid(&self) -> bool {
        !self.transfers.is_empty()
            && self.transfers.iter().all(|t| t.to != self.from && t.amount > U256::ZERO)
    }
}

/// Detects conflicts between payment intents and groups them into
/// non-conflicting parallel batches.
#[derive(Debug, Default)]
pub struct ConflictDetector {
    /// Map from conflict key to the set of payment indices that touch it.
    key_to_payments: HashMap<ConflictKey, HashSet<usize>>,
}

impl ConflictDetector {
    /// Creates a new empty [`ConflictDetector`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Partitions a set of payment intents into non-conflicting parallel batches.
    ///
    /// Returns a vector of batches, where each batch contains indices of payments
    /// that can execute in parallel without conflicts.
    pub fn partition(&mut self, payments: &[PaymentIntent]) -> Vec<Vec<usize>> {
        self.key_to_payments.clear();

        // Build conflict index
        for (idx, payment) in payments.iter().enumerate() {
            for key in payment.conflict_keys() {
                self.key_to_payments.entry(key).or_default().insert(idx);
            }
        }

        // Build conflict graph (adjacency list)
        let n = payments.len();
        let mut conflicts: Vec<HashSet<usize>> = vec![HashSet::new(); n];
        for group in self.key_to_payments.values() {
            let indices: Vec<usize> = group.iter().copied().collect();
            for i in 0..indices.len() {
                for j in (i + 1)..indices.len() {
                    conflicts[indices[i]].insert(indices[j]);
                    conflicts[indices[j]].insert(indices[i]);
                }
            }
        }

        // Greedy graph coloring for batch assignment
        let mut colors: Vec<Option<usize>> = vec![None; n];
        let mut batches: Vec<Vec<usize>> = Vec::new();

        for i in 0..n {
            // Find the first color not used by neighbors
            let used_colors: HashSet<usize> =
                conflicts[i].iter().filter_map(|&j| colors[j]).collect();

            let color = (0..=batches.len()).find(|c| !used_colors.contains(c)).unwrap_or(0);

            colors[i] = Some(color);
            if color >= batches.len() {
                batches.push(Vec::new());
            }
            batches[color].push(i);
        }

        batches
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn make_payment(from: Address, to: Address) -> PaymentIntent {
        PaymentIntent {
            from,
            to,
            asset: Address::ZERO,
            amount: U256::from(100),
            nonce_key: 0,
            nonce_value: 0,
        }
    }

    #[test]
    fn test_non_conflicting_payments_parallel() {
        let mut detector = ConflictDetector::new();

        let alice = address!("0x0000000000000000000000000000000000000001");
        let bob = address!("0x0000000000000000000000000000000000000002");
        let charlie = address!("0x0000000000000000000000000000000000000003");
        let dave = address!("0x0000000000000000000000000000000000000004");

        let payments = vec![
            make_payment(alice, bob),    // touches alice, bob
            make_payment(charlie, dave), // touches charlie, dave (no conflict)
        ];

        let batches = detector.partition(&payments);
        // Both should be in the same batch (no conflict)
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
    }

    #[test]
    fn test_conflicting_payments_serialized() {
        let mut detector = ConflictDetector::new();

        let alice = address!("0x0000000000000000000000000000000000000001");
        let bob = address!("0x0000000000000000000000000000000000000002");
        let charlie = address!("0x0000000000000000000000000000000000000003");

        let payments = vec![
            make_payment(alice, bob),     // touches alice, bob
            make_payment(alice, charlie), // touches alice, charlie (conflict on alice)
        ];

        let batches = detector.partition(&payments);
        // Should be in separate batches due to conflict on alice
        assert_eq!(batches.len(), 2);
    }

    #[test]
    fn test_payment_intent_validity() {
        let alice = address!("0x0000000000000000000000000000000000000001");
        let bob = address!("0x0000000000000000000000000000000000000002");

        let valid = make_payment(alice, bob);
        assert!(valid.is_valid());

        let self_transfer = make_payment(alice, alice);
        assert!(!self_transfer.is_valid());

        let zero_amount = PaymentIntent {
            from: alice,
            to: bob,
            asset: Address::ZERO,
            amount: U256::ZERO,
            nonce_key: 0,
            nonce_value: 0,
        };
        assert!(!zero_amount.is_valid());
    }

    #[test]
    fn test_batch_payment_conflict_keys() {
        let alice = address!("0x0000000000000000000000000000000000000001");
        let bob = address!("0x0000000000000000000000000000000000000002");
        let charlie = address!("0x0000000000000000000000000000000000000003");

        let batch = BatchPaymentIntent {
            from: alice,
            transfers: vec![
                TransferTarget { to: bob, amount: U256::from(50) },
                TransferTarget { to: charlie, amount: U256::from(50) },
            ],
            asset: Address::ZERO,
            nonce_key: 0,
            nonce_value: 0,
        };

        let keys = batch.conflict_keys();
        assert_eq!(keys.len(), 3); // alice, bob, charlie
        assert!(batch.is_valid());
    }

    #[test]
    fn test_three_way_conflict() {
        let mut detector = ConflictDetector::new();

        let alice = address!("0x0000000000000000000000000000000000000001");
        let bob = address!("0x0000000000000000000000000000000000000002");
        let charlie = address!("0x0000000000000000000000000000000000000003");

        // A->B, B->C, C->A : chain of conflicts
        let payments = vec![
            make_payment(alice, bob),
            make_payment(bob, charlie),
            make_payment(charlie, alice),
        ];

        let batches = detector.partition(&payments);
        // All three conflict transitively, need 3 batches
        assert!(batches.len() >= 2);
        // Every payment is assigned exactly once
        let total: usize = batches.iter().map(|b| b.len()).sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn test_empty_payments() {
        let mut detector = ConflictDetector::new();
        let batches = detector.partition(&[]);
        assert!(batches.is_empty());
    }

    #[test]
    fn test_batch_payment_validity_edge_cases() {
        let alice = address!("0x0000000000000000000000000000000000000001");
        let bob = address!("0x0000000000000000000000000000000000000002");

        // Empty batch is invalid.
        let empty_batch = BatchPaymentIntent {
            from: alice,
            transfers: vec![],
            asset: Address::ZERO,
            nonce_key: 0,
            nonce_value: 0,
        };
        assert!(!empty_batch.is_valid());

        // Self-transfer is invalid.
        let self_batch = BatchPaymentIntent {
            from: alice,
            transfers: vec![TransferTarget { to: alice, amount: U256::from(100) }],
            asset: Address::ZERO,
            nonce_key: 0,
            nonce_value: 0,
        };
        assert!(!self_batch.is_valid());

        // Zero amount is invalid.
        let zero_batch = BatchPaymentIntent {
            from: alice,
            transfers: vec![TransferTarget { to: bob, amount: U256::ZERO }],
            asset: Address::ZERO,
            nonce_key: 0,
            nonce_value: 0,
        };
        assert!(!zero_batch.is_valid());
    }

    #[test]
    fn test_multi_asset_conflicts_are_independent() {
        let mut detector = ConflictDetector::new();

        let alice = address!("0x0000000000000000000000000000000000000001");
        let bob = address!("0x0000000000000000000000000000000000000002");
        let asset_a = address!("0x00000000000000000000000000000000000000aa");
        let asset_b = address!("0x00000000000000000000000000000000000000bb");

        // Same sender+receiver but different assets: no conflict.
        let payments = vec![
            PaymentIntent {
                from: alice,
                to: bob,
                asset: asset_a,
                amount: U256::from(100),
                nonce_key: 0,
                nonce_value: 0,
            },
            PaymentIntent {
                from: alice,
                to: bob,
                asset: asset_b,
                amount: U256::from(200),
                nonce_key: 1,
                nonce_value: 0,
            },
        ];

        let batches = detector.partition(&payments);
        assert_eq!(batches.len(), 1);
    }

    #[test]
    fn test_single_payment_single_batch() {
        let mut detector = ConflictDetector::new();
        let alice = address!("0x0000000000000000000000000000000000000001");
        let bob = address!("0x0000000000000000000000000000000000000002");

        let payments = vec![make_payment(alice, bob)];
        let batches = detector.partition(&payments);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0], vec![0]);
    }
}
