//! AWS client facade (PRD §8.2): DynamoClient trait over the SDK.
//!
//! Owns the discovered-schema contract ([`TableDescription`] and its parts),
//! produced from `DescribeTable`, and the request/response vocabulary the scan
//! driver and TUI drive it with. Downstream code holds `Arc<dyn DynamoClient>`
//! and never depends on the SDK types directly.

use std::collections::HashMap;
use std::fmt;

use async_trait::async_trait;
use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::{
    AttributeValue as SdkAttributeValue, BillingMode, KeySchemaElement as SdkKeySchemaElement,
    KeyType, ReturnConsumedCapacity, ScalarAttributeType, TableDescription as SdkTableDescription,
    TimeToLiveStatus,
};
use serde::{Deserialize, Serialize};

use crate::domain::{AttributeValue, Item, KeySchemaElement, TypeCode};

/// A table's schema as discovered via `DescribeTable` (PRD §8.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableDescription {
    pub name: String,
    pub key_schema: TableKeySchema,
    pub gsis: Vec<IndexSchema>,
    pub lsis: Vec<IndexSchema>,
    pub ttl: Option<TtlDescription>,
    /// Provisioned read capacity units; `None` for on-demand tables.
    pub provisioned_rcu: Option<u64>,
    /// Approximate item count, updated by DynamoDB roughly every 6 hours.
    pub item_count: u64,
}

/// A table's own primary key schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableKeySchema {
    pub pk: KeySchemaElement,
    pub sk: Option<KeySchemaElement>,
}

/// The key schema of a GSI or LSI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSchema {
    pub name: String,
    pub pk: KeySchemaElement,
    pub sk: Option<KeySchemaElement>,
}

/// A table's TTL configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtlDescription {
    pub attribute: String,
    pub enabled: bool,
}

/// A single segment page request for a parallel `Scan` (PRD §6.2.2).
#[derive(Debug, Clone, PartialEq)]
pub struct ScanRequest {
    pub table: String,
    /// Total number of parallel segments this scan is divided into.
    pub total_segments: u32,
    /// Zero-based index of the segment this request reads.
    pub segment: u32,
    /// Pagination cursor from the previous page's `last_evaluated_key`.
    pub exclusive_start_key: Option<Item>,
    /// When true, request `ReturnConsumedCapacity=TOTAL` for RCU metering.
    pub return_consumed_capacity: bool,
}

/// One page of a segment scan.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanResponse {
    pub items: Vec<Item>,
    /// Cursor for the next page; `None` when the segment is exhausted.
    pub last_evaluated_key: Option<Item>,
    /// Read capacity units consumed by this call; `None` unless requested.
    pub consumed_rcu: Option<f64>,
}

/// A point read of a single item by its primary key (PRD §6.3.5 drill-in).
#[derive(Debug, Clone, PartialEq)]
pub struct GetItemRequest {
    pub table: String,
    /// The item's full primary key: partition key and, if present, sort key.
    pub key: Item,
}

/// The AWS facade over DynamoDB (PRD §8.2).
///
/// The SDK's `AttributeValue` is converted to [`crate::domain::AttributeValue`]
/// at this boundary; consumers hold `Arc<dyn DynamoClient>` and stay
/// SDK-agnostic.
#[async_trait]
pub trait DynamoClient: Send + Sync {
    async fn list_tables(&self) -> Result<Vec<String>, AwsError>;

    async fn describe_table(&self, name: &str) -> Result<TableDescription, AwsError>;

    async fn scan_segment(&self, req: ScanRequest) -> Result<ScanResponse, AwsError>;

    /// Returns `None` when the item does not exist.
    async fn get_item(&self, req: GetItemRequest) -> Result<Option<Item>, AwsError>;
}

/// A failure from the AWS facade, carrying the SDK error code for display
/// (PRD §6.3.7) and a [`kind`](AwsError::kind) so the TUI can decide between a
/// terminal modal and silent retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsError {
    /// The SDK error code, e.g. `ResourceNotFoundException`.
    pub code: String,
    pub message: String,
    pub kind: AwsErrorKind,
}

