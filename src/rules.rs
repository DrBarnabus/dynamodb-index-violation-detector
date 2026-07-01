//! Violation rule engine (PRD §8.1): pure, synchronous key-schema and TTL checks.
//!
//! Owns the violation contract ([`Violation`], [`ItemViolations`]) and the rule
//! inputs ([`RuleSet`] and its per-index rules).

use serde::{Deserialize, Serialize};

use crate::domain::{AttributeValue, Item, KeyAttribute, KeySchemaElement, TypeCode};

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

/// Maximum byte size of a partition key value (DynamoDB index key limit).
const PARTITION_KEY_MAX_BYTES: usize = 2048;
/// Maximum byte size of a sort key value (DynamoDB index key limit).
const SORT_KEY_MAX_BYTES: usize = 1024;

/// Check one item against one GSI rule (PRD §6.1.1).
///
/// Type mismatch and size violations are always evaluated; a missing key
/// attribute is only reported when the rule opts in via `check_missing`. Applies
/// identically to existing and hypothetical indexes.
pub fn check_gsi(item: &Item, rule: &GsiRule) -> Vec<Violation> {
    let mut out = Vec::new();
    check_gsi_key(
        item,
        &rule.name,
        &rule.pk,
        PARTITION_KEY_MAX_BYTES,
        rule.check_missing,
        &mut out,
    );

    if let Some(sk) = &rule.sk {
        check_gsi_key(
            item,
            &rule.name,
            sk,
            SORT_KEY_MAX_BYTES,
            rule.check_missing,
            &mut out,
        );
    }

    out
}

fn check_gsi_key(
    item: &Item,
    index: &str,
    element: &KeySchemaElement,
    max_bytes: usize,
    check_missing: bool,
    out: &mut Vec<Violation>,
) {
    let Some(value) = item.get(&element.name) else {
        if check_missing {
            out.push(Violation {
                target: Target::Gsi(index.to_string()),
                category: ViolationCategory::MissingKey,
                attribute: Some(element.name.clone()),
                actual_value: None,
                actual_type: None,
                expected_type: Some(element.type_code),
                size_bytes: None,
            });
        }

        return;
    };

    if scalar_type(value) != Some(element.type_code) {
        out.push(Violation {
            target: Target::Gsi(index.to_string()),
            category: ViolationCategory::TypeMismatch,
            attribute: Some(element.name.clone()),
            actual_value: value_repr(value),
            actual_type: Some(type_code_str(value).to_string()),
            expected_type: Some(element.type_code),
            size_bytes: None,
        });

        return;
    }

    if let Some(size) = key_size_bytes(value)
        && size > max_bytes
    {
        out.push(Violation {
            target: Target::Gsi(index.to_string()),
            category: ViolationCategory::SizeExceeded,
            attribute: Some(element.name.clone()),
            actual_value: value_repr(value),
            actual_type: Some(type_code_str(value).to_string()),
            expected_type: None,
            size_bytes: Some(size),
        });
    }
}

/// The DynamoDB type code of a value, for reporting the actual type.
fn type_code_str(value: &AttributeValue) -> &'static str {
    match value {
        AttributeValue::S(_) => "S",
        AttributeValue::N(_) => "N",
        AttributeValue::B(_) => "B",
        AttributeValue::Bool(_) => "BOOL",
        AttributeValue::Null(_) => "NULL",
        AttributeValue::M(_) => "M",
        AttributeValue::L(_) => "L",
        AttributeValue::Ss(_) => "SS",
        AttributeValue::Ns(_) => "NS",
        AttributeValue::Bs(_) => "BS",
    }
}

/// The scalar key type of a value, or `None` if it is not a valid key type.
fn scalar_type(value: &AttributeValue) -> Option<TypeCode> {
    match value {
        AttributeValue::S(_) => Some(TypeCode::S),
        AttributeValue::N(_) => Some(TypeCode::N),
        AttributeValue::B(_) => Some(TypeCode::B),
        _ => None,
    }
}

/// A short human-readable rendering of a scalar value for reporting.
fn value_repr(value: &AttributeValue) -> Option<String> {
    match value {
        AttributeValue::S(s) => Some(s.clone()),
        AttributeValue::N(n) => Some(n.clone()),
        AttributeValue::Bool(b) => Some(b.to_string()),
        AttributeValue::Null(_) => Some("null".to_string()),
        _ => None,
    }
}

