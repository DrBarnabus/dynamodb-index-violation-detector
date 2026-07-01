//! AWS client facade (PRD §8.2): DynamoClient trait over the SDK.
//!
//! Owns the discovered-schema contract ([`TableDescription`] and its parts),
//! produced from `DescribeTable`, and the request/response vocabulary the scan
//! driver and TUI drive it with. Downstream code holds `Arc<dyn DynamoClient>`
//! and never depends on the SDK types directly.

use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::{Item, KeySchemaElement};

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

impl fmt::Display for AwsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AwsError {}
