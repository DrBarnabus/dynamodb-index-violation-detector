//! Screen state machine and key dispatch (PRD §6.3.1, §6.3.6 / §8.6).
//!
//! [`App`] owns the current screen and maps key events onto it, returning a
//! [`Command`] for shell-level actions (start/cancel a scan, save config, quit)
//! while handling navigation, toggles, and editing internally. The flow is
//! linear — Setup → In-flight → Completed — with no back-navigation from the
//! completed screen (PRD §6.3.1); the shell (task #25) drives the forward
//! transitions as the scan starts and finishes.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::{CompletedScreen, InFlightScreen, SetupScreen, centered};
use crate::state::StateSnapshot;

/// A shell-level action requested by the user (PRD §8.6). Navigation, toggles,
/// and text editing are handled inside [`App`] and never surface as commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    StartScan,
    CancelScan,
    SaveConfig,
    Quit,
}

enum Screen {
    Setup(Box<SetupScreen>),
    InFlight(InFlightScreen),
    Completed(CompletedScreen),
}

/// The running TUI: current screen plus the help-overlay flag.
pub struct App {
    screen: Screen,
    show_help: bool,
}

impl App {
    /// Start on the setup screen (PRD §6.3.1); the launch/profile picker is
    /// skipped when the scan is already targeted via config or CLI (§6.3.2).
    pub fn new(setup: SetupScreen) -> Self {
        Self {
            screen: Screen::Setup(Box::new(setup)),
            show_help: false,
        }
    }

    /// Transition Setup → In-flight when a scan starts.
    pub fn begin_scan(&mut self) {
        self.screen = Screen::InFlight(InFlightScreen::new());
    }

    /// Transition to the completed screen for a finished (or cancelled) scan,
    /// browsing `violation_count` retained violations.
    pub fn complete(&mut self, violation_count: usize) {
        self.screen = Screen::Completed(CompletedScreen::new(violation_count));
    }

    /// The setup screen, if that is the current screen — the shell reads it to
    /// build a [`crate::config::ScanConfig`] on [`Command::StartScan`].
    pub fn setup(&self) -> Option<&SetupScreen> {
        match &self.screen {
            Screen::Setup(setup) => Some(setup),
            _ => None,
        }
    }

    /// Map a key event to an optional command, applying in-screen edits as a
    /// side effect. Key-release events are ignored (they fire on Windows).
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Command> {
        if key.kind == KeyEventKind::Release {
            return None;
        }

        if self.show_help {
            self.show_help = false;
            return None;
        }

        if key.code == KeyCode::Char('?') {
            self.show_help = true;
            return None;
        }

        match &mut self.screen {
            Screen::Setup(setup) => handle_setup(setup, key),
            Screen::InFlight(inflight) => handle_inflight(inflight, key),
            Screen::Completed(completed) => handle_completed(completed, key),
        }
    }

    /// Draw the current screen and, when raised, the help overlay. `snapshot`
    /// and `export_paths` are consumed only by the in-flight and completed
    /// screens; the setup screen ignores them.
    pub fn render(
        &self,
        frame: &mut Frame,
        snapshot: Option<&StateSnapshot>,
        export_paths: &[PathBuf],
    ) {
        let area = frame.area();
        match &self.screen {
            Screen::Setup(setup) => setup.render(frame, area),
            Screen::InFlight(inflight) => {
                if let Some(snapshot) = snapshot {
                    inflight.render(snapshot, frame, area);
                }
            }
            Screen::Completed(completed) => {
                if let Some(snapshot) = snapshot {
                    completed.render(snapshot, export_paths, frame, area);
                }
            }
        }

        if self.show_help {
            render_help(frame, area);
        }
    }
}

fn handle_setup(setup: &mut SetupScreen, key: KeyEvent) -> Option<Command> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => return Some(Command::Quit),
        KeyCode::Char('s') if ctrl => return Some(Command::SaveConfig),
        KeyCode::Tab | KeyCode::Down => setup.focus_next(),
        KeyCode::BackTab | KeyCode::Up => setup.focus_prev(),
        KeyCode::Enter => {
            if setup.is_start_focused() {
                return Some(Command::StartScan);
            }

            setup.focus_next();
        }
        KeyCode::Backspace => setup.backspace(),
        KeyCode::Char(' ') if !setup.focus_is_text() => setup.toggle(),
        KeyCode::Char(c) => setup.input_char(c),
        _ => {}
    }

    None
}

fn handle_inflight(inflight: &mut InFlightScreen, key: KeyEvent) -> Option<Command> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if inflight.is_confirming_cancel() {
        match key.code {
            KeyCode::Char('y') => return Some(Command::CancelScan),
            KeyCode::Char('n') | KeyCode::Esc => inflight.dismiss_cancel(),
            _ => {}
        }

        return None;
    }

    match key.code {
        KeyCode::Char('c') if ctrl => inflight.request_cancel(),
        KeyCode::Char('q') | KeyCode::Esc => inflight.request_cancel(),
        _ => {}
    }

    None
}

fn handle_completed(completed: &mut CompletedScreen, key: KeyEvent) -> Option<Command> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Some(Command::Quit),
        KeyCode::Down | KeyCode::Char('j') => completed.select_next(),
        KeyCode::Up | KeyCode::Char('k') => completed.select_prev(),
        _ => {}
    }

    None
}

