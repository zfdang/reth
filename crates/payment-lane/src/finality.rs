//! # BFT Finality Sidecar (Phase 5)
//!
//! Implements a deterministic BFT finality layer for payment transactions.
//! This is designed as a sidecar that drives the reth execution layer via
//! the Engine API (`newPayload` + `forkchoiceUpdated`).
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────┐
//! │  BFT Consensus      │
//! │  (Simplex-style)    │
//! │                     │
//! │  Proposer election  │
//! │  Block voting       │
//! │  Finalization       │
//! └─────────┬───────────┘
//!           │ Engine API
//!           ▼
//! ┌─────────────────────┐
//! │  Reth EL            │
//! │  (execution layer)  │
//! │                     │
//! │  newPayload         │
//! │  forkchoiceUpdated  │
//! └─────────────────────┘
//! ```
//!
//! ## Finality Guarantees
//!
//! - Blocks are finalized with deterministic BFT finality (no reorgs after finalization)
//! - Block production targets ~600ms under normal conditions
//! - Safety: guaranteed as long as <1/3 of validators are Byzantine
//! - Liveness: maintained as long as ≥2/3 of validators are honest and online

use alloy_primitives::{B256, U256};
use std::{collections::BTreeSet, fmt, time::Duration};

/// Configuration for the BFT finality sidecar.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BftConfig {
    /// Target block production interval.
    pub block_interval: Duration,
    /// Number of validators in the committee.
    pub validator_count: u32,
    /// Byzantine fault tolerance threshold: max Byzantine validators = (`validator_count` - 1) /
    /// 3.
    pub fault_tolerance: u32,
    /// Timeout for proposal phase.
    pub proposal_timeout: Duration,
    /// Timeout for voting phase.
    pub vote_timeout: Duration,
}

impl Default for BftConfig {
    fn default() -> Self {
        let validator_count = 4;
        Self {
            block_interval: Duration::from_millis(600),
            validator_count,
            fault_tolerance: (validator_count - 1) / 3,
            proposal_timeout: Duration::from_millis(400),
            vote_timeout: Duration::from_millis(200),
        }
    }
}

impl BftConfig {
    /// Returns the quorum size (2f+1) needed for agreement.
    pub const fn quorum_size(&self) -> u32 {
        2 * self.fault_tolerance + 1
    }

    /// Returns the supermajority threshold (2/3 of validators, rounded up).
    pub const fn supermajority(&self) -> u32 {
        (self.validator_count * 2).div_ceil(3)
    }
}

/// State of a block in the BFT finality pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockFinalityState {
    /// Block has been proposed but not yet voted on.
    Proposed,
    /// Block has received votes but not yet reached quorum.
    Voting {
        /// Number of votes received.
        votes: u32,
    },
    /// Block has been finalized (irreversible).
    Finalized,
    /// Block was rejected (insufficient votes or conflicting proposals).
    Rejected,
}

impl fmt::Display for BlockFinalityState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Proposed => write!(f, "proposed"),
            Self::Voting { votes } => write!(f, "voting ({votes} votes)"),
            Self::Finalized => write!(f, "finalized"),
            Self::Rejected => write!(f, "rejected"),
        }
    }
}

/// A vote from a validator for a specific block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vote {
    /// The validator index that cast this vote.
    pub validator_index: u32,
    /// The block hash being voted on.
    pub block_hash: B256,
    /// The block number.
    pub block_number: u64,
    /// Signature over (`block_hash`, `block_number`) — opaque bytes for now.
    pub signature: Vec<u8>,
}

/// A finality certificate proving that a block has been finalized.
///
/// Contains the supermajority of votes needed to prove finality.
#[derive(Debug, Clone)]
pub struct FinalityCertificate {
    /// The finalized block hash.
    pub block_hash: B256,
    /// The finalized block number.
    pub block_number: u64,
    /// Votes from the supermajority that finalized this block.
    pub votes: Vec<Vote>,
    /// Total fees in the finalized block.
    pub total_fees: U256,
}

impl FinalityCertificate {
    /// Verifies this certificate has enough votes for the given BFT config.
    pub fn is_valid(&self, config: &BftConfig) -> bool {
        let mut voters = BTreeSet::new();

        for vote in &self.votes {
            if vote.block_hash != self.block_hash || vote.block_number != self.block_number {
                return false;
            }

            if vote.validator_index >= config.validator_count
                || !voters.insert(vote.validator_index)
            {
                return false;
            }
        }

        voters.len() >= config.supermajority() as usize
    }

