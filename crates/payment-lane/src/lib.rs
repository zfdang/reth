//! # Payment Lane for Reth
//!
//! This crate implements a multi-phase payment throughput and finality improvement system.
//!
//! ## Phase 1: Soft Payment Lane
//! - [`classifier`]: Stateless transaction classification into payment vs general lanes
//! - [`ordering`]: Payment-aware transaction ordering for the txpool
//! - [`payload`]: Dual-budget payload builder reserving blockspace for payments
//!
//! ## Phase 2: Consensus-Enforced Lane
//! - [`consensus`]: Validation rules enforcing payment lane gas budgets at the consensus level
//!
//! ## Phase 3: Payment Transaction Semantics
//! - [`nonce`]: 2D nonce system for parallel payment transaction execution
//!
//! ## Phase 4: Constrained Fast Path
//! - [`fastpath`]: Protocol-enforced payment precompile with conflict detection
//!
//! ## Phase 5: BFT Finality
//! - [`finality`]: BFT consensus sidecar for deterministic sub-second finality

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod classifier;
pub mod config;
pub mod consensus;
pub mod fastpath;
pub mod finality;
pub mod metrics;
pub mod nonce;
pub mod ordering;
pub mod payload;

// Re-exports for convenience
pub use classifier::{PaymentClassifier, TxLane};
pub use config::PaymentLaneConfig;
pub use consensus::PaymentLaneValidator;
pub use ordering::PaymentAwareOrdering;
pub use payload::PaymentLanePayloadBuilder;
