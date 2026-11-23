use clap::ValueEnum;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::app::Job;

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
                .map(|job| column.span(job).width())
                .max()
                .unwrap_or(0)
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
                let padding = widths[index] - span.width();
                if *column == JobColumn::Time {
                    span.content = format!("{}{}", " ".repeat(padding), span.content).into();
                } else if index + 1 < columns.len() {
                    span.content = format!("{}{}", span.content, " ".repeat(padding)).into();
                }
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
