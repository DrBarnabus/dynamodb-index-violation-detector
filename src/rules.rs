//! Violation rule engine (PRD §8.1): pure, synchronous key-schema and TTL checks.
//!
//! Owns the violation contract ([`Violation`], [`ItemViolations`]) and the rule
//! inputs ([`RuleSet`] and its per-index rules).

use serde::{Deserialize, Serialize};

use crate::domain::{Item, KeyAttribute, KeySchemaElement, TypeCode};

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
