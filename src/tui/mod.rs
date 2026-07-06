//! TUI renderer (PRD §8.6): ratatui screens, keybindings, event dispatch.
//!
//! Each screen owns a self-contained state model and a `render` method. The
//! screens carry the editable/browsable data and expose primitive mutation
//! methods (navigate, toggle, edit); the top-level event loop maps terminal
//! events onto them and drives transitions (task #24).

mod completed;
mod inflight;
mod setup;

#[allow(unused_imports)]
pub use completed::CompletedScreen;
#[allow(unused_imports)]
pub use inflight::InFlightScreen;
#[allow(unused_imports)]
pub use setup::SetupScreen;

use crate::rules::{Target, ViolationCategory};

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

/// The display name of a violation's target bucket (PRD §6.1.4 hierarchy).
pub(super) fn target_label(target: &Target) -> String {
    match target {
        Target::Gsi(name) => format!("GSI {name}"),
        Target::Lsi(name) => format!("LSI {name}"),
        Target::Ttl => "TTL".to_string(),
    }
}