/// How the application should react to an [`AwsError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwsErrorKind {
    /// Missing, invalid or expired credentials (e.g. SSO token lapsed).
    Auth,
    /// The named table or resource does not exist.
    NotFound,
    /// Credentials are valid but lack the required IAM permission.
    AccessDenied,
    /// Throttled beyond the SDK's retry budget.
    Throttling,
    /// Any other terminal failure.
    Other,
}

impl AwsError {
    /// A one-line remediation hint for the terminal error modal (PRD §6.3.7).
    /// `None` for [`AwsErrorKind::Other`], which has no generic fix.
    pub fn remediation(&self) -> Option<&'static str> {
        match self.kind {
            AwsErrorKind::Auth => Some(
                "Credentials are missing or expired. Run `aws sso login --profile <name>` \
                 (or refresh your credentials) and retry.",
            ),
            AwsErrorKind::NotFound => Some(
                "Check the table name and that you are targeting the right account and region.",
            ),
            AwsErrorKind::AccessDenied => Some(
                "Credentials lack the required IAM permission. Grant dynamodb:Scan, \
                 DescribeTable, GetItem and ListTables.",
            ),
            AwsErrorKind::Throttling => Some(
                "The table is throttling. Lower the rate-limit percentage or segment count and retry.",
            ),
            AwsErrorKind::Other => None,
        }
    }
}

impl fmt::Display for AwsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AwsError {}

/// The production [`DynamoClient`] backed by `aws-sdk-dynamodb`.
pub struct RealDynamoClient {
    client: aws_sdk_dynamodb::Client,
}

impl RealDynamoClient {
    /// Builds a client from the default credential provider chain (env, shared
    /// config, SSO, IMDS, container), honouring an optional profile and region
    /// override (PRD §6.4). Region falls back to the profile/environment when
    /// `None`.
    pub async fn new(profile: Option<&str>, region: Option<&str>) -> Self {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
        if let Some(profile) = profile {
            loader = loader.profile_name(profile);
        }

        if let Some(region) = region {
            loader = loader.region(aws_sdk_dynamodb::config::Region::new(region.to_string()));
        }

        let sdk_config = loader.load().await;
        Self {
            client: aws_sdk_dynamodb::Client::new(&sdk_config),
        }
    }

    /// Wraps an already-constructed SDK client (used when the caller owns
    /// `SdkConfig`, e.g. to share it across services).
    pub fn from_client(client: aws_sdk_dynamodb::Client) -> Self {
        Self { client }
    }

    /// Discovers the TTL attribute via `DescribeTimeToLive` (a separate API from
    /// `DescribeTable`). Degrades to `None` when the caller lacks
    /// `dynamodb:DescribeTimeToLive`, so a missing permission never blocks the
    /// rest of schema discovery.
    async fn describe_ttl(&self, name: &str) -> Result<Option<TtlDescription>, AwsError> {
        let out = match self
            .client
            .describe_time_to_live()
            .table_name(name)
            .send()
            .await
        {
            Ok(out) => out,
            Err(err) => {
                let mapped = map_sdk_error(err);
                if mapped.kind == AwsErrorKind::AccessDenied {
                    return Ok(None);
                }

                return Err(mapped);
            }
        };

        Ok(out.time_to_live_description.and_then(|desc| {
            let enabled = matches!(desc.time_to_live_status, Some(TimeToLiveStatus::Enabled));
            desc.attribute_name
                .map(|attribute| TtlDescription { attribute, enabled })
        }))
    }
}

#[async_trait]
impl DynamoClient for RealDynamoClient {
    async fn list_tables(&self) -> Result<Vec<String>, AwsError> {
        let mut names = Vec::new();
        let mut start = None;
        loop {
            let out = self
                .client
                .list_tables()
                .set_exclusive_start_table_name(start)
                .send()
                .await
                .map_err(map_sdk_error)?;
            names.extend(out.table_names.unwrap_or_default());
            match out.last_evaluated_table_name {
                Some(next) => start = Some(next),
                None => break,
            }
        }

        Ok(names)
    }

