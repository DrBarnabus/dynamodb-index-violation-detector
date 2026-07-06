//! RuleSet assembly (PRD §8.8, task #27): merge discovered schema with config.
//!
//! Sits between the AWS facade and the rule engine. [`assemble`] takes a
//! [`TableDescription`] discovered via `DescribeTable` and the pre-schema
//! [`ScanConfig`] intents, and resolves them into the [`RuleSet`] the engine
//! consumes. Pure and synchronous given its inputs.
//!
//! The config drives *which* targets are checked; the schema supplies the key
//! shape for existing indexes. Existing GSIs/LSIs named in the config are
//! resolved against the discovered schema (an unknown name is an error);
//! hypothetical GSIs carry their own key spec from the config; the TTL attribute
//! is always discovered — the config carries only its toggles.

use std::fmt;

use crate::aws::TableDescription;
use crate::config::{GsiEntry, LsiEntry, ScanConfig, TtlSettings};
use crate::rules::{GsiRule, LsiRule, RuleSet, TtlRule};

/// A failure to resolve a [`RuleSet`] from schema and config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssembleError {
    /// A non-hypothetical GSI in the config has no counterpart in the schema.
    UnknownGsi(String),
    /// An LSI in the config has no counterpart in the schema.
    UnknownLsi(String),
    /// A hypothetical GSI reached assembly without a partition key spec.
    HypotheticalMissingPk(String),
    /// A discovered LSI has no sort key, so its missing-key check is meaningless.
    LsiMissingSortKey(String),
    /// TTL auditing is enabled but the table exposes no TTL attribute to audit.
    TtlNotConfigured,
}

impl fmt::Display for AssembleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AssembleError::UnknownGsi(name) => write!(
                f,
                "config references GSI `{name}`, which does not exist on the table; \
                 mark it `hypothetical = true` or correct the name"
            ),
            AssembleError::UnknownLsi(name) => write!(
                f,
                "config references LSI `{name}`, which does not exist on the table; \
                 correct the name"
            ),
            AssembleError::HypotheticalMissingPk(name) => {
                write!(f, "hypothetical GSI `{name}` has no partition key spec")
            }
            AssembleError::LsiMissingSortKey(name) => write!(
                f,
                "LSI `{name}` has no sort key in the discovered schema; \
                 the missing-key check cannot apply"
            ),
            AssembleError::TtlNotConfigured => write!(
                f,
                "TTL auditing is enabled but the table has no TTL attribute; \
                 disable the [ttl] block or configure TTL on the table"
            ),
        }
    }
}

impl std::error::Error for AssembleError {}

/// Default TTL sub-toggles when the config leaves one unset (PRD §10 appendix).
/// Only the past-5-years check is off by default, matching the appendix example.
const TTL_DEFAULT_MISSING: bool = true;
const TTL_DEFAULT_WRONG_TYPE: bool = true;
const TTL_DEFAULT_MS_MAGNITUDE: bool = true;
const TTL_DEFAULT_MALFORMED: bool = true;
const TTL_DEFAULT_PAST_FIVE_YEARS: bool = false;

/// Resolve a discovered schema and pre-schema config into a [`RuleSet`].
///
/// Existing GSIs/LSIs named in the config take their key schema from `desc`;
/// hypothetical GSIs take theirs from the config entry. The TTL attribute is
/// always the discovered one. Preserves config order within each target list.
pub fn assemble(desc: &TableDescription, config: &ScanConfig) -> Result<RuleSet, AssembleError> {
    let gsis = config
        .gsi
        .iter()
        .map(|entry| assemble_gsi(desc, entry))
        .collect::<Result<Vec<_>, _>>()?;

    let lsis = config
        .lsi
        .iter()
        .map(|entry| assemble_lsi(desc, entry))
        .collect::<Result<Vec<_>, _>>()?;

    let ttl = assemble_ttl(desc, config.ttl.as_ref())?;

    Ok(RuleSet {
        table: desc.name.clone(),
        gsis,
        lsis,
        ttl,
    })
}

