//! # Consensus-Enforced Payment Lane Validation (Phase 2)
//!
//! Wraps an existing consensus implementation and adds payment lane gas accounting
//! as a consensus rule. When enabled, blocks must satisfy:
//!
//! - Total `gas_used <= gas_limit` (standard Ethereum rule)
//! - Non-payment transactions' cumulative gas `<= general_gas_limit`
//!
//! This upgrades the payment lane from a local builder policy to a protocol guarantee.

use crate::{
    classifier::{PaymentClassifier, TxLane},
    config::PaymentLaneConfig,
    metrics::PaymentLaneMetrics,
};
use alloy_consensus::{BlockHeader as _, Transaction};
use reth_consensus::ConsensusError;
use reth_primitives_traits::{
    Block, BlockBody, NodePrimitives, RecoveredBlock, SealedBlock, SealedHeader,
};
use std::fmt::Debug;

/// Wraps a consensus implementation to add payment lane gas validation.
///
/// When `config.consensus_enforced` is true, block validation includes checking
/// that non-payment transactions don't exceed the general gas limit.
#[derive(Debug, Clone)]
pub struct PaymentLaneValidator<C> {
    /// The inner consensus implementation.
    inner: C,
    /// Payment classifier for determining transaction lanes.
    classifier: PaymentClassifier,
    /// Payment lane configuration.
    config: PaymentLaneConfig,
    /// Metrics for tracking validation results.
    metrics: PaymentLaneMetrics,
}

impl<C> PaymentLaneValidator<C> {
    /// Creates a new [`PaymentLaneValidator`] wrapping the given consensus.
    pub fn new(inner: C, config: PaymentLaneConfig) -> Self {
        let classifier = PaymentClassifier::new(config.clone());
        Self { inner, classifier, config, metrics: PaymentLaneMetrics::default() }
    }

    /// Returns a reference to the inner consensus.
    pub const fn inner(&self) -> &C {
        &self.inner
    }

    /// Validates the payment lane gas accounting for a block's transactions.
    ///
    /// Returns `Ok(())` if the general gas limit is satisfied, or an error if
    /// non-payment transactions exceed their budget.
    pub fn validate_lane_gas<T>(
        &self,
        transactions: &[T],
        block_gas_limit: u64,
    ) -> Result<(), ConsensusError>
    where
        T: Transaction,
    {
        if !self.config.consensus_enforced {
            return Ok(());
        }

        let general_gas_limit = self.config.compute_general_gas_limit(block_gas_limit);
        let mut general_gas_used = 0u64;

        for tx in transactions {
            let lane = self.classifier.classify(tx.to().as_ref(), tx.input());

            if lane == TxLane::General {
                general_gas_used = general_gas_used.saturating_add(tx.gas_limit());
            }
        }

        if general_gas_used > general_gas_limit {
            self.metrics.consensus_lane_invalid.increment(1);
            return Err(ConsensusError::Other(
                format!(
                    "payment lane violation: general gas used ({general_gas_used}) exceeds general gas limit ({general_gas_limit})"
                ),
            ));
        }

        self.metrics.consensus_lane_valid.increment(1);
        Ok(())
    }
}

impl<H, C> reth_consensus::HeaderValidator<H> for PaymentLaneValidator<C>
where
    C: reth_consensus::HeaderValidator<H>,
{
    fn validate_header(&self, header: &SealedHeader<H>) -> Result<(), ConsensusError> {
        self.inner.validate_header(header)
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<H>,
        parent: &SealedHeader<H>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_header_against_parent(header, parent)
    }
}

impl<B, C> reth_consensus::Consensus<B> for PaymentLaneValidator<C>
where
    B: Block,
    B::Body: BlockBody<Transaction: Transaction>,
    C: reth_consensus::Consensus<B>,
{
    fn validate_body_against_header(
        &self,
        body: &B::Body,
        header: &SealedHeader<B::Header>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_body_against_header(body, header)
    }

    fn validate_block_pre_execution(&self, block: &SealedBlock<B>) -> Result<(), ConsensusError> {
        // Run standard validation first
        self.inner.validate_block_pre_execution(block)?;

        // Then apply payment lane validation
        let gas_limit = block.header().gas_limit();
        self.validate_lane_gas(block.body().transactions(), gas_limit)?;

        Ok(())
    }
}

