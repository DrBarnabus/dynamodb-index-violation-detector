//! Setup screen.
//!
//! Renders the discovered table schema alongside the loaded config as an
//! editable form: table picker, region override, scan settings, export toggles
//! and paths, TTL sub-checks, and a per-index `check_missing` toggle for every
//! GSI/LSI. Hypothetical GSIs, from TOML or the *Add hypothetical GSI* form,
//! appear tagged alongside the discovered indexes.
//!
//! The table field filters the `ListTables` result as the user types; choosing
//! a table hands its name to the shell, which describes it and calls
//! [`SetupScreen::load_table`] to rebuild the index and TTL rows. An edited
//! region is handed over when focus leaves its field, so the shell can
//! reconnect and refresh the table list and loaded schema.
//!
//! The screen holds the form state and exposes primitive mutations — navigate,
//! toggle, edit — that the event loop drives from key events. On
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

use super::gsi_form::GsiForm;
use super::picker::FuzzyList;

/// The largest legal `rate_limit_percent` value.
const MAX_RATE_LIMIT_PERCENT: u8 = 100;

/// Table matches shown beneath the focused table field.
const TABLE_LIST_ROWS: usize = 6;

/// The TTL sub-checks in display order. Index positions are
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
    AddGsi,
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

/// An LSI row. Only `check_missing` is editable.
#[derive(Debug, Clone)]
struct LsiRow {
    entry: LsiEntry,
    key_desc: String,
}

/// The config's per-index and TTL intents, re-applied whenever a table is
/// described so a later table choice still honours them.
struct Intents {
    gsi: Vec<GsiEntry>,
    lsi: Vec<LsiEntry>,
    ttl: Option<TtlSettings>,
}

/// The editable state of the setup screen.
pub struct SetupScreen {
    intents: Intents,
    table: FuzzyList,
    loaded_table: Option<String>,
    region: String,
    connected_region: Option<String>,
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
    gsi_form: Option<GsiForm>,
}

impl SetupScreen {
    /// Build the form from a loaded config, the tables offered by the picker,
    /// and the schema of the configured table when it has been described.
    ///
    /// Discovered GSIs/LSIs seed the rows; a config `check_missing` intent for a
    /// matching name is carried over. Hypothetical GSIs from the config are
    /// appended, tagged, and shown with their declared key schema.
    pub fn new(
        config: &ScanConfig,
        description: Option<&TableDescription>,
        tables: Vec<String>,
    ) -> Self {
        let mut screen = Self {
            intents: Intents {
                gsi: config.gsi.clone(),
                lsi: config.lsi.clone(),
                ttl: config.ttl.clone(),
            },
            table: FuzzyList::new(tables, &config.table),
            loaded_table: None,
            region: config.region.clone().unwrap_or_default(),
            connected_region: config.region.clone(),
            segments: config.segments.to_string(),
            rate_limit: config
                .rate_limit_percent
                .map(|p| p.to_string())
                .unwrap_or_default(),
            csv: config.export.csv,
            csv_path: path_to_string(&config.export.csv_path),
            ndjson: config.export.ndjson,
            ndjson_path: path_to_string(&config.export.ndjson_path),
            ttl: None,
            gsis: Vec::new(),
            lsis: Vec::new(),
            order: Vec::new(),
            focus: 0,
            gsi_form: None,
        };
        screen.rebuild_rows(description);
        screen
    }

    /// Rebuild the index and TTL rows for a newly described table, keeping
    /// focus on the same control where it still exists.
    pub fn load_table(&mut self, description: &TableDescription) {
        self.table.set_query(&description.name);
        self.rebuild_rows(Some(description));
    }

    /// Clear the discovered rows when the loaded table is unavailable, e.g. it
    /// does not exist in a newly selected region.
    pub fn unload_table(&mut self) {
        self.rebuild_rows(None);
    }

