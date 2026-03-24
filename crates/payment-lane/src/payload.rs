//! # Dual-Budget Payment Lane Payload Builder (Phase 1)
//!
//! A payload builder that reserves blockspace for payment transactions.
//! General (non-payment) transactions can only consume up to `general_gas_limit`,
//! while payment transactions can use the remaining capacity.
//!
//! ## Packing Strategy
//!
//! 1. Apply pre-execution changes (system transactions, withdrawals, etc.)
//! 2. Iterate over best transactions from the pool
//! 3. For each transaction, classify it as payment or general
//! 4. General transactions are capped by `general_gas_limit`
//! 5. Payment transactions can use any remaining gas up to `block_gas_limit`
//! 6. When the general budget is exhausted, only payment txs continue

use crate::{
    classifier::{PaymentClassifier, TxLane},
    config::PaymentLaneConfig,
    metrics::PaymentLaneMetrics,
};
use alloy_consensus::Transaction;
use alloy_primitives::U256;
use alloy_rlp::Encodable;
use reth_basic_payload_builder::{
    is_better_payload, BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_consensus_common::validation::MAX_RLP_BLOCK_SIZE;
use reth_errors::{BlockExecutionError, BlockValidationError, ConsensusError};
use reth_ethereum_primitives::{EthPrimitives, TransactionSigned};
use reth_evm::{
    execute::{BlockBuilder, BlockBuilderOutcome},
    ConfigureEvm, Evm, NextBlockEnvAttributes,
};
use reth_evm_ethereum::EthEvmConfig;
use reth_payload_builder::{BlobSidecars, EthBuiltPayload, EthPayloadBuilderAttributes};
use reth_payload_builder_primitives::PayloadBuilderError;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives_traits::transaction::error::InvalidTransactionError;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_storage_api::StateProviderFactory;
use reth_transaction_pool::{
    error::{Eip4844PoolTransactionError, InvalidPoolTransactionError},
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
};
use revm::context_interface::Block as _;
use std::sync::Arc;
use tracing::{debug, trace, warn};

/// Ethereum payload builder configuration with payment lane support.
#[derive(Debug, Clone, Default)]
pub struct PaymentLaneBuilderConfig {
    /// The desired gas limit for the built block.
    pub desired_gas_limit: u64,
    /// Whether to await an in-progress payload on missing.
    pub await_payload_on_missing: bool,
    /// Maximum blobs per block.
    pub max_blobs_per_block: Option<u64>,
    /// Extra data for the block header.
    pub extra_data: alloy_primitives::Bytes,
    /// Payment lane configuration.
    pub payment_config: PaymentLaneConfig,
}

impl PaymentLaneBuilderConfig {
    /// Returns the gas limit for the block, applying any desired limit or using parent's.
    pub const fn gas_limit(&self, parent_gas_limit: u64) -> u64 {
        if self.desired_gas_limit == 0 {
            parent_gas_limit
        } else {
            self.desired_gas_limit
        }
    }
}

/// Payment-lane-aware Ethereum payload builder.
///
/// Wraps the standard Ethereum payload building logic but enforces
/// dual gas budgets: one for general transactions and one shared/reserved for payments.
#[derive(Debug, Clone)]
pub struct PaymentLanePayloadBuilder<Pool, Client, EvmConfig = EthEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool.
    pool: Pool,
    /// EVM configuration.
    evm_config: EvmConfig,
    /// Builder configuration including payment lane settings.
    builder_config: PaymentLaneBuilderConfig,
    /// Payment classifier.
    classifier: PaymentClassifier,
    /// Metrics.
    metrics: PaymentLaneMetrics,
}

impl<Pool, Client, EvmConfig> PaymentLanePayloadBuilder<Pool, Client, EvmConfig> {
    /// Creates a new [`PaymentLanePayloadBuilder`].
    pub fn new(
        client: Client,
        pool: Pool,
        evm_config: EvmConfig,
        builder_config: PaymentLaneBuilderConfig,
    ) -> Self {
        let classifier = PaymentClassifier::new(builder_config.payment_config.clone());
        Self {
            client,
            pool,
            evm_config,
            builder_config,
            classifier,
            metrics: PaymentLaneMetrics::default(),
        }
    }
}

