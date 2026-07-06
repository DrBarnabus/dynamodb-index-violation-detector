//! Setup screen (PRD §6.3.3, MVP-trimmed).
//!
//! Renders the discovered table schema alongside the loaded config as an
//! editable form: table name, region override, scan settings, export toggles
//! and paths, TTL sub-checks, and a per-index `check_missing` toggle for every
//! GSI/LSI. Hypothetical GSIs are authored in TOML (the in-TUI add-form is
//! deferred) and appear tagged alongside the discovered indexes.
//!
//! The screen holds the form state and exposes primitive mutations — navigate,
//! toggle, edit — that the event loop (task #24) drives from key events. On
//! *Start scan* it projects the form back onto a [`ScanConfig`] via
//! [`SetupScreen::to_scan_config`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::aws::TableDescription;
use crate::config::{ExportConfig, GsiEntry, LsiEntry, ScanConfig, TtlSettings};
use crate::domain::KeySchemaElement;

/// The largest legal `rate_limit_percent` value (PRD §6.2.3).
const MAX_RATE_LIMIT_PERCENT: u8 = 100;

/// The TTL sub-checks in display order (PRD §6.1.3). Index positions are
/// referenced by [`Focus::TtlCheck`].
const TTL_CHECK_LABELS: [&str; 5] = [
    "Missing attribute",
    "Wrong type (not N)",
    "Millisecond magnitude",
    "Malformed (zero/negative/non-integer)",
    "Ignored: > 5 years past",
];

/// A single focusable form control, in navigation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Table,
    Region,
    Segments,
    RateLimit,
    Csv,
    CsvPath,
    Ndjson,
    NdjsonPath,
    TtlEnabled,
    TtlCheck(usize),
    Gsi(usize),
    Lsi(usize),
    Start,
}

/// The TTL audit form block, present only when the table has a TTL attribute.
#[derive(Debug, Clone)]
struct TtlRow {
    attribute: String,
    enabled: bool,
    checks: [bool; 5],
}

/// A GSI row: the entry that will be written back to config plus display facts
/// discovered from the table (or declared inline for a hypothetical index).
#[derive(Debug, Clone)]
struct GsiRow {
    entry: GsiEntry,
    key_desc: String,
}

/// An LSI row. Only `check_missing` is editable (PRD §6.1.2).
#[derive(Debug, Clone)]
struct LsiRow {
    entry: LsiEntry,
    key_desc: String,
}

/// The editable state of the setup screen.
pub struct SetupScreen {
    table: String,
    region: String,
    segments: String,
    rate_limit: String,
    csv: bool,
    csv_path: String,
    ndjson: bool,
    ndjson_path: String,
    ttl: Option<TtlRow>,
    gsis: Vec<GsiRow>,
    lsis: Vec<LsiRow>,
    order: Vec<Focus>,
    focus: usize,
}

impl SetupScreen {
    /// Build the form from a loaded config and the table schema discovered via
    /// `DescribeTable` (PRD §6.3.3).
    ///
    /// Discovered GSIs/LSIs seed the rows; a config `check_missing` intent for a
    /// matching name is carried over. Hypothetical GSIs from the config are
    /// appended, tagged, and shown with their declared key schema.
    pub fn new(config: &ScanConfig, description: &TableDescription) -> Self {
        let gsis = build_gsi_rows(config, description);
        let lsis = build_lsi_rows(config, description);
        let ttl = build_ttl_row(config, description);

        let order = build_order(ttl.as_ref(), gsis.len(), lsis.len());

        Self {
            table: config.table.clone(),
            region: config.region.clone().unwrap_or_default(),
            segments: config.segments.to_string(),
            rate_limit: config
                .rate_limit_percent
                .map(|p| p.to_string())
                .unwrap_or_default(),
            csv: config.export.csv,
            csv_path: path_to_string(&config.export.csv_path),
            ndjson: config.export.ndjson,
            ndjson_path: path_to_string(&config.export.ndjson_path),
            ttl,
            gsis,
            lsis,
            order,
            focus: 0,
        }
    }

    /// Move focus to the next control, wrapping at the end.
    pub fn focus_next(&mut self) {
        self.focus = (self.focus + 1) % self.order.len();
    }

    /// Move focus to the previous control, wrapping at the start.
    pub fn focus_prev(&mut self) {
        self.focus = (self.focus + self.order.len() - 1) % self.order.len();
    }

