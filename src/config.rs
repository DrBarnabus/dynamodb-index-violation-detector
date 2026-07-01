//! Config loader (PRD §8.7): TOML parsing and CLI-over-TOML precedence merge.
//!
//! Three layers live here. [`ConfigFile`] is the faithful on-disk TOML
//! representation (PRD §10): fields absent from the file stay `None` so the
//! precedence merge can distinguish "unset" from "set to the default value".
//! [`CliArgs`] carries command-line overrides. [`load`] merges them into a
//! [`ScanConfig`] — the resolved but *pre-schema* runtime config: scalars and
//! export targets are fully resolved, while per-index and TTL intents are
//! carried verbatim for [`crate::rules::RuleSet`] assembly once `DescribeTable`
//! has run (PRD §8.8 / task #27). CLI overrides win over the file; the file
//! wins over built-in defaults.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::domain::KeySchemaElement;

/// The resolved, pre-schema runtime configuration for one scan (PRD §8.7).
///
/// Scalars and [`ExportConfig`] are fully resolved. The `gsi`/`lsi`/`ttl`
/// intents are carried from the config file untouched — CLI flags never address
/// per-index toggles — and are consumed alongside a discovered
/// [`crate::aws::TableDescription`] to build the [`crate::rules::RuleSet`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanConfig {
    pub table: String,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub segments: usize,
    pub rate_limit_percent: Option<u8>,
    pub export: ExportConfig,
    pub gsi: Vec<GsiEntry>,
    pub lsi: Vec<LsiEntry>,
    pub ttl: Option<TtlSettings>,
}

/// Command-line overrides (PRD §6.5). Every field is optional and, when set,
/// takes precedence over the corresponding config-file value. The config file
/// path itself is resolved by the caller and passed to [`load`] separately.
#[derive(Debug, Clone, Default, Parser)]
#[command(about = "Detect DynamoDB items that violate a GSI/LSI key schema or TTL shape")]
pub struct CliArgs {
    /// Path to the TOML config file (default: ./scan.toml if present).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Table to scan.
    #[arg(long)]
    pub table: Option<String>,
    /// AWS profile to use.
    #[arg(long)]
    pub profile: Option<String>,
    /// AWS region to target.
    #[arg(long)]
    pub region: Option<String>,
    /// Parallel scan segment count (default: CPU count).
    #[arg(long)]
    pub segments: Option<usize>,
    /// Percentage of provisioned RCU to consume (1..=100; unlimited if unset).
    #[arg(long)]
    pub rate_limit_percent: Option<u8>,
}

/// Export destinations and format toggles (PRD §6.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportConfig {
    pub csv: bool,
    pub csv_path: Option<PathBuf>,
    pub ndjson: bool,
    pub ndjson_path: Option<PathBuf>,
}

/// The on-disk TOML config (PRD §10), deserialised verbatim.
///
/// Defaultable fields are `Option`: their absence is preserved so the precedence
/// merge (CLI over file over built-in defaults) can tell an unset field from one
/// the user pinned to the default value. Per-index `check_missing` is the sole
/// exception — it has no CLI override and a fixed "off by default" semantic, so
/// it collapses straight to `bool`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub table: String,
    pub region: Option<String>,
    pub profile: Option<String>,
    #[serde(default)]
    pub scan: ScanSettings,
    #[serde(default)]
    pub export: ExportSettings,
    pub ttl: Option<TtlSettings>,
    #[serde(default)]
    pub gsi: Vec<GsiEntry>,
    #[serde(default)]
    pub lsi: Vec<LsiEntry>,
}

/// The `[scan]` table (PRD §6.2).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanSettings {
    pub segments: Option<usize>,
    pub rate_limit_percent: Option<u8>,
}

/// The `[export]` table (PRD §6.6).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportSettings {
    pub csv: Option<bool>,
    pub csv_path: Option<PathBuf>,
    pub ndjson: Option<bool>,
    pub ndjson_path: Option<PathBuf>,
}

/// The `[ttl]` table (PRD §6.1.3). The audited attribute name is discovered via
/// `DescribeTable`, not declared here.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TtlSettings {
    pub enabled: Option<bool>,
    pub check_missing: Option<bool>,
    pub check_wrong_type: Option<bool>,
    pub check_ms_magnitude: Option<bool>,
    pub check_malformed: Option<bool>,
    pub check_past_5_years: Option<bool>,
}

