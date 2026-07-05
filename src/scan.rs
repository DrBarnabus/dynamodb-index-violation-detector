//! Scan driver (PRD §8.3): parallel segment fan-out, pagination, rate limiting.
//!
//! [`run_scan`] fans a table scan out across `config.segments` parallel segments
//! (PRD §6.2.1/§6.2.2), one Tokio task per segment, each paginating its own
//! segment via `LastEvaluatedKey`. Items stream back over a bounded channel so a
//! slow consumer applies back-pressure to the workers rather than letting them
//! run ahead unboundedly.
//!
//! When the table is provisioned and a `rate_limit_percent` is set, a shared
//! token bucket paces every worker against a percentage of the snapshotted
//! provisioned RCU (PRD §6.2.3). On-demand tables and an unset percentage run
//! unlimited. Every `Scan` requests `ReturnConsumedCapacity=TOTAL` and the
//! consumed units are aggregated into a running total the TUI header samples.
//!
//! Cancel (PRD §6.3.4) is cooperative: [`ScanStream::cancel`] flips a shared
//! signal, workers stop issuing scans, fetched items drain, then the stream ends.
//!
//! The PRD sketches an `impl Stream`; [`ScanStream`] is a channel-backed
//! equivalent that keeps the crate off an async-stream dependency. Reads are
//! eventually consistent (the SDK default).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::aws::{AwsError, DynamoClient, ScanRequest};
use crate::config::ScanConfig;
use crate::domain::Item;

/// One item read from the scan, tagged with the segment that produced it so the
/// state aggregator (PRD §8.5) can attribute per-segment progress.
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedItem {
    pub segment: u32,
    pub item: Item,
}

/// How many items may sit unread in the channel before segment workers block.
/// Bounds memory and gives the consumer back-pressure over the fan-out.
const CHANNEL_CAPACITY: usize = 1024;

/// The RCU-per-second ceiling for a scan (PRD §6.2.3), or `None` for unlimited.
///
/// Unlimited when the table is on-demand (`provisioned_rcu` is `None`) or no
/// `rate_limit_percent` is set. Otherwise the configured percentage of the
/// snapshotted provisioned RCU.
pub fn rcu_ceiling(provisioned_rcu: Option<u64>, rate_limit_percent: Option<u8>) -> Option<f64> {
    match (provisioned_rcu, rate_limit_percent) {
        (Some(rcu), Some(percent)) => Some(rcu as f64 * percent as f64 / 100.0),
        _ => None,
    }
}

/// Fan a scan out across `config.segments` parallel segments and stream the
/// items back (PRD §6.2.1/§6.2.2).
///
/// `provisioned_rcu` is the capacity snapshotted from `DescribeTable` at scan
/// start (`None` for on-demand); combined with `config.rate_limit_percent` it
/// sets the shared rate ceiling. Each segment runs on its own Tokio task,
/// paginating independently. The returned [`ScanStream`] yields items as they
/// arrive from any segment; a segment failure surfaces as an `Err` and stops
/// only that segment.
///
/// Cancel (PRD §6.3.4) via [`ScanStream::cancel`] or a detached
/// [`CancelHandle`]: workers stop issuing scans, items already fetched drain to
/// the consumer, and the stream then terminates cleanly (`next` returns `None`).
pub fn run_scan(
    config: &ScanConfig,
    client: Arc<dyn DynamoClient>,
    provisioned_rcu: Option<u64>,
) -> ScanStream {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let total_segments = config.segments as u32;
    let limiter = RateLimiter::new(rcu_ceiling(provisioned_rcu, config.rate_limit_percent));
    let consumed = Arc::new(Mutex::new(0.0f64));
    let cancel = CancelHandle(watch::Sender::new(false));
    let mut handles = Vec::with_capacity(config.segments);
    for segment in 0..total_segments {
        let worker = SegmentWorker {
            client: Arc::clone(&client),
            table: config.table.clone(),
            total_segments,
            segment,
            limiter: limiter.clone(),
            consumed: Arc::clone(&consumed),
            cancel: CancelToken(cancel.0.subscribe()),
            tx: tx.clone(),
        };
        handles.push(tokio::spawn(worker.run()));
    }

    ScanStream {
        rx,
        handles,
        consumed,
        cancel,
    }
}

