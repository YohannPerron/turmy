use crossbeam_channel::{Receiver, TryRecvError, select, unbounded};
use itertools::Either;
use std::{cmp::min, iter::once, path::PathBuf, process::Command, time::Duration};

use crate::file_watcher::{FileWatcherError, FileWatcherHandle};
use crate::job_watcher::JobWatcherHandle;
use crate::viewport::{Pane, PaneViewport, clip_line, display_width, wrap_line};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
};
use std::io;
use tui_input::{Input, backend::crossterm::EventHandler};

pub enum Dialog {
    ConfirmCancelJob(String),
    SelectCancelSignal { id: String, selected_signal: usize },
    EditTimeLimit { id: String, input: Input },
    CommandError { command: String, output: String },
}

struct CommandFailure {
    command: String,
    output: String,
}

#[derive(Clone, Copy)]
pub enum ScrollAnchor {
    Top,
    Bottom,
}

#[derive(Default)]
pub enum OutputFileView {
    #[default]
    Stdout,
    Stderr,
}

pub struct App {
    focus: Pane,
    dialog: Option<Dialog>,
    jobs: Vec<Job>,
    job_list_state: ListState,
    job_output: Result<String, FileWatcherError>,
    job_output_anchor: ScrollAnchor,
    job_output_offset: u16,
    job_output_wrap: bool,
    _job_watcher: JobWatcherHandle,
    job_output_watcher: FileWatcherHandle,
    // sender: Sender<AppMessage>,
    receiver: Receiver<AppMessage>,
    input_receiver: Receiver<std::io::Result<Event>>,
    output_file_view: OutputFileView,
    job_list_height: u16,
    job_output_height: u16,
    jobs_viewport: PaneViewport,
    details_viewport: PaneViewport,
    output_viewport: PaneViewport,
    job_output_content_width: usize,
    pending_input_event: Option<Event>,
}

pub struct Job {
    pub job_id: String,
    pub array_id: String,
    pub array_step: Option<String>,
    pub name: String,
    pub state: String,
    pub state_compact: String,
    pub reason: Option<String>,
    pub user: String,
    pub time: String,
    pub time_limit: String,
    pub start_time: String,
    pub tres: String,
    pub partition: String,
    pub nodelist: String,
    pub stdout: Option<PathBuf>,
    pub stderr: Option<PathBuf>,
    pub command: String,
}

impl Job {
    fn id(&self) -> String {
        match self.array_step.as_ref() {
            Some(array_step) => format!("{}_{}", self.array_id, array_step),
            None => self.job_id.clone(),
        }
    }
}

pub enum AppMessage {
    Jobs(Vec<Job>),
    JobOutput(Result<String, FileWatcherError>),
    Key(KeyEvent),
    MouseFocus(Pane),
    MouseClick(usize),
    MouseWheel {
        target: Pane,
        direction: MouseWheelDirection,
        amount: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MouseWheelDirection {
    Up,
    Down,
    Left,
    Right,
}

const SCANCEL_SIGNALS: &[&str] = &["TERM", "INT", "HUP", "USR1", "USR2", "STOP", "CONT", "KILL"];
const DIALOG_WIDTH: u16 = 80;

impl App {
    pub fn new(
        input_receiver: Receiver<std::io::Result<Event>>,
        slurm_refresh_rate: u64,
        file_refresh_rate: u64,
        squeue_args: Vec<String>,
    ) -> App {
        let (sender, receiver) = unbounded();
        Self {
            focus: Pane::Jobs,
            dialog: None,
            jobs: Vec::new(),
            _job_watcher: JobWatcherHandle::new(
                sender.clone(),
                Duration::from_secs(slurm_refresh_rate),
                squeue_args,
            ),
            job_list_state: ListState::default(),
            job_output: Ok("".to_string()),
            job_output_anchor: ScrollAnchor::Bottom,
            job_output_offset: 0,
            job_output_wrap: false,
            job_output_watcher: FileWatcherHandle::new(
                sender.clone(),
                Duration::from_secs(file_refresh_rate),
            ),
            // sender,
            receiver,
            input_receiver,
            output_file_view: OutputFileView::default(),
            job_list_height: 0,
            job_output_height: 0,
            jobs_viewport: PaneViewport::default(),
            details_viewport: PaneViewport::default(),
            output_viewport: PaneViewport::default(),
            job_output_content_width: 0,
            pending_input_event: None,
        }
    }
}

impl App {
    pub fn run<B: Backend<Error = io::Error>>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> io::Result<()> {
        terminal.draw(|f| self.ui(f))?;

        loop {
            let (should_quit, should_draw) = if let Some(event) = self.pending_input_event.take() {
                self.handle_input_event(event)
            } else {
                select! {
                    recv(self.receiver) -> event => {
                        self.handle(event.unwrap());
                        (false, true)
                    }
                    recv(self.input_receiver) -> input_res => {
                        self.handle_input_event(input_res.unwrap().unwrap())
                    }
                }
            };
            if should_quit {
                return Ok(());
            }

            if should_draw {
                terminal.draw(|f| self.ui(f))?;
            }
        }
    }

    fn try_recv_input_event(&mut self) -> Option<Event> {
        if let Some(event) = self.pending_input_event.take() {
            return Some(event);
        }

        loop {
            match self.input_receiver.try_recv() {
                Ok(Ok(event)) => return Some(event),
                Ok(Err(_)) => continue,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
            }
        }
    }

    fn handle_input_event(&mut self, event: Event) -> (bool, bool) {
        match event {
            Event::Key(key) => {
                if key.code == KeyCode::Char('q') {
                    return (true, false);
                }
                self.handle(AppMessage::Key(key));
                (false, true)
            }
            Event::Paste(_) => (false, false),
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if self.dialog.is_some() {
                        return (false, false);
                    }
                    let mut changed = false;
                    if let Some(pane) = self.pane_at(mouse.column, mouse.row) {
                        if self.focus != pane {
                            self.handle(AppMessage::MouseFocus(pane));
                            changed = true;
                        }
                        if pane == Pane::Jobs
                            && let Some(index) = self.job_index_at(mouse.column, mouse.row)
                            && self.job_list_state.selected() != Some(index)
                        {
                            self.handle(AppMessage::MouseClick(index));
                            changed = true;
                        }
                    }
                    (false, changed)
                }
                MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight => {
                    if self.dialog.is_some() {
                        return (false, false);
                    }
                    let Some(target) = self.pane_at(mouse.column, mouse.row) else {
                        return (false, false);
                    };
                    let direction = mouse_wheel_direction(mouse.kind, mouse.modifiers).unwrap();
                    let mut amount = 1u16;
                    while let Some(next_event) = self.try_recv_input_event() {
                        let should_merge = if let Event::Mouse(next_mouse) = &next_event {
                            mouse_wheel_direction(next_mouse.kind, next_mouse.modifiers)
                                == Some(direction)
                                && self.pane_at(next_mouse.column, next_mouse.row) == Some(target)
                        } else {
                            false
                        };
                        if should_merge {
                            amount = amount.saturating_add(1);
                        } else {
                            self.pending_input_event = Some(next_event);
                            break;
                        }
                    }
                    self.handle(AppMessage::MouseWheel {
                        target,
                        direction,
                        amount,
                    });
                    (false, true)
                }
                _ => (false, false),
            },
            Event::Resize(_, _) => (false, true),
            _ => (false, false),
        }
    }

