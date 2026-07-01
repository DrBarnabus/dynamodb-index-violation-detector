//! Ownerless value vocabulary shared across every module (PRD §8).
//!
//! Only the foundational data types that have no single owning module live here:
//! items, attribute values, type codes and key-schema elements. Contract types
//! belong to the module that produces them — violations in [`crate::rules`],
//! configuration in [`crate::config`], schema in [`crate::aws`].
//!
//! Pure data: definitions and serde derives only, no behaviour. The AWS facade
//! converts the SDK's `AttributeValue` into [`AttributeValue`] at the boundary so
//! the rest of the crate never depends on the SDK types.

use std::collections::HashMap;

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

/// A named attribute value of an item, e.g. its partition or sort key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyAttribute {
    pub name: String,
    pub value: AttributeValue,
}