    async fn describe_table(&self, name: &str) -> Result<TableDescription, AwsError> {
        let out = self
            .client
            .describe_table()
            .table_name(name)
            .send()
            .await
            .map_err(map_sdk_error)?;
        let table = out
            .table
            .ok_or_else(|| malformed(format!("DescribeTable returned no table for `{name}`")))?;
        let ttl = self.describe_ttl(name).await?;
        map_table_description(table, ttl)
    }

    async fn scan_segment(&self, req: ScanRequest) -> Result<ScanResponse, AwsError> {
        let mut builder = self
            .client
            .scan()
            .table_name(req.table)
            .total_segments(req.total_segments as i32)
            .segment(req.segment as i32)
            .set_exclusive_start_key(req.exclusive_start_key.map(to_sdk_map));
        if req.return_consumed_capacity {
            builder = builder.return_consumed_capacity(ReturnConsumedCapacity::Total);
        }

        let out = builder.send().await.map_err(map_sdk_error)?;
        Ok(ScanResponse {
            items: out
                .items
                .unwrap_or_default()
                .into_iter()
                .map(from_sdk_map)
                .collect(),
            last_evaluated_key: out.last_evaluated_key.map(from_sdk_map),
            consumed_rcu: out.consumed_capacity.and_then(|c| c.capacity_units()),
        })
    }

    async fn get_item(&self, req: GetItemRequest) -> Result<Option<Item>, AwsError> {
        let out = self
            .client
            .get_item()
            .table_name(req.table)
            .set_key(Some(to_sdk_map(req.key)))
            .send()
            .await
            .map_err(map_sdk_error)?;
        Ok(out.item.map(from_sdk_map))
    }
}

fn to_sdk_map(item: Item) -> HashMap<String, SdkAttributeValue> {
    item.into_iter()
        .map(|(k, v)| (k, to_sdk_value(v)))
        .collect()
}

fn from_sdk_map(item: HashMap<String, SdkAttributeValue>) -> Item {
    item.into_iter()
        .map(|(k, v)| (k, from_sdk_value(v)))
        .collect()
}

fn to_sdk_value(value: AttributeValue) -> SdkAttributeValue {
    match value {
        AttributeValue::S(s) => SdkAttributeValue::S(s),
        AttributeValue::N(n) => SdkAttributeValue::N(n),
        AttributeValue::B(b) => SdkAttributeValue::B(Blob::new(b)),
        AttributeValue::Bool(b) => SdkAttributeValue::Bool(b),
        AttributeValue::Null(n) => SdkAttributeValue::Null(n),
        AttributeValue::M(m) => {
            SdkAttributeValue::M(m.into_iter().map(|(k, v)| (k, to_sdk_value(v))).collect())
        }
        AttributeValue::L(l) => SdkAttributeValue::L(l.into_iter().map(to_sdk_value).collect()),
        AttributeValue::Ss(s) => SdkAttributeValue::Ss(s),
        AttributeValue::Ns(n) => SdkAttributeValue::Ns(n),
        AttributeValue::Bs(b) => SdkAttributeValue::Bs(b.into_iter().map(Blob::new).collect()),
    }
}

fn from_sdk_value(value: SdkAttributeValue) -> AttributeValue {
    match value {
        SdkAttributeValue::S(s) => AttributeValue::S(s),
        SdkAttributeValue::N(n) => AttributeValue::N(n),
        SdkAttributeValue::B(b) => AttributeValue::B(b.into_inner()),
        SdkAttributeValue::Bool(b) => AttributeValue::Bool(b),
        SdkAttributeValue::Null(n) => AttributeValue::Null(n),
        SdkAttributeValue::M(m) => {
            AttributeValue::M(m.into_iter().map(|(k, v)| (k, from_sdk_value(v))).collect())
        }
        SdkAttributeValue::L(l) => AttributeValue::L(l.into_iter().map(from_sdk_value).collect()),
        SdkAttributeValue::Ss(s) => AttributeValue::Ss(s),
        SdkAttributeValue::Ns(n) => AttributeValue::Ns(n),
        SdkAttributeValue::Bs(b) => {
            AttributeValue::Bs(b.into_iter().map(Blob::into_inner).collect())
        }
        other => unreachable!("unexpected DynamoDB attribute value: {other:?}"),
    }
}

