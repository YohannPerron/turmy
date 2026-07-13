use std::{
    fmt,
    fs::File,
    io::{self, Read, Seek},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use crossbeam_channel::{Receiver, SendError, Sender, select, unbounded};
use notify::{RecursiveMode, Watcher, event::ModifyKind};
use unicode_width::UnicodeWidthStr;

use crate::app::AppMessage;

const TAB_WIDTH: usize = 8;

struct FileReader {
    content_sender: Sender<FileWatcherUpdate>,
    receiver: Receiver<()>,
    file_path: PathBuf,
    interval: Duration,
    pos: u64,
}

struct FileWatcher {
    app: Sender<AppMessage>,
    receiver: Receiver<FileWatcherMessage>,
    file_path: Option<PathBuf>,
    watching: bool, // Whether notify watch was successfully started for file_path
    interval: Duration,
}

pub enum FileWatcherMessage {
    FilePath(Option<PathBuf>),
}

pub enum FileWatcherUpdate {
    Reset,
    Append(Vec<u8>),
    Error(FileWatcherError),
}

pub struct FileWatcherHandle {
    sender: Sender<FileWatcherMessage>,
    file_path: Option<PathBuf>,
}

pub enum FileWatcherError {
    File(io::Error),
}

impl fmt::Display for FileWatcherError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FileWatcherError::File(e) => write!(f, "Read error: {e}"),
        }
    }
}

#[derive(Default)]
pub struct LogTextDecoder {
    pending_utf8: Vec<u8>,
    escape_state: EscapeState,
    csi_parameters: String,
    line_start: usize,
    cursor: usize,
}

#[derive(Default)]
enum EscapeState {
    #[default]
    Text,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

impl FileWatcher {
    fn new(
        app: Sender<AppMessage>,
        receiver: Receiver<FileWatcherMessage>,
        interval: Duration,
    ) -> Self {
        FileWatcher {
            app,
            receiver,
            file_path: None,
            watching: false,
            interval,
        }
    }

    fn run(&mut self) {
        let (watch_sender, watch_receiver) = unbounded::<()>();
        // Keep a sender alive even when notify cannot create a platform watcher. Polling remains
        // active in that case, and the receive arm must not become a disconnected busy loop.
        let _watch_sender_guard = watch_sender.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res
                && let notify::EventKind::Modify(ModifyKind::Data(_)) = event.kind
            {
                let _ = watch_sender.send(());
            }
        })
        .ok();

        let (mut _content_sender, mut content_receiver) = unbounded::<FileWatcherUpdate>();
        let (mut reader_wake_sender, mut _reader_wake_receiver) = unbounded::<()>();
        let mut reader_had_error = false;
        loop {
            select! {
                recv(self.receiver) -> msg => {
                    let Ok(FileWatcherMessage::FilePath(file_path)) = msg else {
                        break;
                    };

                    // Dropping the old channels stops the previous reader. Updates from it can no
                    // longer be delivered after a new file has been selected.
                    (_content_sender, content_receiver) = unbounded();
                    (reader_wake_sender, _reader_wake_receiver) = unbounded();

                    if self.watching
                        && let (Some(watcher), Some(path)) =
                            (watcher.as_mut(), self.file_path.as_ref())
                    {
                        // An unwatch failure is non-fatal: the polling reader for the new path is
                        // still authoritative, and an obsolete notification only causes a wakeup.
                        let _ = watcher.unwatch(path);
                    }
                    self.file_path = file_path.clone();
                    self.watching = false;
                    reader_had_error = false;

                    if self.app.send(AppMessage::JobOutput(FileWatcherUpdate::Reset)).is_err() {
                        break;
                    }

                    if let Some(path) = file_path {
                        let interval = self.interval;
                        thread::spawn({
                            let path = path.clone();
                            move || FileReader::new(_content_sender, _reader_wake_receiver, path, interval).run()
                        });

                        self.watching = watcher.as_mut().is_some_and(|watcher| {
                            watcher.watch(Path::new(&path), RecursiveMode::NonRecursive).is_ok()
                        });
                    }
                }
                recv(watch_receiver) -> event => {
                    if event.is_ok() {
                        let _ = reader_wake_sender.send(());
                    }
                }
                recv(content_receiver) -> update => {
                    let Ok(update) = update else {
                        continue;
                    };
                    // If the file did not exist when selected but now reads successfully, try to
                    // enable notifications. Polling continues if this still fails (for example on
                    // an NFS mount).
                    if !self.watching
                        && matches!(update, FileWatcherUpdate::Append(_))
                        && let (Some(watcher), Some(path)) =
                            (watcher.as_mut(), self.file_path.as_ref())
                    {
                        self.watching = watcher
                            .watch(Path::new(path), RecursiveMode::NonRecursive)
                            .is_ok();
                    }
                    let should_forward = match &update {
                        FileWatcherUpdate::Append(bytes) => {
                            let should_forward = !bytes.is_empty() || reader_had_error;
                            reader_had_error = false;
                            should_forward
                        }
                        FileWatcherUpdate::Error(_) => {
                            reader_had_error = true;
                            true
                        }
                        FileWatcherUpdate::Reset => true,
                    };
                    if should_forward
                        && self.app.send(AppMessage::JobOutput(update)).is_err()
                    {
                        break;
                    }
                }
            }
        }
    }
}