fn assemble_gsi(desc: &TableDescription, entry: &GsiEntry) -> Result<GsiRule, AssembleError> {
    if entry.hypothetical {
        let pk = entry
            .pk
            .clone()
            .ok_or_else(|| AssembleError::HypotheticalMissingPk(entry.name.clone()))?;
        return Ok(GsiRule {
            name: entry.name.clone(),
            hypothetical: true,
            pk,
            sk: entry.sk.clone(),
            check_missing: entry.check_missing,
        });
    }

    let schema = desc
        .gsis
        .iter()
        .find(|gsi| gsi.name == entry.name)
        .ok_or_else(|| AssembleError::UnknownGsi(entry.name.clone()))?;

    Ok(GsiRule {
        name: schema.name.clone(),
        hypothetical: false,
        pk: schema.pk.clone(),
        sk: schema.sk.clone(),
        check_missing: entry.check_missing,
    })
}

fn assemble_lsi(desc: &TableDescription, entry: &LsiEntry) -> Result<LsiRule, AssembleError> {
    let schema = desc
        .lsis
        .iter()
        .find(|lsi| lsi.name == entry.name)
        .ok_or_else(|| AssembleError::UnknownLsi(entry.name.clone()))?;

    let sort_key = schema
        .sk
        .clone()
        .ok_or_else(|| AssembleError::LsiMissingSortKey(entry.name.clone()))?;

    Ok(LsiRule {
        name: schema.name.clone(),
        sort_key,
        check_missing: entry.check_missing,
    })
}