    fn pane_at(&self, column: u16, row: u16) -> Option<Pane> {
        if self.jobs_viewport.contains(column, row) {
            Some(Pane::Jobs)
        } else if self.details_viewport.contains(column, row) {
            Some(Pane::Details)
        } else if self.output_viewport.contains(column, row) {
            Some(Pane::Output)
        } else {
            None
        }
    }

    fn handle(&mut self, msg: AppMessage) {
        match msg {
            AppMessage::Jobs(jobs) => {
                // On refresh: keep the same job selected if it still exists
                let old_index = self.job_list_state.selected();
                let old_id = old_index.and_then(|i| self.jobs.get(i)).map(|j| j.id());

                self.jobs = jobs;

                if self.jobs.is_empty() {
                    self.job_list_state.select(None);
                } else if let Some(id) = old_id {
                    let new_index = self
                        .jobs
                        .iter()
                        .position(|j| j.id() == id)
                        .unwrap_or(old_index.unwrap_or(0).min(self.jobs.len() - 1));
                    self.job_list_state.select(Some(new_index));
                } else {
                    self.job_list_state.select_first();
                }
            }
            AppMessage::JobOutput(content) => {
                self.job_output_content_width = match &content {
                    Ok(output) => output.lines().map(display_width).max().unwrap_or_default(),
                    Err(error) => display_width(&error.to_string()),
                };
                self.job_output = content;
            }
            AppMessage::Key(key) => {
                if self.dialog.is_some() {
                    let mut close_dialog = false;
                    let mut scancel_request = None;
                    let mut timelimit_request = None;
                    let mut command_failure = None;

                    match self.dialog.as_mut().expect("dialog must exist") {
                        Dialog::ConfirmCancelJob(id) => match key.code {
                            KeyCode::Enter | KeyCode::Char('y') => {
                                scancel_request = Some((id.clone(), None));
                                close_dialog = true;
                            }
                            KeyCode::Esc => {
                                close_dialog = true;
                            }
                            _ => {}
                        },
                        Dialog::SelectCancelSignal {
                            id,
                            selected_signal,
                        } => match key.code {
                            KeyCode::Up | KeyCode::Char('k') => {
                                *selected_signal = selected_signal.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                *selected_signal = min(
                                    selected_signal.saturating_add(1),
                                    SCANCEL_SIGNALS.len().saturating_sub(1),
                                );
                            }
                            KeyCode::Enter => {
                                scancel_request =
                                    Some((id.clone(), Some(SCANCEL_SIGNALS[*selected_signal])));
                                close_dialog = true;
                            }
                            KeyCode::Esc => {
                                close_dialog = true;
                            }
                            KeyCode::Char(c) if c.is_ascii_digit() => {
                                if let Some(index) = signal_index_for_digit(c)
                                    && index < SCANCEL_SIGNALS.len()
                                {
                                    *selected_signal = index;
                                }
                            }
                            _ => {}
                        },
                        Dialog::EditTimeLimit { id, input } => match key.code {
                            KeyCode::Enter => {
                                if let Some(time_limit) = validated_time_limit(input) {
                                    timelimit_request = Some((id.clone(), time_limit));
                                    close_dialog = true;
                                }
                            }
                            KeyCode::Esc => {
                                close_dialog = true;
                            }
                            _ => {
                                input.handle_event(&Event::Key(key));
                            }
                        },
                        Dialog::CommandError { .. } => match key.code {
                            KeyCode::Enter | KeyCode::Esc => {
                                close_dialog = true;
                            }
                            _ => {}
                        },
                    };

                    if let Some((id, signal)) = scancel_request {
                        command_failure = execute_scancel(&id, signal).err();
                    }
                    if let Some((id, time_limit)) = timelimit_request {
                        command_failure = execute_scontrol_update_timelimit(&id, &time_limit).err();
                    }
                    if let Some(CommandFailure { command, output }) = command_failure {
                        self.dialog = Some(Dialog::CommandError { command, output });
                    } else if close_dialog {
                        self.dialog = None;
                    }
                } else {
                    match key.code {
                        KeyCode::Tab => self.focus_next_panel(),
                        KeyCode::BackTab => self.focus_previous_panel(),
                        KeyCode::Char('h') | KeyCode::Left => {
                            self.scroll_focused_horizontal_left(horizontal_scroll_amount(key))
                        }
                        KeyCode::Char('l') | KeyCode::Right => {
                            self.scroll_focused_horizontal_right(horizontal_scroll_amount(key))
                        }
                        KeyCode::Char('k') | KeyCode::Up => match self.focus {
                            Pane::Jobs => self.select_previous_job(),
                            Pane::Details => {}
                            Pane::Output => self.scroll_job_output_up_by(1),
                        },
                        KeyCode::Char('j') | KeyCode::Down => match self.focus {
                            Pane::Jobs => self.select_next_job(),
                            Pane::Details => {}
                            Pane::Output => self.scroll_job_output_down_by(1),
                        },
                        KeyCode::Char('g') => match self.focus {
                            Pane::Jobs => self.select_first_job(),
                            Pane::Details => {
                                self.details_viewport.reset_horizontal();
                            }
                            Pane::Output => {
                                self.job_output_offset = 0;
                                self.job_output_anchor = ScrollAnchor::Top;
                            }
                        },
                        KeyCode::Char('G') => match self.focus {
                            Pane::Jobs => self.select_last_job(),
                            Pane::Details => {
                                self.details_viewport.scroll_right_by(usize::MAX);
                            }
                            Pane::Output => {
                                self.job_output_offset = 0;
                                self.job_output_anchor = ScrollAnchor::Bottom;
                            }
                        },
                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            match self.focus {
                                Pane::Jobs => self.scroll_jobs_half_page_up(),
                                Pane::Details => {}
                                Pane::Output => {
                                    self.scroll_job_output_up_by(self.job_output_height / 2)
                                }
                            }
                        }
                        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            match self.focus {
                                Pane::Jobs => self.scroll_jobs_half_page_down(),
                                Pane::Details => {}
                                Pane::Output => {
                                    self.scroll_job_output_down_by(self.job_output_height / 2)
                                }
                            }
                        }
                        KeyCode::PageDown => {
                            let delta = if key.modifiers.intersects(
                                KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT,
                            ) {
                                50
                            } else {
                                match self.focus {
                                    Pane::Jobs => self.job_list_height.saturating_sub(1),
                                    Pane::Details => 0,
                                    Pane::Output => self.job_output_height.saturating_sub(1),
                                }
                            };
                            match self.focus {
                                Pane::Jobs => self.job_list_state.scroll_down_by(delta),
                                Pane::Details => {}
                                Pane::Output => self.scroll_job_output_down_by(delta),
                            }
                        }
                        KeyCode::PageUp => {
                            let delta = if key.modifiers.intersects(
                                KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT,
                            ) {
                                50
                            } else {
                                match self.focus {
                                    Pane::Jobs => self.job_list_height.saturating_sub(1),
                                    Pane::Details => 0,
                                    Pane::Output => self.job_output_height.saturating_sub(1),
                                }
                            };
                            match self.focus {
                                Pane::Jobs => self.job_list_state.scroll_up_by(delta),
                                Pane::Details => {}
                                Pane::Output => self.scroll_job_output_up_by(delta),
                            }
                        }
                        KeyCode::Home => match self.focus {
                            Pane::Jobs => self.select_first_job(),
                            Pane::Details => {
                                self.details_viewport.reset_horizontal();
                            }
                            Pane::Output => {
                                self.job_output_offset = 0;
                                self.job_output_anchor = ScrollAnchor::Top;
                            }
                        },
                        KeyCode::End => match self.focus {
                            Pane::Jobs => self.select_last_job(),
                            Pane::Details => {
                                self.details_viewport.scroll_right_by(usize::MAX);
                            }
                            Pane::Output => {
                                self.job_output_offset = 0;
                                self.job_output_anchor = ScrollAnchor::Bottom;
                            }
                        },
                        KeyCode::Char('c') => {
                            if let Some(id) = self.selected_job_id() {
                                self.dialog = Some(Dialog::ConfirmCancelJob(id));
                            }
                        }
                        KeyCode::Char('C') => {
                            if let Some(id) = self.selected_job_id() {
                                self.dialog = Some(Dialog::SelectCancelSignal {
                                    id,
                                    selected_signal: 0,
                                });
                            }
                        }
                        KeyCode::Char('t') => {
                            if let Some(job) = self.selected_job() {
                                self.dialog = Some(Dialog::EditTimeLimit {
                                    id: job.id(),
                                    input: Input::new(job.time_limit.clone()),
                                });
                            }
                        }
                        KeyCode::Char('o') => {
                            self.output_file_view = match self.output_file_view {
                                OutputFileView::Stdout => OutputFileView::Stderr,
                                OutputFileView::Stderr => OutputFileView::Stdout,
                            };
                            self.output_viewport.reset_horizontal();
                        }
                        KeyCode::Char('w') => {
                            self.job_output_wrap = !self.job_output_wrap;
                            if self.job_output_wrap {
                                self.output_viewport.reset_horizontal();
                            }
                        }
                        _ => {}
                    };
                }
            }
            AppMessage::MouseFocus(pane) => {
                if self.dialog.is_none() {
                    self.focus = pane;
                }
            }
            AppMessage::MouseClick(index) => {
                if self.dialog.is_none() && index < self.jobs.len() {
                    self.job_list_state.select(Some(index));
                }
            }
            AppMessage::MouseWheel {
                target,
                direction,
                amount,
            } => {
                if self.dialog.is_none() {
                    match target {
                        Pane::Jobs => match direction {
                            MouseWheelDirection::Up => self.job_list_state.scroll_up_by(amount),
                            MouseWheelDirection::Down => self.job_list_state.scroll_down_by(amount),
                            MouseWheelDirection::Left => {
                                self.jobs_viewport.scroll_left_by(amount as usize);
                            }
                            MouseWheelDirection::Right => {
                                self.jobs_viewport.scroll_right_by(amount as usize);
                            }
                        },
                        Pane::Details => match direction {
                            MouseWheelDirection::Left => {
                                self.details_viewport.scroll_left_by(amount as usize);
                            }
                            MouseWheelDirection::Right => {
                                self.details_viewport.scroll_right_by(amount as usize);
                            }
                            MouseWheelDirection::Up | MouseWheelDirection::Down => {}
                        },
                        Pane::Output => match direction {
                            MouseWheelDirection::Up => self.scroll_job_output_up_by(amount),
                            MouseWheelDirection::Down => self.scroll_job_output_down_by(amount),
                            MouseWheelDirection::Left => {
                                if !self.job_output_wrap {
                                    self.output_viewport.scroll_left_by(amount as usize);
                                }
                            }
                            MouseWheelDirection::Right => {
                                if !self.job_output_wrap {
                                    self.output_viewport.scroll_right_by(amount as usize);
                                }
                            }
                        },
                    }
                }
            }
        }

        // update
        self.job_output_watcher
            .set_file_path(self.job_list_state.selected().and_then(|i| {
                self.jobs.get(i).and_then(|j| match self.output_file_view {
                    OutputFileView::Stdout => j.stdout.clone(),
                    OutputFileView::Stderr => j.stderr.clone(),
                })
            }));
    }

    fn ui(&mut self, f: &mut Frame) {
        // Layout

        let content_help = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)].as_ref())
            .split(f.area());

        let master_detail = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(50), Constraint::Percentage(70)].as_ref())
            .split(content_help[0]);

        let job_detail_log = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(8), Constraint::Min(3)].as_ref())
            .split(master_detail[1]);

        // Help
        let help_options = vec![
            ("q", "quit"),
            ("tab", "pane"),
            ("⏶/⏷", "vertical"),
            ("◀/▶", "horizontal"),
            ("pgup/pgdown", "page"),
            ("home/end", "top/bottom"),
            ("esc", "cancel"),
            ("enter", "confirm"),
            ("c/C", "cancel/signal"),
            ("t", "set time limit"),
            ("o", "toggle stdout/stderr"),
            ("w", "toggle text wrap"),
        ];
        let blue_style = Style::default().fg(Color::Blue);
        let light_blue_style = Style::default().fg(Color::LightBlue);

        let help = Line::from(help_options.iter().fold(
            Vec::new(),
            |mut acc, (key, description)| {
                if !acc.is_empty() {
                    acc.push(Span::raw(" | "));
                }
                acc.push(Span::styled(*key, blue_style));
                acc.push(Span::raw(": "));
                acc.push(Span::styled(*description, light_blue_style));
                acc
            },
        ));

        let help = Paragraph::new(help);
        f.render_widget(help, content_help[1]);

        // Jobs
        let max_id_len = self.jobs.iter().map(|j| j.id().len()).max().unwrap_or(0);
        let max_user_len = self.jobs.iter().map(|j| j.user.len()).max().unwrap_or(0);
        let max_partition_len = self
            .jobs
            .iter()
            .map(|j| j.partition.len())
            .max()
            .unwrap_or(0);
        let max_time_len = self.jobs.iter().map(|j| j.time.len()).max().unwrap_or(0);
        let max_state_compact_len = self
            .jobs
            .iter()
            .map(|j| j.state_compact.len())
            .max()
            .unwrap_or(0);
        let job_lines: Vec<Line> = self
            .jobs
            .iter()
            .map(|j| {
                Line::from(vec![
                    Span::styled(
                        format!(
                            "{:<max$.max$}",
                            j.state_compact,
                            max = max_state_compact_len
                        ),
                        Style::default(),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:<max$.max$}", j.id(), max = max_id_len),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:<max$.max$}", j.partition, max = max_partition_len),
                        Style::default().fg(Color::Blue),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:<max$.max$}", j.user, max = max_user_len),
                        Style::default().fg(Color::Green),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:>max$.max$}", j.time, max = max_time_len),
                        Style::default().fg(Color::Red),
                    ),
                    Span::raw(" "),
                    Span::raw(&j.name),
                ])
            })
            .collect();
        let jobs_content_width = job_lines.iter().map(Line::width).max().unwrap_or_default();
        self.jobs_viewport
            .update(master_detail[0], jobs_content_width);
        let jobs_viewport = self.jobs_viewport;
        let jobs: Vec<ListItem> = job_lines
            .iter()
            .map(|line| {
                ListItem::new(clip_line(
                    line,
                    jobs_viewport.horizontal_offset(),
                    jobs_viewport.visible_width(),
                ))
            })
            .collect();
        let job_list = List::new(jobs)
            .block(
                Block::default()
                    .title(pane_title(
                        &format!("Jobs ({})", self.jobs.len()),
                        jobs_viewport,
                    ))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(pane_border_style(
                        self.focus,
                        Pane::Jobs,
                        self.dialog.is_some(),
                    )),
            )
            .highlight_style(Style::default().bg(Color::Green).fg(Color::Black));
        f.render_stateful_widget(job_list, master_detail[0], &mut self.job_list_state);
        render_horizontal_scrollbar(
            f,
            jobs_viewport,
            pane_border_style(self.focus, Pane::Jobs, self.dialog.is_some()),
        );
        self.job_list_height = master_detail[0].height.saturating_sub(2); // account for borders

        // Job details

        let job_detail = self
            .job_list_state
            .selected()
            .and_then(|i| self.jobs.get(i));

        let job_detail_lines = job_detail
            .map(|j| {
                let mut state_spans = vec![
                    Span::styled("State  ", Style::default().fg(Color::Yellow)),
                    Span::raw(" "),
                    Span::raw(&j.state),
                ];
                if j.state == "PENDING" {
                    state_spans.extend([
                        Span::styled(" Start ", Style::default().fg(Color::Yellow)),
                        Span::raw(&j.start_time),
                    ]);
                }
                if let Some(s) = j.reason.as_deref() {
                    state_spans.extend([
                        Span::styled(" Reason ", Style::default().fg(Color::Yellow)),
                        Span::raw(s),
                    ]);
                }
                let state = Line::from(state_spans);
                let name = Line::from(vec![
                    Span::styled("Name   ", Style::default().fg(Color::Yellow)),
                    Span::raw(" "),
                    Span::raw(&j.name),
                ]);
                let command = Line::from(vec![
                    Span::styled("Command", Style::default().fg(Color::Yellow)),
                    Span::raw(" "),
                    Span::raw(&j.command),
                ]);
                let nodes = Line::from(vec![
                    Span::styled("Nodes  ", Style::default().fg(Color::Yellow)),
                    Span::raw(" "),
                    Span::raw(&j.nodelist),
                ]);
                let tres = Line::from(vec![
                    Span::styled("TRES   ", Style::default().fg(Color::Yellow)),
                    Span::raw(" "),
                    Span::raw(&j.tres),
                ]);
                let ui_stdout_text = match self.output_file_view {
                    OutputFileView::Stdout => "stdout ",
                    OutputFileView::Stderr => "stderr ",
                };
                let stdout = Line::from(vec![
                    Span::styled(ui_stdout_text, Style::default().fg(Color::Yellow)),
                    Span::raw(" "),
                    Span::raw(
                        match self.output_file_view {
                            OutputFileView::Stdout => &j.stdout,
                            OutputFileView::Stderr => &j.stderr,
                        }
                        .as_ref()
                        .map(|p| p.to_str().unwrap_or_default())
                        .unwrap_or_default(),
                    ),
                ]);

                vec![state, name, command, nodes, tres, stdout]
            })
            .unwrap_or_default();
        let details_content_width = job_detail_lines
            .iter()
            .map(Line::width)
            .max()
            .unwrap_or_default();
        self.details_viewport
            .update(job_detail_log[0], details_content_width);
        let details_viewport = self.details_viewport;
        let visible_job_details = Text::from(
            job_detail_lines
                .iter()
                .map(|line| {
                    clip_line(
                        line,
                        details_viewport.horizontal_offset(),
                        details_viewport.visible_width(),
                    )
                })
                .collect::<Vec<_>>(),
        );
        let job_detail = Paragraph::new(visible_job_details).block(
            Block::default()
                .title(pane_title("Details", details_viewport))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(pane_border_style(
                    self.focus,
                    Pane::Details,
                    self.dialog.is_some(),
                )),
        );
        f.render_widget(job_detail, job_detail_log[0]);
        render_horizontal_scrollbar(
            f,
            details_viewport,
            pane_border_style(self.focus, Pane::Details, self.dialog.is_some()),
        );

        // Log
        let log_area = job_detail_log[1];
        let output_content_width = if self.job_output_wrap {
            log_area.width.saturating_sub(2) as usize
        } else {
            self.job_output_content_width
        };
        self.output_viewport.update(log_area, output_content_width);
        if self.job_output_wrap {
            self.output_viewport.reset_horizontal();
        }
        let output_viewport = self.output_viewport;
        let mut log_title_spans = vec![
            Span::raw("─"),
            Span::raw(match self.output_file_view {
                OutputFileView::Stdout => "stdout",
                OutputFileView::Stderr => "stderr",
            }),
            Span::styled(
                match self.job_output_anchor {
                    ScrollAnchor::Top if self.job_output_offset == 0 => "[T]".to_string(),
                    ScrollAnchor::Top => format!("[T+{}]", self.job_output_offset),
                    ScrollAnchor::Bottom if self.job_output_offset == 0 => "".to_string(),
                    ScrollAnchor::Bottom => format!("[B-{}]", self.job_output_offset),
                },
                Style::default().add_modifier(Modifier::DIM),
            ),
        ];
        if let Some(indicator) = horizontal_indicator(output_viewport) {
            log_title_spans.push(Span::styled(
                indicator,
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        let log_title = Line::from(log_title_spans);
        let log_block = Block::default()
            .title(log_title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(pane_border_style(
                self.focus,
                Pane::Output,
                self.dialog.is_some(),
            ));
        let log_inner = log_block.inner(log_area);
        self.job_output_height = log_inner.height;

        // let job_log = self.job_stdout.as_deref().map(|s| {
        //     string_for_paragraph(
        //         s,
        //         log_block.inner(log_area).height as usize,
        //         log_block.inner(log_area).width as usize,
        //         self.job_stdout_offset as usize,
        //     )
        // }).unwrap_or_else(|e| {
        //     self.job_stdout_offset = 0;
        //     "".to_string()
        // });

        let log = match self.job_output.as_deref() {
            Ok(s) => Paragraph::new(fit_text(
                s,
                log_inner.height as usize,
                log_inner.width as usize,
                self.job_output_anchor,
                self.job_output_offset as usize,
                output_viewport.horizontal_offset(),
                self.job_output_wrap,
            )),
            Err(e) => Paragraph::new(e.to_string())
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: true }),
        }
        .block(log_block);

        f.render_widget(log, log_area);
        render_horizontal_scrollbar(
            f,
            output_viewport,
            pane_border_style(self.focus, Pane::Output, self.dialog.is_some()),
        );

        if let Some(dialog) = &self.dialog {
            match dialog {
                Dialog::ConfirmCancelJob(id) => {
                    let content = Text::from(Line::from(vec![
                        Span::raw("Cancel job "),
                        Span::styled(id, Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw("?"),
                    ]));

                    render_dialog(
                        f,
                        "Cancel",
                        Color::Green,
                        3,
                        content,
                        Some(Wrap { trim: true }),
                    );
                }
                Dialog::SelectCancelSignal {
                    id,
                    selected_signal,
                } => {
                    let mut rows = vec![
                        Line::from(vec![
                            Span::raw("Send signal to job "),
                            Span::styled(id, Style::default().add_modifier(Modifier::BOLD)),
                            Span::raw(":"),
                        ]),
                        Line::default(),
                    ];
                    rows.extend(SCANCEL_SIGNALS.iter().enumerate().map(|(i, signal)| {
                        let signal_style = if i == *selected_signal {
                            Style::default().fg(Color::Black).bg(Color::Green)
                        } else {
                            Style::default()
                        };
                        let shortcut_style = signal_style.add_modifier(Modifier::DIM);
                        Line::from(vec![
                            Span::styled(format!("{}. ", i + 1), shortcut_style),
                            Span::styled(*signal, signal_style),
                        ])
                    }));
                    let content = Text::from(rows);

                    render_dialog(
                        f,
                        "Signal",
                        Color::Green,
                        SCANCEL_SIGNALS.len() as u16 + 4,
                        content,
                        Some(Wrap { trim: true }),
                    );
                }
                Dialog::EditTimeLimit { id, input } => {
                    let area = dialog_area(3, f.area());
                    let inner = Block::default().borders(Borders::ALL).inner(area);

                    let prompt_prefix = "Set time limit for job ";
                    let prompt_suffix = ": ";
                    let prompt_width = (prompt_prefix.chars().count()
                        + id.chars().count()
                        + prompt_suffix.chars().count())
                        as u16;
                    let available_width = inner.width.saturating_sub(prompt_width).max(1) as usize;
                    let scroll = input.visual_scroll(available_width);
                    let visible_value = input
                        .value()
                        .chars()
                        .skip(scroll)
                        .take(available_width)
                        .collect::<String>();
                    let content = Text::from(Line::from(vec![
                        Span::raw(prompt_prefix),
                        Span::styled(id, Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(prompt_suffix),
                        Span::styled(visible_value, Style::default().fg(Color::Blue)),
                    ]));

                    let inner = render_dialog(f, "Time Limit", Color::Green, 3, content, None);

                    let cursor_offset = input.visual_cursor().saturating_sub(scroll) as u16;
                    let cursor_x = inner
                        .x
                        .saturating_add(prompt_width)
                        .saturating_add(cursor_offset)
                        .min(inner.x.saturating_add(inner.width.saturating_sub(1)));
                    let cursor_y = inner.y;
                    f.set_cursor_position((cursor_x, cursor_y));
                }
                Dialog::CommandError { command, output } => {
                    let dialog_text = format!("Command: {command}\n\n{output}");
                    let lines = dialog_text
                        .lines()
                        .count()
                        .saturating_add(2)
                        .min(u16::MAX as usize) as u16;
                    let content = Text::from(dialog_text);

                    render_dialog(
                        f,
                        "Command Error",
                        Color::Red,
                        lines,
                        content,
                        Some(Wrap { trim: false }),
                    );
                }
            }
        }
    }
}

fn pane_border_style(focused: Pane, pane: Pane, dialog_open: bool) -> Style {
    if !dialog_open && focused == pane {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
    }
}

fn horizontal_indicator(viewport: PaneViewport) -> Option<String> {
    (viewport.max_horizontal_offset() > 0).then(|| {
        format!(
            "[X+{}/{}]",
            viewport.horizontal_offset(),
            viewport.max_horizontal_offset()
        )
    })
}

fn pane_title(label: &str, viewport: PaneViewport) -> String {
    format!(
        "─{label}{}",
        horizontal_indicator(viewport).unwrap_or_default()
    )
}

fn render_horizontal_scrollbar(f: &mut Frame, viewport: PaneViewport, border_style: Style) {
    if viewport.max_horizontal_offset() == 0 || viewport.visible_width() == 0 {
        return;
    }

    let scrollbar = Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
        .thumb_symbol("═")
        .thumb_style(border_style)
        .track_symbol(Some("─"))
        .track_style(border_style)
        .begin_symbol(None)
        .end_symbol(None);
    let mut state = ScrollbarState::new(viewport.max_horizontal_offset().saturating_add(1))
        .position(viewport.horizontal_offset())
        .viewport_content_length(viewport.visible_width());
    let area = viewport.area().inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    f.render_stateful_widget(scrollbar, area, &mut state);
}

fn dialog_area(height: u16, viewport: Rect) -> Rect {
    let dialog_width = min(DIALOG_WIDTH, viewport.width);
    let dialog_height = min(height, viewport.height);
    let dialog_x = viewport.x + viewport.width.saturating_sub(dialog_width) / 2;
    let dialog_y = viewport.y + viewport.height.saturating_sub(dialog_height) / 2;

    Rect::new(dialog_x, dialog_y, dialog_width, dialog_height)
}

fn render_dialog(
    f: &mut Frame,
    title: &str,
    color: Color,
    height: u16,
    content: Text,
    wrap: Option<Wrap>,
) -> Rect {
    let block = Block::default()
        .title(format!("─{title}"))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().fg(color));

    let area = dialog_area(height, f.area());
    let inner = block.inner(area);

    let mut paragraph = Paragraph::new(content)
        .style(Style::default().fg(Color::White))
        .block(block);
    if let Some(wrap) = wrap {
        paragraph = paragraph.wrap(wrap);
    }

    f.render_widget(Clear, area);
    f.render_widget(paragraph, area);

    inner
}

fn fit_text(
    s: &'_ str,
    lines: usize,
    cols: usize,
    anchor: ScrollAnchor,
    offset: usize,
    horizontal_offset: usize,
    wrap: bool,
) -> Text<'_> {
    let s = s.rsplit_once(['\r', '\n']).map_or(s, |(p, _)| p); // skip everything after last line delimiter
    let l = s.lines().flat_map(|l| l.split('\r')); // bandaid for term escape codes
    let iter = match anchor {
        ScrollAnchor::Top => Either::Left(l),
        ScrollAnchor::Bottom => Either::Right(l.rev()),
    };
    let iter = iter
        .skip(offset)
        .flat_map(|l| {
            let iter = if wrap {
                Either::Left(
                    wrap_line(l, cols, cols.saturating_sub(2))
                        .into_iter()
                        .enumerate()
                        .map(|(i, l)| {
                            if i == 0 {
                                Line::raw(l)
                            } else {
                                Line::default().spans(vec![
                                    Span::styled(
                                        "↪ ",
                                        Style::default().add_modifier(Modifier::DIM),
                                    ),
                                    Span::raw(l),
                                ])
                            }
                        }),
                )
            } else {
                let line = Line::raw(l);
                Either::Right(once(clip_line(&line, horizontal_offset, cols)))
            };
            match anchor {
                ScrollAnchor::Top => Either::Left(iter),
                ScrollAnchor::Bottom => Either::Right(iter.rev()),
            }
        })
        .take(lines);

    match anchor {
        ScrollAnchor::Top => Text::from(iter.collect::<Vec<_>>()),
        ScrollAnchor::Bottom => Text::from(
            iter.collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>(),
        ),
    }
}