impl FileReader {
    fn new(
        content_sender: Sender<FileWatcherUpdate>,
        receiver: Receiver<()>,
        file_path: PathBuf,
        interval: Duration,
    ) -> Self {
        FileReader {
            content_sender,
            receiver,
            file_path,
            interval,
            pos: 0,
        }
    }

    fn run(&mut self) {
        loop {
            if self.update().is_err() {
                break;
            }
            select! {
                recv(self.receiver) -> msg => {
                    if msg.is_err() {
                        break;
                    }
                }
                // in case the file watcher doesn't work (e.g. network mounted fs)
                default(self.interval) => {}
            }
        }
    }

    fn update(&mut self) -> Result<(), SendError<FileWatcherUpdate>> {
        let update = File::open(&self.file_path)
            .and_then(|mut file| {
                self.pos = file.seek(io::SeekFrom::Start(self.pos))?;
                let mut appended = Vec::new();
                let bytes_read = file.read_to_end(&mut appended)?;
                self.pos += bytes_read as u64;
                Ok(appended)
            })
            .map(FileWatcherUpdate::Append)
            .unwrap_or_else(|error| FileWatcherUpdate::Error(FileWatcherError::File(error)));
        self.content_sender.send(update)
    }
}

impl FileWatcherHandle {
    pub fn new(app: Sender<AppMessage>, interval: Duration) -> Self {
        let (sender, receiver) = unbounded();
        let mut actor = FileWatcher::new(app, receiver, interval);
        thread::spawn(move || actor.run());

        Self {
            sender,
            file_path: None,
        }
    }

    pub fn set_file_path(&mut self, file_path: Option<PathBuf>) {
        if self.file_path != file_path
            && self
                .sender
                .send(FileWatcherMessage::FilePath(file_path.clone()))
                .is_ok()
        {
            self.file_path = file_path;
        }
    }
}

impl LogTextDecoder {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Decode and normalize newly appended log bytes directly into the UI-owned output buffer.
    /// Invalid UTF-8 is replaced with U+FFFD, while an incomplete trailing sequence is retained
    /// until the next append.
    pub fn push(&mut self, bytes: &[u8], output: &mut String) {
        self.pending_utf8.extend_from_slice(bytes);
        let mut consumed = 0;

        while consumed < self.pending_utf8.len() {
            match std::str::from_utf8(&self.pending_utf8[consumed..]) {
                Ok(valid) => {
                    let valid = valid.to_string();
                    consumed = self.pending_utf8.len();
                    self.push_valid(&valid, output);
                }
                Err(error) => {
                    let valid_end = consumed + error.valid_up_to();
                    if valid_end > consumed {
                        let valid =
                            String::from_utf8_lossy(&self.pending_utf8[consumed..valid_end])
                                .into_owned();
                        self.push_valid(&valid, output);
                    }
                    consumed = valid_end;

                    let Some(error_len) = error.error_len() else {
                        break;
                    };
                    consumed += error_len;
                    self.push_char('\u{fffd}', output);
                }
            }
        }

        self.pending_utf8.drain(..consumed);
    }

    fn push_valid(&mut self, valid: &str, output: &mut String) {
        for character in valid.chars() {
            self.push_char(character, output);
        }
    }

