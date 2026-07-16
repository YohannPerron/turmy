use crossbeam_channel::{Receiver, TryRecvError, select, unbounded};
use std::{
    cmp::min, collections::HashSet, io::Write, path::PathBuf, process::Command, time::Duration,
};

use crate::file_watcher::{FileWatcherError, FileWatcherHandle, FileWatcherUpdate, LogTextDecoder};
use crate::job_watcher::JobWatcherHandle;
use crate::viewport::{Pane, PaneViewport, clip_line, display_width};

use crossterm::{
    clipboard::CopyToClipboard,
    event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind},
    execute,
};
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
use unicode_segmentation::UnicodeSegmentation;

pub enum Dialog {
    Help,
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

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFileView {
    #[default]
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct OutputPoint {
    line: usize,
    byte: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutputCell {
    start: OutputPoint,
    end: OutputPoint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutputSelection {
    start: OutputPoint,
    end: OutputPoint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RenderedOutputLine {
    source_line: usize,
    start_byte: usize,
    end_byte: usize,
    continuation: bool,
}

#[derive(Clone, Copy)]
struct OutputRenderOptions {
    height: usize,
    width: usize,
    anchor: ScrollAnchor,
    offset: usize,
    horizontal_offset: usize,
    wrap: bool,
    selection: Option<OutputSelection>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyKind {
    Selection,
    FullOutput,
}

struct PendingCopy {
    content: String,
    kind: CopyKind,
}

struct CopyFeedback {
    message: String,
    error: bool,
}

pub struct App {
    focus: Pane,
    dialog: Option<Dialog>,
    remembered_jobs: Vec<Job>,
    jobs: Vec<Job>,
    show_finished_jobs: bool,
    job_list_state: ListState,
    job_output: String,
    job_output_error: Option<FileWatcherError>,
    job_output_decoder: LogTextDecoder,
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
    output_inner_area: Rect,
    rendered_output_lines: Vec<RenderedOutputLine>,
    output_selection: Option<OutputSelection>,
    selection_anchor: Option<OutputCell>,
    selection_dragged: bool,
    pending_copy: Option<PendingCopy>,
    copy_feedback: Option<CopyFeedback>,
}

#[derive(Clone, Debug)]
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
    pub finished: bool,
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
    JobOutput(FileWatcherUpdate),
    Key(KeyEvent),
    MouseFocus(Pane),
    MouseClick(usize),
    MouseSelectionStart {
        column: u16,
        row: u16,
    },
    MouseSelectionUpdate {
        column: u16,
        row: u16,
    },
    MouseSelectionEnd {
        column: u16,
        row: u16,
    },
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
const SHIFT_WHEEL_SCROLL_MULTIPLIER: u16 = 3;

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
            remembered_jobs: Vec::new(),
            jobs: Vec::new(),
            show_finished_jobs: false,
            _job_watcher: JobWatcherHandle::new(
                sender.clone(),
                Duration::from_secs(slurm_refresh_rate),
                squeue_args,
            ),
            job_list_state: ListState::default(),
            job_output: String::new(),
            job_output_error: None,
            job_output_decoder: LogTextDecoder::default(),
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
            output_inner_area: Rect::default(),
            rendered_output_lines: Vec::new(),
            output_selection: None,
            selection_anchor: None,
            selection_dragged: false,
            pending_copy: None,
            copy_feedback: None,
        }
    }
}

impl App {
    pub fn run<B: Backend<Error = io::Error> + Write>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> io::Result<()> {
        terminal.draw(|f| self.ui(f))?;

        loop {
            let (should_quit, mut should_draw) =
                if let Some(event) = self.pending_input_event.take() {
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

            if self.pending_copy.is_some() {
                self.flush_pending_copy(terminal.backend_mut());
                should_draw = true;
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
                        if pane == Pane::Output
                            && rect_contains(self.output_inner_area, mouse.column, mouse.row)
                        {
                            self.handle(AppMessage::MouseSelectionStart {
                                column: mouse.column,
                                row: mouse.row,
                            });
                            changed = true;
                        }
                    }
                    (false, changed)
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if self.dialog.is_none() && self.selection_anchor.is_some() {
                        self.handle(AppMessage::MouseSelectionUpdate {
                            column: mouse.column,
                            row: mouse.row,
                        });
                        (false, true)
                    } else {
                        (false, false)
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if self.dialog.is_none() && self.selection_anchor.is_some() {
                        self.handle(AppMessage::MouseSelectionEnd {
                            column: mouse.column,
                            row: mouse.row,
                        });
                        (false, true)
                    } else {
                        (false, false)
                    }
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
                    let scroll_multiplier =
                        mouse_wheel_scroll_multiplier(mouse.kind, mouse.modifiers);
                    let mut amount = 1u16;
                    while let Some(next_event) = self.try_recv_input_event() {
                        let should_merge = if let Event::Mouse(next_mouse) = &next_event {
                            mouse_wheel_direction(next_mouse.kind, next_mouse.modifiers)
                                == Some(direction)
                                && mouse_wheel_scroll_multiplier(
                                    next_mouse.kind,
                                    next_mouse.modifiers,
                                ) == scroll_multiplier
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
                    amount = amount.saturating_mul(scroll_multiplier);
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
                self.remembered_jobs =
                    remember_finished_jobs(std::mem::take(&mut self.remembered_jobs), jobs);
                self.rebuild_visible_jobs();
            }
            AppMessage::JobOutput(update) => {
                match update {
                    FileWatcherUpdate::Reset => {
                        self.job_output.clear();
                        self.job_output_error = None;
                        self.job_output_decoder.reset();
                        self.clear_output_selection();
                    }
                    FileWatcherUpdate::Append(bytes) => {
                        self.job_output_decoder.push(&bytes, &mut self.job_output);
                        self.job_output_error = None;
                    }
                    FileWatcherUpdate::Error(error) => self.job_output_error = Some(error),
                }
                self.job_output_content_width = self
                    .job_output_error
                    .as_ref()
                    .map(|error| display_width(&error.to_string()))
                    .unwrap_or_else(|| {
                        self.job_output
                            .lines()
                            .map(display_width)
                            .max()
                            .unwrap_or_default()
                    });
            }
            AppMessage::Key(key) => {
                if self.dialog.is_some() {
                    let mut close_dialog = false;
                    let mut scancel_request = None;
                    let mut timelimit_request = None;
                    let mut command_failure = None;

                    match self.dialog.as_mut().expect("dialog must exist") {
                        Dialog::Help => match key.code {
                            KeyCode::Char('?') | KeyCode::Enter | KeyCode::Esc => {
                                close_dialog = true;
                            }
                            _ => {}
                        },
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
                        KeyCode::Char('?') => self.dialog = Some(Dialog::Help),
                        KeyCode::Char('c' | 'C')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            self.copy_selected_output()
                        }
                        KeyCode::Char('y') => self.copy_selected_output(),
                        KeyCode::Char('Y') => self.copy_full_output(),
                        KeyCode::Char('f') => {
                            self.show_finished_jobs = !self.show_finished_jobs;
                            self.rebuild_visible_jobs();
                        }
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
                                Pane::Jobs => self.scroll_jobs_down_by(delta),
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
                                Pane::Jobs => self.scroll_jobs_up_by(delta),
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
                        KeyCode::Char('c') if key.modifiers.is_empty() => {
                            if let Some(job) = self.selected_active_job() {
                                self.dialog = Some(Dialog::ConfirmCancelJob(job.id()));
                            }
                        }
                        KeyCode::Char('C') => {
                            if let Some(job) = self.selected_active_job() {
                                self.dialog = Some(Dialog::SelectCancelSignal {
                                    id: job.id(),
                                    selected_signal: 0,
                                });
                            }
                        }
                        KeyCode::Char('t') => {
                            if let Some(job) = self.selected_active_job() {
                                self.dialog = Some(Dialog::EditTimeLimit {
                                    id: job.id(),
                                    input: Input::new(job.time_limit.clone()),
                                });
                            }
                        }
                        KeyCode::Char('o') => {
                            self.clear_output_selection();
                            self.output_file_view = match self.output_file_view {
                                OutputFileView::Stdout => OutputFileView::Stderr,
                                OutputFileView::Stderr => OutputFileView::Stdout,
                            };
                            self.output_viewport.reset_horizontal();
                        }
                        KeyCode::Char('w') => {
                            self.clear_output_selection();
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
                if self.dialog.is_none()
                    && index < self.jobs.len()
                    && self.job_list_state.selected() != Some(index)
                {
                    self.clear_output_selection();
                    self.job_list_state.select(Some(index));
                }
            }
            AppMessage::MouseSelectionStart { column, row } => {
                self.output_selection = None;
                self.copy_feedback = None;
                self.selection_anchor = self.output_cell_at(column, row);
                self.selection_dragged = false;
            }
            AppMessage::MouseSelectionUpdate { column, row } => {
                self.selection_dragged = true;
                self.update_output_selection(column, row);
            }
            AppMessage::MouseSelectionEnd { column, row } => {
                if self.selection_dragged {
                    self.update_output_selection(column, row);
                } else {
                    self.output_selection = None;
                }
                self.selection_anchor = None;
                self.selection_dragged = false;
            }
            AppMessage::MouseWheel {
                target,
                direction,
                amount,
            } => {
                if self.dialog.is_none() {
                    match target {
                        Pane::Jobs => match direction {
                            MouseWheelDirection::Up => self.scroll_jobs_up_by(amount),
                            MouseWheelDirection::Down => self.scroll_jobs_down_by(amount),
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
        let help_options = [
            ("?", "help"),
            ("f", "finished"),
            ("tab", "pane"),
            ("q", "quit"),
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
                let line = Line::from(vec![
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
                ]);
                if j.finished {
                    line.style(Style::default().add_modifier(Modifier::DIM))
                } else {
                    line
                }
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
        let finished_count = self
            .remembered_jobs
            .iter()
            .filter(|job| job.finished)
            .count();
        let jobs_title = match (finished_count, self.show_finished_jobs) {
            (0, _) => format!("Jobs ({})", self.jobs.len()),
            (_, true) => format!("Jobs ({}, {finished_count} finished)", self.jobs.len()),
            (_, false) => format!(
                "Jobs ({}, {finished_count} finished hidden)",
                self.jobs.len()
            ),
        };
        let job_list = List::new(jobs)
            .block(
                Block::default()
                    .title(pane_title(&jobs_title, jobs_viewport))
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
        if let Some(feedback) = &self.copy_feedback {
            log_title_spans.push(Span::styled(
                format!("[{}]", feedback.message),
                Style::default().fg(if feedback.error {
                    Color::Red
                } else {
                    Color::Green
                }),
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
        self.output_inner_area = log_inner;
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

        let (log_text, rendered_output_lines, log_error) = match &self.job_output_error {
            None => {
                let (text, lines) = render_output_text(
                    &self.job_output,
                    OutputRenderOptions {
                        height: log_inner.height as usize,
                        width: log_inner.width as usize,
                        anchor: self.job_output_anchor,
                        offset: self.job_output_offset as usize,
                        horizontal_offset: output_viewport.horizontal_offset(),
                        wrap: self.job_output_wrap,
                        selection: self.output_selection,
                    },
                );
                (text, lines, false)
            }
            Some(error) => (Text::from(error.to_string()), Vec::new(), true),
        };
        self.rendered_output_lines = rendered_output_lines;
        let mut log = Paragraph::new(log_text).block(log_block);
        if log_error {
            log = log
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: true });
        }

        f.render_widget(log, log_area);
        render_horizontal_scrollbar(
            f,
            output_viewport,
            pane_border_style(self.focus, Pane::Output, self.dialog.is_some()),
        );

        if let Some(dialog) = &self.dialog {
            match dialog {
                Dialog::Help => {
                    let content = Text::from(vec![
                        Line::from("Keyboard"),
                        Line::from("  Tab / Shift-Tab       Focus next / previous pane"),
                        Line::from("  Up/Down or j/k         Select or vertically scroll"),
                        Line::from("  Left/Right or h/l      Horizontally scroll focused pane"),
                        Line::from("  PgUp/PgDown, Ctrl-u/d  Scroll by a page / half page"),
                        Line::from("  Home/End or g/G        Move to the beginning / end"),
                        Line::from("  c / C / t              Cancel / signal / set time limit"),
                        Line::from("  o / w                  Stdout/stderr / output wrapping"),
                        Line::from("  f                      Show / hide finished jobs"),
                        Line::from("  y or Ctrl-c / Y        Copy selection / all output"),
                        Line::default(),
                        Line::from("Mouse"),
                        Line::from("  Click                  Focus a pane or select a job"),
                        Line::from("  Drag in output         Select text"),
                        Line::from("  Wheel / Shift-wheel    Vertical / horizontal scroll"),
                        Line::default(),
                        Line::from("Copy uses OSC 52 and requires terminal support."),
                        Line::from("Press ?, Enter, or Esc to close help."),
                    ]);
                    render_dialog(
                        f,
                        "Help",
                        Color::Green,
                        20,
                        content,
                        Some(Wrap { trim: false }),
                    );
                }
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

fn remember_finished_jobs(previous: Vec<Job>, mut current: Vec<Job>) -> Vec<Job> {
    for job in &mut current {
        job.finished = false;
    }
    let current_ids = current.iter().map(Job::id).collect::<HashSet<_>>();

    current.extend(previous.into_iter().filter_map(|mut job| {
        (!current_ids.contains(&job.id())).then(|| {
            if !job.finished {
                job.finished = true;
                job.state = "FINISHED".to_string();
                job.state_compact = "F".to_string();
                job.reason = None;
            }
            job
        })
    }));
    current
}

fn visible_jobs(remembered: &[Job], show_finished: bool) -> Vec<Job> {
    remembered
        .iter()
        .filter(|job| show_finished || !job.finished)
        .cloned()
        .collect()
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

fn render_output_text(
    output: &str,
    options: OutputRenderOptions,
) -> (Text<'static>, Vec<RenderedOutputLine>) {
    let source_lines = output_lines(output);
    let rendered_lines = visible_output_lines(
        &source_lines,
        options.height,
        options.width,
        options.anchor,
        options.offset,
        options.wrap,
    );
    let text = Text::from(
        rendered_lines
            .iter()
            .map(|rendered| {
                render_output_line(
                    source_lines[rendered.source_line],
                    *rendered,
                    options.width,
                    options.horizontal_offset,
                    options.wrap,
                    options.selection,
                )
            })
            .collect::<Vec<_>>(),
    );
    (text, rendered_lines)
}

fn output_lines(output: &str) -> Vec<&str> {
    output.lines().flat_map(|line| line.split('\r')).collect()
}

fn visible_output_lines(
    source_lines: &[&str],
    height: usize,
    width: usize,
    anchor: ScrollAnchor,
    offset: usize,
    wrap: bool,
) -> Vec<RenderedOutputLine> {
    let mut rendered = Vec::new();
    let source_indices: Box<dyn Iterator<Item = usize>> = match anchor {
        ScrollAnchor::Top => Box::new(offset..source_lines.len()),
        ScrollAnchor::Bottom => Box::new((0..source_lines.len()).rev().skip(offset)),
    };

    for source_line in source_indices {
        let mut ranges = if wrap {
            wrap_byte_ranges(source_lines[source_line], width, width.saturating_sub(2))
        } else {
            vec![(0, source_lines[source_line].len())]
        };
        if matches!(anchor, ScrollAnchor::Bottom) {
            ranges.reverse();
        }

        let range_count = ranges.len();
        for (range_index, (start_byte, end_byte)) in ranges.into_iter().enumerate() {
            let continuation = if matches!(anchor, ScrollAnchor::Top) {
                range_index > 0
            } else {
                range_index + 1 < range_count
            };
            rendered.push(RenderedOutputLine {
                source_line,
                start_byte,
                end_byte,
                continuation,
            });
            if rendered.len() >= height {
                break;
            }
        }
        if rendered.len() >= height {
            break;
        }
    }

    if matches!(anchor, ScrollAnchor::Bottom) {
        rendered.reverse();
    }
    rendered
}

fn wrap_byte_ranges(
    text: &str,
    first_width: usize,
    continuation_width: usize,
) -> Vec<(usize, usize)> {
    if text.is_empty() {
        return vec![(0, 0)];
    }
    if first_width == 0 && continuation_width == 0 {
        return vec![(0, text.len())];
    }

    let mut ranges = Vec::new();
    let mut start_byte = 0usize;
    let mut current_width = 0usize;
    let mut available_width = if first_width == 0 {
        continuation_width
    } else {
        first_width
    };

    for (byte, grapheme) in text.grapheme_indices(true) {
        let grapheme_width = display_width(grapheme);
        if available_width > 0
            && byte > start_byte
            && current_width.saturating_add(grapheme_width) > available_width
        {
            ranges.push((start_byte, byte));
            start_byte = byte;
            current_width = 0;
            available_width = continuation_width;
        }

        current_width = current_width.saturating_add(grapheme_width);
        let grapheme_end = byte + grapheme.len();
        if available_width > 0 && current_width >= available_width {
            ranges.push((start_byte, grapheme_end));
            start_byte = grapheme_end;
            current_width = 0;
            available_width = continuation_width;
        }
    }

    if start_byte < text.len() {
        ranges.push((start_byte, text.len()));
    }
    ranges
}

fn render_output_line(
    source: &str,
    rendered: RenderedOutputLine,
    width: usize,
    horizontal_offset: usize,
    wrap: bool,
    selection: Option<OutputSelection>,
) -> Line<'static> {
    let selection_style = Style::default().fg(Color::Black).bg(Color::LightBlue);
    let mut spans = Vec::new();
    if wrap && rendered.continuation {
        spans.push(Span::styled(
            "↪ ",
            Style::default().add_modifier(Modifier::DIM),
        ));
    }

    let source_slice = if wrap {
        &source[rendered.start_byte..rendered.end_byte]
    } else {
        source
    };
    let base_byte = if wrap { rendered.start_byte } else { 0 };
    for (relative_byte, grapheme) in source_slice.grapheme_indices(true) {
        let start = OutputPoint {
            line: rendered.source_line,
            byte: base_byte + relative_byte,
        };
        let end = OutputPoint {
            line: rendered.source_line,
            byte: start.byte + grapheme.len(),
        };
        let selected =
            selection.is_some_and(|selection| start < selection.end && end > selection.start);
        spans.push(Span::styled(
            grapheme.to_string(),
            if selected {
                selection_style
            } else {
                Style::default()
            },
        ));
    }

    let line = Line::from(spans);
    if wrap {
        line
    } else {
        clip_line(&line, horizontal_offset, width)
    }
}

fn selected_output_text(output: &str, selection: OutputSelection) -> Option<String> {
    if selection.start >= selection.end {
        return None;
    }
    let lines = output_lines(output);
    let first = *lines.get(selection.start.line)?;
    let last = *lines.get(selection.end.line)?;
    if selection.start.byte > first.len()
        || selection.end.byte > last.len()
        || !first.is_char_boundary(selection.start.byte)
        || !last.is_char_boundary(selection.end.byte)
    {
        return None;
    }

    let mut selected = String::new();
    for (line_index, line) in lines
        .iter()
        .enumerate()
        .take(selection.end.line + 1)
        .skip(selection.start.line)
    {
        if line_index > selection.start.line {
            selected.push('\n');
        }
        let start = if line_index == selection.start.line {
            selection.start.byte
        } else {
            0
        };
        let end = if line_index == selection.end.line {
            selection.end.byte
        } else {
            line.len()
        };
        selected.push_str(&line[start..end]);
    }
    Some(selected)
}

impl App {
    fn clear_output_selection(&mut self) {
        self.output_selection = None;
        self.selection_anchor = None;
        self.selection_dragged = false;
        self.copy_feedback = None;
    }

    fn output_cell_at(&self, column: u16, row: u16) -> Option<OutputCell> {
        let area = self.output_inner_area;
        if area.width == 0 || area.height == 0 || self.rendered_output_lines.is_empty() {
            return None;
        }
        let column = column.clamp(area.x, area.x.saturating_add(area.width - 1));
        let row = row.clamp(area.y, area.y.saturating_add(area.height - 1));
        let rendered = *self.rendered_output_lines.get((row - area.y) as usize)?;
        if self.job_output_error.is_some() {
            return None;
        }
        let source_lines = output_lines(&self.job_output);
        let source = *source_lines.get(rendered.source_line)?;
        let screen_column = (column - area.x) as usize;

        let (start_byte, end_byte, display_column) = if self.job_output_wrap {
            let prefix_width = usize::from(rendered.continuation) * 2;
            if screen_column < prefix_width {
                return Some(OutputCell {
                    start: OutputPoint {
                        line: rendered.source_line,
                        byte: rendered.start_byte,
                    },
                    end: OutputPoint {
                        line: rendered.source_line,
                        byte: rendered.start_byte,
                    },
                });
            }
            (
                rendered.start_byte,
                rendered.end_byte,
                screen_column - prefix_width,
            )
        } else {
            (
                0,
                source.len(),
                screen_column + self.output_viewport.horizontal_offset(),
            )
        };

        let (start_byte, end_byte) =
            grapheme_cell_at_display_column(source, start_byte, end_byte, display_column);
        Some(OutputCell {
            start: OutputPoint {
                line: rendered.source_line,
                byte: start_byte,
            },
            end: OutputPoint {
                line: rendered.source_line,
                byte: end_byte,
            },
        })
    }

    fn update_output_selection(&mut self, column: u16, row: u16) {
        let (Some(anchor), Some(active)) =
            (self.selection_anchor, self.output_cell_at(column, row))
        else {
            return;
        };
        let (start, end) = if active.start < anchor.start {
            (active.start, anchor.end)
        } else {
            (anchor.start, active.end)
        };
        self.output_selection = (start < end).then_some(OutputSelection { start, end });
    }

    fn copy_selected_output(&mut self) {
        let content = self.output_selection.and_then(|selection| {
            self.job_output_error
                .is_none()
                .then(|| selected_output_text(&self.job_output, selection))
                .flatten()
        });
        match content {
            Some(content) if !content.is_empty() => {
                self.pending_copy = Some(PendingCopy {
                    content,
                    kind: CopyKind::Selection,
                });
            }
            _ => {
                self.pending_copy = None;
                self.copy_feedback = Some(CopyFeedback {
                    message: "Nothing selected".to_string(),
                    error: true,
                });
            }
        }
    }

    fn copy_full_output(&mut self) {
        let content = self
            .job_output_error
            .is_none()
            .then_some(&self.job_output)
            .filter(|output| !output.is_empty());
        match content {
            Some(content) => {
                self.pending_copy = Some(PendingCopy {
                    content: content.clone(),
                    kind: CopyKind::FullOutput,
                });
            }
            None => {
                self.pending_copy = None;
                self.copy_feedback = Some(CopyFeedback {
                    message: "No output to copy".to_string(),
                    error: true,
                });
            }
        }
    }

    fn flush_pending_copy(&mut self, writer: &mut impl Write) {
        let Some(copy) = self.pending_copy.take() else {
            return;
        };
        let length = copy.content.chars().count();
        self.copy_feedback = Some(
            match write_osc52_clipboard(writer, copy.content.as_bytes()) {
                Ok(()) => CopyFeedback {
                    message: match copy.kind {
                        CopyKind::Selection => format!("Copied {length} chars"),
                        CopyKind::FullOutput => format!("Copied all ({length} chars)"),
                    },
                    error: false,
                },
                Err(error) => CopyFeedback {
                    message: format!("Copy failed: {error}"),
                    error: true,
                },
            },
        );
    }

    fn selected_job(&self) -> Option<&Job> {
        self.job_list_state
            .selected()
            .and_then(|i| self.jobs.get(i))
    }

    fn selected_active_job(&self) -> Option<&Job> {
        self.selected_job().filter(|job| !job.finished)
    }

    fn selected_job_id(&self) -> Option<String> {
        self.selected_job().map(Job::id)
    }

    fn rebuild_visible_jobs(&mut self) {
        let old_index = self.job_list_state.selected();
        let previous_id = old_index
            .and_then(|index| self.jobs.get(index))
            .map(Job::id);

        self.jobs = visible_jobs(&self.remembered_jobs, self.show_finished_jobs);

        if self.jobs.is_empty() {
            self.job_list_state.select(None);
        } else if let Some(id) = previous_id.as_deref() {
            let new_index = self
                .jobs
                .iter()
                .position(|job| job.id() == id)
                .unwrap_or(old_index.unwrap_or(0).min(self.jobs.len() - 1));
            self.job_list_state.select(Some(new_index));
        } else {
            self.job_list_state.select_first();
        }

        if previous_id != self.selected_job_id() {
            self.clear_output_selection();
        }
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
        let previous = self.job_list_state.selected();
        self.job_list_state.select_next();
        if previous != self.job_list_state.selected() {
            self.clear_output_selection();
        }
    }

    fn select_previous_job(&mut self) {
        let previous = self.job_list_state.selected();
        self.job_list_state.select_previous();
        if previous != self.job_list_state.selected() {
            self.clear_output_selection();
        }
    }

    fn select_first_job(&mut self) {
        let previous = self.job_list_state.selected();
        self.job_list_state.select_first();
        if previous != self.job_list_state.selected() {
            self.clear_output_selection();
        }
    }

    fn select_last_job(&mut self) {
        let previous = self.job_list_state.selected();
        self.job_list_state.select_last();
        if previous != self.job_list_state.selected() {
            self.clear_output_selection();
        }
    }

    fn scroll_jobs_half_page_down(&mut self) {
        self.scroll_jobs_down_by(self.job_list_height / 2);
    }

    fn scroll_jobs_half_page_up(&mut self) {
        self.scroll_jobs_up_by(self.job_list_height / 2);
    }

    fn scroll_jobs_down_by(&mut self, amount: u16) {
        let previous = self.job_list_state.selected();
        self.job_list_state.scroll_down_by(amount);
        if previous != self.job_list_state.selected() {
            self.clear_output_selection();
        }
    }

    fn scroll_jobs_up_by(&mut self, amount: u16) {
        let previous = self.job_list_state.selected();
        self.job_list_state.scroll_up_by(amount);
        if previous != self.job_list_state.selected() {
            self.clear_output_selection();
        }
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

fn grapheme_cell_at_display_column(
    source: &str,
    start_byte: usize,
    end_byte: usize,
    display_column: usize,
) -> (usize, usize) {
    let mut source_column = 0usize;
    for (relative_byte, grapheme) in source[start_byte..end_byte].grapheme_indices(true) {
        let grapheme_end_column = source_column.saturating_add(display_width(grapheme));
        let grapheme_start_byte = start_byte + relative_byte;
        if display_column < grapheme_end_column {
            return (grapheme_start_byte, grapheme_start_byte + grapheme.len());
        }
        source_column = grapheme_end_column;
    }
    (end_byte, end_byte)
}

fn write_osc52_clipboard(writer: &mut impl Write, content: &[u8]) -> io::Result<()> {
    execute!(writer, CopyToClipboard::to_clipboard_from(content))
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
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        10
    } else {
        1
    }
}

fn mouse_wheel_scroll_multiplier(kind: MouseEventKind, modifiers: KeyModifiers) -> u16 {
    if matches!(kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown)
        && modifiers.contains(KeyModifiers::SHIFT)
    {
        SHIFT_WHEEL_SCROLL_MULTIPLIER
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

    fn test_job(id: &str) -> Job {
        Job {
            job_id: id.to_string(),
            array_id: id.to_string(),
            array_step: None,
            name: format!("job-{id}"),
            state: "RUNNING".to_string(),
            state_compact: "R".to_string(),
            reason: None,
            user: "user".to_string(),
            time: "00:01".to_string(),
            time_limit: "01:00:00".to_string(),
            start_time: "N/A".to_string(),
            tres: "cpu=1".to_string(),
            partition: "debug".to_string(),
            nodelist: "node01".to_string(),
            stdout: None,
            stderr: None,
            command: "true".to_string(),
            finished: false,
        }
    }

    #[test]
    fn disappeared_jobs_are_remembered_and_can_be_filtered() {
        let previous = vec![test_job("1"), test_job("2")];
        let mut refreshed_job = test_job("2");
        refreshed_job.time = "00:02".to_string();

        let remembered = remember_finished_jobs(previous, vec![refreshed_job]);

        assert_eq!(remembered.len(), 2);
        assert_eq!(remembered[0].id(), "2");
        assert_eq!(remembered[0].time, "00:02");
        assert!(!remembered[0].finished);
        assert_eq!(remembered[1].id(), "1");
        assert_eq!(remembered[1].state, "FINISHED");
        assert_eq!(remembered[1].state_compact, "F");
        assert!(remembered[1].finished);

        assert_eq!(
            visible_jobs(&remembered, false)
                .iter()
                .map(Job::id)
                .collect::<Vec<_>>(),
            ["2"]
        );
        assert_eq!(
            visible_jobs(&remembered, true)
                .iter()
                .map(Job::id)
                .collect::<Vec<_>>(),
            ["2", "1"]
        );
    }

    #[test]
    fn reappearing_job_becomes_active_again() {
        let remembered = remember_finished_jobs(vec![test_job("1")], Vec::new());
        assert!(remembered[0].finished);

        let mut reappeared = test_job("1");
        reappeared.time = "00:03".to_string();
        let remembered = remember_finished_jobs(remembered, vec![reappeared]);

        assert_eq!(remembered.len(), 1);
        assert_eq!(remembered[0].state, "RUNNING");
        assert_eq!(remembered[0].time, "00:03");
        assert!(!remembered[0].finished);
    }

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
    fn control_but_not_shift_accelerates_horizontal_keyboard_scrolling() {
        assert_eq!(
            horizontal_scroll_amount(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            1
        );
        assert_eq!(
            horizontal_scroll_amount(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT)),
            1
        );
        assert_eq!(
            horizontal_scroll_amount(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL)),
            10
        );
        assert_eq!(
            horizontal_scroll_amount(KeyEvent::new(
                KeyCode::Right,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            10
        );
    }

    #[test]
    fn only_shift_wheel_gets_the_horizontal_scroll_multiplier() {
        assert_eq!(
            mouse_wheel_scroll_multiplier(MouseEventKind::ScrollDown, KeyModifiers::SHIFT),
            3
        );
        assert_eq!(
            mouse_wheel_scroll_multiplier(MouseEventKind::ScrollRight, KeyModifiers::NONE),
            1
        );
    }

    #[test]
    fn test_fit_text_applies_horizontal_offset_without_wrapping() {
        let (text, _) = render_output_text(
            "abcdef\n",
            OutputRenderOptions {
                height: 1,
                width: 3,
                anchor: ScrollAnchor::Top,
                offset: 0,
                horizontal_offset: 2,
                wrap: false,
                selection: None,
            },
        );

        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].to_string(), "cde");
    }

    #[test]
    fn live_progress_is_rendered_before_its_final_newline() {
        let (text, _) = render_output_text(
            "Starting workload\nTraining:  34%|██████████",
            OutputRenderOptions {
                height: 2,
                width: 80,
                anchor: ScrollAnchor::Bottom,
                offset: 0,
                horizontal_offset: 0,
                wrap: false,
                selection: None,
            },
        );

        assert_eq!(text.lines.len(), 2);
        assert_eq!(text.lines[1].to_string(), "Training:  34%|██████████");
    }

    #[test]
    fn visible_lines_map_top_and_bottom_vertical_offsets() {
        let source = ["zero", "one", "two", "three"];

        let top = visible_output_lines(&source, 2, 20, ScrollAnchor::Top, 1, false);
        let bottom = visible_output_lines(&source, 2, 20, ScrollAnchor::Bottom, 1, false);

        assert_eq!(
            top.iter().map(|line| line.source_line).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(
            bottom
                .iter()
                .map(|line| line.source_line)
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn wrapped_lines_retain_source_byte_ranges() {
        let source = ["abcdef"];
        let lines = visible_output_lines(&source, 4, 3, ScrollAnchor::Top, 0, true);

        assert_eq!(
            lines,
            [
                RenderedOutputLine {
                    source_line: 0,
                    start_byte: 0,
                    end_byte: 3,
                    continuation: false,
                },
                RenderedOutputLine {
                    source_line: 0,
                    start_byte: 3,
                    end_byte: 4,
                    continuation: true,
                },
                RenderedOutputLine {
                    source_line: 0,
                    start_byte: 4,
                    end_byte: 5,
                    continuation: true,
                },
                RenderedOutputLine {
                    source_line: 0,
                    start_byte: 5,
                    end_byte: 6,
                    continuation: true,
                },
            ]
        );
    }

    #[test]
    fn horizontal_columns_map_to_whole_unicode_graphemes() {
        let source = "a测试b";

        assert_eq!(
            grapheme_cell_at_display_column(source, 0, source.len(), 2),
            (1, 4)
        );
        assert_eq!(
            grapheme_cell_at_display_column(source, 0, source.len(), 3),
            (4, 7)
        );
    }

    #[test]
    fn selected_text_preserves_logical_newlines_and_unicode() {
        let output = "zero\nab测试cd\nlast\n";
        let selection = OutputSelection {
            start: OutputPoint { line: 1, byte: 2 },
            end: OutputPoint { line: 2, byte: 2 },
        };

        assert_eq!(
            selected_output_text(output, selection).as_deref(),
            Some("测试cd\nla")
        );
    }

    #[test]
    fn selection_highlight_survives_horizontal_clipping() {
        let selection = OutputSelection {
            start: OutputPoint { line: 0, byte: 2 },
            end: OutputPoint { line: 0, byte: 4 },
        };
        let (text, _) = render_output_text(
            "abcdef\n",
            OutputRenderOptions {
                height: 1,
                width: 3,
                anchor: ScrollAnchor::Top,
                offset: 0,
                horizontal_offset: 2,
                wrap: false,
                selection: Some(selection),
            },
        );

        assert_eq!(text.lines[0].to_string(), "cde");
        assert_eq!(text.lines[0].spans[0].style.bg, Some(Color::LightBlue));
        assert_eq!(text.lines[0].spans[1].style.bg, Some(Color::LightBlue));
        assert_eq!(text.lines[0].spans[2].style.bg, None);
    }

    #[test]
    fn clipboard_copy_writes_osc52_sequence() {
        let mut output = Vec::new();

        write_osc52_clipboard(&mut output, b"hello").unwrap();

        assert_eq!(output, b"\x1b]52;c;aGVsbG8=\x1b\\");
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
