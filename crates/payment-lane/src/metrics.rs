//! # Payment Lane Metrics
//!
//! Observability metrics for payment lane performance monitoring.

use metrics::Counter;
use reth_metrics::Metrics;

/// Metrics for the payment lane system.
#[derive(Metrics, Clone)]
#[metrics(scope = "payment_lane")]
pub struct PaymentLaneMetrics {
    /// Total number of transactions classified as payment.
    pub payment_tx_classified: Counter,
    /// Total number of transactions classified as general.
    pub general_tx_classified: Counter,
    /// Number of payment transactions included in built payloads.
    pub payment_tx_included: Counter,
    /// Number of general transactions included in built payloads.
    pub general_tx_included: Counter,
    /// Number of general transactions skipped due to general gas limit exhaustion.
    pub general_tx_skipped_lane_full: Counter,
    /// Number of payment transactions that consumed reserved gas.
    pub payment_gas_used_reserved: Counter,
    /// Number of payment transactions that consumed shared/overflow gas.
    pub payment_gas_used_overflow: Counter,
    /// Number of blocks built with payment lane policy.
    pub blocks_built: Counter,
    /// Consensus lane validation passes.
    pub consensus_lane_valid: Counter,
    /// Consensus lane validation failures.
    pub consensus_lane_invalid: Counter,
    /// Fast-path transactions processed.
    pub fastpath_tx_processed: Counter,
    /// Fast-path conflicts detected.
    pub fastpath_conflicts: Counter,
    /// Parallel nonce transactions validated.
    pub parallel_nonce_validated: Counter,
    /// Expired nonce transactions rejected.
    pub expired_nonce_rejected: Counter,
}
