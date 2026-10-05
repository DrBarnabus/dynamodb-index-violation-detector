//! In-flight scan screen.
//!
//! Renders live scan progress from a [`StateSnapshot`]: a fixed header of
//! aggregate stats over a body that `Tab` swaps between detailed progress
//! (per-category violation counts and per-segment progress bars) and the live
//! violations feed. `Ctrl+C` raises a cancel-confirmation modal the screen
//! owns; the event loop turns a confirmed cancel into a `CancelScan` command.

use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::feed::{FeedView, render_feed};
use super::{ALL_CATEGORIES, category_label, centered, fmt_duration};
use crate::state::StateSnapshot;

/// Width of the inline per-segment progress bars, in cells.
const BAR_WIDTH: usize = 24;

/// Which view fills the body beneath the stats header.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Body {
    #[default]
    Progress,
    Feed,
}

/// The in-flight scan screen. Holds only transient UI state; all progress data
/// is read from the [`StateSnapshot`] passed to [`render`](InFlightScreen::render).
#[derive(Debug, Default)]
pub struct InFlightScreen {
    confirming_cancel: bool,
    body: Body,
}

impl InFlightScreen {
    pub fn new() -> Self {
        Self::default()
    }

    /// Swap the body between detailed progress and the live violations feed.
    pub fn toggle_body(&mut self) {
        self.body = match self.body {
            Body::Progress => Body::Feed,
            Body::Feed => Body::Progress,
        };
    }

    /// Raise the cancel-confirmation modal (`Ctrl+C`).
    pub fn request_cancel(&mut self) {
        self.confirming_cancel = true;
    }

    /// Dismiss the modal, keeping the scan running.
    pub fn dismiss_cancel(&mut self) {
        self.confirming_cancel = false;
    }

    /// True while the confirmation modal is showing, so the event loop can route
    /// `y`/`n` to confirm or dismiss rather than to the body.
    pub fn is_confirming_cancel(&self) -> bool {
        self.confirming_cancel
    }

    /// Draw the header, body, and — when active — the cancel modal.
    pub fn render(&self, snapshot: &StateSnapshot, frame: &mut Frame, area: Rect) {
        let [header_area, body_area] =
            Layout::vertical([Constraint::Length(6), Constraint::Min(0)]).areas(area);

        self.render_header(snapshot, frame, header_area);
        match self.body {
            Body::Progress => self.render_progress(snapshot, frame, body_area),
            Body::Feed => render_live_feed(snapshot, frame, body_area),
        }

        if self.confirming_cancel {
            render_cancel_modal(frame, area);
        }
    }

    fn render_header(&self, snapshot: &StateSnapshot, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Scan in progress ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let stat = |label: &str, value: String| -> Line<'static> {
            Line::from(vec![
                Span::styled(format!("{label:<16}"), Style::default().fg(Color::DarkGray)),
                Span::raw(value),
            ])
        };

        let lines = vec![
            stat(
                "Items scanned",
                format!(
                    "{}  ({} of ~{}, {})",
                    snapshot.items_scanned,
                    snapshot.items_scanned,
                    snapshot.item_count,
                    fmt_progress(snapshot.progress),
                ),
            ),
            stat(
                "Throughput",
                format!(
                    "{:.0} items/s · {:.1} RCU/s",
                    snapshot.items_per_sec, snapshot.rcu_per_sec
                ),
            ),
            stat("Violations", format!("{} total", snapshot.total_violations)),
            stat(
                "Time",
                format!(
                    "{} elapsed · {} remaining · {:.1} RCU used",
                    fmt_duration(snapshot.elapsed),
                    fmt_eta(snapshot.eta),
                    snapshot.consumed_rcu,
                ),
            ),
        ];

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_progress(&self, snapshot: &StateSnapshot, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Progress · Tab: live violations ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines = vec![section("Violations by category")];
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
        lines.push(section(&format!(
            "Segments ({})",
            snapshot.per_segment_items.len()
        )));

        let per_segment_target = segment_target(snapshot);
        for (segment, items) in snapshot.per_segment_items.iter().enumerate() {
            lines.push(Line::from(format!(
                "  #{segment:<3} {}  {items}",
                bar(*items, per_segment_target)
            )));
        }

        frame.render_widget(Paragraph::new(lines), inner);
    }
}

fn render_live_feed(snapshot: &StateSnapshot, frame: &mut Frame, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(format!(
        " Live violations (last {}) · Tab: progress ",
        snapshot.recent_violations.len()
    ));
    render_feed(
        frame,
        area,
        block,
        &snapshot.recent_violations,
        FeedView::Tail,
        "No violations yet.",
    );
}