    /// True when the *Start scan* button is focused, so the event loop can turn
    /// an `Enter` into a `StartScan` command (task #24).
    pub fn is_start_focused(&self) -> bool {
        self.order.get(self.focus) == Some(&Focus::Start)
    }

    /// Flip the focused toggle. No-op on text fields and the Start button.
    pub fn toggle(&mut self) {
        match self.order[self.focus] {
            Focus::Csv => self.csv = !self.csv,
            Focus::Ndjson => self.ndjson = !self.ndjson,
            Focus::TtlEnabled => {
                if let Some(ttl) = &mut self.ttl {
                    ttl.enabled = !ttl.enabled;
                }
            }
            Focus::TtlCheck(i) => {
                if let Some(ttl) = &mut self.ttl {
                    ttl.checks[i] = !ttl.checks[i];
                }
            }
            Focus::Gsi(i) => {
                let entry = &mut self.gsis[i].entry;
                entry.check_missing = !entry.check_missing;
            }
            Focus::Lsi(i) => {
                let entry = &mut self.lsis[i].entry;
                entry.check_missing = !entry.check_missing;
            }
            _ => {}
        }
    }

    /// Append a character to the focused text field. Numeric fields accept
    /// digits only; toggles and the Start button ignore input.
    pub fn input_char(&mut self, c: char) {
        match self.order[self.focus] {
            Focus::Table => self.table.push(c),
            Focus::Region => self.region.push(c),
            Focus::CsvPath => self.csv_path.push(c),
            Focus::NdjsonPath => self.ndjson_path.push(c),
            Focus::Segments if c.is_ascii_digit() => self.segments.push(c),
            Focus::RateLimit if c.is_ascii_digit() => self.rate_limit.push(c),
            _ => {}
        }
    }

    /// Delete the last character of the focused text field.
    pub fn backspace(&mut self) {
        if let Some(field) = self.focused_text_field() {
            field.pop();
        }
    }

    fn focused_text_field(&mut self) -> Option<&mut String> {
        match self.order[self.focus] {
            Focus::Table => Some(&mut self.table),
            Focus::Region => Some(&mut self.region),
            Focus::Segments => Some(&mut self.segments),
            Focus::RateLimit => Some(&mut self.rate_limit),
            Focus::CsvPath => Some(&mut self.csv_path),
            Focus::NdjsonPath => Some(&mut self.ndjson_path),
            _ => None,
        }
    }

    /// Project the form back onto a [`ScanConfig`] for the scan driver.
    ///
    /// Validates the same scalar constraints as the config loader; returns an
    /// actionable message rather than a partially-built config on failure.
    pub fn to_scan_config(&self) -> Result<ScanConfig, String> {
        let table = self.table.trim();
        if table.is_empty() {
            return Err("Table name is required; type a table to scan.".to_string());
        }

        let segments = self
            .segments
            .trim()
            .parse::<usize>()
            .map_err(|_| "Segments must be a whole number.".to_string())?;
        if segments == 0 {
            return Err("Segments must be at least 1.".to_string());
        }

        let rate_limit_percent = match self.rate_limit.trim() {
            "" => None,
            raw => {
                let percent = raw
                    .parse::<u8>()
                    .map_err(|_| "Rate limit must be a whole percentage.".to_string())?;
                if !(1..=MAX_RATE_LIMIT_PERCENT).contains(&percent) {
                    return Err(format!(
                        "Rate limit must be 1..={MAX_RATE_LIMIT_PERCENT}; leave blank for unlimited."
                    ));
                }

                Some(percent)
            }
        };

        Ok(ScanConfig {
            table: table.to_string(),
            region: trimmed_opt(&self.region),
            profile: None,
            segments,
            rate_limit_percent,
            export: ExportConfig {
                csv: self.csv,
                csv_path: trimmed_opt(&self.csv_path).map(Into::into),
                ndjson: self.ndjson,
                ndjson_path: trimmed_opt(&self.ndjson_path).map(Into::into),
            },
            gsi: self.gsis.iter().map(|row| row.entry.clone()).collect(),
            lsi: self.lsis.iter().map(|row| row.entry.clone()).collect(),
            ttl: self.ttl.as_ref().map(TtlRow::to_settings),
        })
    }

    /// Draw the form into `area` (PRD §8.6).
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" Scan setup ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let (lines, focused_line) = self.build_lines();