/// A `[[gsi]]` entry (PRD §6.1.1). Existing indexes carry only a name; the key
/// schema is discovered later. Hypothetical indexes declare `pk` (and optional
/// `sk`) inline and set `hypothetical = true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GsiEntry {
    pub name: String,
    #[serde(default)]
    pub hypothetical: bool,
    pub pk: Option<KeySchemaElement>,
    pub sk: Option<KeySchemaElement>,
    #[serde(default)]
    pub check_missing: bool,
}

/// A `[[lsi]]` entry (PRD §6.1.2). Only the missing-sort-key check is
/// configurable; the sort key schema is discovered via `DescribeTable`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LsiEntry {
    pub name: String,
    #[serde(default)]
    pub check_missing: bool,
}

/// The largest legal `rate_limit_percent` value (PRD §6.2.3).
const MAX_RATE_LIMIT_PERCENT: u8 = 100;

/// A failure to read, parse, validate or resolve configuration.
#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(toml::de::Error),
    MissingTable,
    EmptyTable,
    EmptyIndexName,
    DuplicateGsi(String),
    DuplicateLsi(String),
    HypotheticalMissingPk(String),
    ExistingGsiHasKeySpec(String),
    RateLimitOutOfRange(u8),
    ZeroSegments,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "cannot read config `{}`: {source}", path.display())
            }
            ConfigError::Parse(e) => write!(f, "invalid TOML: {e}"),
            ConfigError::MissingTable => write!(
                f,
                "no table specified; set `table` in the config or pass --table"
            ),
            ConfigError::EmptyTable => {
                write!(f, "`table` must be a non-empty table name")
            }
            ConfigError::EmptyIndexName => {
                write!(
                    f,
                    "every [[gsi]] and [[lsi]] entry needs a non-empty `name`"
                )
            }
            ConfigError::DuplicateGsi(name) => {
                write!(
                    f,
                    "GSI `{name}` is declared more than once; give each [[gsi]] a unique name"
                )
            }
            ConfigError::DuplicateLsi(name) => {
                write!(
                    f,
                    "LSI `{name}` is declared more than once; give each [[lsi]] a unique name"
                )
            }
            ConfigError::HypotheticalMissingPk(name) => write!(
                f,
                "hypothetical GSI `{name}` needs a `pk = {{ name, type }}`; only existing indexes may omit the key schema"
            ),
            ConfigError::ExistingGsiHasKeySpec(name) => write!(
                f,
                "GSI `{name}` declares `pk`/`sk` but is not hypothetical; set `hypothetical = true` or drop the key schema so it is discovered via DescribeTable"
            ),
            ConfigError::RateLimitOutOfRange(value) => write!(
                f,
                "`rate_limit_percent = {value}` is out of range; use 1..={MAX_RATE_LIMIT_PERCENT}"
            ),
            ConfigError::ZeroSegments => {
                write!(f, "`scan.segments` must be at least 1")
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Read { source, .. } => Some(source),
            ConfigError::Parse(e) => Some(e),
            _ => None,
        }
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Parse(e)
    }
}

/// Parse a TOML config string into a validated [`ConfigFile`] (PRD §8.7).
///
/// Deserialises the document, then applies the structural rules that serde
/// cannot express: non-empty names, unique index names, the hypothetical-vs-
/// existing GSI key-schema invariant, and numeric ranges.
pub fn parse(toml_str: &str) -> Result<ConfigFile, ConfigError> {
    let config: ConfigFile = toml::from_str(toml_str)?;
    config.validate()?;
    Ok(config)
}

impl ConfigFile {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.table.trim().is_empty() {
            return Err(ConfigError::EmptyTable);
        }

        if let Some(percent) = self.scan.rate_limit_percent
            && !(1..=MAX_RATE_LIMIT_PERCENT).contains(&percent)
        {
            return Err(ConfigError::RateLimitOutOfRange(percent));
        }

        if self.scan.segments == Some(0) {
            return Err(ConfigError::ZeroSegments);
        }

        let mut gsi_names = Vec::with_capacity(self.gsi.len());
        for gsi in &self.gsi {
            if gsi.name.trim().is_empty() {
                return Err(ConfigError::EmptyIndexName);
            }

            if gsi_names.contains(&gsi.name) {
                return Err(ConfigError::DuplicateGsi(gsi.name.clone()));
            }

            if gsi.hypothetical && gsi.pk.is_none() {
                return Err(ConfigError::HypotheticalMissingPk(gsi.name.clone()));
            }

            if !gsi.hypothetical && (gsi.pk.is_some() || gsi.sk.is_some()) {
                return Err(ConfigError::ExistingGsiHasKeySpec(gsi.name.clone()));
            }

            gsi_names.push(gsi.name.clone());
        }