    /// Returns the number of votes in this certificate.
    pub const fn vote_count(&self) -> usize {
        self.votes.len()
    }
}

/// Preconfirmation for a payment transaction.
///
/// This provides a fast, verifiable signal that a payment has been accepted
/// and will be included in the next block, without waiting for full finality.
#[derive(Debug, Clone)]
pub struct PaymentPreconfirmation {
    /// The transaction hash that was preconfirmed.
    pub tx_hash: B256,
    /// The block number this transaction is targeting.
    pub target_block: u64,
    /// The proposer that issued this preconfirmation.
    pub proposer_index: u32,
    /// Signature from the proposer.
    pub signature: Vec<u8>,
}

/// Tracks the finality state machine for blocks.
#[derive(Debug, Default)]
pub struct FinalityTracker {
    /// The latest finalized block number.
    pub latest_finalized: u64,
    /// The latest finalized block hash.
    pub latest_finalized_hash: B256,
    /// Pending blocks awaiting finalization.
    pub pending_blocks: Vec<PendingBlock>,
}

/// A block pending finalization in the BFT pipeline.
#[derive(Debug, Clone)]
pub struct PendingBlock {
    /// Block hash.
    pub hash: B256,
    /// Block number.
    pub number: u64,
    /// Current finality state.
    pub state: BlockFinalityState,
    /// Collected votes.
    pub votes: Vec<Vote>,
}

impl FinalityTracker {
    /// Creates a new finality tracker starting from the given finalized block.
    pub const fn new(finalized_number: u64, finalized_hash: B256) -> Self {
        Self {
            latest_finalized: finalized_number,
            latest_finalized_hash: finalized_hash,
            pending_blocks: Vec::new(),
        }
    }

    /// Records a new block proposal.
    pub fn propose_block(&mut self, hash: B256, number: u64) {
        self.pending_blocks.push(PendingBlock {
            hash,
            number,
            state: BlockFinalityState::Proposed,
            votes: Vec::new(),
        });
    }

    /// Records a vote for a pending block. Returns the updated finality state.
    pub fn add_vote(&mut self, vote: Vote, config: &BftConfig) -> Option<BlockFinalityState> {
        let block = self
            .pending_blocks
            .iter_mut()
            .find(|b| b.hash == vote.block_hash && b.number == vote.block_number)?;

        // Don't accept votes for already finalized blocks
        if block.state == BlockFinalityState::Finalized {
            return Some(BlockFinalityState::Finalized);
        }

        if vote.validator_index >= config.validator_count {
            return Some(block.state);
        }

        if block.votes.iter().any(|existing| existing.validator_index == vote.validator_index) {
            return Some(block.state);
        }

        block.votes.push(vote);
        let vote_count = block.votes.len() as u32;

        if vote_count >= config.supermajority() {
            block.state = BlockFinalityState::Finalized;
            self.latest_finalized = block.number;
            self.latest_finalized_hash = block.hash;
            Some(BlockFinalityState::Finalized)
        } else {
            block.state = BlockFinalityState::Voting { votes: vote_count };
            Some(block.state)
        }
    }