fn render_cancel_modal(frame: &mut Frame, area: Rect) {
    let modal = centered(area, 48, 5);
    frame.render_widget(Clear, modal);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Cancel scan? ")
        .border_style(Style::default().fg(Color::Yellow));

    let text = Paragraph::new(vec![
        Line::from("Partial export files are kept on disk."),
        Line::from(Span::styled(
            "[y] cancel scan     [n] keep scanning",
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ])
    .block(block);

    frame.render_widget(text, modal);
}

/// Best-effort per-segment target: the approximate item count spread evenly
/// across segments. Zero when the table's item count is unknown.
fn segment_target(snapshot: &StateSnapshot) -> u64 {
    let segments = snapshot.per_segment_items.len() as u64;
    if segments == 0 {
        return 0;
    }

    snapshot.item_count / segments
}

fn bar(value: u64, target: u64) -> String {
    if target == 0 {
        return "─".repeat(BAR_WIDTH);
    }

    let fraction = (value as f64 / target as f64).min(1.0);
    let filled = (fraction * BAR_WIDTH as f64).round() as usize;
    let empty = BAR_WIDTH - filled;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn fmt_progress(progress: f64) -> String {
    format!("~{}%", (progress * 100.0).round() as i64)
}

fn fmt_eta(eta: Option<Duration>) -> String {
    match eta {
        Some(eta) => fmt_duration(eta),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AttributeValue, KeyAttribute};
    use crate::rules::{Target, Violation, ViolationCategory};
    use crate::state::RecentViolation;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::HashMap;

    fn snapshot() -> StateSnapshot {
        let mut category_counts = HashMap::new();
        category_counts.insert(ViolationCategory::TypeMismatch, 7);
        category_counts.insert(ViolationCategory::TtlMalformed, 3);

        StateSnapshot {
            items_scanned: 1500,
            items_per_sec: 250.0,
            total_violations: 10,
            category_counts,
            per_segment_items: vec![500, 500, 500],
            consumed_rcu: 42.5,
            rcu_per_sec: 12.0,
            elapsed: Duration::from_secs(75),
            eta: Some(Duration::from_secs(3725)),
            item_count: 6000,
            progress: 0.25,
            recent_violations: Vec::new(),
        }
    }

    fn draw(screen: &InFlightScreen, snap: &StateSnapshot, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| screen.render(snap, frame, frame.area()))
            .unwrap();

        crate::tui::buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn header_shows_aggregate_stats() {
        let text = draw(&InFlightScreen::new(), &snapshot(), 80, 24);

        assert!(text.contains("Scan in progress"));
        assert!(text.contains("1500"));
        assert!(text.contains("~25%"));
        assert!(text.contains("250 items/s"));
        assert!(text.contains("10 total"));
        assert!(text.contains("1m15s elapsed"));
        assert!(text.contains("1h02m05s remaining"));
    }

    #[test]
    fn body_lists_every_category_and_each_segment() {
        let text = draw(&InFlightScreen::new(), &snapshot(), 80, 24);

        assert!(text.contains("Type mismatch"));
        assert!(text.contains("TTL >5y past"));
        assert!(text.contains("Segments (3)"));
        assert!(text.contains("#0"));
        assert!(text.contains("#2"));
    }

    fn recent(pk: &str) -> RecentViolation {
        RecentViolation {
            pk: KeyAttribute {
                name: "id".to_string(),
                value: AttributeValue::S(pk.to_string()),
            },
            sk: None,
            violation: Violation {
                target: Target::Ttl,
                category: ViolationCategory::TtlMalformed,
                attribute: Some("expiresAt".to_string()),
                actual_value: None,
                actual_type: None,
                expected_type: None,
                size_bytes: None,
            },
        }
    }

    #[test]
    fn tab_swaps_the_body_to_the_live_feed_and_back() {
        let mut snap = snapshot();
        snap.recent_violations = (0..40).map(|i| recent(&format!("u-{i:02}"))).collect();
        let mut screen = InFlightScreen::new();

        screen.toggle_body();
        let text = draw(&screen, &snap, 80, 24);
        assert!(text.contains("Scan in progress"), "header stays fixed");
        assert!(text.contains("Live violations (last 40)"));
        assert!(text.contains("id=u-39  TTL · TTL malformed"));
        assert!(!text.contains("Segments (3)"));

        screen.toggle_body();
        let text = draw(&screen, &snap, 80, 24);
        assert!(text.contains("Segments (3)"));
        assert!(!text.contains("Live violations"));
    }

    #[test]
    fn empty_live_feed_says_so() {
        let mut screen = InFlightScreen::new();
        screen.toggle_body();

        assert!(draw(&screen, &snapshot(), 80, 24).contains("No violations yet."));
    }

    #[test]
    fn unknown_eta_renders_a_dash() {
        let mut snap = snapshot();
        snap.eta = None;
        let text = draw(&InFlightScreen::new(), &snap, 80, 24);

        assert!(text.contains("— remaining"));
    }

    #[test]
    fn cancel_modal_toggles_with_request_and_dismiss() {
        let mut screen = InFlightScreen::new();
        assert!(!screen.is_confirming_cancel());

        screen.request_cancel();
        assert!(screen.is_confirming_cancel());
        let text = draw(&screen, &snapshot(), 80, 24);
        assert!(text.contains("Cancel scan?"));
        assert!(text.contains("[y] cancel scan"));

        screen.dismiss_cancel();
        assert!(!screen.is_confirming_cancel());
        let text = draw(&screen, &snapshot(), 80, 24);
        assert!(!text.contains("Cancel scan?"));
    }

    #[test]
    fn segment_bar_fills_proportionally_to_the_target() {
        assert_eq!(bar(0, 100), format!("[{}]", "░".repeat(BAR_WIDTH)));
        assert_eq!(bar(100, 100), format!("[{}]", "█".repeat(BAR_WIDTH)));
        assert_eq!(bar(200, 100), format!("[{}]", "█".repeat(BAR_WIDTH)));
    }

    #[test]
    fn segment_bar_is_indeterminate_without_an_item_count() {
        assert_eq!(bar(50, 0), "─".repeat(BAR_WIDTH));
    }

    #[test]
    fn duration_formatting_scales_with_magnitude() {
        assert_eq!(fmt_duration(Duration::from_secs(5)), "5s");
        assert_eq!(fmt_duration(Duration::from_secs(75)), "1m15s");
        assert_eq!(fmt_duration(Duration::from_secs(3725)), "1h02m05s");
    }
}