impl App {
    fn selected_job(&self) -> Option<&Job> {
        self.job_list_state
            .selected()
            .and_then(|i| self.jobs.get(i))
    }

    fn selected_job_id(&self) -> Option<String> {
        self.selected_job().map(Job::id)
    }

    fn focus_next_panel(&mut self) {
        self.focus = self.focus.next();
    }

    fn focus_previous_panel(&mut self) {
        self.focus = self.focus.previous();
    }

    fn scroll_focused_horizontal_left(&mut self, amount: usize) {
        match self.focus {
            Pane::Jobs => {
                self.jobs_viewport.scroll_left_by(amount);
            }
            Pane::Details => {
                self.details_viewport.scroll_left_by(amount);
            }
            Pane::Output if !self.job_output_wrap => {
                self.output_viewport.scroll_left_by(amount);
            }
            Pane::Output => {}
        }
    }

    fn scroll_focused_horizontal_right(&mut self, amount: usize) {
        match self.focus {
            Pane::Jobs => {
                self.jobs_viewport.scroll_right_by(amount);
            }
            Pane::Details => {
                self.details_viewport.scroll_right_by(amount);
            }
            Pane::Output if !self.job_output_wrap => {
                self.output_viewport.scroll_right_by(amount);
            }
            Pane::Output => {}
        }
    }

