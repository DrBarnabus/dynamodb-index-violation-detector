//! Violation feed: the aggregator's rolling window as a list of item keys and
//! their violations. The in-flight screen tails the newest entries as they
//! stream in; the completed screen browses the same list with a cursor.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};

use super::{category_label, focus_style, target_label};
use crate::domain::KeyAttribute;
use crate::export::render_key;
use crate::state::RecentViolation;

/// How the feed positions itself in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FeedView {
    /// Keep the newest entry in view, without a cursor.
    Tail,
    /// Highlight the entry at this index, scrolling it into view.
    Cursor(usize),
}

/// Draw `violations` (oldest first) inside `block`, or `empty` when there are
/// none.
pub(super) fn render_feed(
    frame: &mut Frame,
    area: Rect,
    block: Block,
    violations: &[RecentViolation],
    view: FeedView,
    empty: &str,
) {
    if violations.is_empty() {
        let empty = Paragraph::new(empty.to_string())
            .style(Style::default().fg(Color::Green))
            .block(block);
        frame.render_widget(empty, area);
        return;
    }

    let shown = match view {
        FeedView::Tail => {
            let rows = block.inner(area).height as usize;
            &violations[violations.len().saturating_sub(rows)..]
        }
        FeedView::Cursor(_) => violations,
    };
    let last = shown.len() - 1;
    let list = List::new(shown.iter().map(|recent| ListItem::new(feed_line(recent)))).block(block);
    let (selected, list) = match view {
        FeedView::Tail => (last, list),
        FeedView::Cursor(index) => (
            index.min(last),
            list.highlight_symbol("▶ ")
                .highlight_style(focus_style(true)),
        ),
    };

    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn feed_line(recent: &RecentViolation) -> Line<'static> {
    let violation = &recent.violation;
    let mut key = key_text(&recent.pk);
    if let Some(sk) = &recent.sk {
        key.push_str(&format!(" {}", key_text(sk)));
    }

    let mut spans = vec![
        Span::styled(key, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(
            target_label(&violation.target),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw(" · "),
        Span::raw(category_label(violation.category)),
    ];
    if let Some(attribute) = &violation.attribute {
        spans.push(Span::raw(format!("  attr `{attribute}`")));
    }

    Line::from(spans)
}

/// `name=value` for a key attribute, rendered as in the CSV export.
fn key_text(key: &KeyAttribute) -> String {
    let (value, _) = render_key(key);
    format!("{}={value}", key.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AttributeValue;
    use crate::rules::{Target, ViolationCategory};
    use crate::tui::recent_violation;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Borders;

    fn recent(pk: &str, sk: Option<AttributeValue>) -> RecentViolation {
        let mut recent = recent_violation(
            pk,
            Target::Gsi("GSI1".to_string()),
            ViolationCategory::TypeMismatch,
            Some("email"),
        );
        recent.sk = sk.map(|value| KeyAttribute {
            name: "ts".to_string(),
            value,
        });
        recent
    }

    fn draw(violations: &[RecentViolation], view: FeedView, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(60, height)).unwrap();
        terminal
            .draw(|frame| {
                let block = Block::default().borders(Borders::ALL).title(" Feed ");
                render_feed(frame, frame.area(), block, violations, view, "Nothing yet.");
            })
            .unwrap();

        crate::tui::buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn lines_show_the_item_key_then_the_violation() {
        let text = draw(
            &[recent("u-1", Some(AttributeValue::N("42".to_string())))],
            FeedView::Tail,
            5,
        );

        assert!(text.contains("id=u-1 ts=42  GSI GSI1 · Type mismatch  attr `email`"));
    }

    #[test]
    fn binary_keys_render_as_base64() {
        let mut entry = recent("u-1", Some(AttributeValue::B(vec![1, 2, 3])));
        entry.pk.value = AttributeValue::B(b"hi".to_vec());

        let text = draw(&[entry], FeedView::Tail, 5);
        assert!(text.contains("id=aGk= ts=AQID"));
    }

    #[test]
    fn tail_keeps_the_newest_in_view_without_a_cursor() {
        let violations: Vec<_> = (0..20)
            .map(|i| recent(&format!("u-{i:02}"), None))
            .collect();
        let text = draw(&violations, FeedView::Tail, 6);

        assert!(text.contains("id=u-19"));
        assert!(!text.contains("id=u-00"));
        assert!(!text.contains("▶"));
    }

    #[test]
    fn cursor_highlights_and_clamps_to_the_last_entry() {
        let violations: Vec<_> = (0..3).map(|i| recent(&format!("u-{i}"), None)).collect();

        assert!(draw(&violations, FeedView::Cursor(0), 6).contains("▶ id=u-0"));
        assert!(draw(&violations, FeedView::Cursor(9), 6).contains("▶ id=u-2"));
    }

    #[test]
    fn empty_feed_shows_the_placeholder() {
        assert!(draw(&[], FeedView::Tail, 5).contains("Nothing yet."));
    }
}
