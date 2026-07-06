//! Export writer (PRD §8.4): streaming CSV and NDJSON output.
//!
//! Writers consume [`ItemViolations`] groups and stream them to disk as they
//! arrive, flushing after every group so a partial file left by a crash or
//! cancel (PRD §6.6) contains everything scanned up to that point.

use std::fmt;
use std::io::{self, Write};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use crate::domain::{AttributeValue, KeyAttribute, TypeCode};
use crate::rules::{ItemViolations, Target, Violation, ViolationCategory};

/// A failure while serialising or flushing export output.
#[derive(Debug)]
pub enum ExportError {
    Io(io::Error),
    Csv(csv::Error),
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportError::Io(e) => write!(f, "export I/O failure: {e}"),
            ExportError::Csv(e) => write!(f, "CSV serialisation failure: {e}"),
        }
    }
}

impl std::error::Error for ExportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExportError::Io(e) => Some(e),
            ExportError::Csv(e) => Some(e),
        }
    }
}

impl From<io::Error> for ExportError {
    fn from(e: io::Error) -> Self {
        ExportError::Io(e)
    }
}

impl From<csv::Error> for ExportError {
    fn from(e: csv::Error) -> Self {
        ExportError::Csv(e)
    }
}

/// A streaming sink for violation groups (PRD §8.4).
///
/// One [`write`](ExportWriter::write) call per item; a single item with multiple
/// violations expands to multiple output records for CSV. [`close`](ExportWriter::close)
/// consumes the writer for a final flush and error surfacing that a bare `Drop`
/// cannot report.
pub trait ExportWriter {
    fn write(&mut self, group: &ItemViolations) -> Result<(), ExportError>;
    fn close(self: Box<Self>) -> Result<(), ExportError>;
}

/// One CSV row per violation (PRD §6.6). The item's `pk`/`sk` are repeated across
/// every row belonging to the same item; binary key values are base64-encoded.
pub struct CsvWriter<W: Write> {
    writer: csv::Writer<W>,
}

const CSV_HEADERS: &[&str] = &[
    "table",
    "target",
    "category",
    "pk",
    "sk",
    "pk_type",
    "sk_type",
    "attribute",
    "actual_value",
    "actual_type",
    "expected_type",
    "size_bytes",
    "detected_at",
];

impl<W: Write> CsvWriter<W> {
    /// Build a writer over `inner`, emitting the header row immediately so a
    /// zero-violation scan still produces a well-formed file.
    pub fn new(inner: W) -> Result<Self, ExportError> {
        let mut writer = csv::Writer::from_writer(inner);
        writer.write_record(CSV_HEADERS)?;
        writer.flush()?;

        Ok(Self { writer })
    }

    /// Flush and reclaim the underlying writer. Used by tests to assert on exact
    /// bytes; production paths go through [`ExportWriter::close`].
    pub fn into_inner(mut self) -> Result<W, ExportError> {
        self.writer.flush()?;
        self.writer.into_inner().map_err(|e| e.into_error().into())
    }
}

impl<W: Write> ExportWriter for CsvWriter<W> {
    fn write(&mut self, group: &ItemViolations) -> Result<(), ExportError> {
        let (pk_value, pk_type) = render_key(&group.pk);
        let (sk_value, sk_type) = match &group.sk {
            Some(sk) => {
                let (v, t) = render_key(sk);
                (v, t.to_string())
            }
            None => (String::new(), String::new()),
        };
        let detected_at = group.detected_at.to_string();

        for violation in &group.violations {
            self.writer.write_record([
                group.table.as_str(),
                target_label(&violation.target).as_str(),
                category_label(violation.category),
                pk_value.as_str(),
                sk_value.as_str(),
                pk_type,
                sk_type.as_str(),
                violation.attribute.as_deref().unwrap_or(""),
                violation.actual_value.as_deref().unwrap_or(""),
                violation.actual_type.as_deref().unwrap_or(""),
                expected_type_label(violation),
                size_bytes_label(violation).as_str(),
                detected_at.as_str(),
            ])?;
        }

        self.writer.flush()?;
        Ok(())
    }