    fn select_next_job(&mut self) {
        self.job_list_state.select_next();
    }

    fn select_previous_job(&mut self) {
        self.job_list_state.select_previous();
    }

    fn select_first_job(&mut self) {
        self.job_list_state.select_first();
    }

    fn select_last_job(&mut self) {
        self.job_list_state.select_last();
    }

    fn scroll_jobs_half_page_down(&mut self) {
        self.job_list_state.scroll_down_by(self.job_list_height / 2);
    }

    fn scroll_jobs_half_page_up(&mut self) {
        self.job_list_state.scroll_up_by(self.job_list_height / 2);
    }

    fn job_index_at(&self, column: u16, row: u16) -> Option<usize> {
        if self.jobs.is_empty() {
            return None;
        }
        let area = self.jobs_viewport.area();
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        if !rect_contains(inner, column, row) {
            return None;
        }

        let row_in_list = (row - inner.y) as usize;
        let index = self.job_list_state.offset().saturating_add(row_in_list);
        (index < self.jobs.len()).then_some(index)
    }

    fn scroll_job_output_down_by(&mut self, delta: u16) {
        match self.job_output_anchor {
            ScrollAnchor::Top => {
                self.job_output_offset = self.job_output_offset.saturating_add(delta)
            }
            ScrollAnchor::Bottom => {
                self.job_output_offset = self.job_output_offset.saturating_sub(delta)
            }
        }
    }