    fn push_char(&mut self, character: char, output: &mut String) {
        match self.escape_state {
            EscapeState::Escape => {
                self.escape_state = match character {
                    '[' => {
                        self.csi_parameters.clear();
                        EscapeState::Csi
                    }
                    ']' => EscapeState::Osc,
                    _ => EscapeState::Text,
                };
                return;
            }
            EscapeState::Csi => {
                if ('@'..='~').contains(&character) {
                    if character == 'K' {
                        self.erase_line(output);
                    }
                    self.escape_state = EscapeState::Text;
                } else if self.csi_parameters.len() < 32 {
                    self.csi_parameters.push(character);
                }
                return;
            }
            EscapeState::Osc => {
                self.escape_state = match character {
                    '\u{7}' => EscapeState::Text,
                    '\u{1b}' => EscapeState::OscEscape,
                    _ => EscapeState::Osc,
                };
                return;
            }
            EscapeState::OscEscape => {
                self.escape_state = if character == '\\' {
                    EscapeState::Text
                } else {
                    EscapeState::Osc
                };
                return;
            }
            EscapeState::Text => {}
        }

        match character {
            '\u{1b}' => self.escape_state = EscapeState::Escape,
            '\u{9b}' => {
                self.csi_parameters.clear();
                self.escape_state = EscapeState::Csi;
            }
            '\u{9d}' => self.escape_state = EscapeState::Osc,
            '\n' => {
                output.push('\n');
                self.line_start = output.len();
                self.cursor = self.line_start;
            }
            '\r' => self.cursor = self.line_start,
            '\u{8}' => self.cursor = previous_char_boundary(output, self.cursor, self.line_start),
            '\t' => {
                let column = UnicodeWidthStr::width(&output[self.line_start..self.cursor]);
                let spaces = TAB_WIDTH - column % TAB_WIDTH;
                for _ in 0..spaces {
                    self.write_printable(' ', output);
                }
            }
            character if character.is_control() => {}
            character => self.write_printable(character, output),
        }
    }

    fn write_printable(&mut self, character: char, output: &mut String) {
        if self.cursor < output.len() {
            let old_end = self.cursor
                + output[self.cursor..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or_default();
            output.replace_range(self.cursor..old_end, &character.to_string());
        } else {
            output.push(character);
        }
        self.cursor += character.len_utf8();
    }

    fn erase_line(&mut self, output: &mut String) {
        match self.csi_parameters.as_str() {
            "2" => {
                output.truncate(self.line_start);
                self.cursor = self.line_start;
            }
            "1" => {
                let spaces = output[self.line_start..self.cursor].chars().count();
                output.replace_range(self.line_start..self.cursor, &" ".repeat(spaces));
                self.cursor = self.line_start + spaces;
            }
            _ => output.truncate(self.cursor),
        }
    }
}

fn previous_char_boundary(output: &str, cursor: usize, line_start: usize) -> usize {
    if cursor <= line_start {
        return line_start;
    }
    output[..cursor]
        .char_indices()
        .next_back()
        .map_or(line_start, |(index, _)| index.max(line_start))
}

#[cfg(test)]
mod tests {
    use super::LogTextDecoder;

    #[test]
    fn decoder_preserves_utf8_split_between_appends() {
        let mut decoder = LogTextDecoder::default();
        let mut output = String::new();
        decoder.push(&[b'a', 0xe2, 0x82], &mut output);
        assert_eq!(output, "a");
        decoder.push(&[0xac, b'b'], &mut output);
        assert_eq!(output, "a€b");
    }

    #[test]
    fn decoder_replaces_invalid_utf8_and_continues() {
        let mut decoder = LogTextDecoder::default();
        let mut output = String::new();
        decoder.push(b"before\xffafter", &mut output);
        assert_eq!(output, "before\u{fffd}after");
    }

    #[test]
    fn decoder_normalizes_common_terminal_output() {
        let mut decoder = LogTextDecoder::default();
        let mut output = String::new();
        decoder.push(b"progress 10%\rprogress 20%\nabc\x08D\tx", &mut output);
        assert_eq!(output, "progress 20%\nabD     x");
    }

    #[test]
    fn decoder_strips_ansi_sequences_across_appends() {
        let mut decoder = LogTextDecoder::default();
        let mut output = String::new();
        decoder.push(b"plain \x1b[31", &mut output);
        decoder.push(b"mred\x1b[0m \x1b]0;title", &mut output);
        decoder.push(b"\x07text", &mut output);
        assert_eq!(output, "plain red text");
    }

    #[test]
    fn decoder_applies_ansi_erase_line() {
        let mut decoder = LogTextDecoder::default();
        let mut output = String::new();
        decoder.push(b"a long status\rshort\x1b[K", &mut output);
        assert_eq!(output, "short");
    }
}