impl<N, C> reth_consensus::FullConsensus<N> for PaymentLaneValidator<C>
where
    N: NodePrimitives,
    N::Block: Block,
    <N::Block as Block>::Body: BlockBody<Transaction: Transaction>,
    C: reth_consensus::FullConsensus<N>,
{
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<N::Block>,
        result: &reth_execution_types::BlockExecutionResult<N::Receipt>,
        receipt_root_bloom: Option<reth_consensus::ReceiptRootBloom>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_block_post_execution(block, result, receipt_root_bloom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxLegacy;

    fn make_tx(to: Option<alloy_primitives::Address>, gas_limit: u64) -> TxLegacy {
        TxLegacy {
            to: to.map_or(alloy_primitives::TxKind::Create, alloy_primitives::TxKind::Call),
            gas_limit,
            ..Default::default()
        }
    }

    #[test]
    fn test_lane_validation_disabled() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig { consensus_enforced: false, ..Default::default() };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config);

        // Should always pass when disabled
        let result = validator.validate_lane_gas::<TxLegacy>(&[], 30_000_000);
        assert!(result.is_ok());
    }

    #[test]
    fn test_lane_validation_enabled_empty_block() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig { consensus_enforced: true, ..Default::default() };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config);

        let result = validator.validate_lane_gas::<TxLegacy>(&[], 30_000_000);
        assert!(result.is_ok());
    }

    #[test]
    fn test_lane_validation_general_gas_under_limit() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig {
            consensus_enforced: true,
            payment_gas_fraction: 0.3,
            ..Default::default()
        };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config.clone());

        let block_gas = 30_000_000u64;
        let general_limit = config.compute_general_gas_limit(block_gas);

        // One general tx well under the limit
        let txs = vec![make_tx(
            Some(alloy_primitives::address!("0xdead000000000000000000000000000000000001")),
            general_limit - 1,
        )];
        assert!(validator.validate_lane_gas(&txs, block_gas).is_ok());
    }

    #[test]
    fn test_lane_validation_general_gas_over_limit() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig {
            consensus_enforced: true,
            payment_gas_fraction: 0.3,
            ..Default::default()
        };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config.clone());

        let block_gas = 30_000_000u64;
        let general_limit = config.compute_general_gas_limit(block_gas);

        // Two general txs that together exceed the limit
        let txs = vec![
            make_tx(
                Some(alloy_primitives::address!("0xdead000000000000000000000000000000000001")),
                general_limit,
            ),
            make_tx(
                Some(alloy_primitives::address!("0xdead000000000000000000000000000000000002")),
                1,
            ),
        ];
        assert!(validator.validate_lane_gas(&txs, block_gas).is_err());
    }

    #[test]
    fn test_lane_validation_payment_tx_not_counted() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig {
            consensus_enforced: true,
            payment_gas_fraction: 0.3,
            ..Default::default()
        };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config.clone());

        let block_gas = 30_000_000u64;
        let general_limit = config.compute_general_gas_limit(block_gas);

        // A payment tx (prefix match) should not count toward general limit
        let payment_addr = alloy_primitives::Address::new([
            0x20, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        ]);
        let txs = vec![
            // General tx at the limit
            make_tx(
                Some(alloy_primitives::address!("0xdead000000000000000000000000000000000001")),
                general_limit,
            ),
            // Payment tx shouldn't affect the count
            make_tx(Some(payment_addr), 5_000_000),
        ];
        assert!(validator.validate_lane_gas(&txs, block_gas).is_ok());
    }

    #[test]
    fn test_inner_accessor() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig::default();
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config);
        let _inner = validator.inner();
    }
}