impl<Pool, Client, EvmConfig> PayloadBuilder for PaymentLanePayloadBuilder<Pool, Client, EvmConfig>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks> + Clone,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    type Attributes = EthPayloadBuilderAttributes;
    type BuiltPayload = EthBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<EthPayloadBuilderAttributes, EthBuiltPayload>,
    ) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError> {
        build_payment_lane_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            self.classifier.clone(),
            self.metrics.clone(),
            args,
            |attributes| self.pool.best_transactions_with_attributes(attributes),
        )
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        if self.builder_config.await_payload_on_missing {
            MissingPayloadBehaviour::AwaitInProgress
        } else {
            MissingPayloadBehaviour::RaceEmptyPayload
        }
    }

    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes>,
    ) -> Result<EthBuiltPayload, PayloadBuilderError> {
        let args = BuildArguments::new(Default::default(), config, Default::default(), None);

        build_payment_lane_payload(
            self.evm_config.clone(),
            self.client.clone(),
            self.pool.clone(),
            self.builder_config.clone(),
            self.classifier.clone(),
            self.metrics.clone(),
            args,
            |attributes| self.pool.best_transactions_with_attributes(attributes),
        )?
        .into_payload()
        .ok_or_else(|| PayloadBuilderError::MissingPayload)
    }
}

type BestTransactionsIter<Pool> = Box<
    dyn BestTransactions<Item = Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>,
>;

