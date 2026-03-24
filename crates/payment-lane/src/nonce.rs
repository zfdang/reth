//! # Payment Transaction Semantics — 2D Nonce System (Phase 3)
//!
//! Implements parallel nonce support for payment transactions, solving the
//! hot sender serialization problem where a single high-frequency payment
//! account blocks itself with sequential nonces.
//!
//! ## Nonce Model
//!
//! - **Key 0** (protocol nonce): Standard sequential Ethereum nonce.
//! - **Keys 1+** (user nonces): Independent nonce sequences that allow concurrent transaction
//!   submission from the same sender.
//!
//! ## Expiring Nonces
//!
//! Transactions can specify a `valid_before` timestamp. If the current block timestamp
//! exceeds `valid_before`, the transaction is invalid. This provides automatic replay
//! protection without permanent state bloat from unused nonce keys.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A 2D nonce identifier: (`nonce_key`, `nonce_value`).
///
/// - `nonce_key = 0`: protocol nonce (sequential, standard Ethereum behavior)
/// - `nonce_key > 0`: user nonce key (independent parallel sequence)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NonceKey {
    /// The nonce key (0 = protocol, 1+ = user-defined parallel lanes).
    pub key: u64,
    /// The nonce value within this key's sequence.
    pub value: u64,
}

impl NonceKey {
    /// Creates a protocol nonce (key 0).
    pub const fn protocol(value: u64) -> Self {
        Self { key: 0, value }
    }

    /// Creates a user nonce with the given key and value.
    pub const fn user(key: u64, value: u64) -> Self {
        Self { key, value }
    }

    /// Returns true if this is the protocol nonce (key 0).
    pub const fn is_protocol(&self) -> bool {
        self.key == 0
    }

    /// Returns the next nonce in this key's sequence.
    pub const fn next(&self) -> Self {
        Self { key: self.key, value: self.value + 1 }
    }
}

/// Validity window for a payment transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidityWindow {
    /// Earliest timestamp (inclusive) at which the tx is valid. 0 means no lower bound.
    pub valid_after: u64,
    /// Latest timestamp (exclusive) before which the tx is valid. 0 means no upper bound.
    pub valid_before: u64,
}

impl ValidityWindow {
    /// Creates an unbounded validity window (always valid).
    pub const fn unbounded() -> Self {
        Self { valid_after: 0, valid_before: 0 }
    }

    /// Creates a validity window with only an upper bound (expiring nonce).
    pub const fn expiring(valid_before: u64) -> Self {
        Self { valid_after: 0, valid_before }
    }

    /// Creates a validity window for scheduled transactions.
    pub const fn scheduled(valid_after: u64, valid_before: u64) -> Self {
        Self { valid_after, valid_before }
    }

    /// Checks whether the transaction is valid at the given timestamp.
    pub const fn is_valid_at(&self, timestamp: u64) -> bool {
        if self.valid_after > 0 && timestamp < self.valid_after {
            return false;
        }
        if self.valid_before > 0 && timestamp >= self.valid_before {
            return false;
        }
        true
    }
}

/// Payment transaction metadata that extends a standard transaction with
/// 2D nonce and validity window semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentTxMeta {
    /// The 2D nonce key for this transaction.
    pub nonce_key: NonceKey,
    /// Validity window for the transaction.
    pub validity: ValidityWindow,
    /// Optional memo (32 bytes) for payment reference/invoice ID.
    pub memo: Option<B256>,
}

impl PaymentTxMeta {
    /// Creates metadata with a protocol nonce and no expiry.
    pub const fn standard(nonce: u64) -> Self {
        Self {
            nonce_key: NonceKey::protocol(nonce),
            validity: ValidityWindow::unbounded(),
            memo: None,
        }
    }

    /// Creates metadata with a user nonce key (for parallel execution).
    pub const fn parallel(key: u64, value: u64) -> Self {
        Self {
            nonce_key: NonceKey::user(key, value),
            validity: ValidityWindow::unbounded(),
            memo: None,
        }
    }

    /// Creates metadata with an expiring nonce.
    pub const fn expiring(key: u64, value: u64, valid_before: u64) -> Self {
        Self {
            nonce_key: NonceKey::user(key, value),
            validity: ValidityWindow::expiring(valid_before),
            memo: None,
        }
    }

    /// Attaches a memo to this payment metadata.
    pub const fn with_memo(mut self, memo: B256) -> Self {
        self.memo = Some(memo);
        self
    }
}

/// Tracks nonce state for a single sender across all nonce keys.
///
/// This enables validation of parallel nonce transactions from the same account.
#[derive(Debug, Clone, Default)]
pub struct SenderNonceState {
    /// Current nonce for each key. Key 0 is the protocol nonce.
    nonces: HashMap<u64, u64>,
    /// Maximum allowed nonce keys for this sender.
    max_keys: u16,
}