    /// The table whose schema the index rows reflect, if any.
    pub fn loaded_table(&self) -> Option<&str> {
        self.loaded_table.as_deref()
    }

    /// Replace the tables offered by the picker, keeping what has been typed.
    pub fn set_tables(&mut self, tables: Vec<String>) {
        self.table.set_items(tables);
    }

    /// The region to reconnect to once focus has left an edited region field;
    /// the inner `None` means the profile's default region. Marks the change as
    /// handed over, so each edit is reported once.
    pub fn take_region_change(&mut self) -> Option<Option<String>> {
        let region = trimmed_opt(&self.region);
        if self.order[self.focus] == Focus::Region || region == self.connected_region {
            return None;
        }

        self.connected_region = region.clone();
        Some(region)
    }

    fn rebuild_rows(&mut self, description: Option<&TableDescription>) {
        let focused = self.order.get(self.focus).copied();

        self.gsis = build_gsi_rows(&self.intents.gsi, description);
        self.lsis = build_lsi_rows(&self.intents.lsi, description);
        self.ttl = build_ttl_row(self.intents.ttl.as_ref(), description);
        self.rebuild_order(focused);
        self.loaded_table = description.map(|d| d.name.clone());
    }

    fn rebuild_order(&mut self, focused: Option<Focus>) {
        self.order = build_order(self.ttl.as_ref(), self.gsis.len(), self.lsis.len());
        self.focus = focused
            .and_then(|focused| self.order.iter().position(|f| *f == focused))
            .unwrap_or(0);
    }

    /// True when the table field is focused and has matches to move through,
    /// so the event loop routes ↑/↓ to the list rather than between fields.
    pub fn is_table_list_active(&self) -> bool {
        self.is_table_focused() && self.table.has_matches()
    }

    pub fn table_list_next(&mut self) {
        self.table.select_next();
    }

    pub fn table_list_prev(&mut self) {
        self.table.select_prev();
    }

    /// Commit the table field: the highlighted match, or the typed name when
    /// nothing matches (e.g. `ListTables` is not permitted). Advances focus and
    /// returns the name when it still needs describing; `None` when the field
    /// is empty or the table is already loaded.
    pub fn choose_table(&mut self) -> Option<String> {
        let name = match self.table.selected() {
            Some(name) => name.to_string(),
            None => self.table.query().trim().to_string(),
        };
        if name.is_empty() {
            return None;
        }

        self.table.set_query(&name);
        self.focus_next();
        (self.loaded_table.as_deref() != Some(name.as_str())).then_some(name)
    }