    fn scroll_job_output_up_by(&mut self, delta: u16) {
        match self.job_output_anchor {
            ScrollAnchor::Top => {
                self.job_output_offset = self.job_output_offset.saturating_sub(delta)
            }
            ScrollAnchor::Bottom => {
                self.job_output_offset = self.job_output_offset.saturating_add(delta)
            }
        }
    }
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn mouse_wheel_direction(
    kind: MouseEventKind,
    modifiers: KeyModifiers,
) -> Option<MouseWheelDirection> {
    let horizontal = modifiers.contains(KeyModifiers::SHIFT);
    match kind {
        MouseEventKind::ScrollUp if horizontal => Some(MouseWheelDirection::Left),
        MouseEventKind::ScrollDown if horizontal => Some(MouseWheelDirection::Right),
        MouseEventKind::ScrollUp => Some(MouseWheelDirection::Up),
        MouseEventKind::ScrollDown => Some(MouseWheelDirection::Down),
        MouseEventKind::ScrollLeft => Some(MouseWheelDirection::Left),
        MouseEventKind::ScrollRight => Some(MouseWheelDirection::Right),
        _ => None,
    }
}

fn horizontal_scroll_amount(key: KeyEvent) -> usize {
    if key
        .modifiers
        .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        10
    } else {
        1
    }
}

