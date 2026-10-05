//! End-to-end tests against DynamoDB Local, started in Docker per test.
//!
//! Each test drives the real SDK-backed client through schema discovery, the
//! parallel scan driver, the rule engine and the export writers, asserting on
//! the files written to disk. Requires a running Docker daemon.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use aws_sdk_dynamodb::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue as SdkValue, GlobalSecondaryIndex, KeySchemaElement,
    KeyType, LocalSecondaryIndex, Projection, ProjectionType, ProvisionedThroughput, PutRequest,
    ScalarAttributeType, TimeToLiveSpecification, WriteRequest,
};
use dynamodb_violation_detector::aws::{
    AwsError, AwsErrorKind, DynamoClient, GetItemRequest, RealDynamoClient, ScanRequest,
    ScanResponse, TableDescription,
};
use dynamodb_violation_detector::config::{
    self, CliArgs, GsiEntry, LsiEntry, ScanConfig, TtlSettings,
};
use dynamodb_violation_detector::domain::{self, Item, TypeCode};
use dynamodb_violation_detector::estimate::{self, RateBound};
use dynamodb_violation_detector::inspect::{self, Inspection};
use dynamodb_violation_detector::pipeline::{Pipeline, now_epoch_secs};
use dynamodb_violation_detector::rules::{RuleSet, Target, ViolationCategory};
use dynamodb_violation_detector::state::StateSnapshot;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage};

const IMAGE: &str = "amazon/dynamodb-local";
const IMAGE_TAG: &str = "3.3.1";
const PORT: u16 = 8000;
const BATCH_WRITE_LIMIT: usize = 25;

/// A DynamoDB Local container plus SDK and tool clients pointed at it. The
/// container is removed when this is dropped.
struct LocalDynamo {
    _container: ContainerAsync<GenericImage>,
    sdk: aws_sdk_dynamodb::Client,
}