fn map_sdk_error<E, R>(err: SdkError<E, R>) -> AwsError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug,
{
    match &err {
        SdkError::ConstructionFailure(_) | SdkError::DispatchFailure(_) => AwsError {
            code: "CredentialsError".to_string(),
            message: format!("could not sign or dispatch the request: {err}"),
            kind: AwsErrorKind::Auth,
        },
        SdkError::TimeoutError(_) => AwsError {
            code: "TimeoutError".to_string(),
            message: "the request timed out after the SDK retry budget was exhausted".to_string(),
            kind: AwsErrorKind::Throttling,
        },
        _ => {
            let code = err.code().unwrap_or("UnknownError").to_string();
            let message = err
                .message()
                .map(str::to_string)
                .unwrap_or_else(|| err.to_string());
            let kind = classify_code(&code);
            AwsError {
                code,
                message,
                kind,
            }
        }
    }
}

fn classify_code(code: &str) -> AwsErrorKind {
    match code {
        "ResourceNotFoundException" => AwsErrorKind::NotFound,
        "AccessDeniedException" | "UnrecognizedClientException" => AwsErrorKind::AccessDenied,
        "ExpiredTokenException" | "ExpiredToken" | "InvalidSignatureException" => {
            AwsErrorKind::Auth
        }
        "ProvisionedThroughputExceededException"
        | "ThrottlingException"
        | "RequestLimitExceeded" => AwsErrorKind::Throttling,
        _ => AwsErrorKind::Other,
    }
}

/// Maps a raw `DescribeTable` result (plus separately-fetched TTL) into the
/// crate's [`TableDescription`] (PRD §6.3.3): index key schemas resolved to
/// scalar type codes, provisioned RCU snapshotted (`None` for on-demand) and
/// the approximate item count for progress estimation.
fn map_table_description(
    table: SdkTableDescription,
    ttl: Option<TtlDescription>,
) -> Result<TableDescription, AwsError> {
    let name = table
        .table_name
        .ok_or_else(|| malformed("DescribeTable returned no table name".to_string()))?;

    let types = table
        .attribute_definitions
        .unwrap_or_default()
        .into_iter()
        .map(|def| {
            Ok((
                def.attribute_name,
                scalar_to_type_code(&def.attribute_type)?,
            ))
        })
        .collect::<Result<HashMap<String, TypeCode>, AwsError>>()?;

    let (pk, sk) = resolve_key_schema(
        table.key_schema.unwrap_or_default(),
        &types,
        "table key schema",
    )?;

    let gsis = table
        .global_secondary_indexes
        .unwrap_or_default()
        .into_iter()
        .map(|gsi| resolve_index(gsi.index_name, gsi.key_schema, &types, "GSI"))
        .collect::<Result<Vec<_>, AwsError>>()?;

    let lsis = table
        .local_secondary_indexes
        .unwrap_or_default()
        .into_iter()
        .map(|lsi| resolve_index(lsi.index_name, lsi.key_schema, &types, "LSI"))
        .collect::<Result<Vec<_>, AwsError>>()?;

    let on_demand = matches!(
        table.billing_mode_summary.and_then(|b| b.billing_mode),
        Some(BillingMode::PayPerRequest)
    );
    let provisioned_rcu = if on_demand {
        None
    } else {
        table
            .provisioned_throughput
            .and_then(|p| p.read_capacity_units)
            .filter(|&rcu| rcu > 0)
            .map(|rcu| rcu as u64)
    };

    Ok(TableDescription {
        name,
        key_schema: TableKeySchema { pk, sk },
        gsis,
        lsis,
        ttl,
        provisioned_rcu,
        item_count: table.item_count.unwrap_or(0).max(0) as u64,
    })
}