    fn close(mut self: Box<Self>) -> Result<(), ExportError> {
        self.writer.flush()?;
        Ok(())
    }
}

/// Render a key attribute to its CSV value and type code. Binary is base64.
fn render_key(key: &KeyAttribute) -> (String, &'static str) {
    match &key.value {
        AttributeValue::S(s) => (s.clone(), "S"),
        AttributeValue::N(n) => (n.clone(), "N"),
        AttributeValue::B(b) => (BASE64.encode(b), "B"),
        other => (String::new(), non_scalar_type_code(other)),
    }
}

fn target_label(target: &Target) -> String {
    match target {
        Target::Gsi(name) => format!("GSI:{name}"),
        Target::Lsi(name) => format!("LSI:{name}"),
        Target::Ttl => "TTL".to_string(),
    }
}

fn category_label(category: ViolationCategory) -> &'static str {
    match category {
        ViolationCategory::TypeMismatch => "type_mismatch",
        ViolationCategory::SizeExceeded => "size_exceeded",
        ViolationCategory::MissingKey => "missing_key",
        ViolationCategory::TtlMissing => "ttl_missing",
        ViolationCategory::TtlWrongType => "ttl_wrong_type",
        ViolationCategory::TtlMsMagnitude => "ttl_ms_magnitude",
        ViolationCategory::TtlMalformed => "ttl_malformed",
        ViolationCategory::TtlPastFiveYears => "ttl_past_five_years",
    }
}

fn expected_type_label(violation: &Violation) -> &'static str {
    match violation.expected_type {
        Some(TypeCode::S) => "S",
        Some(TypeCode::N) => "N",
        Some(TypeCode::B) => "B",
        None => "",
    }
}