        let height = inner.height as usize;
        let scroll = focused_line
            .saturating_sub(height.saturating_sub(1))
            .min(lines.len().saturating_sub(height)) as u16;

        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), inner);
    }

    fn build_lines(&self) -> (Vec<Line<'static>>, usize) {
        let mut b = LineBuilder::new(self.order.get(self.focus).copied());

        b.header("AWS");
        b.text_field(Focus::Table, "Table", &self.table, None);
        b.text_field(
            Focus::Region,
            "Region override",
            &self.region,
            Some("(profile default)"),
        );

        b.header("Scan");
        b.text_field(Focus::Segments, "Segments", &self.segments, None);
        b.text_field(
            Focus::RateLimit,
            "Rate limit %",
            &self.rate_limit,
            Some("unlimited"),
        );

        b.header("Export");
        b.toggle(Focus::Csv, "CSV", self.csv, 0);
        b.text_field(Focus::CsvPath, "  path", &self.csv_path, Some("(default)"));
        b.toggle(Focus::Ndjson, "NDJSON", self.ndjson, 0);
        b.text_field(
            Focus::NdjsonPath,
            "  path",
            &self.ndjson_path,
            Some("(default)"),
        );

        if let Some(ttl) = &self.ttl {
            b.header(&format!("TTL  (attribute `{}`)", ttl.attribute));
            b.toggle(Focus::TtlEnabled, "Enabled", ttl.enabled, 0);
            for (i, label) in TTL_CHECK_LABELS.iter().enumerate() {
                b.toggle(Focus::TtlCheck(i), label, ttl.checks[i], 1);
            }
        }

        if !self.gsis.is_empty() {
            b.header("GSIs  (type + size checks always on)");
            for (i, row) in self.gsis.iter().enumerate() {
                let label = format!(
                    "{} {}  {}  — check missing key",
                    row.entry.name,
                    tag(row.entry.hypothetical),
                    row.key_desc,
                );
                b.toggle(Focus::Gsi(i), &label, row.entry.check_missing, 0);
            }
        }

        if !self.lsis.is_empty() {
            b.header("LSIs");
            for (i, row) in self.lsis.iter().enumerate() {
                let label = format!("{}  {}  — check missing key", row.entry.name, row.key_desc);
                b.toggle(Focus::Lsi(i), &label, row.entry.check_missing, 0);
            }
        }

        b.blank();
        b.button(Focus::Start, "Start scan");
        b.hint("↑/↓ move · space toggle · type to edit · enter start · q quit");

        (b.lines, b.focused_line)
    }
}

impl TtlRow {
    fn to_settings(&self) -> TtlSettings {
        TtlSettings {
            enabled: Some(self.enabled),
            check_missing: Some(self.checks[0]),
            check_wrong_type: Some(self.checks[1]),
            check_ms_magnitude: Some(self.checks[2]),
            check_malformed: Some(self.checks[3]),
            check_past_5_years: Some(self.checks[4]),
        }
    }
}

/// Accumulates styled lines and tracks which line holds the focused control so
/// the caller can scroll it into view.
struct LineBuilder {
    lines: Vec<Line<'static>>,
    focused: Option<Focus>,
    focused_line: usize,
}

impl LineBuilder {
    fn new(focused: Option<Focus>) -> Self {
        Self {
            lines: Vec::new(),
            focused,
            focused_line: 0,
        }
    }

