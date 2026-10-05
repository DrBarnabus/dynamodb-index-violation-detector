//! Completed scan screen.
//!
//! A static, browsable summary of a finished scan: final per-category counts,
//! the paths of the export files, and the violation feed of the last 1000
//! violations (the aggregator's rolling window), now static. A selected
//! violation opens a detail view of its item as re-fetched by key, and any of
//! its key, attribute or item JSON can be yanked to the clipboard.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::feed::{FeedView, item_key_text, render_feed};
use super::{ALL_CATEGORIES, category_label, hint_line, target_label};
use crate::export::{expected_type_code, item_json};
use crate::inspect::{Inspection, primary_key};
use crate::rules::Violation;
use crate::state::{RecentViolation, StateSnapshot};

/// What `y` copies from the selected violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YankTarget {
    /// The item's primary key, as native DynamoDB JSON.
    PrimaryKey,
    /// The name of the violating attribute.
    Attribute,
    /// The re-fetched item, as native DynamoDB JSON.
    ItemJson,
}

/// The completed scan screen. Owns the finished scan's violations, the browse
/// cursor and the detail view; counts and export paths are read from the final
/// [`StateSnapshot`] passed to [`render`](CompletedScreen::render).
#[derive(Debug)]
pub struct CompletedScreen {
    violations: Vec<RecentViolation>,
    selected: usize,
    detail: Option<Detail>,
    yank_pending: bool,
    notice: Option<String>,
}

/// The selected violation's item as re-fetched, with its JSON scroll offset.
#[derive(Debug)]
struct Detail {
    inspection: Inspection,
    json: String,
    scroll: usize,
}

impl CompletedScreen {
    /// Build the screen over a finished scan's rolling window of violations.
    pub fn new(violations: Vec<RecentViolation>) -> Self {
        Self {
            violations,
            selected: 0,
            detail: None,
            yank_pending: false,
            notice: None,
        }
    }

    /// Move the browse cursor down one, stopping at the last violation, or
    /// scroll the open detail view's item down.
    pub fn select_next(&mut self) {
        if let Some(detail) = &mut self.detail {
            let last = detail.json.lines().count().saturating_sub(1);
            detail.scroll = (detail.scroll + 1).min(last);
            return;
        }

        if self.selected + 1 < self.violations.len() {
            self.selected += 1;
        }
    }

    /// Move the browse cursor up one, stopping at the first violation, or
    /// scroll the open detail view's item up.
    pub fn select_prev(&mut self) {
        match &mut self.detail {
            Some(detail) => detail.scroll = detail.scroll.saturating_sub(1),
            None => self.selected = self.selected.saturating_sub(1),
        }
    }

    /// The index of the highlighted violation.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The highlighted violation, if there are any.
    pub fn selected_violation(&self) -> Option<&RecentViolation> {
        self.violations.get(self.selected)
    }

    /// Open the detail view on the highlighted violation's re-fetched item.
    pub fn show_detail(&mut self, inspection: Inspection) {
        let json = match &inspection {
            Inspection::Gone => String::new(),
            Inspection::Present { item, .. } => pretty_json(&item_json(item)),
        };
        self.detail = Some(Detail {
            inspection,
            json,
            scroll: 0,
        });
    }

    pub fn is_detail_open(&self) -> bool {
        self.detail.is_some()
    }

    /// Return from the detail view to the violation list.
    pub fn close_detail(&mut self) {
        self.detail = None;
    }

    /// Prompt for what to copy; the next key picks a [`YankTarget`].
    pub fn begin_yank(&mut self) {
        self.yank_pending = true;
    }

    pub fn is_yank_pending(&self) -> bool {
        self.yank_pending
    }

    /// Dismiss the copy prompt without copying.
    pub fn cancel_yank(&mut self) {
        self.yank_pending = false;
    }

    /// Clear the transient status line.
    pub fn clear_notice(&mut self) {
        self.notice = None;
    }

    /// The text to copy for `target`, closing the copy prompt. When there is
    /// nothing to copy the status line says why and `None` is returned.
    pub fn yank(&mut self, target: YankTarget) -> Option<String> {
        self.yank_pending = false;
        let (text, notice) = match self.yank_text(target) {
            Ok((text, copied)) => (Some(text), format!("Copied {copied} to the clipboard")),
            Err(reason) => (None, reason.to_string()),
        };
        self.notice = Some(notice);
        text
    }