impl SenderNonceState {
    /// Creates a new sender nonce state with the given protocol nonce and max keys.
    pub fn new(protocol_nonce: u64, max_keys: u16) -> Self {
        let mut nonces = HashMap::new();
        nonces.insert(0, protocol_nonce);
        Self { nonces, max_keys }
    }

    /// Returns the current nonce for the given key, or 0 if the key hasn't been used.
    pub fn get_nonce(&self, key: u64) -> u64 {
        self.nonces.get(&key).copied().unwrap_or(0)
    }

    /// Validates and applies a nonce for the given key.
    ///
    /// Returns `Ok(())` if the nonce is valid and was applied, or an error describing
    /// why it's invalid.
    pub fn validate_and_advance(&mut self, nonce_key: &NonceKey) -> Result<(), NonceError> {
        let current = self.get_nonce(nonce_key.key);

        if nonce_key.value < current {
            return Err(NonceError::NonceTooLow {
                key: nonce_key.key,
                expected: current,
                got: nonce_key.value,
            });
        }

        if nonce_key.value > current {
            return Err(NonceError::NonceTooHigh {
                key: nonce_key.key,
                expected: current,
                got: nonce_key.value,
            });
        }

        // Check if adding a new key would exceed the limit
        if !self.nonces.contains_key(&nonce_key.key) && self.nonces.len() >= self.max_keys as usize
        {
            return Err(NonceError::TooManyNonceKeys { max: self.max_keys, sender: Address::ZERO });
        }

        self.nonces.insert(nonce_key.key, nonce_key.value + 1);
        Ok(())
    }

    /// Returns the number of active nonce keys.
    pub fn active_keys(&self) -> usize {
        self.nonces.len()
    }
}

/// Errors in 2D nonce validation.
#[derive(Debug, Clone, thiserror::Error)]
pub enum NonceError {
    /// Nonce is lower than expected (already used).
    #[error("nonce too low for key {key}: expected {expected}, got {got}")]
    NonceTooLow {
        /// The nonce key.
        key: u64,
        /// Expected nonce value.
        expected: u64,
        /// Actual nonce value.
        got: u64,
    },
    /// Nonce is higher than expected (gap).
    #[error("nonce too high for key {key}: expected {expected}, got {got}")]
    NonceTooHigh {
        /// The nonce key.
        key: u64,
        /// Expected nonce value.
        expected: u64,
        /// Actual nonce value.
        got: u64,
    },
    /// Too many nonce keys for this sender.
    #[error("too many nonce keys for sender {sender}: max {max}")]
    TooManyNonceKeys {
        /// Maximum allowed keys.
        max: u16,
        /// The sender address.
        sender: Address,
    },
    /// Transaction has expired.
    #[error("transaction expired: valid_before {valid_before}, current timestamp {timestamp}")]
    Expired {
        /// The `valid_before` value of the transaction.
        valid_before: u64,
        /// The current block timestamp.
        timestamp: u64,
    },
    /// Transaction is not yet valid.
    #[error("transaction not yet valid: valid_after {valid_after}, current timestamp {timestamp}")]
    NotYetValid {
        /// The `valid_after` value of the transaction.
        valid_after: u64,
        /// The current block timestamp.
        timestamp: u64,
    },
}