/// Builds a payload with dual-budget payment lane support.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn build_payment_lane_payload<EvmConfig, Client, Pool, F>(
    evm_config: EvmConfig,
    client: Client,
    pool: Pool,
    builder_config: PaymentLaneBuilderConfig,
    classifier: PaymentClassifier,
    metrics: PaymentLaneMetrics,
    args: BuildArguments<EthPayloadBuilderAttributes, EthBuiltPayload>,
    best_txs: F,
) -> Result<BuildOutcome<EthBuiltPayload>, PayloadBuilderError>
where
    EvmConfig: ConfigureEvm<Primitives = EthPrimitives, NextBlockEnvCtx = NextBlockEnvAttributes>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec: EthereumHardforks>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
    F: FnOnce(BestTransactionsAttributes) -> BestTransactionsIter<Pool>,
{
    let BuildArguments { mut cached_reads, config, cancel, best_payload } = args;
    let PayloadConfig { parent_header, attributes } = config;

    let state_provider = client.state_by_block_hash(parent_header.hash())?;
    let state = StateProviderDatabase::new(state_provider.as_ref());
    let mut db =
        State::builder().with_database(cached_reads.as_db_mut(state)).with_bundle_update().build();

    let mut builder = evm_config
        .builder_for_next_block(
            &mut db,
            &parent_header,
            NextBlockEnvAttributes {
                timestamp: attributes.timestamp(),
                suggested_fee_recipient: attributes.suggested_fee_recipient(),
                prev_randao: attributes.prev_randao(),
                gas_limit: builder_config.gas_limit(parent_header.gas_limit),
                parent_beacon_block_root: attributes.parent_beacon_block_root(),
                withdrawals: Some(attributes.withdrawals().clone()),
                extra_data: builder_config.extra_data.clone(),
            },
        )
        .map_err(PayloadBuilderError::other)?;

    let chain_spec = client.chain_spec();

    debug!(target: "payload_builder",
        id=%attributes.id,
        parent_header = ?parent_header.hash(),
        parent_number = parent_header.number,
        "building payment-lane-aware payload"
    );

    let block_gas_limit: u64 = builder.evm_mut().block().gas_limit();
    let base_fee = builder.evm_mut().block().basefee();

    // Compute dual budgets
    let general_gas_limit =
        builder_config.payment_config.compute_general_gas_limit(block_gas_limit);

    debug!(target: "payload_builder",
        block_gas_limit,
        general_gas_limit,
        payment_reserved = block_gas_limit - general_gas_limit,
        "payment lane gas budgets"
    );

    let mut best_txs = best_txs(BestTransactionsAttributes::new(
        base_fee,
        builder.evm_mut().block().blob_gasprice().map(|gasprice| gasprice as u64),
    ));

    let mut total_fees = U256::ZERO;
    let mut cumulative_gas_used = 0u64;
    let mut general_gas_used = 0u64;
    let mut payment_gas_used = 0u64;

    builder.apply_pre_execution_changes().map_err(|err| {
        warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
        PayloadBuilderError::Internal(err.into())
    })?;

    let mut blob_sidecars = BlobSidecars::Empty;
    let mut block_blob_count = 0u64;
    let mut block_transactions_rlp_length = 0usize;

    let blob_params = chain_spec.blob_params_at_timestamp(attributes.timestamp);
    let protocol_max_blob_count =
        blob_params.as_ref().map(|params| params.max_blob_count).unwrap_or_default();

    let max_blob_count = builder_config
        .max_blobs_per_block
        .map(|user_limit| std::cmp::min(user_limit, protocol_max_blob_count).max(1))
        .unwrap_or(protocol_max_blob_count);

    let is_osaka = chain_spec.is_osaka_active_at_timestamp(attributes.timestamp);
    let withdrawals_rlp_length = attributes.withdrawals().length();

    while let Some(pool_tx) = best_txs.next() {
        // Check total block gas capacity
        if cumulative_gas_used + pool_tx.gas_limit() > block_gas_limit {
            best_txs.mark_invalid(
                &pool_tx,
                &InvalidPoolTransactionError::ExceedsGasLimit(pool_tx.gas_limit(), block_gas_limit),
            );
            continue;
        }

        if cancel.is_cancelled() {
            return Ok(BuildOutcome::Cancelled);
        }

        // Classify this transaction
        let lane =
            classifier.classify(pool_tx.transaction.to().as_ref(), pool_tx.transaction.input());

        // Enforce dual gas budget
        match lane {
            TxLane::General => {
                if general_gas_used + pool_tx.gas_limit() > general_gas_limit {
                    // General budget exhausted — skip this general tx but don't mark sender
                    // as invalid (other txs from same sender might be payment txs)
                    metrics.general_tx_skipped_lane_full.increment(1);
                    trace!(target: "payload_builder",
                        general_gas_used,
                        general_gas_limit,
                        tx_gas = pool_tx.gas_limit(),
                        "skipping general tx: lane budget exhausted"
                    );
                    continue;
                }
                metrics.general_tx_classified.increment(1);
            }
            TxLane::Payment => {
                metrics.payment_tx_classified.increment(1);
            }
        }

        // Convert to signed transaction
        let tx = pool_tx.to_consensus();
        let tx_rlp_len = tx.inner().length();

        let estimated_block_size_with_tx =
            block_transactions_rlp_length + tx_rlp_len + withdrawals_rlp_length + 1024;

        if is_osaka && estimated_block_size_with_tx > MAX_RLP_BLOCK_SIZE {
            best_txs.mark_invalid(
                &pool_tx,
                &InvalidPoolTransactionError::OversizedData {
                    size: estimated_block_size_with_tx,
                    limit: MAX_RLP_BLOCK_SIZE,
                },
            );
            continue;
        }

        // Handle blob transactions (EIP-4844)
        #[allow(clippy::useless_let_if_seq)]
        let mut blob_tx_sidecar = None;
        if let Some(blob_hashes) = tx.blob_versioned_hashes() {
            let tx_blob_count = blob_hashes.len() as u64;

            if block_blob_count + tx_blob_count > max_blob_count {
                trace!(target: "payload_builder",
                    tx=?tx.hash(),
                    ?block_blob_count,
                    "skipping blob transaction because it would exceed the max blob count"
                );
                best_txs.mark_invalid(
                    &pool_tx,
                    &InvalidPoolTransactionError::Eip4844(
                        Eip4844PoolTransactionError::TooManyEip4844Blobs {
                            have: block_blob_count + tx_blob_count,
                            permitted: max_blob_count,
                        },
                    ),
                );
                continue;
            }

            let blob_sidecar_result = 'sidecar: {
                let Some(sidecar) =
                    pool.get_blob(*tx.hash()).map_err(PayloadBuilderError::other)?
                else {
                    break 'sidecar Err(Eip4844PoolTransactionError::MissingEip4844BlobSidecar);
                };

                if is_osaka {
                    if sidecar.is_eip7594() {
                        Ok(sidecar)
                    } else {
                        Err(Eip4844PoolTransactionError::UnexpectedEip4844SidecarAfterOsaka)
                    }
                } else if sidecar.is_eip4844() {
                    Ok(sidecar)
                } else {
                    Err(Eip4844PoolTransactionError::UnexpectedEip7594SidecarBeforeOsaka)
                }
            };

            blob_tx_sidecar = match blob_sidecar_result {
                Ok(sidecar) => Some(sidecar),
                Err(error) => {
                    best_txs.mark_invalid(&pool_tx, &InvalidPoolTransactionError::Eip4844(error));
                    continue;
                }
            };
        }

        // Execute the transaction
        let gas_used = match builder.execute_transaction(tx.clone()) {
            Ok(gas_used) => gas_used,
            Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                error, ..
            })) => {
                if error.is_nonce_too_low() {
                    trace!(target: "payload_builder", %error, ?tx, "skipping nonce too low transaction");
                } else {
                    trace!(target: "payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
                    best_txs.mark_invalid(
                        &pool_tx,
                        &InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::TxTypeNotSupported,
                        ),
                    );
                }
                continue;
            }
            Err(err) => return Err(PayloadBuilderError::evm(err)),
        };

        // Update blob tracking
        if let Some(blob_hashes) = tx.blob_versioned_hashes() {
            block_blob_count += blob_hashes.len() as u64;
            if block_blob_count == max_blob_count {
                best_txs.skip_blobs();
            }
        }

        block_transactions_rlp_length += tx_rlp_len;

        // Update dual-budget tracking
        match lane {
            TxLane::General => {
                general_gas_used += gas_used;
                metrics.general_tx_included.increment(1);
            }
            TxLane::Payment => {
                payment_gas_used += gas_used;
                metrics.payment_tx_included.increment(1);
                if payment_gas_used <= (block_gas_limit - general_gas_limit) {
                    metrics.payment_gas_used_reserved.increment(1);
                } else {
                    metrics.payment_gas_used_overflow.increment(1);
                }
            }
        }

        let miner_fee =
            tx.effective_tip_per_gas(base_fee).expect("fee is always valid; execution succeeded");
        total_fees += U256::from(miner_fee) * U256::from(gas_used);
        cumulative_gas_used += gas_used;

        if let Some(sidecar) = blob_tx_sidecar {
            blob_sidecars.push_sidecar_variant(sidecar.as_ref().clone());
        }
    }

    debug!(target: "payload_builder",
        cumulative_gas_used,
        general_gas_used,
        payment_gas_used,
        "payment lane payload build complete"
    );

    metrics.blocks_built.increment(1);

    if !is_better_payload(best_payload.as_ref(), total_fees) {
        drop(builder);
        return Ok(BuildOutcome::Aborted { fees: total_fees, cached_reads });
    }

    let BlockBuilderOutcome { execution_result, block, .. } =
        builder.finish(state_provider.as_ref())?;

    let requests = chain_spec
        .is_prague_active_at_timestamp(attributes.timestamp)
        .then_some(execution_result.requests);

    let sealed_block = Arc::new(block.into_sealed_block());
    debug!(target: "payload_builder",
        id=%attributes.id,
        sealed_block_header = ?sealed_block.sealed_header(),
        "sealed payment-lane-aware block"
    );

    if is_osaka && sealed_block.rlp_length() > MAX_RLP_BLOCK_SIZE {
        return Err(PayloadBuilderError::other(ConsensusError::BlockTooLarge {
            rlp_length: sealed_block.rlp_length(),
            max_rlp_length: MAX_RLP_BLOCK_SIZE,
        }));
    }

    let payload = EthBuiltPayload::new(attributes.id, sealed_block, total_fees, requests)
        .with_sidecars(blob_sidecars);

    Ok(BuildOutcome::Better { payload, cached_reads })
}