fn size_bytes_label(violation: &Violation) -> String {
    violation
        .size_bytes
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn non_scalar_type_code(value: &AttributeValue) -> &'static str {
    match value {
        AttributeValue::Bool(_) => "BOOL",
        AttributeValue::Null(_) => "NULL",
        AttributeValue::M(_) => "M",
        AttributeValue::L(_) => "L",
        AttributeValue::Ss(_) => "SS",
        AttributeValue::Ns(_) => "NS",
        AttributeValue::Bs(_) => "BS",
        AttributeValue::S(_) | AttributeValue::N(_) | AttributeValue::B(_) => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, value: AttributeValue) -> KeyAttribute {
        KeyAttribute {
            name: name.to_string(),
            value,
        }
    }

    fn group(
        pk: KeyAttribute,
        sk: Option<KeyAttribute>,
        violations: Vec<Violation>,
    ) -> ItemViolations {
        ItemViolations {
            table: "users".to_string(),
            pk,
            sk,
            item: Default::default(),
            violations,
            detected_at: 1_750_000_000,
        }
    }

    fn to_csv(groups: &[ItemViolations]) -> String {
        let mut writer = CsvWriter::new(Vec::new()).unwrap();
        for g in groups {
            writer.write(g).unwrap();
        }
        String::from_utf8(writer.into_inner().unwrap()).unwrap()
    }

    #[test]
    fn header_only_when_no_violations() {
        let csv = to_csv(&[]);
        assert_eq!(
            csv,
            "table,target,category,pk,sk,pk_type,sk_type,attribute,actual_value,actual_type,expected_type,size_bytes,detected_at\n"
        );
    }

    #[test]
    fn type_mismatch_row_is_byte_exact() {
        let g = group(
            key("userId", AttributeValue::S("u-1".to_string())),
            None,
            vec![Violation {
                target: Target::Gsi("GSI1".to_string()),
                category: ViolationCategory::TypeMismatch,
                attribute: Some("email".to_string()),
                actual_value: Some("42".to_string()),
                actual_type: Some("N".to_string()),
                expected_type: Some(TypeCode::S),
                size_bytes: None,
            }],
        );

        let csv = to_csv(&[g]);
        let last = csv.lines().last().unwrap();
        assert_eq!(
            last,
            "users,GSI:GSI1,type_mismatch,u-1,,S,,email,42,N,S,,1750000000"
        );
    }

    #[test]
    fn multiple_violations_expand_to_multiple_rows() {
        let g = group(
            key("userId", AttributeValue::S("u-1".to_string())),
            Some(key("createdAt", AttributeValue::N("100".to_string()))),
            vec![
                Violation {
                    target: Target::Gsi("GSI1".to_string()),
                    category: ViolationCategory::TypeMismatch,
                    attribute: Some("email".to_string()),
                    actual_value: Some("42".to_string()),
                    actual_type: Some("N".to_string()),
                    expected_type: Some(TypeCode::S),
                    size_bytes: None,
                },
                Violation {
                    target: Target::Ttl,
                    category: ViolationCategory::TtlMalformed,
                    attribute: Some("expiresAt".to_string()),
                    actual_value: Some("-1".to_string()),
                    actual_type: Some("N".to_string()),
                    expected_type: None,
                    size_bytes: None,
                },
            ],
        );

        let csv = to_csv(&[g]);
        let rows: Vec<&str> = csv.lines().skip(1).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            "users,GSI:GSI1,type_mismatch,u-1,100,S,N,email,42,N,S,,1750000000"
        );
        assert_eq!(
            rows[1],
            "users,TTL,ttl_malformed,u-1,100,S,N,expiresAt,-1,N,,,1750000000"
        );
    }

    #[test]
    fn size_violation_records_byte_count() {
        let g = group(
            key("pk", AttributeValue::S("x".to_string())),
            None,
            vec![Violation {
                target: Target::Lsi("LSI1".to_string()),
                category: ViolationCategory::SizeExceeded,
                attribute: Some("pk".to_string()),
                actual_value: Some("x".to_string()),
                actual_type: Some("S".to_string()),
                expected_type: None,
                size_bytes: Some(2049),
            }],
        );

        let csv = to_csv(&[g]);
        let last = csv.lines().last().unwrap();
        assert_eq!(
            last,
            "users,LSI:LSI1,size_exceeded,x,,S,,pk,x,S,,2049,1750000000"
        );
    }

    #[test]
    fn binary_key_is_base64_encoded() {
        let g = group(
            key("pk", AttributeValue::B(vec![1, 2, 3, 4])),
            None,
            vec![Violation {
                target: Target::Gsi("GSI1".to_string()),
                category: ViolationCategory::MissingKey,
                attribute: Some("createdAt".to_string()),
                actual_value: None,
                actual_type: None,
                expected_type: Some(TypeCode::N),
                size_bytes: None,
            }],
        );

        let csv = to_csv(&[g]);
        let last = csv.lines().last().unwrap();
        assert_eq!(
            last,
            "users,GSI:GSI1,missing_key,AQIDBA==,,B,,createdAt,,,N,,1750000000"
        );
    }

    #[test]
    fn value_needing_quotes_is_escaped() {
        let g = group(
            key("pk", AttributeValue::S("a,b".to_string())),
            None,
            vec![Violation {
                target: Target::Gsi("GSI1".to_string()),
                category: ViolationCategory::TypeMismatch,
                attribute: Some("pk".to_string()),
                actual_value: Some("has \"quote\"".to_string()),
                actual_type: Some("S".to_string()),
                expected_type: Some(TypeCode::N),
                size_bytes: None,
            }],
        );

        let csv = to_csv(&[g]);
        let last = csv.lines().last().unwrap();
        assert_eq!(
            last,
            r#"users,GSI:GSI1,type_mismatch,"a,b",,S,,pk,"has ""quote""",S,N,,1750000000"#
        );
    }

    #[test]
    fn close_flushes_via_trait_object() {
        let mut writer: Box<dyn ExportWriter> = Box::new(CsvWriter::new(Vec::new()).unwrap());
        let g = group(
            key("pk", AttributeValue::S("u-1".to_string())),
            None,
            vec![],
        );
        writer.write(&g).unwrap();
        writer.close().unwrap();
    }
}
