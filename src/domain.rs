//! Core domain types shared across every module (PRD §8).
//!
//! Pure data: definitions and serde derives only, no behaviour. The AWS facade
//! converts the SDK's `AttributeValue` into [`AttributeValue`] at the boundary so
//! the rest of the crate never depends on the SDK types.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A DynamoDB item: its attribute map keyed by attribute name.
pub type Item = HashMap<String, AttributeValue>;

/// A DynamoDB attribute value in the crate's own representation.
///
/// Numbers are carried as strings, matching the DynamoDB wire format. Binary is
/// raw bytes; export writers are responsible for base64 encoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AttributeValue {
    #[serde(rename = "S")]
    S(String),
    #[serde(rename = "N")]
    N(String),
    #[serde(rename = "B")]
    B(Vec<u8>),
    #[serde(rename = "BOOL")]
    Bool(bool),
    #[serde(rename = "NULL")]
    Null(bool),
    #[serde(rename = "M")]
    M(HashMap<String, AttributeValue>),
    #[serde(rename = "L")]
    L(Vec<AttributeValue>),
    #[serde(rename = "SS")]
    Ss(Vec<String>),
    #[serde(rename = "NS")]
    Ns(Vec<String>),
    #[serde(rename = "BS")]
    Bs(Vec<Vec<u8>>),
}

/// The scalar key-attribute type codes usable in an index key schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeCode {
    S,
    N,
    B,
}

/// A single key-schema element: the attribute name and its declared scalar type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeySchemaElement {
    pub name: String,
    #[serde(rename = "type")]
    pub type_code: TypeCode,
}

/// The target bucket a violation belongs to (PRD §6.1.4 output hierarchy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    Gsi(String),
    Lsi(String),
    Ttl,
}

/// The category of a detected violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViolationCategory {
    TypeMismatch,
    SizeExceeded,
    MissingKey,
    TtlMissing,
    TtlWrongType,
    TtlMsMagnitude,
    TtlMalformed,
    TtlPastFiveYears,
}

/// A single violation of one rule against one item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    pub target: Target,
    pub category: ViolationCategory,
    pub attribute: Option<String>,
    pub actual_value: Option<String>,
    pub actual_type: Option<String>,
    pub expected_type: Option<TypeCode>,
    pub size_bytes: Option<usize>,
}

/// All violations detected against one item, grouped for export (PRD §6.6).
///
/// Carries the table's own primary key (used to re-fetch the item in the detail
/// view) and the full item for NDJSON and full-JSON clipboard copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemViolations {
    pub table: String,
    pub pk: KeyAttribute,
    pub sk: Option<KeyAttribute>,
    pub item: Item,
    pub violations: Vec<Violation>,
    pub detected_at: i64,
}

/// A key attribute of a scanned item: its name and native value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyAttribute {
    pub name: String,
    pub value: AttributeValue,
}

/// The resolved set of checks a single scan will run (PRD §8.1 input).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleSet {
    pub table: String,
    pub gsis: Vec<GsiRule>,
    pub lsis: Vec<LsiRule>,
    pub ttl: Option<TtlRule>,
}

/// A GSI to check, existing or hypothetical, with its resolved key schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GsiRule {
    pub name: String,
    pub hypothetical: bool,
    pub pk: KeySchemaElement,
    pub sk: Option<KeySchemaElement>,
    pub check_missing: bool,
}

/// An LSI to check. The partition key equals the table's; only the sort key can
/// be missing, so type and size checks do not apply (PRD §6.1.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LsiRule {
    pub name: String,
    pub sort_key: KeySchemaElement,
    pub check_missing: bool,
}

/// TTL checks for the scan (PRD §6.1.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TtlRule {
    pub attribute: String,
    pub check_missing: bool,
    pub check_wrong_type: bool,
    pub check_ms_magnitude: bool,
    pub check_malformed: bool,
    pub check_past_5_years: bool,
}

/// The fully resolved runtime configuration for one scan (PRD §8.7 output).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanConfig {
    pub table: String,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub segments: usize,
    pub rate_limit_percent: Option<u8>,
    pub export: ExportConfig,
    pub rules: RuleSet,
}

/// Export destinations and format toggles (PRD §6.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportConfig {
    pub csv: bool,
    pub csv_path: Option<PathBuf>,
    pub ndjson: bool,
    pub ndjson_path: Option<PathBuf>,
}

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