/// The per-segment scan task: everything one Tokio worker needs to paginate its
/// segment, pace against the shared limiter, aggregate consumed RCU and honour
/// cancellation.
struct SegmentWorker {
    client: Arc<dyn DynamoClient>,
    table: String,
    total_segments: u32,
    segment: u32,
    limiter: RateLimiter,
    consumed: Arc<Mutex<f64>>,
    cancel: CancelToken,
    tx: mpsc::Sender<Result<ScannedItem, AwsError>>,
}

impl SegmentWorker {
    /// Paginate the segment to exhaustion, sending each item downstream.
    ///
    /// Before each `Scan` the worker acquires permits equal to the previous
    /// page's consumed RCU from the shared [`RateLimiter`], so the collective
    /// read rate stays under the ceiling. `LastEvaluatedKey` threads one page
    /// into the next until the segment is exhausted. Cancellation is checked
    /// before pagination and races both the rate-limit wait and the in-flight
    /// scan, so a cancel interrupts a long wait promptly; a page already fetched
    /// is delivered before stopping. Returns early if the receiver has been
    /// dropped or the client returns an error (which is forwarded first).
    async fn run(mut self) {
        let mut exclusive_start_key: Option<Item> = None;
        let mut pending_permits = 0.0f64;
        loop {
            if self.cancel.is_cancelled() {
                return;
            }

            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return,
                _ = self.limiter.acquire(pending_permits) => {}
            }

            let request = ScanRequest {
                table: self.table.clone(),
                total_segments: self.total_segments,
                segment: self.segment,
                exclusive_start_key: exclusive_start_key.take(),
                return_consumed_capacity: true,
            };
            let response = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return,
                response = self.client.scan_segment(request) => response,
            };
            match response {
                Ok(response) => {
                    let page_rcu = response.consumed_rcu.unwrap_or(0.0);
                    *self.consumed.lock().expect("consumed total not poisoned") += page_rcu;
                    pending_permits = page_rcu;

                    for item in response.items {
                        let scanned = ScannedItem {
                            segment: self.segment,
                            item,
                        };
                        if self.tx.send(Ok(scanned)).await.is_err() {
                            return;
                        }
                    }

                    match response.last_evaluated_key {
                        Some(key) => exclusive_start_key = Some(key),
                        None => return,
                    }
                }
                Err(err) => {
                    let _ = self.tx.send(Err(err)).await;
                    return;
                }
            }
        }
    }
}

/// A back-pressured stream of scanned items over all segments.
///
/// Poll with [`next`](ScanStream::next) until it returns `None`, at which point
/// every segment has terminated. Aborts its worker tasks on drop.
pub struct ScanStream {
    rx: mpsc::Receiver<Result<ScannedItem, AwsError>>,
    handles: Vec<JoinHandle<()>>,
    consumed: Arc<Mutex<f64>>,
    cancel: CancelHandle,
}

impl ScanStream {
    /// The next scanned item (or a segment error), or `None` once all segments
    /// have terminated and the channel has drained.
    pub async fn next(&mut self) -> Option<Result<ScannedItem, AwsError>> {
        self.rx.recv().await
    }

    /// Total read capacity units consumed so far across every segment (PRD
    /// §6.2.3), sampled live for the in-flight header.
    pub fn consumed_rcu(&self) -> f64 {
        *self.consumed.lock().expect("consumed total not poisoned")
    }

    /// Signal every segment to stop (PRD §6.3.4). Fetched items still drain;
    /// keep calling [`next`](ScanStream::next) until it returns `None`.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// A detached handle to cancel this scan from elsewhere, e.g. a `Ctrl+C`
    /// task while the consumer owns the stream.
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }
}

