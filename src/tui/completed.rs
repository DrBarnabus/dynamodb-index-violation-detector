//! Completed scan screen (PRD §6.3.5, MVP-trimmed).
//!
//! A static, browsable summary of a finished scan: final per-category counts,
//! the paths of the export files, and a scrollable list of the last 1000
//! violations (the aggregator's rolling window). Navigation moves a selection
//! cursor over the list; the GetItem drill-in detail view and clipboard yank are
//! deferred for MVP.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use super::{ALL_CATEGORIES, category_label, target_label};
use crate::rules::Violation;
use crate::state::StateSnapshot;

/// The completed scan screen. Owns the browse cursor; all data is read from the
/// final [`StateSnapshot`] and the export paths passed to
/// [`render`](CompletedScreen::render).
#[derive(Debug)]
pub struct CompletedScreen {
    selected: usize,
    violation_count: usize,
}

impl CompletedScreen {
    /// Build the screen for a finished scan whose rolling window holds
    /// `violation_count` violations (`snapshot.recent_violations.len()`).
    pub fn new(violation_count: usize) -> Self {
        Self {
            selected: 0,
            violation_count,
        }
    }

    /// Move the browse cursor down one, stopping at the last violation.
    pub fn select_next(&mut self) {
        if self.selected + 1 < self.violation_count {
            self.selected += 1;
        }
    }

    /// Move the browse cursor up one, stopping at the first violation.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// The index of the highlighted violation, for the deferred drill-in.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Draw the summary panel and the browsable violation list.
    pub fn render(
        &self,
        snapshot: &StateSnapshot,
        export_paths: &[PathBuf],
        frame: &mut Frame,
        area: Rect,
    ) {
        let [summary_area, list_area] =
            Layout::vertical([Constraint::Length(16), Constraint::Min(0)]).areas(area);

        self.render_summary(snapshot, export_paths, frame, summary_area);
        self.render_list(snapshot, frame, list_area);
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

    fn render_list(&self, snapshot: &StateSnapshot, frame: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(format!(
            " Recent violations ({}) ",
            snapshot.recent_violations.len()
        ));

        if snapshot.recent_violations.is_empty() {
            let empty = Paragraph::new("No violations found.")
                .style(Style::default().fg(Color::Green))
                .block(block);
            frame.render_widget(empty, area);
            return;
        }

        let items: Vec<ListItem> = snapshot
            .recent_violations
            .iter()
            .map(|v| ListItem::new(violation_line(v)))
            .collect();

        let list = List::new(items)
            .block(block)
            .highlight_symbol("▶ ")
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        let mut state = ListState::default();
        state.select(Some(
            self.selected.min(snapshot.recent_violations.len() - 1),
        ));
        frame.render_stateful_widget(list, area, &mut state);
    }
}

fn violation_line(v: &Violation) -> Line<'static> {
    let mut spans = vec![
        Span::styled(target_label(&v.target), Style::default().fg(Color::Yellow)),
        Span::raw(" · "),
        Span::raw(category_label(v.category)),
    ];

    if let Some(attribute) = &v.attribute {
        spans.push(Span::raw(format!("  attr `{attribute}`")));
    }

    Line::from(spans)
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
    use crate::rules::{Target, ViolationCategory};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::HashMap;
    use std::time::Duration;

    fn violation(
        target: Target,
        category: ViolationCategory,
        attribute: Option<&str>,
    ) -> Violation {
        Violation {
            target,
            category,
            attribute: attribute.map(str::to_string),
            actual_value: None,
            actual_type: None,
            expected_type: None,
            size_bytes: None,
        }
    }

    fn snapshot(violations: Vec<Violation>) -> StateSnapshot {
        let mut category_counts = HashMap::new();
        for v in &violations {
            *category_counts.entry(v.category).or_insert(0) += 1;
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

        let screen = CompletedScreen::new(snap.recent_violations.len());
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

        let screen = CompletedScreen::new(snap.recent_violations.len());
        let text = draw(&screen, &snap, &[], 80, 40);

        assert!(text.contains("Recent violations (1)"));
        assert!(text.contains("GSI GSI1"));
        assert!(text.contains("attr `email`"));
    }

    #[test]
    fn empty_scan_reports_no_violations() {
        let snap = snapshot(Vec::new());
        let screen = CompletedScreen::new(0);
        let text = draw(&screen, &snap, &[], 80, 40);

        assert!(text.contains("No violations found."));
    }

    #[test]
    fn export_disabled_when_no_paths() {
        let snap = snapshot(Vec::new());
        let screen = CompletedScreen::new(0);
        let text = draw(&screen, &snap, &[], 80, 40);

        assert!(text.contains("(export disabled)"));
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        let mut screen = CompletedScreen::new(3);
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
        let mut screen = CompletedScreen::new(0);
        screen.select_next();
        assert_eq!(screen.selected(), 0);
    }
}
