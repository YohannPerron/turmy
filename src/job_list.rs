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
            Self::Id | Self::Partition | Self::User | Self::Time => 12,
            Self::Name => usize::MAX,
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

pub fn job_lines<'a>(
    jobs: &'a [Job],
    columns: &[JobColumn],
    available_width: usize,
) -> Vec<Line<'a>> {
    let mut widths: Vec<usize> = columns
        .iter()
        .map(|column| {
            jobs.iter()
                .map(|job| display_width(column.span(job).content.trim()))
                .max()
                .unwrap_or(0)
                .min(column.max_width())
        })
        .collect();
    let mut fixed_width = widths
        .iter()
        .filter(|width| **width > 0)
        .count()
        .saturating_sub(1);
    let mut name_columns = 0;
    for (column, width) in columns.iter().zip(&widths) {
        if *column == JobColumn::Name {
            name_columns += 1;
        } else {
            fixed_width += width;
        }
    }
    if let Some(name_width) = available_width
        .saturating_sub(fixed_width)
        .checked_div(name_columns)
    {
        for (column, width) in columns.iter().zip(&mut widths) {
            if *column == JobColumn::Name {
                // Keep a short name visible via horizontal scrolling in narrow panes.
                *width = (*width).min(name_width.max(6));
            }
        }
    }

    let last_visible = widths.iter().rposition(|width| *width > 0);
    jobs.iter()
        .map(|job| {
            let mut spans = Vec::new();
            for (index, (column, width)) in columns.iter().zip(&widths).enumerate() {
                let width = *width;
                if width == 0 {
                    continue;
                }
                if !spans.is_empty() {
                    spans.push(Span::raw(" "));
                }
                let mut span = column.span(job);
                let field = span.content.trim();
                let cell = if display_width(field) > width {
                    let mut remaining = width - 1;
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
                let padding = " ".repeat(width - display_width(&cell));
                span.content = if *column == JobColumn::Time {
                    format!("{padding}{cell}")
                } else if Some(index) != last_visible {
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