    fn yank_text(&self, target: YankTarget) -> Result<(String, &'static str), &'static str> {
        let recent = self
            .selected_violation()
            .ok_or("No violation is selected to copy from")?;

        match target {
            YankTarget::PrimaryKey => {
                let key = primary_key(&recent.pk, recent.sk.as_ref());
                Ok((item_json(&key).to_string(), "the primary key"))
            }
            YankTarget::Attribute => recent
                .violation
                .attribute
                .clone()
                .map(|attribute| (attribute, "the attribute name"))
                .ok_or("This violation has no attribute to copy"),
            YankTarget::ItemJson => match &self.detail {
                None => Err("Press Enter to fetch the item before copying its JSON"),
                Some(Detail {
                    inspection: Inspection::Gone,
                    ..
                }) => Err("The item has been deleted; there is no JSON to copy"),
                Some(detail) => Ok((detail.json.clone(), "the item JSON")),
            },
        }
    }

    /// Draw the summary panel, the violation list or detail view, and the
    /// status line.
    pub fn render(
        &self,
        snapshot: &StateSnapshot,
        export_paths: &[PathBuf],
        frame: &mut Frame,
        area: Rect,
    ) {
        let [summary_area, body_area, status_area] = Layout::vertical([
            Constraint::Length(16),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);

        self.render_summary(snapshot, export_paths, frame, summary_area);
        match (&self.detail, self.selected_violation()) {
            (Some(detail), Some(recent)) => render_detail(recent, detail, frame, body_area),
            _ => self.render_list(frame, body_area),
        }
        frame.render_widget(Paragraph::new(self.status_line()), status_area);
    }

    fn render_summary(
        &self,
        snapshot: &StateSnapshot,
        export_paths: &[PathBuf],
        frame: &mut Frame,
        area: Rect,
    ) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Scan complete ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines = vec![
            Line::from(format!(
                "Scanned {} items · {} violations total",
                snapshot.items_scanned, snapshot.total_violations
            )),
            Line::from(String::new()),
            section("Violations by category"),
        ];

        for category in ALL_CATEGORIES {
            let count = snapshot
                .category_counts
                .get(&category)
                .copied()
                .unwrap_or(0);
            lines.push(Line::from(format!(
                "  {:<20} {}",
                category_label(category),
                count
            )));
        }

        lines.push(Line::from(String::new()));
        lines.push(section("Export files"));
        if export_paths.is_empty() {
            lines.push(Line::from("  (export disabled)"));
        } else {
            for path in export_paths {
                lines.push(Line::from(format!("  {}", path.display())));
            }
        }

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_list(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Recent violations ({}) ", self.violations.len()));
        render_feed(
            frame,
            area,
            block,
            &self.violations,
            FeedView::Cursor(self.selected),
            "No violations found.",
        );
    }

    fn status_line(&self) -> Line<'static> {
        if self.yank_pending {
            return Line::from(vec![
                Span::styled(" Copy: ", Style::default().fg(Color::Cyan)),
                Span::raw("p primary key · a attribute · j item JSON · Esc cancel"),
            ]);
        }

        if let Some(notice) = &self.notice {
            return Line::from(Span::styled(
                format!(" {notice}"),
                Style::default().fg(Color::Cyan),
            ));
        }

        if self.detail.is_some() {
            hint_line(" ↑/↓ scroll · y copy · Esc back")
        } else {
            hint_line(" ↑/↓ move · Enter inspect · y copy · q quit")
        }
    }
}

fn render_detail(recent: &RecentViolation, detail: &Detail, frame: &mut Frame, area: Rect) {
    let mut lines = vec![
        field("Key", Span::raw(item_key_text(recent))),
        field("Violation", Span::raw(violation_summary(&recent.violation))),
    ];
    if let Some(found) = violation_values(&recent.violation) {
        lines.push(field("Found", Span::raw(found)));
    }
    lines.extend(inspection_lines(&detail.inspection));

    let header_height = lines.len() as u16 + 2;
    let [header_area, item_area] =
        Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(area);

    let header = Block::default()
        .borders(Borders::ALL)
        .title(" Violation detail ");
    frame.render_widget(Paragraph::new(lines).block(header), header_area);

    let item_block = Block::default()
        .borders(Borders::ALL)
        .title(" Current item ");
    let item = match &detail.inspection {
        Inspection::Gone => Paragraph::new("(deleted)"),
        Inspection::Present { .. } => {
            Paragraph::new(detail.json.as_str()).scroll((detail.scroll as u16, 0))
        }
    };
    frame.render_widget(item.block(item_block), item_area);
}

fn field(label: &str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), Style::default().fg(Color::Cyan)),
        value,
    ])
}