/// Validates payment transaction metadata (nonce + validity window).
pub fn validate_payment_meta(
    meta: &PaymentTxMeta,
    sender_state: &mut SenderNonceState,
    block_timestamp: u64,
) -> Result<(), NonceError> {
    // Check validity window
    if !meta.validity.is_valid_at(block_timestamp) {
        if meta.validity.valid_before > 0 && block_timestamp >= meta.validity.valid_before {
            return Err(NonceError::Expired {
                valid_before: meta.validity.valid_before,
                timestamp: block_timestamp,
            });
        }
        return Err(NonceError::NotYetValid {
            valid_after: meta.validity.valid_after,
            timestamp: block_timestamp,
        });
    }

    // Validate and advance nonce
    sender_state.validate_and_advance(&meta.nonce_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_nonce_sequential() {
        let mut state = SenderNonceState::new(0, 256);

        let meta0 = PaymentTxMeta::standard(0);
        assert!(validate_payment_meta(&meta0, &mut state, 100).is_ok());

        let meta1 = PaymentTxMeta::standard(1);
        assert!(validate_payment_meta(&meta1, &mut state, 100).is_ok());

        // Replay should fail
        let replay = PaymentTxMeta::standard(0);
        assert!(validate_payment_meta(&replay, &mut state, 100).is_err());
    }

    #[test]
    fn test_parallel_nonces() {
        let mut state = SenderNonceState::new(0, 256);

        // Three parallel nonce keys, all starting at 0
        let tx_a = PaymentTxMeta::parallel(1, 0);
        let tx_b = PaymentTxMeta::parallel(2, 0);
        let tx_c = PaymentTxMeta::parallel(3, 0);

        assert!(validate_payment_meta(&tx_a, &mut state, 100).is_ok());
        assert!(validate_payment_meta(&tx_b, &mut state, 100).is_ok());
        assert!(validate_payment_meta(&tx_c, &mut state, 100).is_ok());

        // Advance key 1 to nonce 1
        let tx_a1 = PaymentTxMeta::parallel(1, 1);
        assert!(validate_payment_meta(&tx_a1, &mut state, 100).is_ok());

        // Key 2 is still at nonce 1 (not 0 anymore), should fail with nonce 0
        let tx_b0 = PaymentTxMeta::parallel(2, 0);
        assert!(validate_payment_meta(&tx_b0, &mut state, 100).is_err());
    }

    #[test]
    fn test_expiring_nonce() {
        let mut state = SenderNonceState::new(0, 256);

        // Transaction valid before timestamp 200
        let meta = PaymentTxMeta::expiring(1, 0, 200);

        // Valid at timestamp 100
        assert!(validate_payment_meta(&meta, &mut state, 100).is_ok());

        // Re-create state for clean test
        let mut state2 = SenderNonceState::new(0, 256);

        // Expired at timestamp 200
        let meta2 = PaymentTxMeta::expiring(1, 0, 200);
        assert!(validate_payment_meta(&meta2, &mut state2, 200).is_err());
    }

    #[test]
    fn test_scheduled_transaction() {
        let mut state = SenderNonceState::new(0, 256);

        // Valid between 100 and 200
        let meta = PaymentTxMeta {
            nonce_key: NonceKey::user(1, 0),
            validity: ValidityWindow::scheduled(100, 200),
            memo: None,
        };

        // Too early
        assert!(validate_payment_meta(&meta, &mut state, 50).is_err());

        // Valid
        assert!(validate_payment_meta(&meta, &mut state, 150).is_ok());
    }

    #[test]
    fn test_max_nonce_keys() {
        let mut state = SenderNonceState::new(0, 3);

        // Key 0 (protocol) already exists, so we can add 2 more
        let tx1 = PaymentTxMeta::parallel(1, 0);
        assert!(validate_payment_meta(&tx1, &mut state, 100).is_ok());

        let tx2 = PaymentTxMeta::parallel(2, 0);
        assert!(validate_payment_meta(&tx2, &mut state, 100).is_ok());

        // This should fail — max 3 keys and we already have keys 0, 1, 2
        let tx3 = PaymentTxMeta::parallel(3, 0);
        assert!(validate_payment_meta(&tx3, &mut state, 100).is_err());
    }

    #[test]
    fn test_nonce_gap() {
        let mut state = SenderNonceState::new(0, 256);

        // Skip nonce 0, try nonce 1 — should fail
        let meta = PaymentTxMeta::standard(1);
        assert!(validate_payment_meta(&meta, &mut state, 100).is_err());
    }

    #[test]
    fn test_memo_attachment() {
        let meta = PaymentTxMeta::standard(0).with_memo(B256::ZERO);
        assert!(meta.memo.is_some());
    }

    #[test]
    fn test_validity_window_unbounded() {
        let w = ValidityWindow::unbounded();
        assert!(w.is_valid_at(0));
        assert!(w.is_valid_at(u64::MAX));
    }

    #[test]
    fn test_nonce_key_helpers() {
        let protocol = NonceKey::protocol(5);
        assert!(protocol.is_protocol());
        assert_eq!(protocol.key, 0);
        assert_eq!(protocol.value, 5);

        let next = protocol.next();
        assert_eq!(next.key, 0);
        assert_eq!(next.value, 6);

        let user = NonceKey::user(3, 10);
        assert!(!user.is_protocol());
        assert_eq!(user.key, 3);
        assert_eq!(user.value, 10);
    }

    #[test]
    fn test_sender_nonce_state_active_keys() {
        let mut state = SenderNonceState::new(0, 256);
        assert_eq!(state.active_keys(), 1); // key 0 (protocol)

        let tx = PaymentTxMeta::parallel(1, 0);
        assert!(validate_payment_meta(&tx, &mut state, 100).is_ok());
        assert_eq!(state.active_keys(), 2);
    }

    #[test]
    fn test_get_nonce_nonexistent_key_returns_zero() {
        let state = SenderNonceState::new(0, 256);
        assert_eq!(state.get_nonce(999), 0);
    }

    #[test]
    fn test_validity_window_boundary_conditions() {
        // Exactly at valid_after boundary: should be valid.
        let w = ValidityWindow::scheduled(100, 200);
        assert!(w.is_valid_at(100));
        // Exactly at valid_before boundary: should be invalid.
        assert!(!w.is_valid_at(200));
        // One before valid_before: valid.
        assert!(w.is_valid_at(199));
    }

    #[test]
    fn test_nonce_error_display() {
        let err = NonceError::NonceTooLow { key: 0, expected: 5, got: 3 };
        let msg = err.to_string();
        assert!(msg.contains("too low"));
        assert!(msg.contains('5'));
        assert!(msg.contains('3'));
    }
}
