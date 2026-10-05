//! Launch screen: choose the AWS profile to scan with.
//!
//! Shown only when nothing on the command line or in a config file already
//! targets the scan. Typing filters the discovered profiles; the chosen
//! profile's region pre-fills the setup screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::hint_line;
use super::picker::FuzzyList;
use crate::profiles::Profile;

pub struct ProfilePicker {
    profiles: Vec<Profile>,
    list: FuzzyList,
}

impl ProfilePicker {
    pub fn new(profiles: Vec<Profile>) -> Self {
        let names = profiles.iter().map(|p| p.name.clone()).collect();
        Self {
            profiles,
            list: FuzzyList::new(names, ""),
        }
    }

    pub fn input_char(&mut self, c: char) {
        self.list.push(c);
    }

    pub fn backspace(&mut self) {
        self.list.backspace();
    }

    pub fn select_next(&mut self) {
        self.list.select_next();
    }

    pub fn select_prev(&mut self) {
        self.list.select_prev();
    }

    /// The highlighted profile, or `None` when the filter matches nothing.
    pub fn selected(&self) -> Option<&Profile> {
        self.list.selected_index().map(|i| &self.profiles[i])
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Choose AWS profile ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [filter_area, list_area, hint_area] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .areas(inner);

        let filter = Line::from(vec![
            Span::styled("  Filter: ", Style::default().fg(Color::Cyan)),
            Span::raw(format!("{}█", self.list.query())),
        ]);
        frame.render_widget(Paragraph::new(filter), filter_area);

        let width = self
            .profiles
            .iter()
            .map(|p| p.name.chars().count())
            .max()
            .unwrap_or(0);
        let mut lines = self.list.lines(list_area.height as usize, |i| {
            let profile = &self.profiles[i];
            let region = profile.region.as_deref().unwrap_or("(no region)");
            format!("  {:<width$}  {region}", profile.name)
        });
        if lines.is_empty() {
            lines.push(hint_line("  no profile matches the filter"));
        }
        frame.render_widget(Paragraph::new(lines), list_area);

        let hint = hint_line("type to filter · ↑/↓ move · enter choose · esc quit");
        frame.render_widget(Paragraph::new(hint), hint_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn picker() -> ProfilePicker {
        ProfilePicker::new(vec![
            Profile {
                name: "default".to_string(),
                region: Some("eu-west-1".to_string()),
            },
            Profile {
                name: "prod".to_string(),
                region: None,
            },
        ])
    }

    fn render_text(picker: &ProfilePicker) -> String {
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal
            .draw(|frame| picker.render(frame, frame.area()))
            .unwrap();
        crate::tui::buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn filters_and_selects_profiles() {
        let mut picker = picker();
        assert_eq!(picker.selected().unwrap().name, "default");

        picker.select_next();
        assert_eq!(picker.selected().unwrap().name, "prod");

        picker.input_char('d');
        picker.input_char('e');
        assert_eq!(picker.selected().unwrap().name, "default");

        picker.input_char('x');
        assert!(picker.selected().is_none());
        picker.backspace();
        assert!(picker.selected().is_some());
    }

    #[test]
    fn render_lists_profiles_with_regions() {
        let text = render_text(&picker());
        assert!(text.contains("Choose AWS profile"));
        assert!(text.contains("default  eu-west-1"));
        assert!(text.contains("prod     (no region)"));
    }

    #[test]
    fn render_explains_an_empty_filter_result() {
        let mut picker = picker();
        picker.input_char('z');
        assert!(render_text(&picker).contains("no profile matches"));
    }
}
