//! Scan pipeline: one scan's lifecycle from rule assembly to closed exports.
//!
//! Fans the scan out, evaluates each scanned item against the rule set and
//! streams its violations to the export writers and the state aggregator. The
//! TUI event loop and the integration tests both drive a scan through
//! [`Pipeline`], so they exercise the identical path.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::assemble::{AssembleError, assemble};
use crate::aws::{AwsError, DynamoClient, TableDescription, TableKeySchema};
use crate::config::ScanConfig;
use crate::domain::{AttributeValue, Item, KeyAttribute};
use crate::export::{ExportError, ExportWriter, open_writers};
use crate::rules::{ItemViolations, RuleSet, Violation, check_item};
use crate::scan::{ScanStream, ScannedItem, run_scan};
use crate::state::{Aggregator, StateSnapshot};

/// A failure setting up a scan before any item is read.
#[derive(Debug)]
pub enum PipelineError {
    Assemble(AssembleError),
    Export(ExportError),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineError::Assemble(err) => write!(f, "{err}"),
            PipelineError::Export(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PipelineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PipelineError::Assemble(err) => Some(err),
            PipelineError::Export(err) => Some(err),
        }
    }
}

impl From<AssembleError> for PipelineError {
    fn from(err: AssembleError) -> Self {
        PipelineError::Assemble(err)
    }
}

impl From<ExportError> for PipelineError {
    fn from(err: ExportError) -> Self {
        PipelineError::Export(err)
    }
}

/// A running or finished scan: the item stream, export writers and aggregator.
///
/// Poll [`next`](Pipeline::next) and feed each item to
/// [`process`](Pipeline::process) until it returns `None`, then call
/// [`finish`](Pipeline::finish) to close the exports.
pub struct Pipeline {
    stream: Option<ScanStream>,
    writer: Option<Box<dyn ExportWriter>>,
    aggregator: Aggregator,
    rules: RuleSet,
    table_key: TableKeySchema,
    detected_at: i64,
    export_paths: Vec<PathBuf>,
    consumed_synced: f64,
}

impl Pipeline {
    /// Assemble the rules, open the export writers and fan the scan out.
    /// Export paths must already be resolved.
    pub fn start(
        description: &TableDescription,
        config: &ScanConfig,
        client: Arc<dyn DynamoClient>,
    ) -> Result<Self, PipelineError> {
        let rules = assemble(description, config)?;
        let (writer, export_paths) = open_writers(&config.export)?;
        Ok(Self::with_writer(
            description,
            config,
            client,
            rules,
            writer,
            export_paths,
        ))
    }

    fn with_writer(
        description: &TableDescription,
        config: &ScanConfig,
        client: Arc<dyn DynamoClient>,
        rules: RuleSet,
        writer: Box<dyn ExportWriter>,
        export_paths: Vec<PathBuf>,
    ) -> Self {
        Self {
            stream: Some(run_scan(config, client, description.provisioned_rcu)),
            writer: Some(writer),
            aggregator: Aggregator::with_system_clock(
                config.segments as u32,
                description.item_count,
            ),
            rules,
            table_key: description.key_schema.clone(),
            detected_at: now_epoch_secs(),
            export_paths,
            consumed_synced: 0.0,
        }
    }

    /// True until [`finish`](Pipeline::finish) is called.
    pub fn is_running(&self) -> bool {
        self.stream.is_some()
    }

    /// The next scanned item, or `None` once every segment has terminated.
    pub async fn next(&mut self) -> Option<Result<ScannedItem, AwsError>> {
        self.stream.as_mut()?.next().await
    }

    /// Evaluate one scanned item and stream any violations to disk and the
    /// aggregator.
    pub fn process(&mut self, scanned: ScannedItem) -> Result<(), ExportError> {
        self.aggregator.record_item(scanned.segment);
        let violations = check_item(&scanned.item, &self.rules, self.detected_at);
        if violations.is_empty() {
            return Ok(());
        }

        let group = build_group(
            &self.rules.table,
            &self.table_key,
            scanned.item,
            violations,
            self.detected_at,
        );
        for violation in &group.violations {
            self.aggregator
                .record_violation(&group.pk, group.sk.as_ref(), violation);
        }

        match &mut self.writer {
            Some(writer) => writer.write(&group),
            None => Ok(()),
        }
    }

