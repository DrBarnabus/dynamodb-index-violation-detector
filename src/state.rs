//! State/progress aggregator (PRD §8.5): counters, rolling window, ETA.
//!
//! [`Aggregator`] is the single shared sink every scan worker and the TUI touch.
//! Segment workers call [`record_item`](Aggregator::record_item) and
//! [`record_consumed`](Aggregator::record_consumed) concurrently; the consumer
//! calls [`record_violation`](Aggregator::record_violation); the render loop calls
//! [`snapshot`](Aggregator::snapshot) each frame. Counts are atomic; only the
//! rolling window, per-category tallies and rate history take a short lock.
//!
//! Memory is bounded regardless of violation count (PRD §7): the feed retains a
//! fixed rolling window of the last [`ROLLING_WINDOW_CAP`] violations and tallies
//! are O(categories).
//!
//! Rates are trailing-window, sampled lazily inside `snapshot` from the cumulative
//! counters, so the hot record paths stay lock-light. The [`Clock`] is injectable
//! so tests can drive time-derived fields deterministically (PRD §8.5).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::rules::{Violation, ViolationCategory};

/// Fixed cap on the in-memory violation feed (PRD §6.3.4 / §7).
pub const ROLLING_WINDOW_CAP: usize = 1000;

/// Trailing window over which items/sec and RCU/sec are averaged.
const RATE_WINDOW: Duration = Duration::from_secs(5);

/// A monotonic time source, injectable so tests can advance time deterministically.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The wall-clock source used in production.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Thread-safe scan progress and violation aggregator (PRD §8.5).
pub struct Aggregator {
    clock: Arc<dyn Clock>,
    started_at: Instant,
    item_count: u64,
    total_items: AtomicU64,
    per_segment_items: Vec<AtomicU64>,
    total_violations: AtomicU64,
    consumed_rcu: Mutex<f64>,
    log: Mutex<ViolationLog>,
    rate: Mutex<RateHistory>,
}

/// Per-category tallies plus the bounded rolling window of recent violations.
struct ViolationLog {
    counts: HashMap<ViolationCategory, u64>,
    window: VecDeque<Violation>,
}

/// Trailing samples of the cumulative counters, used to derive rates.
struct RateHistory {
    samples: VecDeque<RateSample>,
}

struct RateSample {
    at: Instant,
    items: u64,
    rcu: f64,
}

/// An immutable view of scan state for one render frame (PRD §6.3.4).
#[derive(Debug, Clone, PartialEq)]
pub struct StateSnapshot {
    pub items_scanned: u64,
    pub items_per_sec: f64,
    pub total_violations: u64,
    pub category_counts: HashMap<ViolationCategory, u64>,
    pub per_segment_items: Vec<u64>,
    pub consumed_rcu: f64,
    pub rcu_per_sec: f64,
    pub elapsed: Duration,
    pub eta: Option<Duration>,
    pub item_count: u64,
    pub progress: f64,
    pub recent_violations: Vec<Violation>,
}

impl Aggregator {
    /// Build an aggregator for a scan over `segments` parallel segments, sized
    /// against the table's approximate `item_count` (PRD §6.2.4) with an
    /// injectable clock. The scan start is anchored to `clock.now()`.
    pub fn new(segments: u32, item_count: u64, clock: Arc<dyn Clock>) -> Self {
        let started_at = clock.now();
        Aggregator {
            clock,
            started_at,
            item_count,
            total_items: AtomicU64::new(0),
            per_segment_items: (0..segments).map(|_| AtomicU64::new(0)).collect(),
            total_violations: AtomicU64::new(0),
            consumed_rcu: Mutex::new(0.0),
            log: Mutex::new(ViolationLog {
                counts: HashMap::new(),
                window: VecDeque::new(),
            }),
            rate: Mutex::new(RateHistory {
                samples: VecDeque::new(),
            }),
        }
    }

    /// As [`new`](Aggregator::new) but on the wall clock, for production wiring.
    pub fn with_system_clock(segments: u32, item_count: u64) -> Self {
        Self::new(segments, item_count, Arc::new(SystemClock))
    }

