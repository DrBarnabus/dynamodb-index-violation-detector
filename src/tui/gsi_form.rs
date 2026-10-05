//! Hypothetical GSI add-form, raised as a modal over the setup screen.
//!
//! Collects a name, a partition key attribute and type, and an optional sort
//! key attribute and type, then validates them into a [`GsiEntry`] under the
//! same rules the config loader applies to a TOML `[[gsi]]` entry.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::centered;
use crate::config::GsiEntry;
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

    pub(super) fn focus_next(&mut self) {
        self.focus = (self.focus + 1) % FIELDS.len();
    }

    pub(super) fn focus_prev(&mut self) {
        self.focus = (self.focus + FIELDS.len() - 1) % FIELDS.len();
    }

    pub(super) fn is_add_focused(&self) -> bool {
        FIELDS[self.focus] == Field::Add
    }

    /// True when a key-type selector is focused, so the event loop routes
    /// space and arrows to cycling the type rather than editing text.
    pub(super) fn is_type_focused(&self) -> bool {
        matches!(FIELDS[self.focus], Field::PkType | Field::SkType)
    }

    /// Step the focused key type through S → N → B, or back when `forward` is
    /// false. No-op off the type selectors.
    pub(super) fn cycle_type(&mut self, forward: bool) {
        let type_code = match FIELDS[self.focus] {
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

    pub(super) fn input_char(&mut self, c: char) {
        if let Some(field) = self.focused_text() {
            field.push(c);
        }
    }

    pub(super) fn backspace(&mut self) {
        if let Some(field) = self.focused_text() {
            field.pop();
        }
    }

    fn focused_text(&mut self) -> Option<&mut String> {
        match FIELDS[self.focus] {
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
        match self.validate(taken) {
            Ok(entry) => Some(entry),
            Err(message) => {
                self.error = Some(message);
                None
            }
        }
    }

    fn validate<'a>(&self, taken: impl IntoIterator<Item = &'a str>) -> Result<GsiEntry, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Index name is required.".to_string());
        }

        if taken.into_iter().any(|existing| existing == name) {
            return Err(format!(
                "A GSI named `{name}` already exists; choose a unique name."
            ));
        }

        let pk_name = self.pk_name.trim();
        if pk_name.is_empty() {
            return Err("Partition key attribute is required.".to_string());
        }

        let sk_name = self.sk_name.trim();
        let sk = (!sk_name.is_empty()).then(|| KeySchemaElement {
            name: sk_name.to_string(),
            type_code: self.sk_type,
        });

        Ok(GsiEntry {
            name: name.to_string(),
            hypothetical: true,
            pk: Some(KeySchemaElement {
                name: pk_name.to_string(),
                type_code: self.pk_type,
            }),
            sk,
            check_missing: false,
        })
    }

    pub(super) fn render(&self, frame: &mut Frame, area: Rect) {
        let modal = centered(area, FORM_WIDTH, FORM_HEIGHT);
        frame.render_widget(Clear, modal);

        let focused = FIELDS[self.focus];
        let text = |field: Field, label: &str, value: &str, placeholder: &str| {
            let is_focused = field == focused;
            let shown = if value.is_empty() { placeholder } else { value };
            let cursor = if is_focused { "█" } else { "" };
            focusable(format!("  {label:<14}{shown}{cursor}"), is_focused)
        };
        let selector = |field: Field, type_code: TypeCode| {
            focusable(
                format!("  {:<14}◂ {type_code:?} ▸", "  type"),
                field == focused,
            )
        };

        let mut lines = vec![
            text(Field::Name, "Index name", &self.name, ""),
            text(Field::PkName, "Partition key", &self.pk_name, ""),
            selector(Field::PkType, self.pk_type),
            text(Field::SkName, "Sort key", &self.sk_name, "(none)"),
            selector(Field::SkType, self.sk_type),
            Line::from(""),
            focusable("  [ Add index ]".to_string(), focused == Field::Add),
            Line::from(""),
        ];
        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("  {error}"),
                Style::default().fg(Color::Red),
            )));
        }
        lines.push(Line::from(Span::styled(
            "  space/←/→ change type · enter next / add · esc cancel",
            Style::default().fg(Color::DarkGray),
        )));

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

fn focusable(content: String, is_focused: bool) -> Line<'static> {
    let style = if is_focused {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    Line::from(Span::styled(content, style))
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
        assert!(form.is_add_focused());
        form.input_char('x');
        form.backspace();
        form.focus_next();
        form.backspace();

        assert_eq!(form.name, "");
        assert_eq!(form.pk_name, "");
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
