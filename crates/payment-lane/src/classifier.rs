//! # Payment Transaction Classifier (Phase 1)
//!
//! Stateless classification of transactions into payment vs general lanes.
//! Classification depends only on transaction payload (no chain state access).
//!
//! ## Classification Rules
//!
//! A transaction is classified as a payment only when:
//! 1. The `to` address matches a known payment contract (prefix or allowlist), AND
//! 2. The calldata matches a supported payment selector with the exact static ABI length.
//!
//! This intentionally favors a conservative false-negative bias over heuristic false positives.

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

/// Well-known payment function selectors.
///
/// The initial selector set is intentionally narrow until protocol-defined payment
/// ABIs are introduced.
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb]; // transfer(address,uint256)

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

        if !self.is_payment_target(to) {
            return TxLane::General;
        }

        if self.is_supported_payment_call(input) {
            return TxLane::Payment;
        }

        TxLane::General
    }

    fn is_payment_target(&self, to: &Address) -> bool {
        self.config.payment_allowlist.contains(to)
            || self.config.payment_address_prefixes.iter().any(|prefix| prefix.matches(to))
    }

    fn is_supported_payment_call(&self, input: &[u8]) -> bool {
        if input.len() < 4 {
            return false;
        }

        let selector: [u8; 4] = [input[0], input[1], input[2], input[3]];
        selector == TRANSFER_SELECTOR && input.len() == 4 + 64
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

    fn transfer_calldata() -> Vec<u8> {
        let mut calldata = vec![0xa9, 0x05, 0x9c, 0xbb];
        calldata.extend_from_slice(&[0u8; 64]);
        calldata
    }

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
        assert_eq!(classifier.classify(Some(&payment_addr), &transfer_calldata()), TxLane::Payment);
    }

    #[test]
    fn test_allowlist_classification() {
        let allowed = address!("0x1111111111111111111111111111111111111111");
        let config = PaymentLaneConfig { payment_allowlist: vec![allowed], ..Default::default() };
        let classifier = PaymentClassifier::new(config);
        assert_eq!(classifier.classify(Some(&allowed), &transfer_calldata()), TxLane::Payment);
    }

    #[test]
    fn test_selector_without_payment_target_is_general() {
        let classifier = PaymentClassifier::default();
        let to = address!("0xdead000000000000000000000000000000000001");
        assert_eq!(classifier.classify(Some(&to), &transfer_calldata()), TxLane::General);
    }

    #[test]
    fn test_transfer_with_extra_data_is_general() {
        let allowed = address!("0x1111111111111111111111111111111111111111");
        let config = PaymentLaneConfig { payment_allowlist: vec![allowed], ..Default::default() };
        let classifier = PaymentClassifier::new(config);

        // transfer selector but with extra data (could be a hook)
        let mut calldata = transfer_calldata();
        calldata.push(0x01); // extra byte
        assert_eq!(classifier.classify(Some(&allowed), &calldata), TxLane::General);
    }

    #[test]
    fn test_payment_target_without_supported_selector_is_general() {
        let allowed = address!("0x1111111111111111111111111111111111111111");
        let config = PaymentLaneConfig { payment_allowlist: vec![allowed], ..Default::default() };
        let classifier = PaymentClassifier::new(config);
        assert_eq!(classifier.classify(Some(&allowed), &[]), TxLane::General);
    }

    #[test]
    fn test_unknown_selector_is_general() {
        let allowed = address!("0x1111111111111111111111111111111111111111");
        let config = PaymentLaneConfig { payment_allowlist: vec![allowed], ..Default::default() };
        let classifier = PaymentClassifier::new(config);
        let calldata = bytes!("deadbeef");
        assert_eq!(classifier.classify(Some(&allowed), &calldata), TxLane::General);
    }

    #[test]
    fn test_simple_eth_transfer_is_general() {
        // Plain ETH transfer (no calldata) stays in the general lane.
        let classifier = PaymentClassifier::default();
        let to = address!("0xdead000000000000000000000000000000000001");
        assert_eq!(classifier.classify(Some(&to), &[]), TxLane::General);
    }

    #[test]
    fn test_transfer_from_is_not_payment_by_default() {
        let allowed = address!("0x1111111111111111111111111111111111111111");
        let config = PaymentLaneConfig { payment_allowlist: vec![allowed], ..Default::default() };
        let classifier = PaymentClassifier::new(config);

        let mut calldata = vec![0x23, 0xb8, 0x72, 0xdd];
        calldata.extend_from_slice(&[0u8; 96]);
        assert_eq!(classifier.classify(Some(&allowed), &calldata), TxLane::General);
    }

    #[test]
    fn test_short_calldata_with_payment_target_is_general() {
        let allowed = address!("0x1111111111111111111111111111111111111111");
        let config = PaymentLaneConfig { payment_allowlist: vec![allowed], ..Default::default() };
        let classifier = PaymentClassifier::new(config);
        // Only 3 bytes: too short for any selector.
        assert_eq!(classifier.classify(Some(&allowed), &[0xa9, 0x05, 0x9c]), TxLane::General);
    }

    #[test]
    fn test_multiple_prefixes() {
        use crate::config::PaymentPrefix;
        let config = PaymentLaneConfig {
            payment_address_prefixes: vec![
                PaymentPrefix::new(vec![0x20, 0xc0]),
                PaymentPrefix::new(vec![0xAA, 0xBB]),
            ],
            ..Default::default()
        };
        let classifier = PaymentClassifier::new(config);
        let addr = Address::new([
            0xAA, 0xBB, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        ]);
        assert_eq!(classifier.classify(Some(&addr), &transfer_calldata()), TxLane::Payment);
    }

    #[test]
    fn test_tx_lane_display() {
        assert_eq!(TxLane::Payment.to_string(), "payment");
        assert_eq!(TxLane::General.to_string(), "general");
    }
}