fn assemble_ttl(
    desc: &TableDescription,
    settings: Option<&TtlSettings>,
) -> Result<Option<TtlRule>, AssembleError> {
    let Some(settings) = settings.filter(|s| s.enabled.unwrap_or(true)) else {
        return Ok(None);
    };

    let attribute = desc
        .ttl
        .as_ref()
        .map(|ttl| ttl.attribute.clone())
        .ok_or(AssembleError::TtlNotConfigured)?;

    Ok(Some(TtlRule {
        attribute,
        check_missing: settings.check_missing.unwrap_or(TTL_DEFAULT_MISSING),
        check_wrong_type: settings.check_wrong_type.unwrap_or(TTL_DEFAULT_WRONG_TYPE),
        check_ms_magnitude: settings
            .check_ms_magnitude
            .unwrap_or(TTL_DEFAULT_MS_MAGNITUDE),
        check_malformed: settings.check_malformed.unwrap_or(TTL_DEFAULT_MALFORMED),
        check_past_5_years: settings
            .check_past_5_years
            .unwrap_or(TTL_DEFAULT_PAST_FIVE_YEARS),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::{IndexSchema, TableDescription, TableKeySchema, TtlDescription};
    use crate::config::{ExportConfig, GsiEntry, LsiEntry, ScanConfig, TtlSettings};
    use crate::domain::{KeySchemaElement, TypeCode};

    fn element(name: &str, type_code: TypeCode) -> KeySchemaElement {
        KeySchemaElement {
            name: name.to_string(),
            type_code,
        }
    }

    fn index(name: &str, pk: KeySchemaElement, sk: Option<KeySchemaElement>) -> IndexSchema {
        IndexSchema {
            name: name.to_string(),
            pk,
            sk,
        }
    }

    fn table(
        gsis: Vec<IndexSchema>,
        lsis: Vec<IndexSchema>,
        ttl: Option<TtlDescription>,
    ) -> TableDescription {
        TableDescription {
            name: "users".to_string(),
            key_schema: TableKeySchema {
                pk: element("id", TypeCode::S),
                sk: None,
            },
            gsis,
            lsis,
            ttl,
            provisioned_rcu: None,
            item_count: 0,
        }
    }

    fn config(gsi: Vec<GsiEntry>, lsi: Vec<LsiEntry>, ttl: Option<TtlSettings>) -> ScanConfig {
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
            gsi,
            lsi,
            ttl,
        }
    }

    fn existing_gsi(name: &str, check_missing: bool) -> GsiEntry {
        GsiEntry {
            name: name.to_string(),
            hypothetical: false,
            pk: None,
            sk: None,
            check_missing,
        }
    }

    #[test]
    fn existing_gsi_takes_key_schema_from_the_discovered_table() {
        let desc = table(
            vec![index(
                "byEmail",
                element("email", TypeCode::S),
                Some(element("createdAt", TypeCode::N)),
            )],
            vec![],
            None,
        );
        let cfg = config(vec![existing_gsi("byEmail", true)], vec![], None);

        let rules = assemble(&desc, &cfg).unwrap();
        assert_eq!(rules.table, "users");
        assert_eq!(rules.gsis.len(), 1);
        let gsi = &rules.gsis[0];
        assert_eq!(gsi.name, "byEmail");
        assert!(!gsi.hypothetical);
        assert_eq!(gsi.pk, element("email", TypeCode::S));
        assert_eq!(gsi.sk, Some(element("createdAt", TypeCode::N)));
        assert!(gsi.check_missing);
    }

    #[test]
    fn unknown_existing_gsi_is_an_error() {
        let desc = table(vec![], vec![], None);
        let cfg = config(vec![existing_gsi("ghost", false)], vec![], None);

        assert_eq!(
            assemble(&desc, &cfg).unwrap_err(),
            AssembleError::UnknownGsi("ghost".to_string())
        );
    }

    #[test]
    fn hypothetical_gsi_carries_its_own_key_spec() {
        let desc = table(vec![], vec![], None);
        let cfg = config(
            vec![GsiEntry {
                name: "proposed".to_string(),
                hypothetical: true,
                pk: Some(element("userId", TypeCode::S)),
                sk: Some(element("ts", TypeCode::N)),
                check_missing: false,
            }],
            vec![],
            None,
        );

        let rules = assemble(&desc, &cfg).unwrap();
        let gsi = &rules.gsis[0];
        assert!(gsi.hypothetical);
        assert_eq!(gsi.pk, element("userId", TypeCode::S));
        assert_eq!(gsi.sk, Some(element("ts", TypeCode::N)));
    }

    #[test]
    fn hypothetical_gsi_without_pk_is_an_error() {
        let desc = table(vec![], vec![], None);
        let cfg = config(
            vec![GsiEntry {
                name: "proposed".to_string(),
                hypothetical: true,
                pk: None,
                sk: None,
                check_missing: false,
            }],
            vec![],
            None,
        );

        assert_eq!(
            assemble(&desc, &cfg).unwrap_err(),
            AssembleError::HypotheticalMissingPk("proposed".to_string())
        );
    }

    #[test]
    fn config_order_is_preserved_across_existing_and_hypothetical() {
        let desc = table(
            vec![index("real", element("a", TypeCode::S), None)],
            vec![],
            None,
        );
        let cfg = config(
            vec![
                GsiEntry {
                    name: "proposed".to_string(),
                    hypothetical: true,
                    pk: Some(element("b", TypeCode::S)),
                    sk: None,
                    check_missing: false,
                },
                existing_gsi("real", false),
            ],
            vec![],
            None,
        );

        let rules = assemble(&desc, &cfg).unwrap();
        assert_eq!(rules.gsis[0].name, "proposed");
        assert_eq!(rules.gsis[1].name, "real");
    }

    #[test]
    fn existing_lsi_takes_sort_key_from_the_discovered_table() {
        let desc = table(
            vec![],
            vec![index(
                "byCreatedAt",
                element("id", TypeCode::S),
                Some(element("createdAt", TypeCode::N)),
            )],
            None,
        );
        let cfg = config(
            vec![],
            vec![LsiEntry {
                name: "byCreatedAt".to_string(),
                check_missing: true,
            }],
            None,
        );

        let rules = assemble(&desc, &cfg).unwrap();
        assert_eq!(rules.lsis.len(), 1);
        let lsi = &rules.lsis[0];
        assert_eq!(lsi.name, "byCreatedAt");
        assert_eq!(lsi.sort_key, element("createdAt", TypeCode::N));
        assert!(lsi.check_missing);
    }

    #[test]
    fn unknown_lsi_is_an_error() {
        let desc = table(vec![], vec![], None);
        let cfg = config(
            vec![],
            vec![LsiEntry {
                name: "ghost".to_string(),
                check_missing: true,
            }],
            None,
        );

        assert_eq!(
            assemble(&desc, &cfg).unwrap_err(),
            AssembleError::UnknownLsi("ghost".to_string())
        );
    }

    #[test]
    fn lsi_without_a_discovered_sort_key_is_an_error() {
        let desc = table(
            vec![],
            vec![index("weird", element("id", TypeCode::S), None)],
            None,
        );
        let cfg = config(
            vec![],
            vec![LsiEntry {
                name: "weird".to_string(),
                check_missing: true,
            }],
            None,
        );

        assert_eq!(
            assemble(&desc, &cfg).unwrap_err(),
            AssembleError::LsiMissingSortKey("weird".to_string())
        );
    }

    fn ttl_desc() -> Option<TtlDescription> {
        Some(TtlDescription {
            attribute: "expiresAt".to_string(),
            enabled: true,
        })
    }

    #[test]
    fn ttl_resolves_discovered_attribute_with_toggles() {
        let desc = table(vec![], vec![], ttl_desc());
        let cfg = config(
            vec![],
            vec![],
            Some(TtlSettings {
                enabled: Some(true),
                check_missing: Some(false),
                check_past_5_years: Some(true),
                ..TtlSettings::default()
            }),
        );

        let ttl = assemble(&desc, &cfg)
            .unwrap()
            .ttl
            .expect("ttl rule present");
        assert_eq!(ttl.attribute, "expiresAt");
        assert!(!ttl.check_missing);
        assert!(ttl.check_past_5_years);
    }

    #[test]
    fn ttl_sub_toggles_default_when_unset() {
        let desc = table(vec![], vec![], ttl_desc());
        let cfg = config(vec![], vec![], Some(TtlSettings::default()));

        let ttl = assemble(&desc, &cfg)
            .unwrap()
            .ttl
            .expect("ttl rule present");
        assert!(ttl.check_missing);
        assert!(ttl.check_wrong_type);
        assert!(ttl.check_ms_magnitude);
        assert!(ttl.check_malformed);
        assert!(!ttl.check_past_5_years);
    }

    #[test]
    fn absent_ttl_settings_yields_no_ttl_rule() {
        let desc = table(vec![], vec![], ttl_desc());
        let cfg = config(vec![], vec![], None);

        assert!(assemble(&desc, &cfg).unwrap().ttl.is_none());
    }

    #[test]
    fn ttl_disabled_in_config_yields_no_ttl_rule() {
        let desc = table(vec![], vec![], ttl_desc());
        let cfg = config(
            vec![],
            vec![],
            Some(TtlSettings {
                enabled: Some(false),
                ..TtlSettings::default()
            }),
        );

        assert!(assemble(&desc, &cfg).unwrap().ttl.is_none());
    }

    #[test]
    fn ttl_enabled_without_a_discovered_attribute_is_an_error() {
        let desc = table(vec![], vec![], None);
        let cfg = config(vec![], vec![], Some(TtlSettings::default()));

        assert_eq!(
            assemble(&desc, &cfg).unwrap_err(),
            AssembleError::TtlNotConfigured
        );
    }

    #[test]
    fn empty_config_against_a_rich_table_checks_nothing() {
        let desc = table(
            vec![index("byEmail", element("email", TypeCode::S), None)],
            vec![index(
                "byCreatedAt",
                element("id", TypeCode::S),
                Some(element("createdAt", TypeCode::N)),
            )],
            ttl_desc(),
        );
        let cfg = config(vec![], vec![], None);

        let rules = assemble(&desc, &cfg).unwrap();
        assert!(rules.gsis.is_empty());
        assert!(rules.lsis.is_empty());
        assert!(rules.ttl.is_none());
    }
}
