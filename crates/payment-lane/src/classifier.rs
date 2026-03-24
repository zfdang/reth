//! # Payment Transaction Classifier (Phase 1)
//!
//! Stateless classification of transactions into payment vs general lanes.
//! Classification depends only on transaction payload (no chain state access).
//!
//! ## Classification Rules
//!
//! A transaction is classified as a payment when:
//! 1. The `to` address starts with a known payment prefix (TIP-20 style), OR
//! 2. The `to` address is in the payment allowlist, OR
//! 3. The calldata matches known payment function selectors (`transfer`, `batchTransfer`) AND the
//!    transaction has no contract creation.

use crate::config::PaymentLaneConfig;
use alloy_primitives::Address;
use std::fmt;

/// Lane classification for a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TxLane {
    /// Payment transaction — gets reserved blockspace and priority.
    Payment,
    /// General transaction — uses the general gas budget.
    General,
}

impl fmt::Display for TxLane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Payment => write!(f, "payment"),
            Self::General => write!(f, "general"),
        }
    }
}

/// Well-known ERC-20 / TIP-20 function selectors for payment operations.
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb]; // transfer(address,uint256)
const TRANSFER_FROM_SELECTOR: [u8; 4] = [0x23, 0xb8, 0x72, 0xdd]; // transferFrom(address,address,uint256)

/// Stateless payment transaction classifier.
///
/// Determines whether a transaction belongs to the payment lane or general lane
/// based solely on the transaction's envelope data.
#[derive(Debug, Clone)]
pub struct PaymentClassifier {
    config: PaymentLaneConfig,
}

impl PaymentClassifier {
    /// Creates a new [`PaymentClassifier`] with the given configuration.
    pub const fn new(config: PaymentLaneConfig) -> Self {
        Self { config }
    }

    /// Classifies a transaction based on its `to` address and calldata.
    ///
    /// This is a pure stateless function — it never reads chain state.
    pub fn classify(&self, to: Option<&Address>, input: &[u8]) -> TxLane {
        let Some(to) = to else {
            // Contract creation — never a payment
            return TxLane::General;
        };

        // Rule 1: Check payment address prefixes (TIP-20 style)
        for prefix in &self.config.payment_address_prefixes {
            if prefix.matches(to) {
                return TxLane::Payment;
            }
        }

        // Rule 2: Check payment allowlist
        if self.config.payment_allowlist.contains(to) {
            return TxLane::Payment;
        }

        // Rule 3: Check known payment selectors (only for simple transfers)
        if input.len() >= 4 {
            let selector: [u8; 4] = [input[0], input[1], input[2], input[3]];
            if selector == TRANSFER_SELECTOR || selector == TRANSFER_FROM_SELECTOR {
                // Only classify as payment if the calldata is exactly the expected length
                // (no extra data that might indicate complex logic)
                let expected_len = if selector == TRANSFER_SELECTOR {
                    4 + 64 // selector + address(32) + uint256(32)
                } else {
                    4 + 96 // selector + address(32) + address(32) + uint256(32)
                };
                if input.len() == expected_len {
                    return TxLane::Payment;
                }
            }
        }

        TxLane::General
    }

    /// Returns a reference to the underlying config.
    pub const fn config(&self) -> &PaymentLaneConfig {
        &self.config
    }
}

impl Default for PaymentClassifier {
    fn default() -> Self {
        Self::new(PaymentLaneConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, bytes};

    #[test]
    fn test_contract_creation_is_general() {
        let classifier = PaymentClassifier::default();
        assert_eq!(classifier.classify(None, &[]), TxLane::General);
    }

    #[test]
    fn test_payment_prefix_classification() {
        let classifier = PaymentClassifier::default();
        let payment_addr = Address::new([
            0x20, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        ]);
        assert_eq!(classifier.classify(Some(&payment_addr), &[]), TxLane::Payment);
    }

    #[test]
    fn test_allowlist_classification() {
        let allowed = address!("0x1111111111111111111111111111111111111111");
        let config = PaymentLaneConfig { payment_allowlist: vec![allowed], ..Default::default() };
        let classifier = PaymentClassifier::new(config);
        assert_eq!(classifier.classify(Some(&allowed), &[]), TxLane::Payment);
    }

    #[test]
    fn test_transfer_selector_classification() {
        let classifier = PaymentClassifier::default();
        let to = address!("0xdead000000000000000000000000000000000001");

        // ERC-20 transfer(address, uint256) — 4 + 32 + 32 = 68 bytes
        let mut calldata = vec![0xa9, 0x05, 0x9c, 0xbb]; // selector
        calldata.extend_from_slice(&[0u8; 64]); // address + uint256
        assert_eq!(classifier.classify(Some(&to), &calldata), TxLane::Payment);
    }

    #[test]
    fn test_transfer_with_extra_data_is_general() {
        let classifier = PaymentClassifier::default();
        let to = address!("0xdead000000000000000000000000000000000001");

        // transfer selector but with extra data (could be a hook)
        let mut calldata = vec![0xa9, 0x05, 0x9c, 0xbb];
        calldata.extend_from_slice(&[0u8; 64]);
        calldata.push(0x01); // extra byte
        assert_eq!(classifier.classify(Some(&to), &calldata), TxLane::General);
    }

    #[test]
    fn test_unknown_selector_is_general() {
        let classifier = PaymentClassifier::default();
        let to = address!("0xdead000000000000000000000000000000000001");
        let calldata = bytes!("deadbeef");
        assert_eq!(classifier.classify(Some(&to), &calldata), TxLane::General);
    }

    #[test]
    fn test_simple_eth_transfer_is_general() {
        // Plain ETH transfer (no calldata, no payment prefix) is general
        let classifier = PaymentClassifier::default();
        let to = address!("0xdead000000000000000000000000000000000001");
        assert_eq!(classifier.classify(Some(&to), &[]), TxLane::General);
    }

    #[test]
    fn test_transfer_from_selector_classification() {
        let classifier = PaymentClassifier::default();
        let to = address!("0xdead000000000000000000000000000000000001");

        // transferFrom(address, address, uint256) — 4 + 32 + 32 + 32 = 100 bytes
        let mut calldata = vec![0x23, 0xb8, 0x72, 0xdd]; // selector
        calldata.extend_from_slice(&[0u8; 96]); // from + to + amount
        assert_eq!(classifier.classify(Some(&to), &calldata), TxLane::Payment);
    }
}