    /// True when the table field is focused.
    pub fn is_table_focused(&self) -> bool {
        self.order[self.focus] == Focus::Table
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
    /// an `Enter` into a `StartScan` command.
    pub fn is_start_focused(&self) -> bool {
        self.order.get(self.focus) == Some(&Focus::Start)
    }

    /// True when the focused control is a text field, so the event loop routes
    /// printable characters (including space) to editing rather than toggling.
    pub fn focus_is_text(&self) -> bool {
        matches!(
            self.order[self.focus],
            Focus::Table
                | Focus::Region
                | Focus::Segments
                | Focus::RateLimit
                | Focus::CsvPath
                | Focus::NdjsonPath
        )
    }

    /// True when *Add hypothetical GSI* is focused, so the event loop can turn
    /// an `Enter` into opening the add-form.
    pub fn is_add_gsi_focused(&self) -> bool {
        self.order[self.focus] == Focus::AddGsi
    }

    pub fn open_gsi_form(&mut self) {
        self.gsi_form = Some(GsiForm::new());
    }

    pub fn close_gsi_form(&mut self) {
        self.gsi_form = None;
    }

    /// The open add-form, which takes all key input while raised.
    pub(super) fn gsi_form_mut(&mut self) -> Option<&mut GsiForm> {
        self.gsi_form.as_mut()
    }

    /// Validate the open add-form and, when valid, append its index as a
    /// focused hypothetical row and close the form. An invalid form stays open
    /// showing why.
    pub fn submit_gsi_form(&mut self) {
        let Some(form) = &mut self.gsi_form else {
            return;
        };
        let taken = self.gsis.iter().map(|row| row.entry.name.as_str());
        let Some(entry) = form.build(taken) else {
            return;
        };

        self.gsi_form = None;
        self.intents.gsi.push(entry.clone());
        self.gsis.push(hypothetical_row(entry));
        self.rebuild_order(Some(Focus::Gsi(self.gsis.len() - 1)));
    }

    /// Remove the focused GSI when it is hypothetical; discovered indexes stay.
    pub fn remove_focused_gsi(&mut self) {
        let Focus::Gsi(i) = self.order[self.focus] else {
            return;
        };
        if !self.gsis[i].entry.hypothetical {
            return;
        }

        let removed = self.gsis.remove(i);
        self.intents
            .gsi
            .retain(|g| !(g.hypothetical && g.name == removed.entry.name));
        let next = if i < self.gsis.len() {
            Focus::Gsi(i)
        } else {
            Focus::AddGsi
        };
        self.rebuild_order(Some(next));
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
        let field = match self.order[self.focus] {
            Focus::Table => return self.table.backspace(),
            Focus::Region => &mut self.region,
            Focus::Segments => &mut self.segments,
            Focus::RateLimit => &mut self.rate_limit,
            Focus::CsvPath => &mut self.csv_path,
            Focus::NdjsonPath => &mut self.ndjson_path,
            _ => return,
        };
        field.pop();
    }

    /// Project the form back onto a [`ScanConfig`] for the scan driver.
    ///
    /// Validates the same scalar constraints as the config loader; returns an
    /// actionable message rather than a partially-built config on failure.
    pub fn to_scan_config(&self) -> Result<ScanConfig, String> {
        let table = self.table.query().trim();
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

    /// Draw the form into `area`.
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

        if let Some(form) = &self.gsi_form {
            form.render(frame, area);
        }
    }

    fn build_lines(&self) -> (Vec<Line<'static>>, usize) {
        let mut b = LineBuilder::new(self.order.get(self.focus).copied());

        b.header("AWS");
        b.text_field(
            Focus::Table,
            "Table",
            self.table.query(),
            Some("type to filter"),
        );
        if self.is_table_focused() && self.table.has_items() {
            let matches = self
                .table
                .lines(TABLE_LIST_ROWS, |i| format!("      {}", self.table.item(i)));
            if matches.is_empty() {
                b.hint("      no listed table matches; enter uses the typed name");
            }
            b.lines.extend(matches);
        }
        if self.loaded_table.is_none() {
            b.hint("  choose a table to discover its indexes and TTL");
        }
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
        b.button(Focus::AddGsi, "+ Add hypothetical GSI");

        if !self.lsis.is_empty() {
            b.header("LSIs");
            for (i, row) in self.lsis.iter().enumerate() {
                let label = format!("{}  {}  — check missing key", row.entry.name, row.key_desc);
                b.toggle(Focus::Lsi(i), &label, row.entry.check_missing, 0);
            }
        }

        b.blank();
        b.button(Focus::Start, "Start scan");
        b.hint("↑/↓ move · space toggle · type to edit · enter choose / start · esc quit");
        b.hint("del removes a hypothetical GSI");

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

fn build_gsi_rows(intents: &[GsiEntry], description: Option<&TableDescription>) -> Vec<GsiRow> {
    let mut rows: Vec<GsiRow> = description
        .map(|d| d.gsis.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|schema| {
            let check_missing = intents
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

    rows.extend(
        intents
            .iter()
            .filter(|g| g.hypothetical)
            .cloned()
            .map(hypothetical_row),
    );
    rows
}

fn hypothetical_row(entry: GsiEntry) -> GsiRow {
    let key_desc = match &entry.pk {
        Some(pk) => fmt_key(pk, entry.sk.as_ref()),
        None => "no key schema".to_string(),
    };
    GsiRow { entry, key_desc }
}

fn build_lsi_rows(intents: &[LsiEntry], description: Option<&TableDescription>) -> Vec<LsiRow> {
    description
        .map(|d| d.lsis.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|schema| {
            let check_missing = intents
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

fn build_ttl_row(
    intent: Option<&TtlSettings>,
    description: Option<&TableDescription>,
) -> Option<TtlRow> {
    let ttl = description?.ttl.as_ref()?;
    let settings = intent.cloned().unwrap_or_default();
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
    order.push(Focus::AddGsi);
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
        let screen = SetupScreen::new(&config(), Some(&description()), Vec::new());

        assert_eq!(screen.table.query(), "users");
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

        let screen = SetupScreen::new(&cfg, Some(&description()), Vec::new());
        assert_eq!(screen.rate_limit, "");
        assert_eq!(screen.region, "");
    }

    #[test]
    fn gsi_rows_union_discovered_and_hypothetical_with_carried_intent() {
        let screen = SetupScreen::new(&config(), Some(&description()), Vec::new());

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
        let screen = SetupScreen::new(&config(), Some(&description()), Vec::new());

        assert_eq!(screen.lsis.len(), 1);
        assert!(screen.lsis[0].entry.check_missing);
    }

    #[test]
    fn ttl_row_defaults_when_config_absent_but_attribute_discovered() {
        let screen = SetupScreen::new(&config(), Some(&description()), Vec::new());

        let ttl = screen.ttl.as_ref().expect("ttl row present");
        assert_eq!(ttl.attribute, "expiresAt");
        assert!(ttl.enabled);
        assert_eq!(ttl.checks, [true, true, true, true, false]);
    }

    #[test]
    fn no_ttl_row_when_table_lacks_attribute() {
        let mut desc = description();
        desc.ttl = None;

        let screen = SetupScreen::new(&config(), Some(&desc), Vec::new());
        assert!(screen.ttl.is_none());
        assert!(!screen.order.contains(&Focus::TtlEnabled));
    }

    #[test]
    fn focus_navigation_wraps_both_directions() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        assert_eq!(screen.order[screen.focus], Focus::Table);

        screen.focus_prev();
        assert_eq!(screen.order[screen.focus], Focus::Start);
        assert!(screen.is_start_focused());

        screen.focus_next();
        assert_eq!(screen.order[screen.focus], Focus::Table);
    }

    #[test]
    fn toggle_flips_focused_check_missing() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        focus_on(&mut screen, Focus::Gsi(1));

        assert!(!screen.gsis[1].entry.check_missing);
        screen.toggle();
        assert!(screen.gsis[1].entry.check_missing);
    }

    #[test]
    fn toggle_is_noop_on_text_field() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        focus_on(&mut screen, Focus::Table);
        let before = screen.table.query().to_string();

        screen.toggle();
        assert_eq!(screen.table.query(), before);
    }

    #[test]
    fn input_and_backspace_edit_focused_text_field() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        focus_on(&mut screen, Focus::Table);
        screen.backspace();
        screen.input_char('X');

        assert_eq!(screen.table.query(), "userX");
    }

    #[test]
    fn numeric_fields_reject_non_digits() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        focus_on(&mut screen, Focus::Segments);
        screen.input_char('a');
        assert_eq!(screen.segments, "8");

        screen.input_char('0');
        assert_eq!(screen.segments, "80");
    }

    #[test]
    fn to_scan_config_round_trips_indexes_and_ttl() {
        let screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
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
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
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
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        screen.table.set_query("");

        assert!(screen.to_scan_config().is_err());
    }

    #[test]
    fn to_scan_config_rejects_zero_segments() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        screen.segments = "0".to_string();

        assert!(screen.to_scan_config().is_err());
    }

    #[test]
    fn to_scan_config_rejects_out_of_range_rate() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        screen.rate_limit = "150".to_string();

        assert!(screen.to_scan_config().is_err());
    }

