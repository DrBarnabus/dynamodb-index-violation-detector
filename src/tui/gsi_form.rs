//! Hypothetical GSI add-form, raised as a modal over the setup screen.
//!
//! Collects a name, a partition key attribute and type, and an optional sort
//! key attribute and type, then validates them into a [`GsiEntry`] with the
//! config loader's own [`GsiEntry::validate`].

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::{button_line, centered, focusable_line, hint_line, text_field_line, trimmed_opt};
use crate::config::{ConfigError, GsiEntry};
use crate::domain::{KeySchemaElement, TypeCode};

const FORM_WIDTH: u16 = 60;
const FORM_HEIGHT: u16 = 14;

/// A single focusable form control, in navigation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    PkName,
    PkType,
    SkName,
    SkType,
    Add,
}

const FIELDS: [Field; 6] = [
    Field::Name,
    Field::PkName,
    Field::PkType,
    Field::SkName,
    Field::SkType,
    Field::Add,
];

/// What a keypress asks of the screen hosting the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormAction {
    Cancel,
    Submit,
}

/// The editable state of the add-form, including the last validation failure.
#[derive(Debug, Clone)]
pub(super) struct GsiForm {
    name: String,
    pk_name: String,
    pk_type: TypeCode,
    sk_name: String,
    sk_type: TypeCode,
    focus: usize,
    error: Option<String>,
}

impl GsiForm {
    pub(super) fn new() -> Self {
        Self {
            name: String::new(),
            pk_name: String::new(),
            pk_type: TypeCode::S,
            sk_name: String::new(),
            sk_type: TypeCode::S,
            focus: 0,
            error: None,
        }
    }

    /// Apply a keypress to the form, returning the action it asks of the host.
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> Option<FormAction> {
        match key.code {
            KeyCode::Esc => return Some(FormAction::Cancel),
            KeyCode::Enter if self.focused() == Field::Add => return Some(FormAction::Submit),
            KeyCode::Tab | KeyCode::Down | KeyCode::Enter => self.focus_next(),
            KeyCode::BackTab | KeyCode::Up => self.focus_prev(),
            KeyCode::Char(' ') if self.is_type_focused() => self.cycle_type(true),
            KeyCode::Right => self.cycle_type(true),
            KeyCode::Left => self.cycle_type(false),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Char(c) => self.input_char(c),
            _ => {}
        }

        None
    }

    fn focused(&self) -> Field {
        FIELDS[self.focus]
    }

    fn focus_next(&mut self) {
        self.focus = (self.focus + 1) % FIELDS.len();
    }

    fn focus_prev(&mut self) {
        self.focus = (self.focus + FIELDS.len() - 1) % FIELDS.len();
    }

    fn is_type_focused(&self) -> bool {
        matches!(self.focused(), Field::PkType | Field::SkType)
    }

    /// Step the focused key type through S → N → B, or back when `forward` is
    /// false. No-op off the type selectors.
    fn cycle_type(&mut self, forward: bool) {
        let type_code = match self.focused() {
            Field::PkType => &mut self.pk_type,
            Field::SkType => &mut self.sk_type,
            _ => return,
        };
        *type_code = match (*type_code, forward) {
            (TypeCode::S, true) | (TypeCode::B, false) => TypeCode::N,
            (TypeCode::N, true) | (TypeCode::S, false) => TypeCode::B,
            (TypeCode::B, true) | (TypeCode::N, false) => TypeCode::S,
        };
    }

    fn input_char(&mut self, c: char) {
        if let Some(field) = self.focused_text() {
            field.push(c);
        }
    }

    fn backspace(&mut self) {
        if let Some(field) = self.focused_text() {
            field.pop();
        }
    }

    fn focused_text(&mut self) -> Option<&mut String> {
        match self.focused() {
            Field::Name => Some(&mut self.name),
            Field::PkName => Some(&mut self.pk_name),
            Field::SkName => Some(&mut self.sk_name),
            _ => None,
        }
    }

    /// Validate the form into a hypothetical [`GsiEntry`] whose name differs
    /// from every name in `taken`. On failure the reason is kept for display
    /// and `None` is returned.
    pub(super) fn build<'a>(
        &mut self,
        taken: impl IntoIterator<Item = &'a str>,
    ) -> Option<GsiEntry> {
        let key = |name: &str, type_code| {
            trimmed_opt(name).map(|name| KeySchemaElement { name, type_code })
        };
        let entry = GsiEntry {
            name: self.name.trim().to_string(),
            hypothetical: true,
            pk: key(&self.pk_name, self.pk_type),
            sk: key(&self.sk_name, self.sk_type),
            check_missing: false,
        };

        match entry.validate(taken) {
            Ok(()) => Some(entry),
            Err(err) => {
                self.error = Some(describe(&err));
                None
            }
        }
    }

    pub(super) fn render(&self, frame: &mut Frame, area: Rect) {
        let modal = centered(area, FORM_WIDTH, FORM_HEIGHT);
        frame.render_widget(Clear, modal);

        let focused = self.focused();
        let text = |field: Field, label: &str, value: &str, placeholder: &str| {
            text_field_line(label, value, placeholder, field == focused)
        };
        let selector = |field: Field, type_code: TypeCode| {
            focusable_line(format!("    type: ◂ {type_code:?} ▸"), field == focused)
        };

        let mut lines = vec![
            text(Field::Name, "Index name", &self.name, ""),
            text(Field::PkName, "Partition key", &self.pk_name, ""),
            selector(Field::PkType, self.pk_type),
            text(Field::SkName, "Sort key", &self.sk_name, "(none)"),
            selector(Field::SkType, self.sk_type),
            Line::from(""),
            button_line("Add index", focused == Field::Add),
            Line::from(""),
        ];
        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("  {error}"),
                Style::default().fg(Color::Red),
            )));
        }
        lines.push(hint_line(
            "  space/←/→ change type · enter next / add · esc cancel",
        ));

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Add hypothetical GSI ")
            .border_style(Style::default().fg(Color::Cyan));

        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            modal,
        );
    }
}