impl LocalDynamo {
    async fn start() -> Self {
        let container = GenericImage::new(IMAGE, IMAGE_TAG)
            .with_exposed_port(PORT.tcp())
            .with_wait_for(WaitFor::message_on_stdout("Initializing DynamoDB Local"))
            .start()
            .await
            .expect("DynamoDB Local container failed to start; is the Docker daemon running?");
        let host = container.get_host().await.expect("container host");
        let port = container
            .get_host_port_ipv4(PORT)
            .await
            .expect("container port mapping");

        let config = aws_sdk_dynamodb::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("eu-west-1"))
            .endpoint_url(format!("http://{host}:{port}"))
            .credentials_provider(Credentials::new("local", "local", None, None, "static"))
            .build();
        let local = Self {
            _container: container,
            sdk: aws_sdk_dynamodb::Client::from_conf(config),
        };
        local.await_ready().await;
        local
    }

    /// The startup log line precedes the listener binding, so poll until a
    /// request succeeds.
    async fn await_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.sdk.list_tables().send().await {
                Ok(_) => return,
                Err(err) if Instant::now() > deadline => {
                    panic!("DynamoDB Local did not accept requests within 30s: {err}")
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    fn client(&self) -> Arc<dyn DynamoClient> {
        Arc::new(RealDynamoClient::from_client(self.sdk.clone()))
    }

    /// Create a provisioned table keyed `pk`/`sk` with a `byEmail` GSI on
    /// `email`, a `byCreated` LSI on `createdAt` and TTL on `expiresAt`.
    async fn create_users_table(&self, name: &str, rcu: i64) {
        let key = |attr: &str, kind| {
            KeySchemaElement::builder()
                .attribute_name(attr)
                .key_type(kind)
                .build()
                .unwrap()
        };
        let attr = |attr: &str, kind| {
            AttributeDefinition::builder()
                .attribute_name(attr)
                .attribute_type(kind)
                .build()
                .unwrap()
        };
        let throughput = ProvisionedThroughput::builder()
            .read_capacity_units(rcu)
            .write_capacity_units(1000)
            .build()
            .unwrap();
        let keys_only = Projection::builder()
            .projection_type(ProjectionType::KeysOnly)
            .build();

        self.sdk
            .create_table()
            .table_name(name)
            .key_schema(key("pk", KeyType::Hash))
            .key_schema(key("sk", KeyType::Range))
            .attribute_definitions(attr("pk", ScalarAttributeType::S))
            .attribute_definitions(attr("sk", ScalarAttributeType::S))
            .attribute_definitions(attr("email", ScalarAttributeType::S))
            .attribute_definitions(attr("createdAt", ScalarAttributeType::N))
            .provisioned_throughput(throughput.clone())
            .global_secondary_indexes(
                GlobalSecondaryIndex::builder()
                    .index_name("byEmail")
                    .key_schema(key("email", KeyType::Hash))
                    .projection(keys_only.clone())
                    .provisioned_throughput(throughput)
                    .build()
                    .unwrap(),
            )
            .local_secondary_indexes(
                LocalSecondaryIndex::builder()
                    .index_name("byCreated")
                    .key_schema(key("pk", KeyType::Hash))
                    .key_schema(key("createdAt", KeyType::Range))
                    .projection(keys_only)
                    .build()
                    .unwrap(),
            )
            .send()
            .await
            .expect("create table");

        self.sdk
            .update_time_to_live()
            .table_name(name)
            .time_to_live_specification(
                TimeToLiveSpecification::builder()
                    .attribute_name("expiresAt")
                    .enabled(true)
                    .build()
                    .unwrap(),
            )
            .send()
            .await
            .expect("enable TTL");
    }

    async fn put_items(&self, table: &str, items: Vec<HashMap<String, SdkValue>>) {
        let requests: Vec<WriteRequest> = items
            .into_iter()
            .map(|item| {
                WriteRequest::builder()
                    .put_request(PutRequest::builder().set_item(Some(item)).build().unwrap())
                    .build()
            })
            .collect();

        for chunk in requests.chunks(BATCH_WRITE_LIMIT) {
            let mut batch = chunk.to_vec();
            while !batch.is_empty() {
                let out = self
                    .sdk
                    .batch_write_item()
                    .request_items(table, batch)
                    .send()
                    .await
                    .expect("batch write");
                batch = out
                    .unprocessed_items
                    .and_then(|mut unprocessed| unprocessed.remove(table))
                    .unwrap_or_default();
            }
        }
    }
}

/// Records every scan page so tests can assert on pagination and pacing.
struct RecordingClient {
    inner: Arc<dyn DynamoClient>,
    pages: Mutex<Vec<PageRecord>>,
}

#[derive(Clone, Copy)]
struct PageRecord {
    segment: u32,
    continued: bool,
    consumed_rcu: Option<f64>,
    issued_at: Instant,
}

impl RecordingClient {
    fn new(inner: Arc<dyn DynamoClient>) -> Self {
        Self {
            inner,
            pages: Mutex::new(Vec::new()),
        }
    }

    fn pages(&self) -> Vec<PageRecord> {
        self.pages.lock().unwrap().clone()
    }
}

#[async_trait]
impl DynamoClient for RecordingClient {
    async fn list_tables(&self) -> Result<Vec<String>, AwsError> {
        self.inner.list_tables().await
    }

    async fn describe_table(&self, name: &str) -> Result<TableDescription, AwsError> {
        self.inner.describe_table(name).await
    }

    async fn scan_segment(&self, req: ScanRequest) -> Result<ScanResponse, AwsError> {
        let segment = req.segment;
        let continued = req.exclusive_start_key.is_some();
        let issued_at = Instant::now();
        let response = self.inner.scan_segment(req).await?;
        self.pages.lock().unwrap().push(PageRecord {
            segment,
            continued,
            consumed_rcu: response.consumed_rcu,
            issued_at,
        });
        Ok(response)
    }

    async fn get_item(&self, req: GetItemRequest) -> Result<Option<Item>, AwsError> {
        self.inner.get_item(req).await
    }
}

/// The outcome of one scan: final aggregator state, the rules checked and the
/// export file paths.
struct ScanOutcome {
    snapshot: StateSnapshot,
    rules: RuleSet,
    csv: PathBuf,
    ndjson: PathBuf,
}

/// Run a scan through the same [`Pipeline`] the TUI shell drives.
async fn scan(client: Arc<dyn DynamoClient>, config: ScanConfig) -> ScanOutcome {
    let description = client
        .describe_table(&config.table)
        .await
        .expect("describe table");
    let mut pipeline = Pipeline::start(&description, &config, client).expect("start pipeline");
    while let Some(next) = pipeline.next().await {
        pipeline
            .process(next.expect("scan page"))
            .expect("process item");
    }
    pipeline.finish().expect("close export writers");

    ScanOutcome {
        snapshot: pipeline.snapshot(),
        rules: pipeline.rules().clone(),
        csv: config.export.csv_path.expect("CSV path"),
        ndjson: config.export.ndjson_path.expect("NDJSON path"),
    }
}

/// Built-in defaults for `table`, exporting both formats into `export_dir`.
fn scan_config(table: &str, segments: usize, export_dir: &Path) -> ScanConfig {
    let cli = CliArgs {
        table: Some(table.to_string()),
        segments: Some(segments),
        ..CliArgs::default()
    };
    let mut config = config::merge(None, &cli).expect("valid scan config");
    config.export.csv_path = Some(export_dir.join("violations.csv"));
    config.export.ndjson_path = Some(export_dir.join("violations.ndjson"));
    config
}

/// A fresh, empty directory under Cargo's per-target scratch space.
fn export_dir(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    if dir.exists() {
        fs::remove_dir_all(&dir).expect("clear previous export dir");
    }

    fs::create_dir_all(&dir).expect("create export dir");
    dir
}

fn s(value: &str) -> SdkValue {
    SdkValue::S(value.to_string())
}

fn n(value: impl ToString) -> SdkValue {
    SdkValue::N(value.to_string())
}

/// An item satisfying every rule in [`all_rules_config`].
fn clean_item(pk: &str) -> HashMap<String, SdkValue> {
    HashMap::from([
        ("pk".to_string(), s(pk)),
        ("sk".to_string(), s("profile")),
        ("email".to_string(), s("someone@example.com")),
        ("createdAt".to_string(), n(1)),
        ("userId".to_string(), s("u-1")),
        ("score".to_string(), n(10)),
        ("expiresAt".to_string(), n(now_epoch_secs() + 86_400)),
    ])
}

/// Every rule enabled: the existing GSI and LSI as dense indexes, a hypothetical
/// `byUser` GSI, and all TTL sub-checks.
fn all_rules_config(table: &str, export_dir: &Path) -> ScanConfig {
    ScanConfig {
        gsi: vec![
            GsiEntry {
                name: "byEmail".to_string(),
                hypothetical: false,
                pk: None,
                sk: None,
                check_missing: true,
            },
            GsiEntry {
                name: "byUser".to_string(),
                hypothetical: true,
                pk: Some(domain::KeySchemaElement {
                    name: "userId".to_string(),
                    type_code: TypeCode::S,
                }),
                sk: Some(domain::KeySchemaElement {
                    name: "score".to_string(),
                    type_code: TypeCode::N,
                }),
                check_missing: true,
            },
        ],
        lsi: vec![LsiEntry {
            name: "byCreated".to_string(),
            check_missing: true,
        }],
        ttl: Some(TtlSettings {
            enabled: Some(true),
            check_missing: Some(true),
            check_wrong_type: Some(true),
            check_ms_magnitude: Some(true),
            check_malformed: Some(true),
            check_past_5_years: Some(true),
        }),
        ..scan_config(table, 2, export_dir)
    }
}

type Finding = (String, String);

/// Per-item findings from the NDJSON export, keyed by the item's `pk`.
fn ndjson_findings(path: &Path) -> BTreeMap<String, BTreeSet<Finding>> {
    fs::read_to_string(path)
        .expect("read NDJSON export")
        .lines()
        .map(|line| {
            let record: serde_json::Value = serde_json::from_str(line).expect("valid NDJSON line");
            let pk = record["pk"]["pk"]["S"]
                .as_str()
                .expect("string pk")
                .to_string();
            let findings = record["violations"]
                .as_array()
                .expect("violations array")
                .iter()
                .map(|v| {
                    (
                        v["target"].as_str().unwrap().to_string(),
                        v["category"].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            (pk, findings)
        })
        .collect()
}

/// `(pk, target, category)` for every CSV row.
fn csv_rows(path: &Path) -> Vec<(String, String, String)> {
    let mut reader = csv::Reader::from_path(path).expect("open CSV export");
    let headers = reader.headers().expect("CSV header").clone();
    let column = |name: &str| {
        headers
            .iter()
            .position(|h| h == name)
            .unwrap_or_else(|| panic!("CSV header lacks `{name}`: {headers:?}"))
    };
    let (pk, target, category) = (column("pk"), column("target"), column("category"));
    reader
        .records()
        .map(|record| {
            let record = record.expect("CSV record");
            (
                record[pk].to_string(),
                record[target].to_string(),
                record[category].to_string(),
            )
        })
        .collect()
}

fn finding(target: &str, category: &str) -> Finding {
    (target.to_string(), category.to_string())
}

#[tokio::test]
async fn discovers_table_schema_indexes_and_ttl() {
    let local = LocalDynamo::start().await;
    local.create_users_table("users", 40).await;

    let description = local.client().describe_table("users").await.unwrap();

    assert_eq!(description.name, "users");
    assert_eq!(description.key_schema.pk.name, "pk");
    assert_eq!(description.key_schema.pk.type_code, TypeCode::S);
    assert_eq!(description.key_schema.sk.as_ref().unwrap().name, "sk");
    assert_eq!(description.provisioned_rcu, Some(40));

    let gsi = &description.gsis[0];
    assert_eq!(gsi.name, "byEmail");
    assert_eq!(gsi.pk.name, "email");
    assert_eq!(gsi.pk.type_code, TypeCode::S);
    assert!(gsi.sk.is_none());

    let lsi = &description.lsis[0];
    assert_eq!(lsi.name, "byCreated");
    let lsi_sk = lsi.sk.as_ref().expect("LSI sort key");
    assert_eq!(lsi_sk.name, "createdAt");
    assert_eq!(lsi_sk.type_code, TypeCode::N);

    let ttl = description.ttl.expect("TTL discovered");
    assert_eq!(ttl.attribute, "expiresAt");
    assert!(ttl.enabled);
}

#[tokio::test]
async fn missing_table_maps_to_not_found_with_remediation() {
    let local = LocalDynamo::start().await;

    let err = local.client().describe_table("absent").await.unwrap_err();

    assert_eq!(err.kind, AwsErrorKind::NotFound);
    assert_eq!(err.code, "ResourceNotFoundException");
    assert!(err.remediation().is_some());
}

#[tokio::test]
async fn scan_detects_and_exports_every_violation_kind() {
    let local = LocalDynamo::start().await;
    local.create_users_table("users", 1000).await;

    let with = |pk: &str, attr: &str, value: SdkValue| {
        let mut item = clean_item(pk);
        item.insert(attr.to_string(), value);
        item
    };
    let without = |pk: &str, attrs: &[&str]| {
        let mut item = clean_item(pk);
        for attr in attrs {
            item.remove(*attr);
        }
        item
    };
    let mut multi = without("multi", &["email"]);
    multi.insert("expiresAt".to_string(), s("tomorrow"));

    local
        .put_items(
            "users",
            vec![
                clean_item("clean"),
                without("no-email", &["email"]),
                without("no-created", &["createdAt"]),
                with("user-number", "userId", n(7)),
                with("user-oversize", "userId", s(&"x".repeat(3000))),
                with("score-string", "score", s("high")),
                without("no-user", &["userId"]),
                without("ttl-missing", &["expiresAt"]),
                with("ttl-string", "expiresAt", s("tomorrow")),
                with("ttl-millis", "expiresAt", n(1_700_000_000_000i64)),
                with("ttl-negative", "expiresAt", n(-5)),
                with("ttl-fraction", "expiresAt", n("1.5")),
                with("ttl-ancient", "expiresAt", n(1_000_000_000)),
                multi,
            ],
        )
        .await;

    let dir = export_dir("scan_detects_and_exports_every_violation_kind");
    let outcome = scan(local.client(), all_rules_config("users", &dir)).await;

    let expected: BTreeMap<String, BTreeSet<Finding>> = [
        ("no-email", vec![finding("GSI:byEmail", "missing_key")]),
        ("no-created", vec![finding("LSI:byCreated", "missing_key")]),
        ("user-number", vec![finding("GSI:byUser", "type_mismatch")]),
        (
            "user-oversize",
            vec![finding("GSI:byUser", "size_exceeded")],
        ),
        ("score-string", vec![finding("GSI:byUser", "type_mismatch")]),
        ("no-user", vec![finding("GSI:byUser", "missing_key")]),
        ("ttl-missing", vec![finding("TTL", "ttl_missing")]),
        ("ttl-string", vec![finding("TTL", "ttl_wrong_type")]),
        ("ttl-millis", vec![finding("TTL", "ttl_ms_magnitude")]),
        ("ttl-negative", vec![finding("TTL", "ttl_malformed")]),
        ("ttl-fraction", vec![finding("TTL", "ttl_malformed")]),
        ("ttl-ancient", vec![finding("TTL", "ttl_past_five_years")]),
        (
            "multi",
            vec![
                finding("GSI:byEmail", "missing_key"),
                finding("TTL", "ttl_wrong_type"),
            ],
        ),
    ]
    .into_iter()
    .map(|(pk, findings)| (pk.to_string(), findings.into_iter().collect()))
    .collect();

    assert_eq!(ndjson_findings(&outcome.ndjson), expected);

    let mut rows = csv_rows(&outcome.csv);
    rows.sort();
    let mut expected_rows: Vec<_> = expected
        .iter()
        .flat_map(|(pk, findings)| {
            findings
                .iter()
                .map(|(target, category)| (pk.clone(), target.clone(), category.clone()))
        })
        .collect();
    expected_rows.sort();
    assert_eq!(rows, expected_rows, "one CSV row per violation");

    assert_eq!(outcome.snapshot.items_scanned, 14);
    assert_eq!(outcome.snapshot.total_violations, 14);
    assert_eq!(outcome.snapshot.recent_violations.len(), 14);
}

#[tokio::test]
async fn exported_key_round_trips_through_get_item() {
    let local = LocalDynamo::start().await;
    local.create_users_table("users", 1000).await;
    let mut item = clean_item("no-email");
    item.remove("email");
    local.put_items("users", vec![item]).await;

    let dir = export_dir("exported_key_round_trips_through_get_item");
    let outcome = scan(local.client(), all_rules_config("users", &dir)).await;

    let line = fs::read_to_string(&outcome.ndjson).unwrap();
    let record: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let mut key: Item = serde_json::from_value(record["pk"].clone()).expect("exported pk");
    key.extend(serde_json::from_value::<Item>(record["sk"].clone()).expect("exported sk"));
    assert_eq!(
        key.get("pk"),
        Some(&domain::AttributeValue::S("no-email".to_string()))
    );

    let fetched = local
        .client()
        .get_item(GetItemRequest {
            table: "users".to_string(),
            key: key.clone(),
        })
        .await
        .unwrap()
        .expect("violating item still present");
    assert!(!fetched.contains_key("email"));
    assert_eq!(
        fetched.get("userId"),
        Some(&domain::AttributeValue::S("u-1".to_string()))
    );

    local
        .sdk
        .delete_item()
        .table_name("users")
        .key("pk", s("no-email"))
        .key("sk", s("profile"))
        .send()
        .await
        .unwrap();
    let gone = local
        .client()
        .get_item(GetItemRequest {
            table: "users".to_string(),
            key,
        })
        .await
        .unwrap();
    assert!(gone.is_none(), "a deleted item reads back as absent");
}

#[tokio::test]
async fn drill_in_tells_unchanged_changed_fixed_and_deleted_items_apart() {
    let local = LocalDynamo::start().await;
    local.create_users_table("users", 1000).await;
    let without_email = |pk: &str| {
        let mut item = clean_item(pk);
        item.remove("email");
        item
    };
    local
        .put_items(
            "users",
            ["unchanged", "edited", "fixed", "deleted"]
                .map(without_email)
                .into(),
        )
        .await;

    let dir = export_dir("drill_in_tells_unchanged_changed_fixed_and_deleted_items_apart");
    let outcome = scan(local.client(), all_rules_config("users", &dir)).await;

    let mut edited = without_email("edited");
    edited.insert("score".to_string(), n(11));
    let mut fixed = without_email("fixed");
    fixed.insert("email".to_string(), s("fixed@example.com"));
    local.put_items("users", vec![edited, fixed]).await;
    local
        .sdk
        .delete_item()
        .table_name("users")
        .key("pk", s("deleted"))
        .key("sk", s("profile"))
        .send()
        .await
        .unwrap();

    let mut inspections = BTreeMap::new();
    for recent in &outcome.snapshot.recent_violations {
        assert_eq!(recent.violation.target, Target::Gsi("byEmail".to_string()));
        assert_eq!(recent.violation.category, ViolationCategory::MissingKey);
        let current = local
            .client()
            .get_item(GetItemRequest {
                table: "users".to_string(),
                key: inspect::primary_key(&recent.pk, recent.sk.as_ref()),
            })
            .await
            .unwrap();
        let summary = match inspect::inspect(recent, current, &outcome.rules, now_epoch_secs()) {
            Inspection::Gone => "gone",
            Inspection::Present {
                changed: false,
                still_violating: true,
                ..
            } => "unchanged, violating",
            Inspection::Present {
                changed: true,
                still_violating: true,
                ..
            } => "changed, violating",
            Inspection::Present {
                changed: true,
                still_violating: false,
                ..
            } => "changed, fixed",
            other => panic!("unexpected inspection {other:?}"),
        };
        let domain::AttributeValue::S(pk) = &recent.pk.value else {
            panic!("string partition key expected");
        };
        inspections.insert(pk.clone(), summary);
    }

    assert_eq!(
        inspections,
        BTreeMap::from([
            ("deleted".to_string(), "gone"),
            ("edited".to_string(), "changed, violating"),
            ("fixed".to_string(), "changed, fixed"),
            ("unchanged".to_string(), "unchanged, violating"),
        ])
    );
}

/// Items padded to ~4KB, so a 1MB scan page holds ~250 of them.
fn padded_items(count: usize) -> Vec<HashMap<String, SdkValue>> {
    let padding = "p".repeat(4000);
    (0..count)
        .map(|i| {
            let mut item = clean_item(&format!("item-{i:05}"));
            item.insert("padding".to_string(), s(&padding));
            item
        })
        .collect()
}

#[tokio::test]
async fn cost_estimate_sizes_the_scan_from_the_described_table() {
    let local = LocalDynamo::start().await;
    local.create_users_table("users", 40).await;
    local.put_items("users", padded_items(100)).await;

    let description = local.client().describe_table("users").await.unwrap();
    assert!(
        description.table_size_bytes >= 100 * 4000,
        "table size {} should cover the padded items",
        description.table_size_bytes
    );

    let estimate = estimate::estimate(&description, 4, Some(50));
    let expected_rcu = description.table_size_bytes as f64 / 4096.0 / 2.0;
    assert!((estimate.rcu - expected_rcu).abs() <= 0.5);
    assert_eq!(estimate.rcu_per_sec, 20.0);
    assert_eq!(
        estimate.bound,
        RateBound::RateLimit {
            percent: 50,
            provisioned_rcu: 40
        }
    );
}

#[tokio::test]
async fn parallel_scan_reads_every_item_exactly_once_across_pages() {
    let local = LocalDynamo::start().await;
    local.create_users_table("users", 1000).await;
    local.put_items("users", padded_items(1600)).await;

    let recorder = Arc::new(RecordingClient::new(local.client()));
    let dir = export_dir("parallel_scan_reads_every_item_exactly_once_across_pages");
    let mut config = all_rules_config("users", &dir);
    config.segments = 4;
    config.gsi[0].check_missing = false;
    let outcome = scan(recorder.clone(), config).await;

    assert_eq!(outcome.snapshot.items_scanned, 1600);
    assert_eq!(outcome.snapshot.total_violations, 0);
    assert!(
        outcome.snapshot.per_segment_items.iter().all(|&n| n > 0),
        "every segment returns items: {:?}",
        outcome.snapshot.per_segment_items
    );

    let pages = recorder.pages();
    let segments: HashSet<u32> = pages.iter().map(|p| p.segment).collect();
    assert_eq!(segments, HashSet::from([0, 1, 2, 3]));
    assert!(
        pages.iter().any(|p| p.continued),
        "~6.4MB across 4 segments must paginate via LastEvaluatedKey"
    );
}

#[tokio::test]
async fn rate_limit_paces_scan_to_provisioned_capacity() {
    const RCU: i64 = 100;
    let local = LocalDynamo::start().await;
    local.create_users_table("users", RCU).await;
    local.put_items("users", padded_items(600)).await;

    let recorder = Arc::new(RecordingClient::new(local.client()));
    let dir = export_dir("rate_limit_paces_scan_to_provisioned_capacity");
    let mut config = scan_config("users", 1, &dir);
    config.rate_limit_percent = Some(100);
    let started = Instant::now();
    let outcome = scan(recorder.clone(), config).await;
    let elapsed = started.elapsed();

    let pages = recorder.pages();
    assert!(pages.len() >= 3, "~2.4MB must span at least 3 pages");
    let costs: Vec<f64> = pages
        .iter()
        .map(|p| {
            p.consumed_rcu
                .expect("DynamoDB Local reports consumed capacity")
        })
        .collect();

    assert_eq!(outcome.snapshot.consumed_rcu, costs.iter().sum::<f64>());

    // Each page is paid for before the next is issued, so the final page is free.
    let paid: f64 = costs[..costs.len() - 1].iter().sum();
    let minimum = Duration::from_secs_f64(paid / RCU as f64);
    assert!(
        elapsed >= minimum.mul_f64(0.95),
        "scan of {paid} paid RCU at {RCU} RCU/s took {elapsed:?}, expected at least {minimum:?}"
    );

    let last = pages.last().unwrap();
    let first = pages.first().unwrap();
    assert!(last.issued_at.duration_since(first.issued_at) >= minimum.mul_f64(0.95));
}