fn resolve_index(
    index_name: Option<String>,
    key_schema: Option<Vec<SdkKeySchemaElement>>,
    types: &HashMap<String, TypeCode>,
    kind: &str,
) -> Result<IndexSchema, AwsError> {
    let name = index_name.ok_or_else(|| malformed(format!("{kind} has no name")))?;
    let (pk, sk) = resolve_key_schema(
        key_schema.unwrap_or_default(),
        types,
        &format!("{kind} `{name}`"),
    )?;
    Ok(IndexSchema { name, pk, sk })
}

fn resolve_key_schema(
    key_schema: Vec<SdkKeySchemaElement>,
    types: &HashMap<String, TypeCode>,
    context: &str,
) -> Result<(KeySchemaElement, Option<KeySchemaElement>), AwsError> {
    let mut pk = None;
    let mut sk = None;
    for element in key_schema {
        let name = element.attribute_name;
        let type_code = *types.get(&name).ok_or_else(|| {
            malformed(format!(
                "{context}: key attribute `{name}` missing from attribute definitions"
            ))
        })?;
        let resolved = KeySchemaElement { name, type_code };
        match element.key_type {
            KeyType::Hash => pk = Some(resolved),
            KeyType::Range => sk = Some(resolved),
            other => {
                return Err(malformed(format!(
                    "{context}: unexpected key type `{other:?}`"
                )));
            }
        }
    }

    let pk = pk.ok_or_else(|| malformed(format!("{context}: no partition key")))?;
    Ok((pk, sk))
}

fn scalar_to_type_code(attribute_type: &ScalarAttributeType) -> Result<TypeCode, AwsError> {
    match attribute_type {
        ScalarAttributeType::S => Ok(TypeCode::S),
        ScalarAttributeType::N => Ok(TypeCode::N),
        ScalarAttributeType::B => Ok(TypeCode::B),
        other => Err(malformed(format!(
            "unsupported key attribute type `{other:?}`"
        ))),
    }
}

fn malformed(message: String) -> AwsError {
    AwsError {
        code: "MalformedDescribeTable".to_string(),
        message,
        kind: AwsErrorKind::Other,
    }
}

#[cfg(test)]
pub mod mock {
    //! A scriptable [`DynamoClient`] for unit tests (PRD §8.2/§8.3).
    //!
    //! Scan responses are queued per segment and popped in call order, so tests
    //! drive pagination, throttling and partial failures by scripting the queue.
    //! Every call is recorded for assertions on segment fan-out, cursor threading
    //! and consumed-capacity requests.

    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::{
        AwsError, AwsErrorKind, DynamoClient, GetItemRequest, ScanRequest, ScanResponse,
        TableDescription,
    };
    use crate::domain::Item;

    #[derive(Default)]
    pub struct MockDynamoClient {
        tables: Vec<String>,
        describe: HashMap<String, Result<TableDescription, AwsError>>,
        scan_pages: Mutex<HashMap<u32, VecDeque<Result<ScanResponse, AwsError>>>>,
        get_item_responses: Mutex<VecDeque<Result<Option<Item>, AwsError>>>,
        scan_calls: Mutex<Vec<ScanRequest>>,
        describe_calls: Mutex<Vec<String>>,
        list_calls: Mutex<u32>,
        get_item_calls: Mutex<Vec<GetItemRequest>>,
    }

    impl MockDynamoClient {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with_tables<I, S>(mut self, tables: I) -> Self
        where
            I: IntoIterator<Item = S>,
            S: Into<String>,
        {
            self.tables = tables.into_iter().map(Into::into).collect();
            self
        }

        pub fn with_describe(mut self, name: &str, desc: TableDescription) -> Self {
            self.describe.insert(name.to_string(), Ok(desc));
            self
        }