/// Form wording for a validation failure; the config loader's own messages
/// speak in TOML terms.
fn describe(err: &ConfigError) -> String {
    match err {
        ConfigError::EmptyIndexName => "Index name is required.".to_string(),
        ConfigError::DuplicateGsi(name) => {
            format!("A GSI named `{name}` already exists; choose a unique name.")
        }
        ConfigError::HypotheticalMissingPk(_) => "Partition key attribute is required.".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn type_into(form: &mut GsiForm, text: &str) {
        for c in text.chars() {
            form.input_char(c);
        }
    }

    fn filled() -> GsiForm {
        let mut form = GsiForm::new();
        type_into(&mut form, "byUser");
        form.focus_next();
        type_into(&mut form, "userId");
        form
    }

    #[test]
    fn builds_a_partition_key_only_hypothetical_entry() {
        let mut form = filled();

        let entry = form.build([]).expect("valid form");
        assert_eq!(
            entry,
            GsiEntry {
                name: "byUser".to_string(),
                hypothetical: true,
                pk: Some(KeySchemaElement {
                    name: "userId".to_string(),
                    type_code: TypeCode::S,
                }),
                sk: None,
                check_missing: false,
            }
        );
    }

    #[test]
    fn builds_a_sort_key_with_its_chosen_type() {
        let mut form = filled();
        form.focus_next();
        form.cycle_type(true);
        form.focus_next();
        type_into(&mut form, " createdAt ");
        form.focus_next();
        form.cycle_type(false);

        let entry = form.build([]).unwrap();
        assert_eq!(entry.pk.unwrap().type_code, TypeCode::N);
        assert_eq!(
            entry.sk,
            Some(KeySchemaElement {
                name: "createdAt".to_string(),
                type_code: TypeCode::B,
            })
        );
    }

    #[test]
    fn cycle_type_wraps_both_ways_and_ignores_text_fields() {
        let mut form = GsiForm::new();
        form.cycle_type(true);
        assert_eq!(form.pk_type, TypeCode::S, "name field focused");

        form.focus_next();
        form.focus_next();
        assert!(form.is_type_focused());
        for expected in [TypeCode::N, TypeCode::B, TypeCode::S] {
            form.cycle_type(true);
            assert_eq!(form.pk_type, expected);
        }
        form.cycle_type(false);
        assert_eq!(form.pk_type, TypeCode::B);
    }

    #[test]
    fn rejects_missing_name_pk_and_duplicate_names() {
        let mut form = GsiForm::new();
        assert!(form.build([]).is_none());
        assert_eq!(form.error.as_deref(), Some("Index name is required."));

        let mut form = GsiForm::new();
        type_into(&mut form, "byUser");
        assert!(form.build([]).is_none());
        assert!(form.error.as_deref().unwrap().contains("Partition key"));

        let mut form = filled();
        assert!(form.build(["GSI1", "byUser"]).is_none());
        assert!(form.error.as_deref().unwrap().contains("`byUser`"));
    }

    #[test]
    fn typing_on_selectors_and_the_add_button_is_ignored() {
        let mut form = GsiForm::new();
        form.focus_prev();
        assert_eq!(form.focused(), Field::Add);
        form.input_char('x');
        form.backspace();
        form.focus_next();
        form.backspace();

        assert_eq!(form.name, "");
        assert_eq!(form.pk_name, "");
    }

    #[test]
    fn handle_key_routes_editing_and_reports_cancel_and_submit() {
        use ratatui::crossterm::event::KeyModifiers;
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);

        let mut form = GsiForm::new();
        assert_eq!(form.handle_key(press(KeyCode::Char('a'))), None);
        assert_eq!(form.handle_key(press(KeyCode::Char(' '))), None);
        assert_eq!(form.name, "a ");

        form.handle_key(press(KeyCode::Enter));
        form.handle_key(press(KeyCode::Enter));
        form.handle_key(press(KeyCode::Char(' ')));
        form.handle_key(press(KeyCode::Right));
        assert_eq!(form.pk_type, TypeCode::B);

        form.handle_key(press(KeyCode::BackTab));
        form.handle_key(press(KeyCode::Up));
        form.handle_key(press(KeyCode::Up));
        assert_eq!(
            form.handle_key(press(KeyCode::Enter)),
            Some(FormAction::Submit)
        );
        assert_eq!(
            form.handle_key(press(KeyCode::Esc)),
            Some(FormAction::Cancel)
        );
    }

    #[test]
    fn render_shows_fields_and_the_validation_error() {
        let mut form = filled();
        assert!(form.build(["byUser"]).is_none());

        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| form.render(frame, frame.area()))
            .unwrap();
        let text = crate::tui::buffer_text(terminal.backend().buffer());

        assert!(text.contains("Add hypothetical GSI"));
        assert!(text.contains("userId"));
        assert!(text.contains("(none)"));
        assert!(text.contains("already exists"));
    }
}