    /// Stop issuing scans; items already fetched still drain through `next`.
    pub fn cancel(&self) {
        if let Some(stream) = &self.stream {
            stream.cancel();
        }
    }

    /// Current progress, including capacity consumed so far.
    pub fn snapshot(&mut self) -> StateSnapshot {
        self.sync_consumed();
        self.aggregator.snapshot()
    }

    /// Stop the scan and close the export writers.
    pub fn finish(&mut self) -> Result<(), ExportError> {
        self.sync_consumed();
        self.stream = None;
        match self.writer.take() {
            Some(writer) => writer.close(),
            None => Ok(()),
        }
    }

    pub fn export_paths(&self) -> &[PathBuf] {
        &self.export_paths
    }

    fn sync_consumed(&mut self) {
        let Some(stream) = &self.stream else { return };
        let total = stream.consumed_rcu();
        let delta = total - self.consumed_synced;
        if delta != 0.0 {
            self.aggregator.record_consumed(0, delta);
            self.consumed_synced = total;
        }
    }
}

/// Current wall-clock time as Unix epoch seconds, used to stamp violations and
/// as the TTL "now".
pub fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Group an item's violations for export, extracting the table's own primary key
/// so the detail view can later re-fetch the item by key.
fn build_group(
    table: &str,
    table_key: &TableKeySchema,
    item: Item,
    violations: Vec<Violation>,
    detected_at: i64,
) -> ItemViolations {
    let pk = key_attribute(&table_key.pk.name, &item);
    let sk = table_key.sk.as_ref().and_then(|element| {
        item.get(&element.name).map(|value| KeyAttribute {
            name: element.name.clone(),
            value: value.clone(),
        })
    });

    ItemViolations {
        table: table.to_string(),
        pk,
        sk,
        item,
        violations,
        detected_at,
    }
}

