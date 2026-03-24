//! Configuration for payment lane behavior.

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};

/// Configuration for payment lane behavior across the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentLaneConfig {
    /// Fraction of total block gas reserved for payment transactions (0.0 to 1.0).
    /// For example, 0.3 means 30% of block gas is reserved for payment txs.
    pub payment_gas_fraction: f64,

    /// Hard cap: maximum gas that general (non-payment) txs can consume.
    /// If `None`, derived from `payment_gas_fraction * block_gas_limit`.
    pub general_gas_limit: Option<u64>,

    /// Address prefixes that identify payment contracts (TIP-20 style).
    /// A transaction whose `to` address starts with any of these prefixes is classified as
    /// payment.
    pub payment_address_prefixes: Vec<PaymentPrefix>,

    /// Known payment contract addresses (allowlist).
    pub payment_allowlist: Vec<Address>,

    /// Whether consensus enforcement of payment lane is active.
    pub consensus_enforced: bool,

    /// Priority boost factor for payment transactions in the ordering.
    /// Payment tx priority = `base_priority` * `boost_factor`.
    pub payment_priority_boost: u64,

    /// Maximum number of parallel nonce keys allowed per sender.
    pub max_nonce_keys: u16,

    /// Validity window for expiring nonces (seconds).
    pub nonce_expiry_window: u64,
}

/// A prefix for identifying payment addresses (like TIP-20's `0x20c0...` prefix).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentPrefix {
    /// The prefix bytes to match against the `to` address.
    pub bytes: Vec<u8>,
}

impl PaymentPrefix {
    /// Creates a new [`PaymentPrefix`] from raw bytes.
    pub const fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Checks whether the given address matches this prefix.
    pub fn matches(&self, address: &Address) -> bool {
        address.as_slice().starts_with(&self.bytes)
    }
}

impl Default for PaymentLaneConfig {
    fn default() -> Self {
        Self {
            payment_gas_fraction: 0.3,
            general_gas_limit: None,
            payment_address_prefixes: vec![
                // TIP-20 style prefix: 0x20c0000000000000000000000000
                PaymentPrefix::new(vec![0x20, 0xc0]),
            ],
            payment_allowlist: Vec::new(),
            consensus_enforced: false,
            payment_priority_boost: 10,
            max_nonce_keys: 256,
            nonce_expiry_window: 30,
        }
    }
}

impl PaymentLaneConfig {
    /// Computes the general gas limit for a given block gas limit.
    pub fn compute_general_gas_limit(&self, block_gas_limit: u64) -> u64 {
        self.general_gas_limit.unwrap_or_else(|| {
            let reserved = (block_gas_limit as f64 * self.payment_gas_fraction) as u64;
            block_gas_limit.saturating_sub(reserved)
        })
    }

    /// Returns the payment-reserved gas for a given block gas limit.
    pub fn compute_payment_reserved_gas(&self, block_gas_limit: u64) -> u64 {
        block_gas_limit.saturating_sub(self.compute_general_gas_limit(block_gas_limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = PaymentLaneConfig::default();
        assert!((config.payment_gas_fraction - 0.3).abs() < f64::EPSILON);
        assert!(!config.consensus_enforced);
        assert_eq!(config.payment_priority_boost, 10);
    }

    #[test]
    fn test_gas_limit_computation() {
        let config = PaymentLaneConfig::default();
        let block_gas_limit = 30_000_000;

        let general = config.compute_general_gas_limit(block_gas_limit);
        let payment = config.compute_payment_reserved_gas(block_gas_limit);

        assert_eq!(general, 21_000_000); // 70% for general
        assert_eq!(payment, 9_000_000); // 30% reserved for payment
        assert_eq!(general + payment, block_gas_limit);
    }

    #[test]
    fn test_explicit_general_gas_limit() {
        let config =
            PaymentLaneConfig { general_gas_limit: Some(20_000_000), ..Default::default() };
        assert_eq!(config.compute_general_gas_limit(30_000_000), 20_000_000);
        assert_eq!(config.compute_payment_reserved_gas(30_000_000), 10_000_000);
    }

    #[test]
    fn test_payment_prefix_matches() {
        let prefix = PaymentPrefix::new(vec![0x20, 0xc0]);
        let addr = Address::new([
            0x20, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        ]);
        assert!(prefix.matches(&addr));

        let non_payment = Address::new([
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        ]);
        assert!(!prefix.matches(&non_payment));
    }

    #[test]
    fn test_zero_gas_fraction() {
        let config = PaymentLaneConfig { payment_gas_fraction: 0.0, ..Default::default() };
        assert_eq!(config.compute_general_gas_limit(30_000_000), 30_000_000);
        assert_eq!(config.compute_payment_reserved_gas(30_000_000), 0);
    }

    #[test]
    fn test_full_gas_fraction() {
        let config = PaymentLaneConfig { payment_gas_fraction: 1.0, ..Default::default() };
        assert_eq!(config.compute_general_gas_limit(30_000_000), 0);
        assert_eq!(config.compute_payment_reserved_gas(30_000_000), 30_000_000);
    }

    #[test]
    fn test_zero_block_gas_limit() {
        let config = PaymentLaneConfig::default();
        assert_eq!(config.compute_general_gas_limit(0), 0);
        assert_eq!(config.compute_payment_reserved_gas(0), 0);
    }

    #[test]
    fn test_empty_prefix() {
        let prefix = PaymentPrefix::new(vec![]);
        // An empty prefix matches every address.
        let any_addr = Address::new([0xAB; 20]);
        assert!(prefix.matches(&any_addr));
    }
}
