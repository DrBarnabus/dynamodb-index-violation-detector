//! Scan cost estimate: the RCU a full scan should consume and how long it
//! should take at the configured segment count and rate limit.
//!
//! Built from `DescribeTable`'s approximate table size, so it inherits that
//! figure's staleness (DynamoDB refreshes it roughly every 6 hours). Reads are
//! eventually consistent, costing half an RCU per 4 KB.

use std::time::Duration;

use crate::aws::TableDescription;
use crate::scan::rcu_ceiling;

/// Bytes read per read capacity unit.
const BYTES_PER_READ_UNIT: f64 = 4096.0;

/// RCU charged per read unit for an eventually consistent read.
const EVENTUALLY_CONSISTENT_RCU: f64 = 0.5;

/// Assumed sustained throughput of one segment: one 1 MB page (128 RCU) per
/// 100 ms. Bounds the estimate when no capacity ceiling applies.
pub const SEGMENT_RCU_PER_SEC: f64 = 1280.0;

/// What limits the estimated scan rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateBound {
    /// The configured percentage of the provisioned capacity.
    RateLimit { percent: u8, provisioned_rcu: u64 },
    /// The table's full provisioned capacity; no rate limit is set.
    Provisioned { rcu: u64 },
    /// [`SEGMENT_RCU_PER_SEC`] per segment.
    Segments { segments: usize },
}

/// The estimated consumption and duration of a full scan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostEstimate {
    pub rcu: f64,
    pub rcu_per_sec: f64,
    pub duration: Duration,
    pub bound: RateBound,
}

/// Estimate a full scan of `description` across `segments` segments, paced by
/// `rate_limit_percent` of provisioned capacity when set. The rate is the lower
/// of the capacity ceiling and the segments' assumed throughput.
pub fn estimate(
    description: &TableDescription,
    segments: usize,
    rate_limit_percent: Option<u8>,
) -> CostEstimate {
    let read_units = (description.table_size_bytes as f64 / BYTES_PER_READ_UNIT).ceil();
    let rcu = read_units * EVENTUALLY_CONSISTENT_RCU;

    let capacity = match (description.provisioned_rcu, rate_limit_percent) {
        (Some(provisioned_rcu), Some(percent)) => rcu_ceiling(Some(provisioned_rcu), Some(percent))
            .map(|ceiling| {
                (
                    ceiling,
                    RateBound::RateLimit {
                        percent,
                        provisioned_rcu,
                    },
                )
            }),
        (Some(rcu), None) => Some((rcu as f64, RateBound::Provisioned { rcu })),
        (None, _) => None,
    };

    let segment_rate = segments.max(1) as f64 * SEGMENT_RCU_PER_SEC;
    let (rcu_per_sec, bound) = match capacity {
        Some((ceiling, bound)) if ceiling < segment_rate => (ceiling, bound),
        _ => (segment_rate, RateBound::Segments { segments }),
    };

    CostEstimate {
        rcu,
        rcu_per_sec,
        duration: Duration::from_secs_f64(rcu / rcu_per_sec),
        bound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::TableKeySchema;
    use crate::domain::{KeySchemaElement, TypeCode};

    fn table(table_size_bytes: u64, provisioned_rcu: Option<u64>) -> TableDescription {
        TableDescription {
            name: "users".to_string(),
            key_schema: TableKeySchema {
                pk: KeySchemaElement {
                    name: "id".to_string(),
                    type_code: TypeCode::S,
                },
                sk: None,
            },
            gsis: Vec::new(),
            lsis: Vec::new(),
            ttl: None,
            provisioned_rcu,
            item_count: 0,
            table_size_bytes,
        }
    }

    #[test]
    fn rcu_is_half_a_unit_per_started_4_kb() {
        assert_eq!(estimate(&table(0, None), 1, None).rcu, 0.0);
        assert_eq!(estimate(&table(1, None), 1, None).rcu, 0.5);
        assert_eq!(estimate(&table(8192, None), 1, None).rcu, 1.0);
        assert_eq!(estimate(&table(8193, None), 1, None).rcu, 1.5);
    }

    #[test]
    fn rate_limit_paces_a_provisioned_table() {
        let estimate = estimate(&table(4096 * 12_000, Some(100)), 4, Some(60));

        assert_eq!(estimate.rcu, 6000.0);
        assert_eq!(estimate.rcu_per_sec, 60.0);
        assert_eq!(estimate.duration, Duration::from_secs(100));
        assert_eq!(
            estimate.bound,
            RateBound::RateLimit {
                percent: 60,
                provisioned_rcu: 100
            }
        );
    }

    #[test]
    fn unlimited_provisioned_table_runs_at_full_capacity() {
        let estimate = estimate(&table(4096 * 2000, Some(50)), 4, None);

        assert_eq!(estimate.rcu_per_sec, 50.0);
        assert_eq!(estimate.duration, Duration::from_secs(20));
        assert_eq!(estimate.bound, RateBound::Provisioned { rcu: 50 });
    }

    #[test]
    fn on_demand_table_scales_with_segments_and_ignores_the_rate_limit() {
        let size = 4096 * 2 * 25_600;
        let one = estimate(&table(size, None), 1, Some(10));
        let four = estimate(&table(size, None), 4, Some(10));

        assert_eq!(one.duration, Duration::from_secs(20));
        assert_eq!(four.duration, Duration::from_secs(5));
        assert_eq!(four.bound, RateBound::Segments { segments: 4 });
    }

    #[test]
    fn segments_bound_a_ceiling_they_cannot_reach() {
        let estimate = estimate(&table(4096, Some(40_000)), 2, Some(50));

        assert_eq!(estimate.rcu_per_sec, 2.0 * SEGMENT_RCU_PER_SEC);
        assert_eq!(estimate.bound, RateBound::Segments { segments: 2 });
    }
}
