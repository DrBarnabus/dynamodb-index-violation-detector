//! TUI renderer: ratatui screens, keybindings, event dispatch.
//!
//! Each screen owns a self-contained state model and a `render` method. The
//! screens carry the editable/browsable data and expose primitive mutation
//! methods (navigate, toggle, edit); the top-level event loop maps terminal
//! events onto them and drives transitions.

mod app;
mod completed;
mod error_modal;
mod feed;
mod gsi_form;
mod inflight;
mod picker;
mod profile_picker;
mod setup;

pub use app::{App, Command};
pub use completed::CompletedScreen;
pub use error_modal::ErrorModal;
pub use inflight::InFlightScreen;
pub use profile_picker::ProfilePicker;
pub use setup::SetupScreen;

use std::time::Duration;

use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::rules::{Target, ViolationCategory};

/// A centred `width`×`height` rectangle within `area`, for modal overlays.
pub(super) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [row] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [cell] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(row);
    cell
}

/// The highlight style for the focused control or list row.
fn focus_style(is_focused: bool) -> Style {
    if is_focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    }
}

fn focusable_line(content: String, is_focused: bool) -> Line<'static> {
    Line::from(Span::styled(content, focus_style(is_focused)))
}

fn hint_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(Color::DarkGray),
    ))
}

/// A `label: value` text field, showing `placeholder` while empty and a block
/// cursor while focused.
fn text_field_line(label: &str, value: &str, placeholder: &str, is_focused: bool) -> Line<'static> {
    let shown = if value.is_empty() { placeholder } else { value };
    let cursor = if is_focused { "█" } else { "" };
    focusable_line(format!("  {label}: {shown}{cursor}"), is_focused)
}

fn button_line(label: &str, is_focused: bool) -> Line<'static> {
    focusable_line(format!("  [ {label} ]"), is_focused)
}

pub(super) fn fmt_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// `value` with surrounding whitespace removed, or `None` when nothing is left.
fn trimmed_opt(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The rendered cells of a test terminal as text, one line per row.
#[cfg(test)]
pub(super) fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
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

/// Every violation category in a stable display order, shared by the in-flight
/// and completed screens so their counts panels are deterministic.
pub(super) const ALL_CATEGORIES: [ViolationCategory; 8] = [
    ViolationCategory::TypeMismatch,
    ViolationCategory::SizeExceeded,
    ViolationCategory::MissingKey,
    ViolationCategory::TtlMissing,
    ViolationCategory::TtlWrongType,
    ViolationCategory::TtlMsMagnitude,
    ViolationCategory::TtlMalformed,
    ViolationCategory::TtlPastFiveYears,
];

/// A short, human-readable name for a violation category.
pub(super) fn category_label(category: ViolationCategory) -> &'static str {
    match category {
        ViolationCategory::TypeMismatch => "Type mismatch",
        ViolationCategory::SizeExceeded => "Size exceeded",
        ViolationCategory::MissingKey => "Missing key",
        ViolationCategory::TtlMissing => "TTL missing",
        ViolationCategory::TtlWrongType => "TTL wrong type",
        ViolationCategory::TtlMsMagnitude => "TTL ms magnitude",
        ViolationCategory::TtlMalformed => "TTL malformed",
        ViolationCategory::TtlPastFiveYears => "TTL >5y past",
    }
}

/// The display name of a violation's target bucket.
pub(super) fn target_label(target: &Target) -> String {
    match target {
        Target::Gsi(name) => format!("GSI {name}"),
        Target::Lsi(name) => format!("LSI {name}"),
        Target::Ttl => "TTL".to_string(),
    }
}
