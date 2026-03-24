//! # Consensus-Enforced Payment Lane Validation (Phase 2)
//!
//! Wraps an existing consensus implementation and adds payment lane gas accounting
//! as a consensus rule. When enabled, blocks must satisfy:
//!
//! - Total `gas_used <= gas_limit` (standard Ethereum rule)
//! - Post-execution gas consumed by non-payment transactions `<= general_gas_limit`
//!
//! This upgrades the payment lane from a local builder policy to a protocol guarantee.

use crate::{
    classifier::{PaymentClassifier, TxLane},
    config::PaymentLaneConfig,
    metrics::PaymentLaneMetrics,
};
use alloy_consensus::{BlockHeader as _, Transaction, TxReceipt};
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

    /// Validates the executed payment lane gas accounting for a block's transactions.
    ///
    /// Returns `Ok(())` if the general gas limit is satisfied, or an error if
    /// non-payment transactions exceed their budget.
    pub fn validate_lane_gas<T, R>(
        &self,
        transactions: &[T],
        receipts: &[R],
        block_gas_limit: u64,
    ) -> Result<(), ConsensusError>
    where
        T: Transaction,
        R: TxReceipt,
    {
        if !self.config.consensus_enforced {
            return Ok(());
        }

        if transactions.len() != receipts.len() {
            self.metrics.consensus_lane_invalid.increment(1);
            return Err(ConsensusError::Other(format!(
                "payment lane violation: transaction/receipt count mismatch ({} txs, {} receipts)",
                transactions.len(),
                receipts.len()
            )));
        }

        let general_gas_limit = self.config.compute_general_gas_limit(block_gas_limit);
        let mut general_gas_used = 0u64;
        let mut previous_cumulative_gas = 0u64;

        for (tx, receipt) in transactions.iter().zip(receipts) {
            let lane = self.classifier.classify(tx.to().as_ref(), tx.input());
            let gas_used = receipt
                .cumulative_gas_used()
                .checked_sub(previous_cumulative_gas)
                .ok_or_else(|| {
                    self.metrics.consensus_lane_invalid.increment(1);
                    ConsensusError::Other(
                        "payment lane violation: receipts are not monotonic".to_string(),
                    )
                })?;
            previous_cumulative_gas = receipt.cumulative_gas_used();

            if lane == TxLane::General {
                general_gas_used = general_gas_used.saturating_add(gas_used);
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
        self.inner.validate_block_pre_execution(block)
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
        self.inner.validate_block_post_execution(block, result, receipt_root_bloom)?;
        self.validate_lane_gas(
            block.body().transactions(),
            &result.receipts,
            block.header().gas_limit(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxLegacy;
    use reth_ethereum_primitives::Receipt;

    fn make_tx(to: Option<alloy_primitives::Address>, gas_limit: u64, input: Vec<u8>) -> TxLegacy {
        TxLegacy {
            to: to.map_or(alloy_primitives::TxKind::Create, alloy_primitives::TxKind::Call),
            gas_limit,
            input: input.into(),
            ..Default::default()
        }
    }

    fn transfer_calldata() -> Vec<u8> {
        let mut calldata = vec![0xa9, 0x05, 0x9c, 0xbb];
        calldata.extend_from_slice(&[0u8; 64]);
        calldata
    }

    fn make_receipts(gas_used: &[u64]) -> Vec<Receipt> {
        let mut cumulative = 0u64;

        gas_used
            .iter()
            .map(|gas| {
                cumulative += gas;
                Receipt { cumulative_gas_used: cumulative, success: true, ..Default::default() }
            })
            .collect()
    }

    #[test]
    fn test_lane_validation_disabled() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig { consensus_enforced: false, ..Default::default() };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config);

        // Should always pass when disabled
        let result = validator.validate_lane_gas::<TxLegacy, Receipt>(&[], &[], 30_000_000);
        assert!(result.is_ok());
    }

    #[test]
    fn test_lane_validation_enabled_empty_block() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig { consensus_enforced: true, ..Default::default() };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config);

        let result = validator.validate_lane_gas::<TxLegacy, Receipt>(&[], &[], 30_000_000);
        assert!(result.is_ok());
    }

    #[test]
    fn test_lane_validation_uses_executed_gas() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig {
            consensus_enforced: true,
            payment_gas_fraction: 0.3,
            ..Default::default()
        };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config.clone());

        let block_gas = 30_000_000u64;
        let general_limit = config.compute_general_gas_limit(block_gas);

        // The declared gas limit exceeds the lane budget, but the executed gas does not.
        let txs = vec![make_tx(
            Some(alloy_primitives::address!("0xdead000000000000000000000000000000000001")),
            general_limit + 5_000_000,
            Vec::new(),
        )];
        let receipts = make_receipts(&[general_limit - 1]);
        assert!(validator.validate_lane_gas(&txs, &receipts, block_gas).is_ok());
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
                Vec::new(),
            ),
            make_tx(
                Some(alloy_primitives::address!("0xdead000000000000000000000000000000000002")),
                1,
                Vec::new(),
            ),
        ];
        let receipts = make_receipts(&[general_limit, 1]);
        assert!(validator.validate_lane_gas(&txs, &receipts, block_gas).is_err());
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
                Vec::new(),
            ),
            // Payment tx shouldn't affect the count
            make_tx(Some(payment_addr), 5_000_000, transfer_calldata()),
        ];
        let receipts = make_receipts(&[general_limit, 5_000_000]);
        assert!(validator.validate_lane_gas(&txs, &receipts, block_gas).is_ok());
    }

    #[test]
    fn test_lane_validation_rejects_receipt_count_mismatch() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig { consensus_enforced: true, ..Default::default() };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config);

        let txs = vec![make_tx(
            Some(alloy_primitives::address!("0xdead000000000000000000000000000000000001")),
            21_000,
            Vec::new(),
        )];

        assert!(validator.validate_lane_gas::<TxLegacy, Receipt>(&txs, &[], 30_000_000).is_err());
    }

    #[test]
    fn test_lane_validation_rejects_non_monotonic_receipts() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig { consensus_enforced: true, ..Default::default() };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config);

        let txs = vec![
            make_tx(
                Some(alloy_primitives::address!("0xdead000000000000000000000000000000000001")),
                21_000,
                Vec::new(),
            ),
            make_tx(
                Some(alloy_primitives::address!("0xdead000000000000000000000000000000000002")),
                21_000,
                Vec::new(),
            ),
        ];
        let receipts: Vec<Receipt> = vec![
            Receipt { cumulative_gas_used: 42_000, success: true, ..Default::default() },
            Receipt { cumulative_gas_used: 21_000, success: true, ..Default::default() },
        ];

        assert!(validator.validate_lane_gas(&txs, &receipts, 30_000_000).is_err());
    }

    #[test]
    fn test_inner_accessor() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig::default();
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config);
        let _inner = validator.inner();
    }

    #[test]
    fn test_lane_validation_mixed_within_limit() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig {
            consensus_enforced: true,
            payment_gas_fraction: 0.3,
            ..Default::default()
        };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config.clone());
        let block_gas = 30_000_000u64;
        let general_limit = config.compute_general_gas_limit(block_gas);

        // Payment + general within budget.
        let payment_addr = alloy_primitives::Address::new([
            0x20, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
        ]);
        let txs = vec![
            make_tx(
                Some(alloy_primitives::address!("0xdead000000000000000000000000000000000001")),
                general_limit,
                Vec::new(),
            ),
            make_tx(Some(payment_addr), 9_000_000, transfer_calldata()),
        ];
        let receipts = make_receipts(&[general_limit - 1_000_000, 5_000_000]);
        assert!(validator.validate_lane_gas(&txs, &receipts, block_gas).is_ok());
    }

    #[test]
    fn test_lane_validation_exactly_at_limit() {
        use reth_consensus::noop::NoopConsensus;

        let config = PaymentLaneConfig {
            consensus_enforced: true,
            payment_gas_fraction: 0.3,
            ..Default::default()
        };
        let validator = PaymentLaneValidator::new(NoopConsensus::default(), config.clone());
        let block_gas = 30_000_000u64;
        let general_limit = config.compute_general_gas_limit(block_gas);

        // Exactly at the limit should pass.
        let txs = vec![make_tx(
            Some(alloy_primitives::address!("0xdead000000000000000000000000000000000001")),
            general_limit,
            Vec::new(),
        )];
        let receipts = make_receipts(&[general_limit]);
        assert!(validator.validate_lane_gas(&txs, &receipts, block_gas).is_ok());
    }
}