impl Drop for ScanStream {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

/// A cloneable handle that signals a running scan to stop (PRD §6.3.4).
#[derive(Clone)]
pub struct CancelHandle(watch::Sender<bool>);

impl CancelHandle {
    /// Request cancellation; idempotent and never fails once workers have ended.
    pub fn cancel(&self) {
        let _ = self.0.send(true);
    }
}

/// A worker's view of the cancel signal. Sticky: once set it stays set.
#[derive(Clone)]
struct CancelToken(watch::Receiver<bool>);

impl CancelToken {
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once cancellation is requested (or the last handle is dropped).
    async fn cancelled(&mut self) {
        let _ = self.0.wait_for(|&cancelled| cancelled).await;
    }
}

/// A shared, cross-worker rate limiter, or a no-op when unlimited.
#[derive(Clone)]
struct RateLimiter(Option<Arc<TokenBucket>>);

impl RateLimiter {
    fn new(ceiling: Option<f64>) -> Self {
        RateLimiter(ceiling.map(|rate| Arc::new(TokenBucket::new(rate))))
    }

    /// Block until `permits` capacity units are available; immediate when
    /// unlimited or `permits` is zero.
    async fn acquire(&self, permits: f64) {
        if let Some(bucket) = &self.0 {
            bucket.acquire(permits).await;
        }
    }
}

/// A token bucket refilling at `rate` permits per second, shared across workers.
///
/// A worker pays for a page's consumed RCU on the *next* acquire, so the
/// aggregate read rate converges on the ceiling without needing to know a page's
/// cost before issuing it. The available balance never accrues beyond one
/// second's worth of burst (or a single oversized request).
struct TokenBucket {
    rate: f64,
    state: Mutex<BucketState>,
}

struct BucketState {
    available: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(rate: f64) -> Self {
        TokenBucket {
            rate,
            state: Mutex::new(BucketState {
                available: 0.0,
                last_refill: Instant::now(),
            }),
        }
    }

