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
use aws_sdk_dynamodb::types::{AttributeValue as SdkAttributeValue, ReturnConsumedCapacity};
use serde::{Deserialize, Serialize};

use crate::domain::{AttributeValue, Item, KeySchemaElement};

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

    async fn describe_table(&self, _name: &str) -> Result<TableDescription, AwsError> {
        Err(AwsError {
            code: "NotImplemented".to_string(),
            message: "DescribeTable schema discovery is delivered by task #13".to_string(),
            kind: AwsErrorKind::Other,
        })
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
