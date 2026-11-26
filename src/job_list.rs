use clap::ValueEnum;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{app::Job, viewport::display_width};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum JobColumn {
    State,
    Id,
    Partition,
    User,
    Time,
    Name,
}

impl JobColumn {
    fn max_width(self) -> usize {
        match self {
            Self::State => 4,
            Self::Id => usize::MAX,
            Self::Partition | Self::User => 12,
            Self::Time => 12,
            Self::Name => 32,
        }
    }

    fn span(self, job: &Job) -> Span<'_> {
        match self {
            Self::State => Span::raw(&job.state_compact),
            Self::Id => Span::styled(job.id(), Style::default().fg(Color::Yellow)),
            Self::Partition => Span::styled(&job.partition, Style::default().fg(Color::Blue)),
            Self::User => Span::styled(&job.user, Style::default().fg(Color::Green)),
            Self::Time => Span::styled(&job.time, Style::default().fg(Color::Red)),
            Self::Name => Span::raw(&job.name),
        }
    }
}

pub fn job_lines<'a>(jobs: &'a [Job], columns: &[JobColumn]) -> Vec<Line<'a>> {
    let widths: Vec<usize> = columns
        .iter()
        .map(|column| {
            jobs.iter()
                .map(|job| display_width(column.span(job).content.trim()))
                .max()
                .unwrap_or(0)
                .min(column.max_width())
        })
        .collect();
    jobs.iter()
        .map(|job| {
            let mut spans = Vec::new();
            for (index, column) in columns.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::raw(" "));
                }
                let mut span = column.span(job);
                let field = span.content.trim();
                let width = display_width(field);
                let cell = if width > widths[index] {
                    let mut remaining = widths[index] - 1;
                    let mut end = 0;
                    for (byte, grapheme) in field.grapheme_indices(true) {
                        let grapheme_width = display_width(grapheme);
                        if grapheme_width > remaining {
                            break;
                        }
                        remaining -= grapheme_width;
                        end = byte + grapheme.len();
                    }
                    format!("{}…", &field[..end])
                } else {
                    field.to_owned()
                };
                let padding = " ".repeat(widths[index] - display_width(&cell));
                span.content = if *column == JobColumn::Time {
                    format!("{padding}{cell}")
                } else if index + 1 < columns.len() {
                    format!("{cell}{padding}")
                } else {
                    cell
                }
                .into();
                spans.push(span);
            }
            let line = Line::from(spans);
            if job.finished {
                line.style(Style::default().add_modifier(Modifier::DIM))
            } else {
                line
            }
        })
        .collect()
}