fn render_help(frame: &mut Frame, area: ratatui::layout::Rect) {
    let modal = centered(area, 46, 11);
    frame.render_widget(Clear, modal);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Keybindings ")
        .border_style(Style::default().fg(Color::Cyan));

    let bind = |keys: &str, desc: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {keys:<12}"), Style::default().fg(Color::Yellow)),
            Span::raw(desc.to_string()),
        ])
    };

    let lines = vec![
        bind("↑/↓ · j/k", "navigate"),
        bind("Tab", "next field"),
        bind("Space", "toggle"),
        bind("Enter", "start / drill in"),
        bind("Ctrl+S", "save config"),
        bind("Ctrl+C", "cancel scan"),
        bind("q / Esc", "quit or back"),
        bind("?", "close this help"),
    ];

    frame.render_widget(Paragraph::new(lines).block(block), modal);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::{TableDescription, TableKeySchema};
    use crate::config::{ExportConfig, ScanConfig};
    use crate::domain::{KeySchemaElement, TypeCode};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn config() -> ScanConfig {
        ScanConfig {
            table: "users".to_string(),
            region: None,
            profile: None,
            segments: 4,
            rate_limit_percent: None,
            export: ExportConfig {
                csv: true,
                csv_path: None,
                ndjson: true,
                ndjson_path: None,
            },
            gsi: Vec::new(),
            lsi: Vec::new(),
            ttl: None,
        }
    }

    fn description() -> TableDescription {
        TableDescription {
            name: "users".to_string(),
            key_schema: TableKeySchema {
                pk: KeySchemaElement {
                    name: "id".to_string(),
                    type_code: TypeCode::S,
                },
                sk: None,
            },
            gsis: Vec::new(),
            lsis: Vec::new(),
            ttl: None,
            provisioned_rcu: None,
            item_count: 10,
        }
    }

    fn app() -> App {
        App::new(SetupScreen::new(&config(), &description()))
    }

    fn snapshot() -> StateSnapshot {
        StateSnapshot {
            items_scanned: 5,
            items_per_sec: 1.0,
            total_violations: 0,
            category_counts: Default::default(),
            per_segment_items: vec![5],
            consumed_rcu: 0.0,
            rcu_per_sec: 0.0,
            elapsed: Duration::from_secs(1),
            eta: None,
            item_count: 10,
            progress: 0.5,
            recent_violations: Vec::new(),
        }
    }

    #[test]
    fn setup_esc_quits_and_ctrl_s_saves() {
        let mut app = app();
        assert_eq!(
            app.handle_key(ctrl(KeyCode::Char('s'))),
            Some(Command::SaveConfig)
        );
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Some(Command::Quit));
    }

    #[test]
    fn setup_enter_on_start_button_starts_scan() {
        let mut app = app();
        // The Start button is the last control; a wrap-around Up lands on it.
        app.handle_key(key(KeyCode::Up));
        assert!(app.setup().unwrap().is_start_focused());
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Command::StartScan)
        );
    }

    #[test]
    fn setup_enter_off_button_advances_focus_without_command() {
        let mut app = app();
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        // Focus moved off the table field, so it is no longer the text target.
        assert!(app.setup().unwrap().focus_is_text());
    }

    #[test]
    fn setup_typing_edits_the_focused_field() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('X')));
        assert_eq!(
            app.setup().unwrap().to_scan_config().unwrap().table,
            "usersX"
        );
    }

    #[test]
    fn release_events_are_ignored() {
        let mut app = app();
        let mut release = key(KeyCode::Esc);
        release.kind = KeyEventKind::Release;
        assert_eq!(app.handle_key(release), None);
    }

    #[test]
    fn help_overlay_opens_and_next_key_closes_it() {
        let mut app = app();
        assert_eq!(app.handle_key(key(KeyCode::Char('?'))), None);
        assert!(app.show_help);

        // The closing key is swallowed, not acted on.
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
        assert!(!app.show_help);
    }

    #[test]
    fn inflight_cancel_requires_confirmation() {
        let mut app = app();
        app.begin_scan();

        assert_eq!(app.handle_key(ctrl(KeyCode::Char('c'))), None);
        let Screen::InFlight(inflight) = &app.screen else {
            panic!("expected in-flight screen");
        };
        assert!(inflight.is_confirming_cancel());

        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            Some(Command::CancelScan)
        );
    }

    #[test]
    fn inflight_dismiss_keeps_scanning() {
        let mut app = app();
        app.begin_scan();
        app.handle_key(key(KeyCode::Char('q')));
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), None);

        let Screen::InFlight(inflight) = &app.screen else {
            panic!("expected in-flight screen");
        };
        assert!(!inflight.is_confirming_cancel());
    }

    #[test]
    fn completed_navigates_and_quits() {
        let mut app = app();
        app.complete(3);

        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Down));
        let Screen::Completed(completed) = &app.screen else {
            panic!("expected completed screen");
        };
        assert_eq!(completed.selected(), 2);

        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Some(Command::Quit));
    }

    #[test]
    fn render_dispatches_to_the_current_screen() {
        let mut app = app();
        assert!(render_text(&app, None).contains("Scan setup"));

        app.begin_scan();
        assert!(render_text(&app, Some(&snapshot())).contains("Scan in progress"));

        app.complete(0);
        assert!(render_text(&app, Some(&snapshot())).contains("Scan complete"));
    }

    #[test]
    fn render_shows_help_overlay_over_any_screen() {
        let mut app = app();
        app.handle_key(key(KeyCode::Char('?')));
        assert!(render_text(&app, None).contains("Keybindings"));
    }

    fn render_text(app: &App, snapshot: Option<&StateSnapshot>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal
            .draw(|frame| app.render(frame, snapshot, &[]))
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
}