fn violation_summary(violation: &Violation) -> String {
    let mut summary = format!(
        "{} · {}",
        target_label(&violation.target),
        category_label(violation.category)
    );
    if let Some(attribute) = &violation.attribute {
        summary.push_str(&format!(" · attr `{attribute}`"));
    }

    summary
}

/// The offending value, its type and size as recorded during the scan.
fn violation_values(violation: &Violation) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(actual_type) = &violation.actual_type {
        parts.push(format!("type {actual_type}"));
    }
    if let Some(expected) = expected_type_code(violation) {
        parts.push(format!("expected {expected}"));
    }
    if let Some(value) = &violation.actual_value {
        parts.push(format!("value {value}"));
    }
    if let Some(size) = violation.size_bytes {
        parts.push(format!("{size} bytes"));
    }

    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn inspection_lines(inspection: &Inspection) -> Vec<Line<'static>> {
    let status =
        |text: &str, color: Color| Span::styled(text.to_string(), Style::default().fg(color));
    match inspection {
        Inspection::Gone => vec![field(
            "Now",
            status("Item deleted since the scan", Color::Red),
        )],
        Inspection::Present {
            changed,
            still_violating,
            ..
        } => {
            let item = if *changed {
                status("Item changed since the scan", Color::Yellow)
            } else {
                status("Item unchanged since the scan", Color::Green)
            };
            let violation = if *still_violating {
                status("Violation still present", Color::Red)
            } else {
                status("Violation no longer present", Color::Green)
            };
            vec![field("Now", item), field("", violation)]
        }
    }
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).expect("a JSON value always serialises")
}

fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AttributeValue;
    use crate::rules::{Target, ViolationCategory};
    use crate::tui::{recent_violation, recent_violations};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::HashMap;
    use std::time::Duration;

    fn violation(
        target: Target,
        category: ViolationCategory,
        attribute: Option<&str>,
    ) -> RecentViolation {
        recent_violation("u-1", target, category, attribute)
    }

    fn snapshot(violations: Vec<RecentViolation>) -> StateSnapshot {
        let mut category_counts = HashMap::new();
        for v in &violations {
            *category_counts.entry(v.violation.category).or_insert(0) += 1;
        }

        StateSnapshot {
            items_scanned: 1200,
            items_per_sec: 0.0,
            total_violations: violations.len() as u64,
            category_counts,
            per_segment_items: vec![600, 600],
            consumed_rcu: 0.0,
            rcu_per_sec: 0.0,
            elapsed: Duration::from_secs(30),
            eta: None,
            item_count: 1200,
            progress: 1.0,
            recent_violations: violations,
        }
    }

    fn draw(
        screen: &CompletedScreen,
        snap: &StateSnapshot,
        paths: &[PathBuf],
        width: u16,
        height: u16,
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| screen.render(snap, paths, frame, frame.area()))
            .unwrap();

        crate::tui::buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn summary_shows_counts_and_export_paths() {
        let snap = snapshot(vec![
            violation(
                Target::Gsi("GSI1".to_string()),
                ViolationCategory::TypeMismatch,
                Some("email"),
            ),
            violation(Target::Ttl, ViolationCategory::TtlMalformed, None),
        ]);
        let paths = vec![PathBuf::from("violations-users.csv")];

        let screen = CompletedScreen::new(snap.recent_violations.clone());
        let text = draw(&screen, &snap, &paths, 80, 40);

        assert!(text.contains("Scan complete"));
        assert!(text.contains("2 violations total"));
        assert!(text.contains("Type mismatch"));
        assert!(text.contains("violations-users.csv"));
    }

    #[test]
    fn list_shows_target_category_hierarchy() {
        let snap = snapshot(vec![violation(
            Target::Gsi("GSI1".to_string()),
            ViolationCategory::TypeMismatch,
            Some("email"),
        )]);

        let screen = CompletedScreen::new(snap.recent_violations.clone());
        let text = draw(&screen, &snap, &[], 80, 40);

        assert!(text.contains("Recent violations (1)"));
        assert!(text.contains("id=u-1  GSI GSI1"));
        assert!(text.contains("attr `email`"));
    }

    #[test]
    fn empty_scan_reports_no_violations() {
        let snap = snapshot(Vec::new());
        let screen = CompletedScreen::new(Vec::new());
        let text = draw(&screen, &snap, &[], 80, 40);

        assert!(text.contains("No violations found."));
    }

    #[test]
    fn export_disabled_when_no_paths() {
        let snap = snapshot(Vec::new());
        let screen = CompletedScreen::new(Vec::new());
        let text = draw(&screen, &snap, &[], 80, 40);

        assert!(text.contains("(export disabled)"));
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        let mut screen = CompletedScreen::new(recent_violations(3));
        assert_eq!(screen.selected(), 0);

        screen.select_prev();
        assert_eq!(screen.selected(), 0);

        screen.select_next();
        screen.select_next();
        screen.select_next();
        screen.select_next();
        assert_eq!(screen.selected(), 2);

        screen.select_prev();
        assert_eq!(screen.selected(), 1);
    }

    #[test]
    fn selection_is_inert_with_no_violations() {
        let mut screen = CompletedScreen::new(Vec::new());
        screen.select_next();
        assert_eq!(screen.selected(), 0);
    }

    fn present(changed: bool, still_violating: bool) -> Inspection {
        let item = [
            ("id".to_string(), AttributeValue::S("u-0".to_string())),
            ("email".to_string(), AttributeValue::N("7".to_string())),
        ]
        .into();
        Inspection::Present {
            item,
            changed,
            still_violating,
        }
    }

    fn draw_screen(screen: &CompletedScreen) -> String {
        let snap = snapshot(recent_violations(3));
        draw(screen, &snap, &[], 80, 50)
    }

    #[test]
    fn detail_reports_an_unchanged_item_still_violating_with_its_json() {
        let mut screen = CompletedScreen::new(recent_violations(3));
        screen.show_detail(present(false, true));
        let text = draw_screen(&screen);

        assert!(text.contains("Violation detail"));
        assert!(text.contains("id=u-0"));
        assert!(text.contains("GSI GSI1 · Type mismatch · attr `email`"));
        assert!(text.contains("Item unchanged since the scan"));
        assert!(text.contains("Violation still present"));
        assert!(text.contains(r#""N": "7""#));
        assert!(!text.contains("Recent violations"));
    }

    #[test]
    fn detail_reports_a_changed_item_whose_violation_is_fixed() {
        let mut screen = CompletedScreen::new(recent_violations(3));
        screen.show_detail(present(true, false));
        let text = draw_screen(&screen);

        assert!(text.contains("Item changed since the scan"));
        assert!(text.contains("Violation no longer present"));
    }

    #[test]
    fn detail_reports_a_deleted_item() {
        let mut screen = CompletedScreen::new(recent_violations(3));
        screen.show_detail(Inspection::Gone);
        let text = draw_screen(&screen);

        assert!(text.contains("Item deleted since the scan"));
        assert!(text.contains("(deleted)"));
    }

    #[test]
    fn navigation_scrolls_the_detail_without_moving_the_cursor() {
        let mut screen = CompletedScreen::new(recent_violations(3));
        screen.show_detail(present(false, true));
        for _ in 0..20 {
            screen.select_next();
        }
        assert_eq!(screen.selected(), 0);
        assert!(!draw_screen(&screen).contains(r#""email""#));

        screen.close_detail();
        assert!(!screen.is_detail_open());
        screen.select_next();
        assert_eq!(screen.selected(), 1);
    }

    #[test]
    fn yank_copies_the_primary_key_and_attribute_name() {
        let mut screen = CompletedScreen::new(recent_violations(3));
        screen.begin_yank();
        assert!(draw_screen(&screen).contains("p primary key"));

        assert_eq!(
            screen.yank(YankTarget::PrimaryKey).as_deref(),
            Some(r#"{"id":{"S":"u-0"}}"#)
        );
        assert!(!screen.is_yank_pending());
        assert!(draw_screen(&screen).contains("Copied the primary key to the clipboard"));

        assert_eq!(screen.yank(YankTarget::Attribute).as_deref(), Some("email"));
    }

    #[test]
    fn yank_item_json_needs_a_fetched_present_item() {
        let mut screen = CompletedScreen::new(recent_violations(3));
        assert_eq!(screen.yank(YankTarget::ItemJson), None);
        assert!(draw_screen(&screen).contains("Press Enter to fetch the item"));

        screen.show_detail(Inspection::Gone);
        assert_eq!(screen.yank(YankTarget::ItemJson), None);

        screen.show_detail(present(false, true));
        let json = screen.yank(YankTarget::ItemJson).unwrap();
        assert!(json.contains(r#""email": {"#));
    }

    #[test]
    fn yank_explains_when_there_is_nothing_to_copy() {
        let mut screen = CompletedScreen::new(Vec::new());
        assert_eq!(screen.yank(YankTarget::PrimaryKey), None);

        let mut ttl = CompletedScreen::new(vec![violation(
            Target::Ttl,
            ViolationCategory::TtlMissing,
            None,
        )]);
        assert_eq!(ttl.yank(YankTarget::Attribute), None);
        assert!(draw_screen(&ttl).contains("no attribute to copy"));
    }
}