    fn header(&mut self, title: &str) {
        self.lines.push(Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    }

    fn blank(&mut self) {
        self.lines.push(Line::from(String::new()));
    }

    fn hint(&mut self, text: &str) {
        self.lines.push(Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    }

    fn text_field(&mut self, focus: Focus, label: &str, value: &str, placeholder: Option<&str>) {
        let is_focused = self.focused == Some(focus);
        let shown = if value.is_empty() {
            placeholder.unwrap_or("").to_string()
        } else {
            value.to_string()
        };
        let cursor = if is_focused { "█" } else { "" };
        let content = format!("  {label}: {shown}{cursor}");
        self.push_focusable(content, is_focused);
    }

    fn toggle(&mut self, focus: Focus, label: &str, on: bool, indent: usize) {
        let is_focused = self.focused == Some(focus);
        let box_ = if on { "[x]" } else { "[ ]" };
        let pad = "  ".repeat(indent + 1);
        let content = format!("{pad}{box_} {label}");
        self.push_focusable(content, is_focused);
    }

    fn button(&mut self, focus: Focus, label: &str) {
        let is_focused = self.focused == Some(focus);
        let content = format!("  [ {label} ]");
        self.push_focusable(content, is_focused);
    }

    fn push_focusable(&mut self, content: String, is_focused: bool) {
        if is_focused {
            self.focused_line = self.lines.len();
        }

        let style = if is_focused {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        self.lines.push(Line::from(Span::styled(content, style)));
    }
}

fn build_gsi_rows(config: &ScanConfig, description: &TableDescription) -> Vec<GsiRow> {
    let mut rows: Vec<GsiRow> = description
        .gsis
        .iter()
        .map(|schema| {
            let check_missing = config
                .gsi
                .iter()
                .find(|g| !g.hypothetical && g.name == schema.name)
                .is_some_and(|g| g.check_missing);
            GsiRow {
                entry: GsiEntry {
                    name: schema.name.clone(),
                    hypothetical: false,
                    pk: None,
                    sk: None,
                    check_missing,
                },
                key_desc: fmt_key(&schema.pk, schema.sk.as_ref()),
            }
        })
        .collect();

    for entry in config.gsi.iter().filter(|g| g.hypothetical) {
        let key_desc = match &entry.pk {
            Some(pk) => fmt_key(pk, entry.sk.as_ref()),
            None => "no key schema".to_string(),
        };
        rows.push(GsiRow {
            entry: entry.clone(),
            key_desc,
        });
    }

    rows
}

fn build_lsi_rows(config: &ScanConfig, description: &TableDescription) -> Vec<LsiRow> {
    description
        .lsis
        .iter()
        .map(|schema| {
            let check_missing = config
                .lsi
                .iter()
                .find(|l| l.name == schema.name)
                .is_some_and(|l| l.check_missing);
            LsiRow {
                entry: LsiEntry {
                    name: schema.name.clone(),
                    check_missing,
                },
                key_desc: fmt_key(&schema.pk, schema.sk.as_ref()),
            }
        })
        .collect()
}

fn build_ttl_row(config: &ScanConfig, description: &TableDescription) -> Option<TtlRow> {
    let ttl = description.ttl.as_ref()?;
    let settings = config.ttl.clone().unwrap_or_default();
    Some(TtlRow {
        attribute: ttl.attribute.clone(),
        enabled: settings.enabled.unwrap_or(true),
        checks: [
            settings.check_missing.unwrap_or(true),
            settings.check_wrong_type.unwrap_or(true),
            settings.check_ms_magnitude.unwrap_or(true),
            settings.check_malformed.unwrap_or(true),
            settings.check_past_5_years.unwrap_or(false),
        ],
    })
}

fn build_order(ttl: Option<&TtlRow>, gsi_count: usize, lsi_count: usize) -> Vec<Focus> {
    let mut order = vec![
        Focus::Table,
        Focus::Region,
        Focus::Segments,
        Focus::RateLimit,
        Focus::Csv,
        Focus::CsvPath,
        Focus::Ndjson,
        Focus::NdjsonPath,
    ];

    if ttl.is_some() {
        order.push(Focus::TtlEnabled);
        order.extend((0..TTL_CHECK_LABELS.len()).map(Focus::TtlCheck));
    }

    order.extend((0..gsi_count).map(Focus::Gsi));
    order.extend((0..lsi_count).map(Focus::Lsi));
    order.push(Focus::Start);
    order
}

fn fmt_key(pk: &KeySchemaElement, sk: Option<&KeySchemaElement>) -> String {
    match sk {
        Some(sk) => format!(
            "pk {}({:?}) · sk {}({:?})",
            pk.name, pk.type_code, sk.name, sk.type_code
        ),
        None => format!("pk {}({:?})", pk.name, pk.type_code),
    }
}

fn tag(hypothetical: bool) -> &'static str {
    if hypothetical {
        "[hypothetical]"
    } else {
        "[existing]"
    }
}

fn path_to_string(path: &Option<std::path::PathBuf>) -> String {
    path.as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

fn trimmed_opt(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::{IndexSchema, TableKeySchema, TtlDescription};
    use crate::domain::TypeCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(name: &str, type_code: TypeCode) -> KeySchemaElement {
        KeySchemaElement {
            name: name.to_string(),
            type_code,
        }
    }

    fn description() -> TableDescription {
        TableDescription {
            name: "users".to_string(),
            key_schema: TableKeySchema {
                pk: key("id", TypeCode::S),
                sk: None,
            },
            gsis: vec![IndexSchema {
                name: "GSI1".to_string(),
                pk: key("email", TypeCode::S),
                sk: Some(key("createdAt", TypeCode::N)),
            }],
            lsis: vec![IndexSchema {
                name: "LSI1".to_string(),
                pk: key("id", TypeCode::S),
                sk: Some(key("status", TypeCode::S)),
            }],
            ttl: Some(TtlDescription {
                attribute: "expiresAt".to_string(),
                enabled: true,
            }),
            provisioned_rcu: Some(100),
            item_count: 42,
        }
    }

    fn config() -> ScanConfig {
        ScanConfig {
            table: "users".to_string(),
            region: Some("eu-west-1".to_string()),
            profile: None,
            segments: 8,
            rate_limit_percent: Some(60),
            export: ExportConfig {
                csv: true,
                csv_path: None,
                ndjson: false,
                ndjson_path: None,
            },
            gsi: vec![
                GsiEntry {
                    name: "GSI1".to_string(),
                    hypothetical: false,
                    pk: None,
                    sk: None,
                    check_missing: true,
                },
                GsiEntry {
                    name: "GSI_hypo".to_string(),
                    hypothetical: true,
                    pk: Some(key("userId", TypeCode::S)),
                    sk: Some(key("ts", TypeCode::N)),
                    check_missing: false,
                },
            ],
            lsi: vec![LsiEntry {
                name: "LSI1".to_string(),
                check_missing: true,
            }],
            ttl: None,
        }
    }

    fn focus_on(screen: &mut SetupScreen, target: Focus) {
        let index = screen
            .order
            .iter()
            .position(|f| *f == target)
            .expect("focus target in order");
        screen.focus = index;
    }

    #[test]
    fn seeds_scalar_fields_from_config() {
        let screen = SetupScreen::new(&config(), &description());

        assert_eq!(screen.table, "users");
        assert_eq!(screen.region, "eu-west-1");
        assert_eq!(screen.segments, "8");
        assert_eq!(screen.rate_limit, "60");
        assert!(screen.csv);
        assert!(!screen.ndjson);
    }

    #[test]
    fn unlimited_rate_and_default_region_render_empty() {
        let mut cfg = config();
        cfg.rate_limit_percent = None;
        cfg.region = None;

        let screen = SetupScreen::new(&cfg, &description());
        assert_eq!(screen.rate_limit, "");
        assert_eq!(screen.region, "");
    }

    #[test]
    fn gsi_rows_union_discovered_and_hypothetical_with_carried_intent() {
        let screen = SetupScreen::new(&config(), &description());

        assert_eq!(screen.gsis.len(), 2);

        let existing = &screen.gsis[0];
        assert_eq!(existing.entry.name, "GSI1");
        assert!(!existing.entry.hypothetical);
        assert!(existing.entry.check_missing);
        assert_eq!(existing.key_desc, "pk email(S) · sk createdAt(N)");

        let hypo = &screen.gsis[1];
        assert_eq!(hypo.entry.name, "GSI_hypo");
        assert!(hypo.entry.hypothetical);
        assert_eq!(hypo.key_desc, "pk userId(S) · sk ts(N)");
    }

    #[test]
    fn lsi_rows_carry_missing_intent() {
        let screen = SetupScreen::new(&config(), &description());

        assert_eq!(screen.lsis.len(), 1);
        assert!(screen.lsis[0].entry.check_missing);
    }

    #[test]
    fn ttl_row_defaults_when_config_absent_but_attribute_discovered() {
        let screen = SetupScreen::new(&config(), &description());

        let ttl = screen.ttl.as_ref().expect("ttl row present");
        assert_eq!(ttl.attribute, "expiresAt");
        assert!(ttl.enabled);
        assert_eq!(ttl.checks, [true, true, true, true, false]);
    }

    #[test]
    fn no_ttl_row_when_table_lacks_attribute() {
        let mut desc = description();
        desc.ttl = None;

        let screen = SetupScreen::new(&config(), &desc);
        assert!(screen.ttl.is_none());
        assert!(!screen.order.contains(&Focus::TtlEnabled));
    }

    #[test]
    fn focus_navigation_wraps_both_directions() {
        let mut screen = SetupScreen::new(&config(), &description());
        assert_eq!(screen.order[screen.focus], Focus::Table);

        screen.focus_prev();
        assert_eq!(screen.order[screen.focus], Focus::Start);
        assert!(screen.is_start_focused());

        screen.focus_next();
        assert_eq!(screen.order[screen.focus], Focus::Table);
    }

    #[test]
    fn toggle_flips_focused_check_missing() {
        let mut screen = SetupScreen::new(&config(), &description());
        focus_on(&mut screen, Focus::Gsi(1));

        assert!(!screen.gsis[1].entry.check_missing);
        screen.toggle();
        assert!(screen.gsis[1].entry.check_missing);
    }

    #[test]
    fn toggle_is_noop_on_text_field() {
        let mut screen = SetupScreen::new(&config(), &description());
        focus_on(&mut screen, Focus::Table);
        let before = screen.table.clone();

        screen.toggle();
        assert_eq!(screen.table, before);
    }

    #[test]
    fn input_and_backspace_edit_focused_text_field() {
        let mut screen = SetupScreen::new(&config(), &description());
        focus_on(&mut screen, Focus::Table);
        screen.backspace();
        screen.input_char('X');

        assert_eq!(screen.table, "userX");
    }

    #[test]
    fn numeric_fields_reject_non_digits() {
        let mut screen = SetupScreen::new(&config(), &description());
        focus_on(&mut screen, Focus::Segments);
        screen.input_char('a');
        assert_eq!(screen.segments, "8");

        screen.input_char('0');
        assert_eq!(screen.segments, "80");
    }

    #[test]
    fn to_scan_config_round_trips_indexes_and_ttl() {
        let screen = SetupScreen::new(&config(), &description());
        let resolved = screen.to_scan_config().expect("valid form");

        assert_eq!(resolved.table, "users");
        assert_eq!(resolved.region.as_deref(), Some("eu-west-1"));
        assert_eq!(resolved.segments, 8);
        assert_eq!(resolved.rate_limit_percent, Some(60));

        assert_eq!(resolved.gsi.len(), 2);
        assert!(resolved.gsi[1].hypothetical);
        assert_eq!(resolved.gsi[1].pk, Some(key("userId", TypeCode::S)));
        assert_eq!(resolved.lsi.len(), 1);

        let ttl = resolved.ttl.expect("ttl carried");
        assert_eq!(ttl.enabled, Some(true));
        assert_eq!(ttl.check_past_5_years, Some(false));
    }

    #[test]
    fn to_scan_config_reflects_edits() {
        let mut screen = SetupScreen::new(&config(), &description());
        focus_on(&mut screen, Focus::Ndjson);
        screen.toggle();
        focus_on(&mut screen, Focus::NdjsonPath);
        for c in "out.ndjson".chars() {
            screen.input_char(c);
        }

        let resolved = screen.to_scan_config().unwrap();
        assert!(resolved.export.ndjson);
        assert_eq!(
            resolved.export.ndjson_path,
            Some(std::path::PathBuf::from("out.ndjson"))
        );
    }

    #[test]
    fn to_scan_config_rejects_empty_table() {
        let mut screen = SetupScreen::new(&config(), &description());
        screen.table.clear();

        assert!(screen.to_scan_config().is_err());
    }

    #[test]
    fn to_scan_config_rejects_zero_segments() {
        let mut screen = SetupScreen::new(&config(), &description());
        screen.segments = "0".to_string();

        assert!(screen.to_scan_config().is_err());
    }

    #[test]
    fn to_scan_config_rejects_out_of_range_rate() {
        let mut screen = SetupScreen::new(&config(), &description());
        screen.rate_limit = "150".to_string();

        assert!(screen.to_scan_config().is_err());
    }

    #[test]
    fn blank_rate_limit_is_unlimited() {
        let mut screen = SetupScreen::new(&config(), &description());
        screen.rate_limit.clear();

        assert_eq!(screen.to_scan_config().unwrap().rate_limit_percent, None);
    }

    fn buffer_text(screen: &SetupScreen) -> String {
        let backend = TestBackend::new(90, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area()))
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        let area = buffer.area;
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }

        text
    }

    #[test]
    fn render_shows_key_facts() {
        let screen = SetupScreen::new(&config(), &description());
        let text = buffer_text(&screen);

        assert!(text.contains("Scan setup"));
        assert!(text.contains("users"));
        assert!(text.contains("GSI1"));
        assert!(text.contains("[hypothetical]"));
        assert!(text.contains("expiresAt"));
        assert!(text.contains("Start scan"));
    }

    #[test]
    fn render_scrolls_focused_control_into_view() {
        let mut screen = SetupScreen::new(&config(), &description());
        focus_on(&mut screen, Focus::Start);

        let backend = TestBackend::new(90, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area()))
            .unwrap();

        let buffer = terminal.backend().buffer().clone();
        let area = buffer.area;
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }

        assert!(
            text.contains("Start scan"),
            "focused button must be visible"
        );
    }
}