    #[test]
    fn blank_rate_limit_is_unlimited() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        screen.rate_limit.clear();

        assert_eq!(screen.to_scan_config().unwrap().rate_limit_percent, None);
    }

    fn buffer_text(screen: &SetupScreen) -> String {
        let backend = TestBackend::new(90, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area()))
            .unwrap();

        crate::tui::buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn render_shows_key_facts() {
        let screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
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
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        focus_on(&mut screen, Focus::Start);

        let backend = TestBackend::new(90, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| screen.render(frame, frame.area()))
            .unwrap();

        let text = crate::tui::buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("Start scan"),
            "focused button must be visible"
        );
    }

    fn tables() -> Vec<String> {
        ["orders", "prod-orders", "users"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn unloaded() -> SetupScreen {
        let mut cfg = config();
        cfg.table = String::new();
        SetupScreen::new(&cfg, None, tables())
    }

    #[test]
    fn without_a_described_table_only_hypothetical_rows_show() {
        let screen = unloaded();

        assert_eq!(screen.loaded_table(), None);
        assert_eq!(screen.gsis.len(), 1);
        assert!(screen.gsis[0].entry.hypothetical);
        assert!(screen.lsis.is_empty());
        assert!(screen.ttl.is_none());
        assert!(buffer_text(&screen).contains("choose a table"));
    }

    #[test]
    fn load_table_rebuilds_rows_and_keeps_focus() {
        let mut screen = unloaded();
        focus_on(&mut screen, Focus::Region);

        screen.load_table(&description());

        assert_eq!(screen.loaded_table(), Some("users"));
        assert_eq!(screen.table.query(), "users");
        assert_eq!(screen.gsis.len(), 2);
        assert!(screen.gsis[0].entry.check_missing, "config intent carried");
        assert_eq!(screen.lsis.len(), 1);
        assert!(screen.ttl.is_some());
        assert!(screen.order.contains(&Focus::TtlEnabled));
        assert_eq!(screen.order[screen.focus], Focus::Region);
    }

    #[test]
    fn typing_filters_tables_and_choose_picks_the_highlight() {
        let mut screen = unloaded();
        for c in "ord".chars() {
            screen.input_char(c);
        }
        screen.table_list_next();

        assert!(screen.is_table_list_active());
        assert_eq!(screen.choose_table().as_deref(), Some("prod-orders"));
        assert_eq!(screen.table.query(), "prod-orders");
        assert_eq!(screen.order[screen.focus], Focus::Region);
        assert!(!screen.is_table_list_active());
    }

    #[test]
    fn choose_table_skips_the_already_loaded_table() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), tables());
        assert_eq!(screen.choose_table(), None);
        assert_eq!(screen.order[screen.focus], Focus::Region);
    }

    #[test]
    fn choose_table_falls_back_to_the_typed_name() {
        let mut screen = unloaded();
        assert_eq!(screen.choose_table(), Some("orders".to_string()));

        let mut screen = unloaded();
        for c in "unlisted".chars() {
            screen.input_char(c);
        }
        assert!(!screen.is_table_list_active());
        assert_eq!(screen.choose_table().as_deref(), Some("unlisted"));

        let mut screen = SetupScreen::new(&config(), None, Vec::new());
        screen.table.set_query("  ");
        assert_eq!(screen.choose_table(), None);
        assert!(screen.is_table_focused());
    }

    #[test]
    fn render_lists_matching_tables_while_the_field_is_focused() {
        let mut screen = unloaded();
        screen.input_char('u');
        let text = buffer_text(&screen);
        assert!(text.contains("users"));
        assert!(!text.contains("prod-orders"));

        focus_on(&mut screen, Focus::Region);
        assert!(!buffer_text(&screen).contains("      users"));
    }

    #[test]
    fn render_explains_an_unmatched_query() {
        let mut screen = unloaded();
        screen.input_char('z');
        assert!(buffer_text(&screen).contains("no listed table matches"));
    }

    #[test]
    fn region_change_is_reported_once_focus_leaves_the_field() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), tables());
        focus_on(&mut screen, Focus::Region);
        screen.backspace();
        screen.input_char('2');
        assert_eq!(screen.take_region_change(), None, "still editing");

        screen.focus_next();
        assert_eq!(
            screen.take_region_change(),
            Some(Some("eu-west-2".to_string()))
        );
        assert_eq!(screen.take_region_change(), None, "reported once");
    }

    #[test]
    fn unchanged_or_restored_region_is_not_reported() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), tables());
        focus_on(&mut screen, Focus::Region);
        screen.input_char(' ');
        screen.focus_next();
        assert_eq!(screen.take_region_change(), None, "whitespace only");

        focus_on(&mut screen, Focus::Region);
        screen.region.clear();
        screen.focus_next();
        assert_eq!(screen.take_region_change(), Some(None), "profile default");
    }

    fn submit_form(screen: &mut SetupScreen, name: &str, pk: &str) {
        focus_on(screen, Focus::AddGsi);
        screen.open_gsi_form();
        let form = screen.gsi_form_mut().expect("form open");
        name.chars().for_each(|c| form.input_char(c));
        form.focus_next();
        pk.chars().for_each(|c| form.input_char(c));
        screen.submit_gsi_form();
    }

    #[test]
    fn submitted_gsi_form_appends_a_focused_hypothetical_row_that_survives_reloads() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        submit_form(&mut screen, "byOrg", "orgId");

        assert!(screen.gsi_form.is_none());
        assert_eq!(screen.gsis.len(), 3);
        assert_eq!(screen.order[screen.focus], Focus::Gsi(2));
        assert_eq!(screen.gsis[2].key_desc, "pk orgId(S)");

        screen.unload_table();
        screen.load_table(&description());
        let resolved = screen.to_scan_config().unwrap();
        let added = resolved.gsi.iter().find(|g| g.name == "byOrg").unwrap();
        assert!(added.hypothetical);
    }

    #[test]
    fn invalid_gsi_form_stays_open_and_adds_nothing() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        submit_form(&mut screen, "GSI1", "orgId");

        assert!(screen.gsi_form.is_some());
        assert_eq!(screen.gsis.len(), 2);
        assert!(buffer_text(&screen).contains("already exists"));
    }

    #[test]
    fn remove_focused_gsi_drops_only_hypothetical_rows() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        focus_on(&mut screen, Focus::Gsi(0));
        screen.remove_focused_gsi();
        assert_eq!(screen.gsis.len(), 2, "discovered index kept");

        focus_on(&mut screen, Focus::Gsi(1));
        screen.remove_focused_gsi();
        assert_eq!(screen.gsis.len(), 1);
        assert_eq!(screen.order[screen.focus], Focus::AddGsi);

        screen.unload_table();
        assert!(screen.gsis.is_empty(), "removal reaches the intents");
    }

    #[test]
    fn add_gsi_button_shows_without_any_index() {
        let mut cfg = config();
        cfg.gsi.clear();
        let screen = SetupScreen::new(&cfg, None, Vec::new());

        assert!(screen.order.contains(&Focus::AddGsi));
        assert!(buffer_text(&screen).contains("+ Add hypothetical GSI"));
    }

    #[test]
    fn unload_table_clears_discovered_rows_and_set_tables_refilters() {
        let mut screen = SetupScreen::new(&config(), Some(&description()), Vec::new());
        screen.unload_table();
        assert_eq!(screen.loaded_table(), None);
        assert!(screen.lsis.is_empty());
        assert!(screen.ttl.is_none());
        assert_eq!(screen.gsis.len(), 1, "hypothetical GSI survives");

        screen.set_tables(tables());
        assert!(screen.is_table_list_active());
    }
}