        pub fn with_describe_err(mut self, name: &str, err: AwsError) -> Self {
            self.describe.insert(name.to_string(), Err(err));
            self
        }

        /// Queues the pages returned for `segment`, in call order. The final page
        /// must carry `last_evaluated_key: None` to terminate the segment.
        pub fn with_scan_pages<I>(mut self, segment: u32, pages: I) -> Self
        where
            I: IntoIterator<Item = Result<ScanResponse, AwsError>>,
        {
            self.scan_pages
                .get_mut()
                .expect("mutex not poisoned before build")
                .insert(segment, pages.into_iter().collect());
            self
        }

        /// Queues one `get_item` response, returned in call order.
        pub fn with_get_item(mut self, response: Result<Option<Item>, AwsError>) -> Self {
            self.get_item_responses
                .get_mut()
                .expect("mutex not poisoned before build")
                .push_back(response);
            self
        }

        pub fn recorded_scans(&self) -> Vec<ScanRequest> {
            self.scan_calls.lock().unwrap().clone()
        }

        pub fn recorded_describes(&self) -> Vec<String> {
            self.describe_calls.lock().unwrap().clone()
        }

        pub fn recorded_get_items(&self) -> Vec<GetItemRequest> {
            self.get_item_calls.lock().unwrap().clone()
        }

        pub fn list_tables_call_count(&self) -> u32 {
            *self.list_calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl DynamoClient for MockDynamoClient {
        async fn list_tables(&self) -> Result<Vec<String>, AwsError> {
            *self.list_calls.lock().unwrap() += 1;
            Ok(self.tables.clone())
        }

        async fn describe_table(&self, name: &str) -> Result<TableDescription, AwsError> {
            self.describe_calls.lock().unwrap().push(name.to_string());
            match self.describe.get(name) {
                Some(Ok(desc)) => Ok(desc.clone()),
                Some(Err(err)) => Err(err.clone()),
                None => Err(AwsError {
                    code: "ResourceNotFoundException".to_string(),
                    message: format!("no describe fixture for table `{name}`"),
                    kind: AwsErrorKind::NotFound,
                }),
            }
        }

        async fn scan_segment(&self, req: ScanRequest) -> Result<ScanResponse, AwsError> {
            let segment = req.segment;
            self.scan_calls.lock().unwrap().push(req);
            let mut pages = self.scan_pages.lock().unwrap();
            match pages.get_mut(&segment) {
                Some(queue) => queue.pop_front().unwrap_or_else(|| {
                    Err(AwsError {
                        code: "MockExhausted".to_string(),
                        message: format!("segment {segment} scanned past its scripted pages"),
                        kind: AwsErrorKind::Other,
                    })
                }),
                None => Ok(ScanResponse {
                    items: Vec::new(),
                    last_evaluated_key: None,
                    consumed_rcu: None,
                }),
            }
        }

        async fn get_item(&self, req: GetItemRequest) -> Result<Option<Item>, AwsError> {
            self.get_item_calls.lock().unwrap().push(req);
            self.get_item_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(None))
        }
    }
}

#[cfg(test)]
mod tests {
    use aws_sdk_dynamodb::types::{
        AttributeDefinition, GlobalSecondaryIndexDescription, KeySchemaElement as SdkKse,
        LocalSecondaryIndexDescription, ProvisionedThroughputDescription,
    };
    use aws_sdk_dynamodb::types::{
        BillingMode as SdkBillingMode, BillingModeSummary, KeyType as SdkKeyType,
        ScalarAttributeType as SdkScalar, TableDescription as SdkTable,
    };

    use super::mock::MockDynamoClient;
    use super::*;
    use crate::domain::{AttributeValue, TypeCode};

    fn attr(name: &str, ty: SdkScalar) -> AttributeDefinition {
        AttributeDefinition::builder()
            .attribute_name(name)
            .attribute_type(ty)
            .build()
            .unwrap()
    }

    fn key(name: &str, key_type: SdkKeyType) -> SdkKse {
        SdkKse::builder()
            .attribute_name(name)
            .key_type(key_type)
            .build()
            .unwrap()
    }