        let mut lsi_names = Vec::with_capacity(self.lsi.len());
        for lsi in &self.lsi {
            if lsi.name.trim().is_empty() {
                return Err(ConfigError::EmptyIndexName);
            }

            if lsi_names.contains(&lsi.name) {
                return Err(ConfigError::DuplicateLsi(lsi.name.clone()));
            }

            lsi_names.push(lsi.name.clone());
        }

        Ok(())
    }
}

/// Load and resolve the runtime configuration (PRD §8.7).
///
/// `path` is the config file to read, already resolved by the caller from
/// `--config` or the `./scan.toml` default; `None` runs on CLI + defaults only.
/// The file is parsed and validated, then [`merge`] applies CLI-over-file-over-
/// defaults precedence.
pub fn load(path: Option<&Path>, cli: &CliArgs) -> Result<ScanConfig, ConfigError> {
    let file = match path {
        Some(path) => {
            let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
                path: path.to_path_buf(),
                source,
            })?;
            Some(parse(&text)?)
        }
        None => None,
    };

    merge(file, cli)
}

/// Resolve a parsed config file and CLI overrides into a [`ScanConfig`].
///
/// Precedence is CLI over file over built-in defaults. Scalars are validated
/// after resolution because CLI overrides bypass the file's own validation.
/// Export paths are intentionally left unresolved (`None`) — timestamped default
/// templating needs a clock and belongs to the save/shell layer (task #9).
pub fn merge(file: Option<ConfigFile>, cli: &CliArgs) -> Result<ScanConfig, ConfigError> {
    let table = match cli
        .table
        .clone()
        .or_else(|| file.as_ref().map(|f| f.table.clone()))
    {
        Some(table) => table,
        None => return Err(ConfigError::MissingTable),
    };

    if table.trim().is_empty() {
        return Err(ConfigError::EmptyTable);
    }

    let segments = cli
        .segments
        .or_else(|| file.as_ref().and_then(|f| f.scan.segments))
        .unwrap_or_else(default_segments);

    if segments == 0 {
        return Err(ConfigError::ZeroSegments);
    }

    let rate_limit_percent = cli
        .rate_limit_percent
        .or_else(|| file.as_ref().and_then(|f| f.scan.rate_limit_percent));

    if let Some(percent) = rate_limit_percent
        && !(1..=MAX_RATE_LIMIT_PERCENT).contains(&percent)
    {
        return Err(ConfigError::RateLimitOutOfRange(percent));
    }

    let region = cli
        .region
        .clone()
        .or_else(|| file.as_ref().and_then(|f| f.region.clone()));
    let profile = cli
        .profile
        .clone()
        .or_else(|| file.as_ref().and_then(|f| f.profile.clone()));

    let export = match &file {
        Some(f) => ExportConfig {
            csv: f.export.csv.unwrap_or(true),
            csv_path: f.export.csv_path.clone(),
            ndjson: f.export.ndjson.unwrap_or(true),
            ndjson_path: f.export.ndjson_path.clone(),
        },
        None => ExportConfig {
            csv: true,
            csv_path: None,
            ndjson: true,
            ndjson_path: None,
        },
    };

    let (gsi, lsi, ttl) = match file {
        Some(f) => (f.gsi, f.lsi, f.ttl),
        None => (Vec::new(), Vec::new(), None),
    };

    Ok(ScanConfig {
        table,
        region,
        profile,
        segments,
        rate_limit_percent,
        export,
        gsi,
        lsi,
        ttl,
    })
}

/// The default segment count: the machine's parallelism, or 1 if unknown.
fn default_segments() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TypeCode;

    const FULL: &str = r#"
table = "users"
region = "eu-west-1"
profile = "prod-readonly"

[scan]
segments = 16
rate_limit_percent = 60

[export]
csv = true
csv_path = "./violations.csv"
ndjson = true
ndjson_path = "./violations.ndjson"

[ttl]
enabled = true
check_missing = true
check_wrong_type = true
check_ms_magnitude = true
check_malformed = true
check_past_5_years = false

[[gsi]]
name = "GSI1"
check_missing = false

[[gsi]]
name = "GSI_proposed_userId_createdAt"
hypothetical = true
pk = { name = "userId", type = "S" }
sk = { name = "createdAt", type = "N" }
check_missing = true