fn signal_index_for_digit(digit: char) -> Option<usize> {
    let value = digit.to_digit(10)? as usize;
    if value == 0 { None } else { Some(value - 1) }
}

fn validated_time_limit(input: &Input) -> Option<String> {
    let time_limit = input.value().trim();
    if time_limit.is_empty() {
        None
    } else {
        Some(time_limit.to_string())
    }
}

fn execute_scancel(job_id: &str, signal: Option<&str>) -> Result<(), CommandFailure> {
    let mut command = Command::new("scancel");
    let mut command_display = String::from("scancel");

    if let Some(signal) = signal {
        command.arg("--signal").arg(signal);
        command_display.push_str(&format!(" --signal {signal}"));
    }
    command.arg(job_id);
    command_display.push_str(&format!(" {job_id}"));

    execute_command(command, command_display)
}

fn execute_scontrol_update_timelimit(job_id: &str, time_limit: &str) -> Result<(), CommandFailure> {
    let mut command = Command::new("scontrol");
    command
        .arg("update")
        .arg(format!("JobId={job_id}"))
        .arg(format!("TimeLimit={time_limit}"));

    execute_command(
        command,
        format!("scontrol update JobId={job_id} TimeLimit={time_limit}"),
    )
}

fn execute_command(mut command: Command, command_label: String) -> Result<(), CommandFailure> {
    let output = command.output().map_err(|error| CommandFailure {
        command: command_label.clone(),
        output: error.to_string(),
    })?;

    if output.status.success() {
        return Ok(());
    }

    let mut details = vec![match output.status.code() {
        Some(code) => format!("Exit code: {code}"),
        None => "Exit code: N/A".to_string(),
    }];

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim_end();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim_end();
    let has_stdout = !stdout.is_empty();
    let has_stderr = !stderr.is_empty();
    match (has_stdout, has_stderr) {
        (true, true) => {
            details.push(format!("stdout:\n{stdout}"));
            details.push(format!("stderr:\n{stderr}"));
        }
        (true, false) => {
            details.push(stdout.to_string());
        }
        (false, true) => {
            details.push(stderr.to_string());
        }
        (false, false) => {}
    }

    if details.len() == 1 {
        details.push("No output.".to_string());
    }

    Err(CommandFailure {
        command: command_label,
        output: details.join("\n\n"),
    })
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn test_mouse_wheel_direction() {
        assert_eq!(
            mouse_wheel_direction(MouseEventKind::ScrollUp, KeyModifiers::NONE),
            Some(MouseWheelDirection::Up)
        );
        assert_eq!(
            mouse_wheel_direction(MouseEventKind::ScrollDown, KeyModifiers::SHIFT),
            Some(MouseWheelDirection::Right)
        );
        assert_eq!(
            mouse_wheel_direction(MouseEventKind::ScrollLeft, KeyModifiers::NONE),
            Some(MouseWheelDirection::Left)
        );
        assert_eq!(
            mouse_wheel_direction(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE),
            None
        );
    }

    #[test]
    fn test_fit_text_applies_horizontal_offset_without_wrapping() {
        let text = fit_text("abcdef\n", 1, 3, ScrollAnchor::Top, 0, 2, false);

        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "cde");
    }

    #[test]
    fn horizontal_scrollbar_reaches_the_right_edge_at_maximum_offset() {
        let backend = TestBackend::new(12, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut viewport = PaneViewport::default();
        viewport.update(Rect::new(0, 0, 12, 2), 30);
        viewport.scroll_right_by(usize::MAX);

        terminal
            .draw(|frame| render_horizontal_scrollbar(frame, viewport, Style::default()))
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 1)].symbol(), "─");
        assert_eq!(buffer[(10, 1)].symbol(), "═");
    }

    #[test]
    fn test_validated_time_limit() {
        assert_eq!(validated_time_limit(&Input::new("".to_string())), None);
        assert_eq!(validated_time_limit(&Input::new("   ".to_string())), None);
        assert_eq!(
            validated_time_limit(&Input::new(" 01:00:00 ".to_string())),
            Some("01:00:00".to_string())
        );
    }
}