    /// Prunes finalized blocks older than `keep` blocks.
    pub fn prune(&mut self, keep: u64) {
        if self.latest_finalized > keep {
            let cutoff = self.latest_finalized - keep;
            self.pending_blocks.retain(|b| b.number > cutoff);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bft_config_defaults() {
        let config = BftConfig::default();
        assert_eq!(config.validator_count, 4);
        assert_eq!(config.fault_tolerance, 1);
        assert_eq!(config.quorum_size(), 3);
        assert_eq!(config.supermajority(), 3);
    }

    #[test]
    fn test_finality_tracker_basic_flow() {
        let config = BftConfig::default();
        let mut tracker = FinalityTracker::new(0, B256::ZERO);

        let block_hash = B256::from([1u8; 32]);
        tracker.propose_block(block_hash, 1);

        // Add votes until supermajority
        for i in 0..3 {
            let vote =
                Vote { validator_index: i, block_hash, block_number: 1, signature: vec![i as u8] };
            let state = tracker.add_vote(vote, &config);
            if i < 2 {
                assert!(matches!(state, Some(BlockFinalityState::Voting { .. })));
            } else {
                assert_eq!(state, Some(BlockFinalityState::Finalized));
            }
        }

        assert_eq!(tracker.latest_finalized, 1);
        assert_eq!(tracker.latest_finalized_hash, block_hash);
    }

    #[test]
    fn test_finality_certificate_validation() {
        let config = BftConfig::default();
        let block_hash = B256::from([1u8; 32]);

        let cert = FinalityCertificate {
            block_hash,
            block_number: 1,
            votes: vec![
                Vote { validator_index: 0, block_hash, block_number: 1, signature: vec![0] },
                Vote { validator_index: 1, block_hash, block_number: 1, signature: vec![1] },
                Vote { validator_index: 2, block_hash, block_number: 1, signature: vec![2] },
            ],
            total_fees: U256::ZERO,
        };

        assert!(cert.is_valid(&config));
        assert_eq!(cert.vote_count(), 3);

        // Insufficient votes
        let bad_cert = FinalityCertificate { votes: vec![cert.votes[0].clone()], ..cert };
        assert!(!bad_cert.is_valid(&config));
    }

    #[test]
    fn test_duplicate_votes_do_not_finalize_block() {
        let config = BftConfig::default();
        let mut tracker = FinalityTracker::new(0, B256::ZERO);

        let block_hash = B256::from([2u8; 32]);
        tracker.propose_block(block_hash, 1);

        let vote = Vote { validator_index: 0, block_hash, block_number: 1, signature: vec![0] };
        assert_eq!(
            tracker.add_vote(vote.clone(), &config),
            Some(BlockFinalityState::Voting { votes: 1 })
        );
        assert_eq!(tracker.add_vote(vote, &config), Some(BlockFinalityState::Voting { votes: 1 }));

        let pending = tracker.pending_blocks.iter().find(|block| block.hash == block_hash).unwrap();
        assert_eq!(pending.votes.len(), 1);
        assert_ne!(pending.state, BlockFinalityState::Finalized);
    }

    #[test]
    fn test_vote_with_wrong_block_number_is_ignored() {
        let config = BftConfig::default();
        let mut tracker = FinalityTracker::new(0, B256::ZERO);

        let block_hash = B256::from([3u8; 32]);
        tracker.propose_block(block_hash, 1);

        let wrong_vote =
            Vote { validator_index: 0, block_hash, block_number: 2, signature: vec![0] };
        assert_eq!(tracker.add_vote(wrong_vote, &config), None);
        assert!(tracker.pending_blocks[0].votes.is_empty());
    }

    #[test]
    fn test_certificate_rejects_duplicate_voters() {
        let config = BftConfig::default();
        let block_hash = B256::from([4u8; 32]);

        let cert = FinalityCertificate {
            block_hash,
            block_number: 7,
            votes: vec![
                Vote { validator_index: 0, block_hash, block_number: 7, signature: vec![0] },
                Vote { validator_index: 0, block_hash, block_number: 7, signature: vec![1] },
                Vote { validator_index: 1, block_hash, block_number: 7, signature: vec![2] },
            ],
            total_fees: U256::ZERO,
        };

        assert!(!cert.is_valid(&config));
    }

    #[test]
    fn test_certificate_rejects_mismatched_vote_target() {
        let config = BftConfig::default();
        let block_hash = B256::from([5u8; 32]);

        let cert = FinalityCertificate {
            block_hash,
            block_number: 7,
            votes: vec![
                Vote { validator_index: 0, block_hash, block_number: 7, signature: vec![0] },
                Vote {
                    validator_index: 1,
                    block_hash: B256::from([6u8; 32]),
                    block_number: 7,
                    signature: vec![1],
                },
                Vote { validator_index: 2, block_hash, block_number: 7, signature: vec![2] },
            ],
            total_fees: U256::ZERO,
        };

        assert!(!cert.is_valid(&config));
    }

    #[test]
    fn test_certificate_rejects_out_of_range_validator() {
        let config = BftConfig::default();
        let block_hash = B256::from([7u8; 32]);

        let cert = FinalityCertificate {
            block_hash,
            block_number: 9,
            votes: vec![
                Vote { validator_index: 0, block_hash, block_number: 9, signature: vec![0] },
                Vote { validator_index: 1, block_hash, block_number: 9, signature: vec![1] },
                Vote {
                    validator_index: config.validator_count,
                    block_hash,
                    block_number: 9,
                    signature: vec![2],
                },
            ],
            total_fees: U256::ZERO,
        };

        assert!(!cert.is_valid(&config));
    }

    #[test]
    fn test_preconfirmation() {
        let preconf = PaymentPreconfirmation {
            tx_hash: B256::from([0xAA; 32]),
            target_block: 42,
            proposer_index: 0,
            signature: vec![1, 2, 3],
        };
        assert_eq!(preconf.target_block, 42);
    }

    #[test]
    fn test_prune_old_blocks() {
        let mut tracker = FinalityTracker::new(0, B256::ZERO);

        for i in 1..=10 {
            tracker.propose_block(B256::from([i as u8; 32]), i);
        }
        tracker.latest_finalized = 10;

        tracker.prune(5);
        assert!(tracker.pending_blocks.iter().all(|b| b.number > 5));
    }

    #[test]
    fn test_bft_fault_tolerance_scales() {
        // 10 validators: f=3, quorum=7, supermajority=7
        let config = BftConfig { validator_count: 10, fault_tolerance: 3, ..Default::default() };
        assert_eq!(config.quorum_size(), 7);
        assert_eq!(config.supermajority(), 7);
    }

    #[test]
    fn test_block_finality_state_display() {
        assert_eq!(BlockFinalityState::Proposed.to_string(), "proposed");
        assert_eq!(BlockFinalityState::Voting { votes: 2 }.to_string(), "voting (2 votes)");
        assert_eq!(BlockFinalityState::Finalized.to_string(), "finalized");
        assert_eq!(BlockFinalityState::Rejected.to_string(), "rejected");
    }

    #[test]
    fn test_vote_on_already_finalized_block() {
        let config = BftConfig::default();
        let mut tracker = FinalityTracker::new(0, B256::ZERO);
        let block_hash = B256::from([0xAA; 32]);
        tracker.propose_block(block_hash, 1);

        // Finalize the block.
        for i in 0..3 {
            let vote =
                Vote { validator_index: i, block_hash, block_number: 1, signature: vec![i as u8] };
            tracker.add_vote(vote, &config);
        }

        // Late vote on an already-finalized block should return Finalized without error.
        let late = Vote { validator_index: 3, block_hash, block_number: 1, signature: vec![3] };
        assert_eq!(tracker.add_vote(late, &config), Some(BlockFinalityState::Finalized));
    }

    #[test]
    fn test_multiple_pending_blocks() {
        let config = BftConfig::default();
        let mut tracker = FinalityTracker::new(0, B256::ZERO);

        let hash_a = B256::from([1u8; 32]);
        let hash_b = B256::from([2u8; 32]);
        tracker.propose_block(hash_a, 1);
        tracker.propose_block(hash_b, 2);

        // Vote on block 2 first.
        for i in 0..3 {
            let vote = Vote {
                validator_index: i,
                block_hash: hash_b,
                block_number: 2,
                signature: vec![i as u8],
            };
            tracker.add_vote(vote, &config);
        }
        assert_eq!(tracker.latest_finalized, 2);

        // Block 1 is still pending, not affected.
        let b1 = tracker.pending_blocks.iter().find(|b| b.number == 1).unwrap();
        assert_eq!(b1.state, BlockFinalityState::Proposed);
    }

    #[test]
    fn test_prune_noop_when_nothing_to_prune() {
        let mut tracker = FinalityTracker::new(0, B256::ZERO);
        tracker.propose_block(B256::from([1u8; 32]), 1);
        tracker.prune(100);
        // Nothing should be pruned since latest_finalized (0) <= keep (100).
        assert_eq!(tracker.pending_blocks.len(), 1);
    }

    #[test]
    fn test_out_of_range_validator_vote_ignored() {
        let config = BftConfig::default(); // 4 validators
        let mut tracker = FinalityTracker::new(0, B256::ZERO);
        let block_hash = B256::from([0xBB; 32]);
        tracker.propose_block(block_hash, 1);

        let bad_vote = Vote {
            validator_index: config.validator_count, // out of range
            block_hash,
            block_number: 1,
            signature: vec![0xFF],
        };
        let state = tracker.add_vote(bad_vote, &config);
        // State should remain Proposed (vote rejected).
        assert_eq!(state, Some(BlockFinalityState::Proposed));
        assert!(tracker.pending_blocks[0].votes.is_empty());
    }
}