    fn provisioned_table() -> SdkTable {
        SdkTable::builder()
            .table_name("users")
            .item_count(1000)
            .set_attribute_definitions(Some(vec![
                attr("id", SdkScalar::S),
                attr("createdAt", SdkScalar::N),
                attr("email", SdkScalar::S),
            ]))
            .key_schema(key("id", SdkKeyType::Hash))
            .global_secondary_indexes(
                GlobalSecondaryIndexDescription::builder()
                    .index_name("email-index")
                    .key_schema(key("email", SdkKeyType::Hash))
                    .key_schema(key("createdAt", SdkKeyType::Range))
                    .build(),
            )
            .local_secondary_indexes(
                LocalSecondaryIndexDescription::builder()
                    .index_name("createdAt-lsi")
                    .key_schema(key("id", SdkKeyType::Hash))
                    .key_schema(key("createdAt", SdkKeyType::Range))
                    .build(),
            )
            .provisioned_throughput(
                ProvisionedThroughputDescription::builder()
                    .read_capacity_units(120)
                    .write_capacity_units(10)
                    .build(),
            )
            .build()
    }

    #[test]
    fn maps_provisioned_table_schema_with_index_types() {
        let ttl = Some(TtlDescription {
            attribute: "expiresAt".to_string(),
            enabled: true,
        });
        let mapped = map_table_description(provisioned_table(), ttl.clone()).unwrap();

        assert_eq!(mapped.name, "users");
        assert_eq!(mapped.item_count, 1000);
        assert_eq!(mapped.provisioned_rcu, Some(120));
        assert_eq!(mapped.ttl, ttl);

        assert_eq!(mapped.key_schema.pk.type_code, TypeCode::S);
        assert!(mapped.key_schema.sk.is_none());

        let gsi = &mapped.gsis[0];
        assert_eq!(gsi.name, "email-index");
        assert_eq!(gsi.pk.type_code, TypeCode::S);
        assert_eq!(gsi.sk.as_ref().unwrap().type_code, TypeCode::N);

        let lsi = &mapped.lsis[0];
        assert_eq!(lsi.name, "createdAt-lsi");
        assert_eq!(lsi.sk.as_ref().unwrap().name, "createdAt");
    }

    #[test]
    fn on_demand_table_has_no_provisioned_rcu() {
        let table = SdkTable::builder()
            .table_name("events")
            .set_attribute_definitions(Some(vec![attr("id", SdkScalar::S)]))
            .key_schema(key("id", SdkKeyType::Hash))
            .billing_mode_summary(
                BillingModeSummary::builder()
                    .billing_mode(SdkBillingMode::PayPerRequest)
                    .build(),
            )
            .build();
        let mapped = map_table_description(table, None).unwrap();
        assert_eq!(mapped.provisioned_rcu, None);
    }

    #[test]
    fn key_attribute_without_type_definition_is_malformed() {
        let table = SdkTable::builder()
            .table_name("broken")
            .key_schema(key("id", SdkKeyType::Hash))
            .build();
        let err = map_table_description(table, None).unwrap_err();
        assert_eq!(err.kind, AwsErrorKind::Other);
        assert_eq!(err.code, "MalformedDescribeTable");
    }

    fn page(rcu: f64, next: Option<&str>) -> ScanResponse {
        ScanResponse {
            items: vec![
                [("pk".to_string(), AttributeValue::S("a".to_string()))]
                    .into_iter()
                    .collect(),
            ],
            last_evaluated_key: next.map(|k| {
                [("pk".to_string(), AttributeValue::S(k.to_string()))]
                    .into_iter()
                    .collect()
            }),
            consumed_rcu: Some(rcu),
        }
    }

    fn sample_table() -> TableDescription {
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
            provisioned_rcu: Some(100),
            item_count: 42,
        }
    }

