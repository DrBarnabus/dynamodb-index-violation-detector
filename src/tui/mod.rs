//! TUI renderer (PRD §8.6): ratatui screens, keybindings, event dispatch.
//!
//! Each screen owns a self-contained state model and a `render` method. The
//! screens carry the editable/browsable data and expose primitive mutation
//! methods (navigate, toggle, edit); the top-level event loop maps terminal
//! events onto them and drives transitions (task #24).

mod setup;

#[allow(unused_imports)]
pub use setup::SetupScreen;
