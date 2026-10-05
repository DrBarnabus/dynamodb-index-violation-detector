//! Terminal-error modal shown over any screen.

use ratatui::Frame;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::centered;
use crate::assemble::AssembleError;
use crate::aws::AwsError;
use crate::config::ConfigError;
use crate::export::ExportError;
use crate::pipeline::PipelineError;

/// A terminal-error modal: headline, optional SDK code, message and
/// a suggested remediation, dismissed by any keypress.
pub struct ErrorModal {
    title: String,
    code: Option<String>,
    message: String,
    remediation: Option<String>,
}

impl ErrorModal {
    pub fn message(title: &str, message: &str) -> Self {
        Self {
            title: title.to_string(),
            code: None,
            message: message.to_string(),
            remediation: None,
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let area = centered(frame.area(), 60, 12);
        frame.render_widget(Clear, area);

        let mut lines = Vec::new();
        if let Some(code) = &self.code {
            lines.push(Line::from(Span::styled(
                code.clone(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
        }

        lines.push(Line::from(self.message.clone()));
        if let Some(hint) = &self.remediation {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                hint.clone(),
                Style::default().fg(Color::Yellow),
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Press any key to dismiss",
            Style::default().add_modifier(Modifier::DIM),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.title))
            .border_style(Style::default().fg(Color::Red));

        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: true }),
            area,
        );
    }
}

impl From<AwsError> for ErrorModal {
    fn from(err: AwsError) -> Self {
        Self {
            title: "AWS error".to_string(),
            code: Some(err.code.clone()),
            message: err.message.clone(),
            remediation: err.remediation().map(str::to_string),
        }
    }
}

impl From<AssembleError> for ErrorModal {
    fn from(err: AssembleError) -> Self {
        Self::message("Cannot assemble rules", &err.to_string())
    }
}

impl From<PipelineError> for ErrorModal {
    fn from(err: PipelineError) -> Self {
        match err {
            PipelineError::Assemble(err) => err.into(),
            PipelineError::Export(err) => err.into(),
        }
    }
}

impl From<ExportError> for ErrorModal {
    fn from(err: ExportError) -> Self {
        Self::message("Export failure", &err.to_string())
    }
}

impl From<ConfigError> for ErrorModal {
    fn from(err: ConfigError) -> Self {
        Self::message("Cannot save config", &err.to_string())
    }
}