    #[tokio::test]
    async fn scan_pages_pop_in_order_and_record_calls() {
        let client = MockDynamoClient::new()
            .with_scan_pages(0, [Ok(page(2.0, Some("cursor"))), Ok(page(1.0, None))]);

        let first = client
            .scan_segment(ScanRequest {
                table: "t".to_string(),
                total_segments: 1,
                segment: 0,
                exclusive_start_key: None,
                return_consumed_capacity: true,
            })
            .await
            .unwrap();
        assert_eq!(first.consumed_rcu, Some(2.0));
        assert!(first.last_evaluated_key.is_some());

        let second = client
            .scan_segment(ScanRequest {
                table: "t".to_string(),
                total_segments: 1,
                segment: 0,
                exclusive_start_key: first.last_evaluated_key.clone(),
                return_consumed_capacity: true,
            })
            .await
            .unwrap();
        assert!(second.last_evaluated_key.is_none());

        let recorded = client.recorded_scans();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[1].exclusive_start_key, first.last_evaluated_key);
    }

    #[tokio::test]
    async fn exhausting_a_scripted_segment_errors_loudly() {
        let client = MockDynamoClient::new().with_scan_pages(0, [Ok(page(1.0, None))]);
        let req = || ScanRequest {
            table: "t".to_string(),
            total_segments: 1,
            segment: 0,
            exclusive_start_key: None,
            return_consumed_capacity: false,
        };
        client.scan_segment(req()).await.unwrap();

        let err = client.scan_segment(req()).await.unwrap_err();
        assert_eq!(err.kind, AwsErrorKind::Other);
    }

    #[tokio::test]
    async fn unscripted_segment_returns_empty_terminal_page() {
        let client = MockDynamoClient::new();
        let resp = client
            .scan_segment(ScanRequest {
                table: "t".to_string(),
                total_segments: 4,
                segment: 3,
                exclusive_start_key: None,
                return_consumed_capacity: false,
            })
            .await
            .unwrap();
        assert!(resp.items.is_empty());
        assert!(resp.last_evaluated_key.is_none());
    }

    #[tokio::test]
    async fn injected_throttle_surfaces_as_error() {
        let throttle = AwsError {
            code: "ProvisionedThroughputExceededException".to_string(),
            message: "slow down".to_string(),
            kind: AwsErrorKind::Throttling,
        };
        let client = MockDynamoClient::new().with_scan_pages(0, [Err(throttle)]);
        let err = client
            .scan_segment(ScanRequest {
                table: "t".to_string(),
                total_segments: 1,
                segment: 0,
                exclusive_start_key: None,
                return_consumed_capacity: false,
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind, AwsErrorKind::Throttling);
    }

    #[tokio::test]
    async fn describe_returns_fixture_or_not_found() {
        let client = MockDynamoClient::new().with_describe("users", sample_table());
        assert_eq!(client.describe_table("users").await.unwrap().item_count, 42);

        let err = client.describe_table("missing").await.unwrap_err();
        assert_eq!(err.kind, AwsErrorKind::NotFound);
        assert_eq!(client.recorded_describes(), vec!["users", "missing"]);
    }

    #[tokio::test]
    async fn list_tables_returns_fixture_and_counts_calls() {
        let client = MockDynamoClient::new().with_tables(["a", "b"]);
        assert_eq!(client.list_tables().await.unwrap(), vec!["a", "b"]);
        client.list_tables().await.unwrap();
        assert_eq!(client.list_tables_call_count(), 2);
    }

    #[tokio::test]
    async fn get_item_pops_queue_then_defaults_to_none() {
        let item: Item = [("pk".to_string(), AttributeValue::S("x".to_string()))]
            .into_iter()
            .collect();
        let client = MockDynamoClient::new().with_get_item(Ok(Some(item.clone())));
        let req = || GetItemRequest {
            table: "t".to_string(),
            key: item.clone(),
        };
        assert_eq!(client.get_item(req()).await.unwrap(), Some(item.clone()));
        assert_eq!(client.get_item(req()).await.unwrap(), None);
        assert_eq!(client.recorded_get_items().len(), 2);
    }
}
