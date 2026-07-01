//! Config loader (PRD §8.7): TOML parsing and CLI-over-TOML precedence merge.
//!
//! Owns the resolved runtime configuration ([`ScanConfig`], [`ExportConfig`]).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::rules::RuleSet;

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