/// The key byte size of a value: UTF-8 length for strings, raw length for
/// binary. Other types have no measurable key size.
fn key_size_bytes(value: &AttributeValue) -> Option<usize> {
    match value {
        AttributeValue::S(s) => Some(s.len()),
        AttributeValue::B(b) => Some(b.len()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(name: &str, type_code: TypeCode) -> KeySchemaElement {
        KeySchemaElement {
            name: name.to_string(),
            type_code,
        }
    }

    fn gsi(pk: KeySchemaElement, sk: Option<KeySchemaElement>, check_missing: bool) -> GsiRule {
        GsiRule {
            name: "GSI1".to_string(),
            hypothetical: false,
            pk,
            sk,
            check_missing,
        }
    }

    fn item(pairs: &[(&str, AttributeValue)]) -> Item {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn matching_key_yields_no_violation() {
        let rule = gsi(element("userId", TypeCode::S), None, true);
        let it = item(&[("userId", AttributeValue::S("u-1".to_string()))]);
        assert!(check_gsi(&it, &rule).is_empty());
    }

    #[test]
    fn type_mismatch_on_partition_key() {
        let rule = gsi(element("userId", TypeCode::S), None, false);
        let it = item(&[("userId", AttributeValue::N("42".to_string()))]);

        let violations = check_gsi(&it, &rule);
        assert_eq!(violations.len(), 1);
        let v = &violations[0];
        assert_eq!(v.category, ViolationCategory::TypeMismatch);
        assert_eq!(v.target, Target::Gsi("GSI1".to_string()));
        assert_eq!(v.attribute.as_deref(), Some("userId"));
        assert_eq!(v.actual_type.as_deref(), Some("N"));
        assert_eq!(v.expected_type, Some(TypeCode::S));
    }

    #[test]
    fn non_scalar_value_is_a_type_mismatch() {
        let rule = gsi(element("userId", TypeCode::S), None, false);
        let it = item(&[("userId", AttributeValue::Bool(true))]);

        let violations = check_gsi(&it, &rule);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].category, ViolationCategory::TypeMismatch);
        assert_eq!(violations[0].actual_type.as_deref(), Some("BOOL"));
    }

    #[test]
    fn missing_key_respects_toggle() {
        let with_check = gsi(element("userId", TypeCode::S), None, true);
        let without_check = gsi(element("userId", TypeCode::S), None, false);
        let it = item(&[("other", AttributeValue::S("x".to_string()))]);

        assert!(check_gsi(&it, &without_check).is_empty());

        let violations = check_gsi(&it, &with_check);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].category, ViolationCategory::MissingKey);
        assert_eq!(violations[0].expected_type, Some(TypeCode::S));
    }

    #[test]
    fn partition_key_size_boundary() {
        let rule = gsi(element("pk", TypeCode::S), None, false);

        let at_limit = item(&[("pk", AttributeValue::S("a".repeat(2048)))]);
        assert!(check_gsi(&at_limit, &rule).is_empty());

        let over_limit = item(&[("pk", AttributeValue::S("a".repeat(2049)))]);
        let violations = check_gsi(&over_limit, &rule);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].category, ViolationCategory::SizeExceeded);
        assert_eq!(violations[0].size_bytes, Some(2049));
    }

    #[test]
    fn sort_key_size_limit_is_smaller() {
        let rule = gsi(
            element("pk", TypeCode::S),
            Some(element("sk", TypeCode::S)),
            false,
        );

        let it = item(&[
            ("pk", AttributeValue::S("ok".to_string())),
            ("sk", AttributeValue::S("a".repeat(1025))),
        ]);

        let violations = check_gsi(&it, &rule);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].attribute.as_deref(), Some("sk"));
        assert_eq!(violations[0].size_bytes, Some(1025));
    }

    #[test]
    fn string_size_counts_utf8_bytes() {
        let rule = gsi(element("pk", TypeCode::S), None, false);
        let it = item(&[("pk", AttributeValue::S("€".repeat(683)))]);

        let violations = check_gsi(&it, &rule);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].size_bytes, Some(2049));
    }

    #[test]
    fn binary_size_counts_raw_bytes() {
        let rule = gsi(element("pk", TypeCode::B), None, false);
        let it = item(&[("pk", AttributeValue::B(vec![0u8; 2049]))]);

        let violations = check_gsi(&it, &rule);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].category, ViolationCategory::SizeExceeded);
        assert_eq!(violations[0].size_bytes, Some(2049));
    }

    #[test]
    fn number_key_has_no_size_check() {
        let rule = gsi(element("pk", TypeCode::N), None, false);
        let it = item(&[("pk", AttributeValue::N("1".repeat(3000)))]);
        assert!(check_gsi(&it, &rule).is_empty());
    }

    #[test]
    fn wrong_type_suppresses_size_check() {
        let rule = gsi(element("pk", TypeCode::N), None, false);
        let it = item(&[("pk", AttributeValue::S("a".repeat(3000)))]);

        let violations = check_gsi(&it, &rule);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].category, ViolationCategory::TypeMismatch);
    }

    #[test]
    fn missing_sort_key_reported_when_enabled() {
        let rule = gsi(
            element("pk", TypeCode::S),
            Some(element("sk", TypeCode::N)),
            true,
        );
        let it = item(&[("pk", AttributeValue::S("ok".to_string()))]);

        let violations = check_gsi(&it, &rule);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].attribute.as_deref(), Some("sk"));
        assert_eq!(violations[0].category, ViolationCategory::MissingKey);
    }

    #[test]
    fn hypothetical_flag_does_not_change_checks() {
        let mut rule = gsi(element("userId", TypeCode::S), None, false);
        rule.hypothetical = true;
        let it = item(&[("userId", AttributeValue::N("42".to_string()))]);

        let violations = check_gsi(&it, &rule);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].category, ViolationCategory::TypeMismatch);
    }
}
