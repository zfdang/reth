//! # Payment-Aware Transaction Ordering (Phase 1)
//!
//! Implements [`TransactionOrdering`] that boosts payment transaction priority.
//! Payment transactions get a configurable priority multiplier, ensuring they
//! are selected first during payload building.

use crate::classifier::{PaymentClassifier, TxLane};
use reth_transaction_pool::{PoolTransaction, Priority, TransactionOrdering};
use std::{fmt, marker::PhantomData};

/// Priority value that encodes both the lane classification and the base priority.
///
/// Payment transactions are always ordered above general transactions at the same
/// effective tip level due to the lane-based comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentPriority {
    /// Whether this is a payment transaction (boosted).
    pub lane: TxLane,
    /// The effective tip per gas (base priority).
    pub tip: u128,
}

impl Default for PaymentPriority {
    fn default() -> Self {
        Self { lane: TxLane::General, tip: 0 }
    }
}

impl PartialOrd for PaymentPriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PaymentPriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Payment txs always come before general txs
        match (&self.lane, &other.lane) {
            (TxLane::Payment, TxLane::General) => std::cmp::Ordering::Greater,
            (TxLane::General, TxLane::Payment) => std::cmp::Ordering::Less,
            _ => self.tip.cmp(&other.tip),
        }
    }
}

/// Transaction ordering that is payment-lane aware.
///
/// Payment transactions receive a priority boost, ensuring they are included
/// preferentially during block building. Within the same lane, transactions
/// are ordered by effective tip per gas (same as [`CoinbaseTipOrdering`]).
pub struct PaymentAwareOrdering<T> {
    /// The classifier used to determine transaction lanes.
    classifier: PaymentClassifier,
    /// Priority boost multiplier for payment transactions.
    boost_factor: u64,
    _marker: PhantomData<T>,
}

impl<T> PaymentAwareOrdering<T> {
    /// Creates a new [`PaymentAwareOrdering`] with the given classifier and boost factor.
    pub const fn new(classifier: PaymentClassifier, boost_factor: u64) -> Self {
        Self { classifier, boost_factor, _marker: PhantomData }
    }
}

impl<T> Default for PaymentAwareOrdering<T> {
    fn default() -> Self {
        let classifier = PaymentClassifier::default();
        let boost = classifier.config().payment_priority_boost;
        Self::new(classifier, boost)
    }
}

impl<T> Clone for PaymentAwareOrdering<T> {
    fn clone(&self) -> Self {
        Self {
            classifier: self.classifier.clone(),
            boost_factor: self.boost_factor,
            _marker: PhantomData,
        }
    }
}

impl<T> fmt::Debug for PaymentAwareOrdering<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentAwareOrdering").field("boost_factor", &self.boost_factor).finish()
    }
}

impl<T> TransactionOrdering for PaymentAwareOrdering<T>
where
    T: PoolTransaction + 'static,
{
    type PriorityValue = PaymentPriority;
    type Transaction = T;

    fn priority(
        &self,
        transaction: &Self::Transaction,
        base_fee: u64,
    ) -> Priority<Self::PriorityValue> {
        let Some(tip) = transaction.effective_tip_per_gas(base_fee) else {
            return Priority::None;
        };

        let lane = self.classifier.classify(transaction.to().as_ref(), transaction.input());

        let boosted_tip = match lane {
            TxLane::Payment => tip.saturating_mul(self.boost_factor as u128),
            TxLane::General => tip,
        };

        Priority::Value(PaymentPriority { lane, tip: boosted_tip })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_payment_priority_ordering() {
        let payment = PaymentPriority { lane: TxLane::Payment, tip: 1 };
        let general = PaymentPriority { lane: TxLane::General, tip: 100 };

        // Payment always beats general regardless of tip
        assert!(payment > general);
    }

    #[test]
    fn test_same_lane_ordering() {
        let high = PaymentPriority { lane: TxLane::Payment, tip: 100 };
        let low = PaymentPriority { lane: TxLane::Payment, tip: 1 };
        assert!(high > low);

        let high_gen = PaymentPriority { lane: TxLane::General, tip: 100 };
        let low_gen = PaymentPriority { lane: TxLane::General, tip: 1 };
        assert!(high_gen > low_gen);
    }

    #[test]
    fn test_default_ordering() {
        let ordering: PaymentAwareOrdering<reth_transaction_pool::EthPooledTransaction> =
            PaymentAwareOrdering::default();
        assert_eq!(ordering.boost_factor, 10);
    }
}