/// The named key attribute of an item. A scanned item always carries the table's
/// primary key; a null placeholder guards the impossible absent case rather than
/// panicking mid-scan.
fn key_attribute(name: &str, item: &Item) -> KeyAttribute {
    KeyAttribute {
        name: name.to_string(),
        value: item
            .get(name)
            .cloned()
            .unwrap_or(AttributeValue::Null(true)),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::aws::mock::MockDynamoClient;
    use crate::aws::{IndexSchema, ScanResponse, TtlDescription};
    use crate::config::{ExportConfig, GsiEntry, LsiEntry, TtlSettings};
    use crate::domain::{KeySchemaElement, TypeCode};
    use crate::export::{CsvWriter, FanOutWriter, NdjsonWriter};
    use crate::rules::ViolationCategory;

    /// A `Write` sink over a shared buffer, so a test can read exactly what an
    /// export writer produced after the writer is closed.
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn new() -> Self {
            SharedBuf(Arc::new(Mutex::new(Vec::new())))
        }

        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn element(name: &str, type_code: TypeCode) -> KeySchemaElement {
        KeySchemaElement {
            name: name.to_string(),
            type_code,
        }
    }

    fn s(value: &str) -> AttributeValue {
        AttributeValue::S(value.to_string())
    }

    fn item(pairs: &[(&str, AttributeValue)]) -> Item {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn description() -> TableDescription {
        TableDescription {
            name: "users".to_string(),
            key_schema: TableKeySchema {
                pk: element("id", TypeCode::S),
                sk: None,
            },
            gsis: vec![IndexSchema {
                name: "byEmail".to_string(),
                pk: element("email", TypeCode::S),
                sk: None,
            }],
            lsis: Vec::new(),
            ttl: Some(TtlDescription {
                attribute: "expiresAt".to_string(),
                enabled: true,
            }),
            provisioned_rcu: None,
            item_count: 3,
            table_size_bytes: 0,
        }
    }

    fn config() -> ScanConfig {
        ScanConfig {
            table: "users".to_string(),
            region: None,
            profile: None,
            segments: 1,
            rate_limit_percent: None,
            export: ExportConfig {
                csv: true,
                csv_path: None,
                ndjson: true,
                ndjson_path: None,
            },
            gsi: vec![GsiEntry {
                name: "byEmail".to_string(),
                hypothetical: false,
                pk: None,
                sk: None,
                check_missing: true,
            }],
            lsi: Vec::<LsiEntry>::new(),
            ttl: Some(TtlSettings {
                enabled: Some(true),
                ..TtlSettings::default()
            }),
        }
    }

    fn page(items: Vec<Item>, next: Option<Item>) -> ScanResponse {
        ScanResponse {
            items,
            last_evaluated_key: next,
            consumed_rcu: Some(1.0),
        }
    }

    #[tokio::test]
    async fn end_to_end_scan_detects_and_exports_violations() {
        let good = item(&[
            ("id", s("u1")),
            ("email", s("a@example.com")),
            ("expiresAt", AttributeValue::N("1700000000".to_string())),
        ]);
        // Missing the `email` GSI key and a wrong-typed TTL attribute.
        let bad = item(&[("id", s("u2")), ("expiresAt", s("not-a-number"))]);

        let client = Arc::new(
            MockDynamoClient::new()
                .with_describe("users", description())
                .with_scan_pages(0, [Ok(page(vec![good, bad], None))]),
        );

        let config = config();
        let rules = assemble(&description(), &config).unwrap();
        let csv = SharedBuf::new();
        let ndjson = SharedBuf::new();
        let writers: Vec<Box<dyn ExportWriter>> = vec![
            Box::new(CsvWriter::new(csv.clone()).unwrap()),
            Box::new(NdjsonWriter::new(ndjson.clone())),
        ];
        let mut pipeline = Pipeline::with_writer(
            &description(),
            &config,
            client,
            rules,
            Box::new(FanOutWriter::new(writers)),
            Vec::new(),
        );

        while let Some(next) = pipeline.next().await {
            pipeline.process(next.unwrap()).unwrap();
        }
        pipeline.finish().unwrap();

        let snapshot = pipeline.snapshot();
        assert!(!pipeline.is_running());
        assert_eq!(snapshot.consumed_rcu, 1.0);
        assert_eq!(snapshot.items_scanned, 2);
        assert_eq!(snapshot.total_violations, 2);
        assert_eq!(
            snapshot.category_counts.get(&ViolationCategory::MissingKey),
            Some(&1)
        );
        assert_eq!(
            snapshot
                .category_counts
                .get(&ViolationCategory::TtlWrongType),
            Some(&1)
        );
        assert!(
            snapshot
                .recent_violations
                .iter()
                .all(|recent| recent.pk.value == s("u2") && recent.sk.is_none()),
            "the feed keys each violation to its item"
        );

        let csv = csv.contents();
        assert!(
            csv.lines()
                .next()
                .unwrap()
                .starts_with("table,target,category")
        );
        assert!(csv.contains("byEmail"));
        assert!(csv.contains("u2"));

        let ndjson = ndjson.contents();
        assert_eq!(
            ndjson.lines().count(),
            1,
            "only the violating item is written"
        );
        assert!(ndjson.contains("\"u2\""));
    }

    #[test]
    fn build_group_extracts_table_key() {
        let table_key = TableKeySchema {
            pk: element("id", TypeCode::S),
            sk: Some(element("ts", TypeCode::N)),
        };
        let it = item(&[("id", s("u9")), ("ts", AttributeValue::N("42".to_string()))]);
        let group = build_group("users", &table_key, it, Vec::new(), 100);

        assert_eq!(group.pk.name, "id");
        assert_eq!(group.pk.value, s("u9"));
        assert_eq!(group.sk.unwrap().value, AttributeValue::N("42".to_string()));
        assert_eq!(group.detected_at, 100);
    }
}