    /// Record one item read from `segment` (PRD §8.5). Feeds both the aggregate
    /// count and the per-segment progress that the in-flight bars render.
    pub fn record_item(&self, segment: u32) {
        self.total_items.fetch_add(1, Ordering::Relaxed);
        if let Some(counter) = self.per_segment_items.get(segment as usize) {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record one detected violation: bump its category tally and push it onto the
    /// rolling window, evicting the oldest once the window is full.
    pub fn record_violation(&self, violation: &Violation) {
        self.total_violations.fetch_add(1, Ordering::Relaxed);
        let mut log = self.log.lock().expect("violation log not poisoned");
        *log.counts.entry(violation.category).or_insert(0) += 1;
        log.window.push_back(violation.clone());
        if log.window.len() > ROLLING_WINDOW_CAP {
            log.window.pop_front();
        }
    }

    /// Record read capacity consumed by one page (PRD §6.2.3). The `segment` is
    /// part of the aggregator contract; the header meters only the aggregate.
    pub fn record_consumed(&self, _segment: u32, rcu: f64) {
        *self
            .consumed_rcu
            .lock()
            .expect("consumed total not poisoned") += rcu;
    }

    /// A coherent snapshot for the current render frame. Rates are averaged over
    /// the trailing [`RATE_WINDOW`], sampled from the live cumulative counters;
    /// the ETA is best-effort from the approximate `item_count` (PRD §6.2.4).
    pub fn snapshot(&self) -> StateSnapshot {
        let now = self.clock.now();
        let items_scanned = self.total_items.load(Ordering::Relaxed);
        let consumed_rcu = *self
            .consumed_rcu
            .lock()
            .expect("consumed total not poisoned");

        let (items_per_sec, rcu_per_sec) = {
            let mut rate = self.rate.lock().expect("rate history not poisoned");
            rate.sample(now, items_scanned, consumed_rcu);
            rate.rates()
        };

        let log = self.log.lock().expect("violation log not poisoned");
        let per_segment_items = self
            .per_segment_items
            .iter()
            .map(|counter| counter.load(Ordering::Relaxed))
            .collect();

        let remaining = self.item_count.saturating_sub(items_scanned);
        let eta = (items_per_sec > 0.0)
            .then(|| Duration::from_secs_f64(remaining as f64 / items_per_sec));
        let progress = if self.item_count == 0 {
            0.0
        } else {
            items_scanned as f64 / self.item_count as f64
        };

        StateSnapshot {
            items_scanned,
            items_per_sec,
            total_violations: self.total_violations.load(Ordering::Relaxed),
            category_counts: log.counts.clone(),
            per_segment_items,
            consumed_rcu,
            rcu_per_sec,
            elapsed: now.duration_since(self.started_at),
            eta,
            item_count: self.item_count,
            progress,
            recent_violations: log.window.iter().cloned().collect(),
        }
    }
}

impl RateHistory {
    /// Append the current cumulative counters and drop samples that have aged out
    /// of the trailing window, always keeping the newest as one endpoint.
    fn sample(&mut self, now: Instant, items: u64, rcu: f64) {
        self.samples.push_back(RateSample {
            at: now,
            items,
            rcu,
        });
        while self.samples.len() > 1 && now.duration_since(self.samples[0].at) > RATE_WINDOW {
            self.samples.pop_front();
        }
    }

    /// Items/sec and RCU/sec across the retained window, or zero until two
    /// samples span a non-zero interval.
    fn rates(&self) -> (f64, f64) {
        let (Some(oldest), Some(newest)) = (self.samples.front(), self.samples.back()) else {
            return (0.0, 0.0);
        };

        let dt = newest.at.duration_since(oldest.at).as_secs_f64();
        if dt <= 0.0 {
            return (0.0, 0.0);
        }

        let items_per_sec = (newest.items - oldest.items) as f64 / dt;
        let rcu_per_sec = (newest.rcu - oldest.rcu) / dt;
        (items_per_sec, rcu_per_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Target, ViolationCategory};

    /// A clock that starts at construction time and only advances when told to.
    struct MockClock {
        base: Instant,
        offset_nanos: AtomicU64,
    }

    impl MockClock {
        fn new() -> Arc<Self> {
            Arc::new(MockClock {
                base: Instant::now(),
                offset_nanos: AtomicU64::new(0),
            })
        }

        fn advance(&self, by: Duration) {
            self.offset_nanos
                .fetch_add(by.as_nanos() as u64, Ordering::Relaxed);
        }
    }

    impl Clock for MockClock {
        fn now(&self) -> Instant {
            self.base + Duration::from_nanos(self.offset_nanos.load(Ordering::Relaxed))
        }
    }

    fn violation(category: ViolationCategory) -> Violation {
        Violation {
            target: Target::Ttl,
            category,
            attribute: None,
            actual_value: None,
            actual_type: None,
            expected_type: None,
            size_bytes: None,
        }
    }

    #[test]
    fn records_per_segment_and_aggregate_items() {
        let agg = Aggregator::new(3, 0, MockClock::new());
        agg.record_item(0);
        agg.record_item(2);
        agg.record_item(2);

        let snap = agg.snapshot();
        assert_eq!(snap.items_scanned, 3);
        assert_eq!(snap.per_segment_items, vec![1, 0, 2]);
    }

    #[test]
    fn out_of_range_segment_still_counts_toward_aggregate() {
        let agg = Aggregator::new(1, 0, MockClock::new());
        agg.record_item(0);
        agg.record_item(9);

        let snap = agg.snapshot();
        assert_eq!(snap.items_scanned, 2);
        assert_eq!(snap.per_segment_items, vec![1]);
    }

    #[test]
    fn tallies_categories_and_total_violations() {
        let agg = Aggregator::new(1, 0, MockClock::new());
        agg.record_violation(&violation(ViolationCategory::TypeMismatch));
        agg.record_violation(&violation(ViolationCategory::TypeMismatch));
        agg.record_violation(&violation(ViolationCategory::MissingKey));

        let snap = agg.snapshot();
        assert_eq!(snap.total_violations, 3);
        assert_eq!(snap.category_counts[&ViolationCategory::TypeMismatch], 2);
        assert_eq!(snap.category_counts[&ViolationCategory::MissingKey], 1);
    }

    #[test]
    fn rolling_window_is_bounded_but_total_is_not() {
        let agg = Aggregator::new(1, 0, MockClock::new());
        for _ in 0..(ROLLING_WINDOW_CAP + 5) {
            agg.record_violation(&violation(ViolationCategory::SizeExceeded));
        }

        let snap = agg.snapshot();
        assert_eq!(snap.total_violations as usize, ROLLING_WINDOW_CAP + 5);
        assert_eq!(snap.recent_violations.len(), ROLLING_WINDOW_CAP);
    }

    #[test]
    fn rolling_window_keeps_the_most_recent_and_evicts_the_oldest() {
        let agg = Aggregator::new(1, 0, MockClock::new());
        agg.record_violation(&violation(ViolationCategory::TtlMissing));
        for _ in 0..ROLLING_WINDOW_CAP {
            agg.record_violation(&violation(ViolationCategory::TtlMalformed));
        }

        let snap = agg.snapshot();
        assert_eq!(snap.recent_violations.len(), ROLLING_WINDOW_CAP);
        assert!(
            snap.recent_violations
                .iter()
                .all(|v| v.category == ViolationCategory::TtlMalformed),
            "the single oldest TtlMissing should have been evicted"
        );
    }

    #[test]
    fn items_per_sec_averages_over_the_elapsed_interval() {
        let clock = MockClock::new();
        let agg = Aggregator::new(2, 1000, clock.clone());

        agg.snapshot();
        for _ in 0..100 {
            agg.record_item(0);
        }
        for _ in 0..100 {
            agg.record_item(1);
        }
        clock.advance(Duration::from_secs(4));

        let snap = agg.snapshot();
        assert_eq!(snap.items_scanned, 200);
        assert_eq!(snap.items_per_sec, 50.0);
        assert_eq!(snap.elapsed, Duration::from_secs(4));
    }

    #[test]
    fn rcu_rate_averages_over_the_elapsed_interval() {
        let clock = MockClock::new();
        let agg = Aggregator::new(1, 0, clock.clone());

        agg.snapshot();
        agg.record_consumed(0, 20.0);
        agg.record_consumed(0, 20.0);
        clock.advance(Duration::from_secs(2));

        let snap = agg.snapshot();
        assert_eq!(snap.consumed_rcu, 40.0);
        assert_eq!(snap.rcu_per_sec, 20.0);
    }

    #[test]
    fn rate_is_zero_before_time_advances() {
        let agg = Aggregator::new(1, 0, MockClock::new());
        agg.record_item(0);

        let snap = agg.snapshot();
        assert_eq!(snap.items_per_sec, 0.0);
        assert_eq!(snap.rcu_per_sec, 0.0);
    }

    #[test]
    fn eta_derived_from_rate_and_approximate_item_count() {
        let clock = MockClock::new();
        let agg = Aggregator::new(1, 1000, clock.clone());

        agg.snapshot();
        for _ in 0..200 {
            agg.record_item(0);
        }
        clock.advance(Duration::from_secs(4));

        let snap = agg.snapshot();
        assert_eq!(snap.items_per_sec, 50.0);
        assert_eq!(snap.progress, 0.2);
        assert_eq!(snap.eta, Some(Duration::from_secs(16)));
    }

    #[test]
    fn no_eta_without_a_rate() {
        let agg = Aggregator::new(1, 1000, MockClock::new());
        agg.record_item(0);

        assert_eq!(agg.snapshot().eta, None);
    }

    #[test]
    fn progress_overshoots_when_item_count_underestimates() {
        let agg = Aggregator::new(1, 100, MockClock::new());
        for _ in 0..150 {
            agg.record_item(0);
        }

        let snap = agg.snapshot();
        assert_eq!(snap.progress, 1.5);
        assert_eq!(snap.eta, None);
    }

    #[test]
    fn stale_samples_age_out_of_the_rate_window() {
        let clock = MockClock::new();
        let agg = Aggregator::new(1, 0, clock.clone());

        for _ in 0..1000 {
            agg.record_item(0);
        }
        agg.snapshot();

        clock.advance(RATE_WINDOW + Duration::from_secs(1));
        agg.snapshot();

        clock.advance(Duration::from_secs(2));
        for _ in 0..100 {
            agg.record_item(0);
        }
        let snap = agg.snapshot();

        assert_eq!(snap.items_scanned, 1100);
        assert_eq!(
            snap.items_per_sec, 50.0,
            "the initial 1000-item burst should have aged out of the window"
        );
    }
}