    async fn acquire(&self, permits: f64) {
        loop {
            let wait = {
                let mut state = self.state.lock().expect("token bucket not poisoned");
                let now = Instant::now();
                let elapsed = now.duration_since(state.last_refill).as_secs_f64();
                let ceiling = self.rate.max(permits);
                state.available = (state.available + elapsed * self.rate).min(ceiling);
                state.last_refill = now;
                if state.available >= permits {
                    state.available -= permits;
                    None
                } else {
                    Some(Duration::from_secs_f64(
                        (permits - state.available) / self.rate,
                    ))
                }
            };
            match wait {
                None => return,
                Some(delay) => tokio::time::sleep(delay).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::aws::mock::MockDynamoClient;
    use crate::aws::{AwsErrorKind, ScanResponse};
    use crate::config::{ExportConfig, ScanConfig};
    use crate::domain::AttributeValue;

    fn config(table: &str, segments: usize) -> ScanConfig {
        ScanConfig {
            table: table.to_string(),
            region: None,
            profile: None,
            segments,
            rate_limit_percent: None,
            export: ExportConfig {
                csv: false,
                csv_path: None,
                ndjson: false,
                ndjson_path: None,
            },
            gsi: Vec::new(),
            lsi: Vec::new(),
            ttl: None,
        }
    }

    fn item(pk: &str) -> Item {
        [("pk".to_string(), AttributeValue::S(pk.to_string()))]
            .into_iter()
            .collect()
    }

    fn page(pk: &str, next: Option<&str>) -> ScanResponse {
        page_rcu(pk, next, None)
    }

    fn page_rcu(pk: &str, next: Option<&str>, consumed_rcu: Option<f64>) -> ScanResponse {
        ScanResponse {
            items: vec![item(pk)],
            last_evaluated_key: next.map(item),
            consumed_rcu,
        }
    }

    async fn drain(mut stream: ScanStream) -> Vec<Result<ScannedItem, AwsError>> {
        let mut out = Vec::new();
        while let Some(next) = stream.next().await {
            out.push(next);
        }

        out
    }

    #[test]
    fn ceiling_is_percentage_of_provisioned_rcu() {
        assert_eq!(rcu_ceiling(Some(100), Some(60)), Some(60.0));
        assert_eq!(rcu_ceiling(Some(200), Some(100)), Some(200.0));
    }

    #[test]
    fn on_demand_or_unset_percent_is_unlimited() {
        assert_eq!(rcu_ceiling(None, Some(60)), None);
        assert_eq!(rcu_ceiling(Some(100), None), None);
        assert_eq!(rcu_ceiling(None, None), None);
    }

    #[tokio::test]
    async fn fans_out_one_task_per_segment_and_drains_every_item() {
        let client = Arc::new(
            MockDynamoClient::new()
                .with_scan_pages(0, [Ok(page("s0", None))])
                .with_scan_pages(1, [Ok(page("s1", None))])
                .with_scan_pages(2, [Ok(page("s2", None))]),
        );

        let results = drain(run_scan(&config("t", 3), Arc::clone(&client) as _, None)).await;

        let scanned: HashSet<(u32, String)> = results
            .into_iter()
            .map(|r| r.unwrap())
            .map(|s| match s.item.get("pk").unwrap() {
                AttributeValue::S(v) => (s.segment, v.clone()),
                other => panic!("unexpected value {other:?}"),
            })
            .collect();

        assert_eq!(
            scanned,
            HashSet::from([
                (0, "s0".to_string()),
                (1, "s1".to_string()),
                (2, "s2".to_string()),
            ])
        );
        assert_eq!(client.recorded_scans().len(), 3);
    }

    #[tokio::test]
    async fn threads_last_evaluated_key_across_pages() {
        let client = Arc::new(MockDynamoClient::new().with_scan_pages(
            0,
            [Ok(page("first", Some("cursor"))), Ok(page("second", None))],
        ));

        let results = drain(run_scan(&config("t", 1), Arc::clone(&client) as _, None)).await;
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(Result::is_ok));

        let scans = client.recorded_scans();
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[0].exclusive_start_key, None);
        assert_eq!(scans[1].exclusive_start_key, Some(item("cursor")));
    }

    #[tokio::test]
    async fn requests_consumed_capacity_on_every_scan() {
        let client = Arc::new(MockDynamoClient::new().with_scan_pages(0, [Ok(page("a", None))]));

        drain(run_scan(
            &config("orders", 1),
            Arc::clone(&client) as _,
            None,
        ))
        .await;

        let scan = &client.recorded_scans()[0];
        assert_eq!(scan.table, "orders");
        assert_eq!(scan.total_segments, 1);
        assert!(scan.return_consumed_capacity);
    }

    #[tokio::test]
    async fn aggregates_consumed_capacity_across_segments() {
        let client = Arc::new(
            MockDynamoClient::new()
                .with_scan_pages(
                    0,
                    [
                        Ok(page_rcu("a", Some("c"), Some(4.5))),
                        Ok(page_rcu("b", None, Some(2.0))),
                    ],
                )
                .with_scan_pages(1, [Ok(page_rcu("c", None, Some(3.5)))]),
        );

        let mut stream = run_scan(&config("t", 2), Arc::clone(&client) as _, None);
        while stream.next().await.is_some() {}

        assert_eq!(stream.consumed_rcu(), 10.0);
    }

    #[tokio::test(start_paused = true)]
    async fn paces_workers_to_the_rcu_ceiling() {
        let client = Arc::new(MockDynamoClient::new().with_scan_pages(
            0,
            [
                Ok(page_rcu("a", Some("c1"), Some(10.0))),
                Ok(page_rcu("b", Some("c2"), Some(10.0))),
                Ok(page_rcu("c", None, Some(10.0))),
            ],
        ));
        let mut cfg = config("t", 1);
        cfg.rate_limit_percent = Some(10);

        let start = Instant::now();
        let results = drain(run_scan(&cfg, Arc::clone(&client) as _, Some(100))).await;
        let elapsed = start.elapsed();

        assert_eq!(results.len(), 3);
        assert!(
            elapsed >= Duration::from_secs(2),
            "10 RCU/s ceiling over three 10-RCU pages should take ~2s, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(2200),
            "pacing should not overshoot the ceiling, took {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_scan_does_not_pace() {
        let client = Arc::new(MockDynamoClient::new().with_scan_pages(
            0,
            [
                Ok(page_rcu("a", Some("c1"), Some(1000.0))),
                Ok(page_rcu("b", None, Some(1000.0))),
            ],
        ));

        let start = Instant::now();
        drain(run_scan(&config("t", 1), Arc::clone(&client) as _, None)).await;

        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn every_segment_scans_with_its_own_index_and_the_total() {
        let client = Arc::new(
            MockDynamoClient::new()
                .with_scan_pages(0, [Ok(page("a", None))])
                .with_scan_pages(1, [Ok(page("b", None))])
                .with_scan_pages(2, [Ok(page("c", None))])
                .with_scan_pages(3, [Ok(page("d", None))]),
        );

        drain(run_scan(&config("t", 4), Arc::clone(&client) as _, None)).await;

        let scans = client.recorded_scans();
        assert_eq!(scans.len(), 4);
        assert!(scans.iter().all(|s| s.total_segments == 4));
        let mut segments: Vec<u32> = scans.iter().map(|s| s.segment).collect();
        segments.sort_unstable();
        assert_eq!(segments, vec![0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn residual_throttle_surfaces_as_error() {
        let throttle = AwsError {
            code: "ProvisionedThroughputExceededException".to_string(),
            message: "slow down".to_string(),
            kind: AwsErrorKind::Throttling,
        };
        let client = Arc::new(MockDynamoClient::new().with_scan_pages(0, [Err(throttle)]));

        let results = drain(run_scan(&config("t", 1), Arc::clone(&client) as _, None)).await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].as_ref().unwrap_err().kind,
            AwsErrorKind::Throttling
        );
    }

    #[tokio::test]
    async fn partial_failure_isolates_the_failed_segment() {
        let boom = AwsError {
            code: "InternalServerError".to_string(),
            message: "boom".to_string(),
            kind: AwsErrorKind::Other,
        };
        let client = Arc::new(
            MockDynamoClient::new()
                .with_scan_pages(0, [Err(boom)])
                .with_scan_pages(1, [Ok(page("b1", Some("c"))), Ok(page("b2", None))])
                .with_scan_pages(2, [Ok(page("c1", None))]),
        );

        let results = drain(run_scan(&config("t", 3), Arc::clone(&client) as _, None)).await;

        let errors = results.iter().filter(|r| r.is_err()).count();
        let items = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(errors, 1);
        assert_eq!(items, 3);
    }

    #[tokio::test]
    async fn cancel_before_draining_stops_every_segment() {
        let client = Arc::new(
            MockDynamoClient::new()
                .with_scan_pages(0, [Ok(page("a", None))])
                .with_scan_pages(1, [Ok(page("b", None))]),
        );

        let stream = run_scan(&config("t", 2), Arc::clone(&client) as _, None);
        stream.cancel();
        let results = drain(stream).await;

        assert!(results.is_empty(), "cancelled workers should emit nothing");
        assert!(
            client.recorded_scans().is_empty(),
            "cancel before the first scan should issue no scans"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_mid_scan_halts_pagination_and_terminates_cleanly() {
        let client = Arc::new(MockDynamoClient::new().with_scan_pages(
            0,
            [
                Ok(page_rcu("a", Some("c1"), Some(10.0))),
                Ok(page_rcu("b", Some("c2"), Some(10.0))),
                Ok(page_rcu("c", None, Some(10.0))),
            ],
        ));
        let mut cfg = config("t", 1);
        cfg.rate_limit_percent = Some(10);

        let mut stream = run_scan(&cfg, Arc::clone(&client) as _, Some(100));
        let first = stream.next().await;
        assert!(matches!(first, Some(Ok(_))), "first page delivered");

        stream.cancel();
        while stream.next().await.is_some() {}

        assert_eq!(
            client.recorded_scans().len(),
            1,
            "cancel during the rate-limit wait should halt before the next scan"
        );
    }

    #[tokio::test]
    async fn failed_segment_makes_no_further_scan_calls() {
        let boom = AwsError {
            code: "InternalServerError".to_string(),
            message: "boom".to_string(),
            kind: AwsErrorKind::Other,
        };
        let client = Arc::new(
            MockDynamoClient::new().with_scan_pages(0, [Err(boom), Ok(page("unreached", None))]),
        );

        let results = drain(run_scan(&config("t", 1), Arc::clone(&client) as _, None)).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
        assert_eq!(client.recorded_scans().len(), 1);
    }
}
