//! AWS client facade (PRD §8.2): DynamoClient trait over the SDK.
//!
//! Owns the discovered-schema contract ([`TableDescription`] and its parts),
//! produced from `DescribeTable`.

use serde::{Deserialize, Serialize};

use crate::domain::KeySchemaElement;

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