[[lsi]]
name = "LSI1"
check_missing = true
"#;

    #[test]
    fn parses_the_full_appendix_example() {
        let config = parse(FULL).expect("appendix example should parse");

        assert_eq!(config.table, "users");
        assert_eq!(config.region.as_deref(), Some("eu-west-1"));
        assert_eq!(config.profile.as_deref(), Some("prod-readonly"));

        assert_eq!(config.scan.segments, Some(16));
        assert_eq!(config.scan.rate_limit_percent, Some(60));

        assert_eq!(config.export.csv, Some(true));
        assert_eq!(
            config.export.csv_path.as_deref(),
            Some(std::path::Path::new("./violations.csv"))
        );
        assert_eq!(config.export.ndjson, Some(true));

        let ttl = config.ttl.expect("ttl block present");
        assert_eq!(ttl.enabled, Some(true));
        assert_eq!(ttl.check_missing, Some(true));
        assert_eq!(ttl.check_past_5_years, Some(false));
    }

    #[test]
    fn discriminates_existing_and_hypothetical_gsis() {
        let config = parse(FULL).unwrap();

        let existing = &config.gsi[0];
        assert_eq!(existing.name, "GSI1");
        assert!(!existing.hypothetical);
        assert!(existing.pk.is_none());
        assert!(!existing.check_missing);

        let hypothetical = &config.gsi[1];
        assert!(hypothetical.hypothetical);
        assert_eq!(
            hypothetical.pk,
            Some(KeySchemaElement {
                name: "userId".to_string(),
                type_code: TypeCode::S,
            })
        );
        assert_eq!(
            hypothetical.sk,
            Some(KeySchemaElement {
                name: "createdAt".to_string(),
                type_code: TypeCode::N,
            })
        );
        assert!(hypothetical.check_missing);
    }

    #[test]
    fn minimal_config_defaults_absent_sections() {
        let config = parse("table = \"orders\"\n").unwrap();

        assert_eq!(config.table, "orders");
        assert_eq!(config.region, None);
        assert_eq!(config.scan, ScanSettings::default());
        assert_eq!(config.export, ExportSettings::default());
        assert!(config.ttl.is_none());
        assert!(config.gsi.is_empty());
        assert!(config.lsi.is_empty());
    }

    #[test]
    fn partition_key_only_hypothetical_gsi_is_valid() {
        let config = parse(
            r#"
table = "t"
[[gsi]]
name = "g"
hypothetical = true
pk = { name = "userId", type = "S" }
"#,
        )
        .unwrap();

        assert!(config.gsi[0].sk.is_none());
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = parse("table = \"t\"\nbogus = 1\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn missing_table_fails_to_parse() {
        let err = parse("region = \"eu-west-1\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    #[test]
    fn empty_table_name_is_rejected() {
        let err = parse("table = \"  \"\n").unwrap_err();
        assert!(matches!(err, ConfigError::EmptyTable));
    }

    #[test]
    fn hypothetical_gsi_without_pk_is_rejected() {
        let err = parse(
            r#"
table = "t"
[[gsi]]
name = "g"
hypothetical = true
"#,
        )
        .unwrap_err();

        assert!(matches!(err, ConfigError::HypotheticalMissingPk(name) if name == "g"));
    }

    #[test]
    fn existing_gsi_with_key_schema_is_rejected() {
        let err = parse(
            r#"
table = "t"
[[gsi]]
name = "g"
pk = { name = "userId", type = "S" }
"#,
        )
        .unwrap_err();

        assert!(matches!(err, ConfigError::ExistingGsiHasKeySpec(name) if name == "g"));
    }

    #[test]
    fn duplicate_gsi_name_is_rejected() {
        let err = parse(
            r#"
table = "t"
[[gsi]]
name = "dup"
[[gsi]]
name = "dup"
"#,
        )
        .unwrap_err();

        assert!(matches!(err, ConfigError::DuplicateGsi(name) if name == "dup"));
    }

    #[test]
    fn duplicate_lsi_name_is_rejected() {
        let err = parse(
            r#"
table = "t"
[[lsi]]
name = "dup"
[[lsi]]
name = "dup"
"#,
        )
        .unwrap_err();

        assert!(matches!(err, ConfigError::DuplicateLsi(name) if name == "dup"));
    }

    #[test]
    fn rate_limit_out_of_range_is_rejected() {
        for bad in ["0", "101", "200"] {
            let toml = format!("table = \"t\"\n[scan]\nrate_limit_percent = {bad}\n");
            let err = parse(&toml).unwrap_err();
            assert!(
                matches!(err, ConfigError::RateLimitOutOfRange(_)),
                "rate_limit_percent = {bad} should be rejected"
            );
        }
    }

    #[test]
    fn rate_limit_boundaries_are_accepted() {
        for ok in ["1", "100"] {
            let toml = format!("table = \"t\"\n[scan]\nrate_limit_percent = {ok}\n");
            parse(&toml).unwrap_or_else(|e| panic!("rate_limit_percent = {ok} should parse: {e}"));
        }
    }

    #[test]
    fn zero_segments_is_rejected() {
        let err = parse("table = \"t\"\n[scan]\nsegments = 0\n").unwrap_err();
        assert!(matches!(err, ConfigError::ZeroSegments));
    }

    fn cli() -> CliArgs {
        CliArgs::default()
    }

    #[test]
    fn cli_overrides_file_values() {
        let file = parse(FULL).unwrap();
        let cli = CliArgs {
            table: Some("orders".to_string()),
            region: Some("us-east-1".to_string()),
            profile: Some("dev".to_string()),
            segments: Some(4),
            rate_limit_percent: Some(25),
            ..cli()
        };

        let resolved = merge(Some(file), &cli).unwrap();
        assert_eq!(resolved.table, "orders");
        assert_eq!(resolved.region.as_deref(), Some("us-east-1"));
        assert_eq!(resolved.profile.as_deref(), Some("dev"));
        assert_eq!(resolved.segments, 4);
        assert_eq!(resolved.rate_limit_percent, Some(25));
    }

    #[test]
    fn file_wins_over_defaults_when_cli_absent() {
        let file = parse(FULL).unwrap();
        let resolved = merge(Some(file), &cli()).unwrap();

        assert_eq!(resolved.table, "users");
        assert_eq!(resolved.region.as_deref(), Some("eu-west-1"));
        assert_eq!(resolved.segments, 16);
        assert_eq!(resolved.rate_limit_percent, Some(60));
        assert!(resolved.export.csv);
        assert_eq!(
            resolved.export.csv_path.as_deref(),
            Some(std::path::Path::new("./violations.csv"))
        );
    }

    #[test]
    fn defaults_apply_with_no_file() {
        let cli = CliArgs {
            table: Some("t".to_string()),
            ..cli()
        };
        let resolved = merge(None, &cli).unwrap();

        assert_eq!(resolved.segments, default_segments());
        assert!(resolved.segments >= 1);
        assert_eq!(resolved.rate_limit_percent, None);
        assert_eq!(resolved.region, None);
        assert!(resolved.export.csv);
        assert!(resolved.export.ndjson);
        assert_eq!(resolved.export.csv_path, None);
        assert!(resolved.gsi.is_empty());
        assert!(resolved.ttl.is_none());
    }

    #[test]
    fn intents_are_carried_verbatim() {
        let file = parse(FULL).unwrap();
        let resolved = merge(Some(file), &cli()).unwrap();

        assert_eq!(resolved.gsi.len(), 2);
        assert!(resolved.gsi[1].hypothetical);
        assert_eq!(resolved.lsi.len(), 1);
        let ttl = resolved.ttl.expect("ttl intent carried");
        assert_eq!(ttl.check_past_5_years, Some(false));
    }

    #[test]
    fn missing_table_everywhere_is_rejected() {
        let err = merge(None, &cli()).unwrap_err();
        assert!(matches!(err, ConfigError::MissingTable));
    }

    #[test]
    fn cli_empty_table_is_rejected() {
        let cli = CliArgs {
            table: Some("   ".to_string()),
            ..cli()
        };
        assert!(matches!(
            merge(None, &cli).unwrap_err(),
            ConfigError::EmptyTable
        ));
    }

    #[test]
    fn cli_zero_segments_is_rejected() {
        let cli = CliArgs {
            table: Some("t".to_string()),
            segments: Some(0),
            ..cli()
        };
        assert!(matches!(
            merge(None, &cli).unwrap_err(),
            ConfigError::ZeroSegments
        ));
    }

    #[test]
    fn cli_rate_out_of_range_is_rejected() {
        let cli = CliArgs {
            table: Some("t".to_string()),
            rate_limit_percent: Some(101),
            ..cli()
        };
        assert!(matches!(
            merge(None, &cli).unwrap_err(),
            ConfigError::RateLimitOutOfRange(101)
        ));
    }

    #[test]
    fn load_reads_and_resolves_a_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("ddb-violation-detector-load-test.toml");
        fs::write(&path, "table = \"from_file\"\n[scan]\nsegments = 3\n").unwrap();

        let resolved = load(Some(&path), &cli()).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(resolved.table, "from_file");
        assert_eq!(resolved.segments, 3);
    }

    #[test]
    fn load_missing_file_is_a_read_error() {
        let path = std::env::temp_dir().join("ddb-violation-detector-does-not-exist.toml");
        let err = load(Some(&path), &cli()).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }));
    }
}
