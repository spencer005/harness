use std::{
    collections::HashMap,
    io,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use thiserror::Error;
use tokio::task::JoinError;

use crate::tools::{
    NativeToolExecutionOutput, TERMINAL_OPEN_TOOL_NAME, TERMINAL_READ_TOOL_NAME,
    TERMINAL_WRITE_TOOL_NAME,
};

const DEFAULT_TERMINAL_ACTIVITY_WAIT: Duration = Duration::from_millis(500);
const OUTPUT_QUIET_FOR: Duration = Duration::from_millis(100);
const TERMINAL_SHELL: &str = "/bin/bash";

/// PTY-backed terminal session manager for native terminal tools.
#[derive(Clone)]
pub struct TerminalManager {
    inner: Arc<TerminalInner>,
}

struct TerminalInner {
    next_terminal_id: AtomicI32,
    next_chunk_id: AtomicU64,
    terminals: Mutex<HashMap<i32, Arc<Mutex<TerminalEntry>>>>,
}

struct TerminalEntry {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
}

#[derive(Debug)]
struct TerminalOpenArgs {
    workdir: Option<String>,
    rows: u16,
    cols: u16,
    command: String,
}

#[derive(Debug)]
struct TerminalWriteArgs {
    terminal_id: i32,
    input: String,
}

#[derive(Debug)]
struct TerminalReadArgs {
    terminal_id: i32,
    poll_after: Option<Duration>,
}

#[derive(Debug, Error)]
/// Errors returned by the PTY-backed terminal manager.
pub enum TerminalError {
    /// Tool input could not be parsed.
    #[error("failed to parse `{tool_name}` input: {message}")]
    ParseInput {
        /// Tool name being parsed.
        tool_name: &'static str,
        /// Parse error detail.
        message: String,
    },
    /// Requested terminal tool is not supported.
    #[error("unsupported terminal tool `{0}`")]
    UnsupportedTool(String),
    /// Referenced terminal id is not active.
    #[error("terminal {0} does not exist")]
    MissingTerminal(i32),
    /// Terminal operation failed with an I/O error.
    #[error("{context}: {source}")]
    Io {
        /// Operation context.
        context: String,
        #[source]
        /// Source I/O error.
        source: io::Error,
    },
    /// PTY backend operation failed.
    #[error("{context}: {message}")]
    Pty {
        /// Operation context.
        context: String,
        /// PTY backend message.
        message: String,
    },
    /// Blocking task join failed.
    #[error("terminal tool execution task failed")]
    Join {
        /// Join error returned by the blocking task.
        #[source]
        source: JoinError,
    },
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalManager {
    /// Create an empty terminal manager.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(TerminalInner {
                next_terminal_id: AtomicI32::new(1),
                next_chunk_id: AtomicU64::new(1),
                terminals: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Execute a terminal tool and return model-visible output.
    #[cfg(test)]
    pub async fn execute_tool(
        &self,
        cwd: impl Into<PathBuf>,
        tool_name: String,
        input: String,
    ) -> Result<String, TerminalError> {
        Ok(self
            .execute_tool_output(cwd, tool_name, input)
            .await?
            .model_output)
    }

    /// Execute a terminal tool and return separate model/display output.
    pub async fn execute_tool_output(
        &self,
        cwd: impl Into<PathBuf>,
        tool_name: String,
        input: String,
    ) -> Result<NativeToolExecutionOutput, TerminalError> {
        let manager = self.clone();
        let cwd = cwd.into();
        tokio::task::spawn_blocking(move || {
            manager.execute_tool_output_sync(&cwd, &tool_name, &input)
        })
        .await
        .map_err(|source| TerminalError::Join { source })?
    }
    /// Terminates all persistent terminal sessions owned by this manager.
    pub fn shutdown(&self) {
        let entries = std::mem::take(&mut *self.inner.terminals.lock().unwrap());
        for (_, entry) in entries {
            let mut entry = entry.lock().unwrap();
            let _ = entry.child.kill();
        }
    }

    fn execute_tool_output_sync(
        &self,
        cwd: &Path,
        tool_name: &str,
        input: &str,
    ) -> Result<NativeToolExecutionOutput, TerminalError> {
        match tool_name {
            TERMINAL_OPEN_TOOL_NAME => self.terminal_open(cwd, input),
            TERMINAL_WRITE_TOOL_NAME => self.terminal_write(input),
            TERMINAL_READ_TOOL_NAME => self.terminal_read(input),
            name => Err(TerminalError::UnsupportedTool(name.to_string())),
        }
    }

    fn terminal_open(
        &self,
        cwd: &Path,
        input: &str,
    ) -> Result<NativeToolExecutionOutput, TerminalError> {
        let args = parse_terminal_open_args(input)?;

        let terminal_id = self.inner.next_terminal_id.fetch_add(1, Ordering::Relaxed);
        let chunk_id = self.next_chunk_id();
        let workdir = resolve_workdir(cwd, args.workdir.as_deref());
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: args.rows,
                cols: args.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| TerminalError::Pty {
                context: "failed to open terminal pty".to_string(),
                message: error.to_string(),
            })?;
        let command = terminal_command(&args.command, &workdir);
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| TerminalError::Pty {
                context: "failed to spawn terminal command".to_string(),
                message: error.to_string(),
            })?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| TerminalError::Pty {
                context: "failed to open terminal input stream".to_string(),
                message: error.to_string(),
            })?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| TerminalError::Pty {
                context: "failed to open terminal output stream".to_string(),
                message: error.to_string(),
            })?;
        drop(pair.slave);
        drop(pair.master);

        let output = Arc::new(Mutex::new(Vec::new()));
        spawn_reader(reader, Arc::clone(&output));

        let exit_code =
            wait_for_terminal_activity(&mut *child, &output, DEFAULT_TERMINAL_ACTIVITY_WAIT)?;
        if exit_code.is_some() {
            wait_for_reader_flush();
        }
        let raw_output = consume_output(&output);

        if exit_code.is_none() {
            self.inner.terminals.lock().unwrap().insert(
                terminal_id,
                Arc::new(Mutex::new(TerminalEntry {
                    child,
                    writer,
                    output: Arc::clone(&output),
                })),
            );
        }

        Ok(format_terminal_output(TerminalOutput {
            chunk_id,
            raw_output,
            echoed_input: None,
            terminal_id: exit_code.is_none().then_some(terminal_id),
            exit_code,
        }))
    }

    fn terminal_write(&self, input: &str) -> Result<NativeToolExecutionOutput, TerminalError> {
        let args = parse_terminal_write_args(input)?;
        let submitted_input = submitted_terminal_input(&args.input);
        let chunk_id = self.next_chunk_id();
        let terminal = self.terminal(args.terminal_id)?;
        let (output, remove_terminal) = {
            let mut entry = terminal.lock().unwrap();
            if let Some(status) = entry.child.try_wait().map_err(|source| TerminalError::Io {
                context: "failed to poll terminal status".to_string(),
                source,
            })? {
                wait_for_reader_flush();
                (
                    TerminalOutput {
                        chunk_id,
                        raw_output: consume_output(&entry.output),
                        echoed_input: None,
                        terminal_id: None,
                        exit_code: Some(status.exit_code()),
                    },
                    true,
                )
            } else {
                entry
                    .writer
                    .write_all(submitted_input.as_bytes())
                    .and_then(|()| entry.writer.flush())
                    .map_err(|source| TerminalError::Io {
                        context: format!("failed to write input for terminal {}", args.terminal_id),
                        source,
                    })?;
                let output_buffer = Arc::clone(&entry.output);
                let exit_code = wait_for_terminal_activity(
                    &mut *entry.child,
                    &output_buffer,
                    DEFAULT_TERMINAL_ACTIVITY_WAIT,
                )?;
                if exit_code.is_some() {
                    wait_for_reader_flush();
                }
                (
                    TerminalOutput {
                        chunk_id,
                        raw_output: consume_output(&entry.output),
                        echoed_input: Some(submitted_input),
                        terminal_id: exit_code.is_none().then_some(args.terminal_id),
                        exit_code,
                    },
                    exit_code.is_some(),
                )
            }
        };
        if remove_terminal {
            self.remove_terminal(args.terminal_id, &terminal);
        }
        Ok(format_terminal_output(output))
    }

    fn terminal_read(&self, input: &str) -> Result<NativeToolExecutionOutput, TerminalError> {
        let args = parse_terminal_read_args(input)?;
        let chunk_id = self.next_chunk_id();
        let terminal = self.terminal(args.terminal_id)?;
        let (output, remove_terminal) = {
            let mut entry = terminal.lock().unwrap();
            let output_buffer = Arc::clone(&entry.output);
            let exit_code = match args.poll_after {
                Some(poll_after) => wait_for_poll_interval(&mut *entry.child, poll_after)?,
                None => wait_for_terminal_activity(
                    &mut *entry.child,
                    &output_buffer,
                    DEFAULT_TERMINAL_ACTIVITY_WAIT,
                )?,
            };
            if exit_code.is_some() {
                wait_for_reader_flush();
            }
            (
                TerminalOutput {
                    chunk_id,
                    raw_output: consume_output(&entry.output),
                    echoed_input: None,
                    terminal_id: exit_code.is_none().then_some(args.terminal_id),
                    exit_code,
                },
                exit_code.is_some(),
            )
        };
        if remove_terminal {
            self.remove_terminal(args.terminal_id, &terminal);
        }
        Ok(format_terminal_output(output))
    }

    fn terminal(&self, terminal_id: i32) -> Result<Arc<Mutex<TerminalEntry>>, TerminalError> {
        self.inner
            .terminals
            .lock()
            .unwrap()
            .get(&terminal_id)
            .cloned()
            .ok_or(TerminalError::MissingTerminal(terminal_id))
    }

    fn remove_terminal(&self, terminal_id: i32, terminal: &Arc<Mutex<TerminalEntry>>) {
        let mut terminals = self.inner.terminals.lock().unwrap();
        if terminals
            .get(&terminal_id)
            .is_some_and(|active| Arc::ptr_eq(active, terminal))
        {
            terminals.remove(&terminal_id);
        }
    }

    fn next_chunk_id(&self) -> String {
        let id = self.inner.next_chunk_id.fetch_add(1, Ordering::Relaxed);
        format!("chunk-{id}")
    }
}

#[derive(Debug)]
struct TerminalOutput {
    chunk_id: String,
    raw_output: Vec<u8>,
    echoed_input: Option<String>,
    terminal_id: Option<i32>,
    exit_code: Option<u32>,
}

#[derive(Debug, Default)]
struct TerminalOpenSeen {
    workdir: bool,
    rows: bool,
    cols: bool,
    command: bool,
}

#[derive(Debug, Default)]
struct TerminalWriteSeen {
    terminal: bool,
    input: bool,
}

#[derive(Debug, Default)]
struct TerminalReadSeen {
    terminal: bool,
    poll_after: bool,
}

fn parse_terminal_open_args(input: &str) -> Result<TerminalOpenArgs, TerminalError> {
    let mut workdir = None;
    let mut rows = 24;
    let mut cols = 80;
    let mut command = None;
    let mut seen = TerminalOpenSeen::default();
    let mut offset = 0;

    for segment in input.split_inclusive('\n') {
        let next_offset = offset + segment.len();
        let (key, value) =
            parse_header_line(TERMINAL_OPEN_TOOL_NAME, line_without_ending(segment))?;
        match key {
            "workdir" => {
                mark_header(TERMINAL_OPEN_TOOL_NAME, "workdir", &mut seen.workdir)?;
                let value = value.trim();
                if !value.is_empty() {
                    workdir = Some(value.to_string());
                }
            }
            "rows" => {
                mark_header(TERMINAL_OPEN_TOOL_NAME, "rows", &mut seen.rows)?;
                rows = parse_u16_header(TERMINAL_OPEN_TOOL_NAME, "rows", value)?;
            }
            "cols" => {
                mark_header(TERMINAL_OPEN_TOOL_NAME, "cols", &mut seen.cols)?;
                cols = parse_u16_header(TERMINAL_OPEN_TOOL_NAME, "cols", value)?;
            }
            "command" => {
                mark_header(TERMINAL_OPEN_TOOL_NAME, "command", &mut seen.command)?;
                command = Some(if value.is_empty() {
                    input[next_offset..].to_string()
                } else {
                    value.to_string()
                });
                break;
            }
            key => {
                return Err(parse_input_error(
                    TERMINAL_OPEN_TOOL_NAME,
                    format!("unsupported header `{key}`"),
                ));
            }
        }
        offset = next_offset;
    }

    Ok(TerminalOpenArgs {
        workdir,
        rows,
        cols,
        command: command.ok_or_else(|| {
            parse_input_error(TERMINAL_OPEN_TOOL_NAME, "`command:` line is required")
        })?,
    })
}

fn parse_terminal_write_args(input: &str) -> Result<TerminalWriteArgs, TerminalError> {
    let mut terminal_id = None;
    let mut terminal_input = None;
    let mut seen = TerminalWriteSeen::default();
    let mut offset = 0;

    for segment in input.split_inclusive('\n') {
        let next_offset = offset + segment.len();
        let (key, value) =
            parse_header_line(TERMINAL_WRITE_TOOL_NAME, line_without_ending(segment))?;
        match key {
            "terminal" => {
                mark_header(TERMINAL_WRITE_TOOL_NAME, "terminal", &mut seen.terminal)?;
                terminal_id = Some(parse_terminal_id(TERMINAL_WRITE_TOOL_NAME, value)?);
            }
            "input" => {
                mark_header(TERMINAL_WRITE_TOOL_NAME, "input", &mut seen.input)?;
                terminal_input = Some(if value.is_empty() {
                    input[next_offset..].to_string()
                } else {
                    value.to_string()
                });
                break;
            }
            key => {
                return Err(parse_input_error(
                    TERMINAL_WRITE_TOOL_NAME,
                    format!("unsupported header `{key}`"),
                ));
            }
        }
        offset = next_offset;
    }

    Ok(TerminalWriteArgs {
        terminal_id: terminal_id.ok_or_else(|| {
            parse_input_error(TERMINAL_WRITE_TOOL_NAME, "`terminal:` line is required")
        })?,
        input: terminal_input.ok_or_else(|| {
            parse_input_error(TERMINAL_WRITE_TOOL_NAME, "`input:` line is required")
        })?,
    })
}

fn parse_terminal_read_args(input: &str) -> Result<TerminalReadArgs, TerminalError> {
    let mut terminal_id = None;
    let mut poll_after = None;
    let mut seen = TerminalReadSeen::default();

    for segment in input.split_inclusive('\n') {
        let (key, value) =
            parse_header_line(TERMINAL_READ_TOOL_NAME, line_without_ending(segment))?;
        match key {
            "terminal" => {
                mark_header(TERMINAL_READ_TOOL_NAME, "terminal", &mut seen.terminal)?;
                terminal_id = Some(parse_terminal_id(TERMINAL_READ_TOOL_NAME, value)?);
            }
            "poll_after" => {
                mark_header(TERMINAL_READ_TOOL_NAME, "poll_after", &mut seen.poll_after)?;
                poll_after = Some(parse_duration_header(
                    TERMINAL_READ_TOOL_NAME,
                    "poll_after",
                    value,
                )?);
            }
            key => {
                return Err(parse_input_error(
                    TERMINAL_READ_TOOL_NAME,
                    format!("unsupported header `{key}`"),
                ));
            }
        }
    }

    Ok(TerminalReadArgs {
        terminal_id: terminal_id.ok_or_else(|| {
            parse_input_error(TERMINAL_READ_TOOL_NAME, "`terminal:` line is required")
        })?,
        poll_after,
    })
}

fn line_without_ending(line: &str) -> &str {
    line.strip_suffix('\n')
        .unwrap_or(line)
        .strip_suffix('\r')
        .unwrap_or_else(|| line.strip_suffix('\n').unwrap_or(line))
}

fn parse_header_line<'a>(
    tool_name: &'static str,
    line: &'a str,
) -> Result<(&'a str, &'a str), TerminalError> {
    let (key, value) = line
        .split_once(':')
        .ok_or_else(|| parse_input_error(tool_name, "expected `key: value` line"))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(parse_input_error(tool_name, "header key must not be empty"));
    }
    Ok((key, value.trim_start()))
}

fn mark_header(
    tool_name: &'static str,
    key: &'static str,
    seen: &mut bool,
) -> Result<(), TerminalError> {
    if *seen {
        return Err(parse_input_error(
            tool_name,
            format!("duplicate `{key}:` line"),
        ));
    }
    *seen = true;
    Ok(())
}

fn parse_duration_header(
    tool_name: &'static str,
    key: &'static str,
    value: &str,
) -> Result<Duration, TerminalError> {
    let value = value.trim();
    let unit_start = value
        .find(|character: char| !character.is_ascii_digit())
        .ok_or_else(|| duration_parse_error(tool_name, key))?;
    let (amount, unit) = value.split_at(unit_start);
    if amount.is_empty() {
        return Err(duration_parse_error(tool_name, key));
    }
    let amount = amount
        .parse::<u64>()
        .map_err(|_| duration_parse_error(tool_name, key))?;
    match unit {
        "ms" => Ok(Duration::from_millis(amount)),
        "s" => Ok(Duration::from_secs(amount)),
        "m" => amount
            .checked_mul(60)
            .map(Duration::from_secs)
            .ok_or_else(|| duration_parse_error(tool_name, key)),
        _ => Err(duration_parse_error(tool_name, key)),
    }
}

fn duration_parse_error(tool_name: &'static str, key: &'static str) -> TerminalError {
    parse_input_error(
        tool_name,
        format!("`{key}:` must be a duration like `250ms`, `30s`, or `2m`"),
    )
}

fn parse_u16_header(
    tool_name: &'static str,
    key: &'static str,
    value: &str,
) -> Result<u16, TerminalError> {
    value
        .trim()
        .parse::<u16>()
        .map_err(|_| parse_input_error(tool_name, format!("`{key}:` must be a positive integer")))
}

fn parse_terminal_id(tool_name: &'static str, value: &str) -> Result<i32, TerminalError> {
    let terminal_id = value
        .trim()
        .parse::<i32>()
        .map_err(|_| parse_input_error(tool_name, "`terminal:` must be a positive integer"))?;
    if terminal_id <= 0 {
        return Err(parse_input_error(
            tool_name,
            "`terminal:` must be a positive integer",
        ));
    }
    Ok(terminal_id)
}

fn parse_input_error(tool_name: &'static str, message: impl Into<String>) -> TerminalError {
    TerminalError::ParseInput {
        tool_name,
        message: message.into(),
    }
}

fn resolve_workdir(cwd: &Path, workdir: Option<&str>) -> PathBuf {
    let Some(workdir) = workdir.filter(|value| !value.is_empty()) else {
        return cwd.to_path_buf();
    };
    let path = PathBuf::from(workdir);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn terminal_command(command: &str, workdir: &Path) -> CommandBuilder {
    let mut builder = CommandBuilder::new(TERMINAL_SHELL);
    builder.args([
        "--noprofile",
        "--norc",
        "-E",
        "-e",
        "-u",
        "-o",
        "pipefail",
        "-O",
        "inherit_errexit",
        "-O",
        "failglob",
        "-c",
        command,
    ]);
    builder.cwd(workdir);
    builder.env_remove("BASH_ENV");
    let imported_functions = builder
        .iter_full_env_as_str()
        .filter_map(|(key, _)| key.starts_with("BASH_FUNC_").then(|| key.to_string()))
        .collect::<Vec<_>>();
    for key in imported_functions {
        builder.env_remove(key);
    }
    builder.env_remove("SHELLOPTS");
    builder.env_remove("BASHOPTS");
    builder.env("TERM", "xterm-direct");
    builder.env("COLORTERM", "truecolor");
    builder.env("PAGER", "cat");
    builder
}

fn submitted_terminal_input(input: &str) -> String {
    if input.is_empty()
        || input.ends_with('\n')
        || input.ends_with('\r')
        || contains_terminal_control(input)
    {
        return input.to_string();
    }
    let mut submitted = input.to_string();
    submitted.push('\n');
    submitted
}

fn contains_terminal_control(input: &str) -> bool {
    input
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
}

fn wait_for_terminal_activity(
    child: &mut dyn Child,
    output: &Arc<Mutex<Vec<u8>>>,
    wait_for: Duration,
) -> Result<Option<u32>, TerminalError> {
    let started_wait = Instant::now();
    let mut previous_len = output_len(output);
    let mut last_output_at = None;
    loop {
        if let Some(status) = child.try_wait().map_err(|source| TerminalError::Io {
            context: "failed to poll terminal status".to_string(),
            source,
        })? {
            return Ok(Some(status.exit_code()));
        }
        let current_len = output_len(output);
        if current_len != previous_len {
            previous_len = current_len;
            last_output_at = Some(Instant::now());
        }
        if last_output_at.is_some_and(|instant| instant.elapsed() >= OUTPUT_QUIET_FOR) {
            return Ok(None);
        }
        if started_wait.elapsed() >= wait_for {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_poll_interval(
    child: &mut dyn Child,
    poll_after: Duration,
) -> Result<Option<u32>, TerminalError> {
    let started_wait = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|source| TerminalError::Io {
            context: "failed to poll terminal status".to_string(),
            source,
        })? {
            return Ok(Some(status.exit_code()));
        }
        if started_wait.elapsed() >= poll_after {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, output: Arc<Mutex<Vec<u8>>>) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(bytes_read) => {
                    output
                        .lock()
                        .unwrap()
                        .extend_from_slice(&buffer[..bytes_read]);
                }
                Err(_) => break,
            }
        }
    });
}

fn wait_for_reader_flush() {
    thread::sleep(Duration::from_millis(20));
}

fn output_len(output: &Arc<Mutex<Vec<u8>>>) -> usize {
    output.lock().unwrap().len()
}

fn consume_output(output: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    std::mem::take(&mut *output.lock().unwrap())
}

fn format_terminal_output(output: TerminalOutput) -> NativeToolExecutionOutput {
    let raw_text = String::from_utf8_lossy(&output.raw_output);
    let model_text = strip_echoed_input(
        &sanitize_terminal_output_for_model(&raw_text),
        output.echoed_input.as_deref(),
    );
    let display_text = raw_text.to_string();
    NativeToolExecutionOutput::split(
        format_terminal_envelope(&output, model_text),
        format_terminal_envelope(&output, display_text),
    )
}

fn format_terminal_envelope(output: &TerminalOutput, text: String) -> String {
    let mut sections = Vec::new();
    sections.push(format!("Chunk ID: {}", output.chunk_id));
    if let Some(exit_code) = output.exit_code {
        sections.push(format!("Process exited with code {exit_code}"));
    }
    if let Some(terminal_id) = output.terminal_id {
        sections.push(format!("Terminal running with ID {terminal_id}"));
    }
    sections.push("Output:".to_string());
    sections.push(text);
    sections.join("\n")
}

pub(crate) fn sanitize_terminal_output_for_model(input: &str) -> String {
    let without_escapes = strip_ansi_escape_sequences(input.as_bytes());
    let text = String::from_utf8_lossy(&without_escapes);
    normalize_terminal_controls(&text)
}

fn strip_echoed_input(output: &str, input: Option<&str>) -> String {
    let Some(input) = input else {
        return output.to_string();
    };
    let echoed = normalize_terminal_controls(input);
    if echoed.is_empty() {
        return output.to_string();
    }
    strip_echoed_input_lines(output, &echoed)
        .or_else(|| output.strip_prefix(&echoed).map(str::to_string))
        .unwrap_or_else(|| output.to_string())
}

fn strip_echoed_input_lines(output: &str, echoed: &str) -> Option<String> {
    let echoed_lines = echoed
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if echoed_lines.is_empty() {
        return None;
    }
    let mut stripped_bytes = 0usize;
    let mut stripped_lines = 0usize;
    for (index, segment) in output.split_inclusive('\n').enumerate() {
        let echoed_line = echoed_lines[index % echoed_lines.len()];
        let output_line = segment.trim_end_matches('\n');
        let stable_prefix = echoed_line
            .split('\\')
            .next()
            .unwrap_or(echoed_line)
            .trim_end();
        let matches_echo = output_line == echoed_line
            || (index == 0 && output_line.trim_start().ends_with(echoed_line))
            || (index == 0
                && stable_prefix.len() >= 3
                && output_line.trim_start().contains(stable_prefix));
        if !matches_echo {
            break;
        }
        stripped_bytes += segment.len();
        stripped_lines += 1;
    }
    if stripped_lines == 0 {
        return None;
    }
    Some(output[stripped_bytes.min(output.len())..].to_string())
}

fn strip_ansi_escape_sequences(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        let byte = input[index];
        if byte != 0x1b {
            output.push(byte);
            index += 1;
            continue;
        }

        index += 1;
        if index >= input.len() {
            break;
        }
        match input[index] {
            b'[' => {
                index += 1;
                while index < input.len() {
                    let byte = input[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            b']' => {
                index += 1;
                while index < input.len() {
                    if input[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if input[index] == 0x1b && index + 1 < input.len() && input[index + 1] == b'\\'
                    {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            b'P' | b'^' | b'_' => {
                index += 1;
                while index + 1 < input.len() {
                    if input[index] == 0x1b && input[index + 1] == b'\\' {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' => {
                index += 2;
            }
            _ => {
                index += 1;
            }
        }
    }
    output
}

fn normalize_terminal_controls(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut line = String::new();
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    output.push_str(&line);
                    output.push('\n');
                }
                line.clear();
            }
            '\u{8}' => {
                line.pop();
            }
            '\n' => {
                output.push_str(&line);
                output.push('\n');
                line.clear();
            }
            '\t' => line.push(character),
            character if character.is_control() => {}
            character => line.push(character),
        }
    }
    output.push_str(&line);
    output
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::AtomicU64,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    static TEMP_ORDINAL: AtomicU64 = AtomicU64::new(0);

    fn temp_root(label: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        let ordinal = TEMP_ORDINAL.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "harness-terminal-{label}-{}-{now}-{ordinal}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp root");
        path
    }

    #[tokio::test]
    async fn terminal_routes_interactive_input_to_the_running_command() {
        let manager = TerminalManager::new();
        let open = manager
            .execute_tool(
                temp_root("interactive"),
                TERMINAL_OPEN_TOOL_NAME.to_string(),
                "command: printf ready; read line; printf ' got:%s\\n' \"$line\"".to_string(),
            )
            .await
            .unwrap();
        assert!(open.contains("Terminal running with ID 1"), "{open}");
        assert!(open.contains("ready"), "{open}");

        let write = manager
            .execute_tool(
                PathBuf::new(),
                TERMINAL_WRITE_TOOL_NAME.to_string(),
                "terminal: 1\ninput: world".to_string(),
            )
            .await
            .unwrap();
        assert!(write.contains("Process exited with code 0"), "{write}");
        assert!(write.contains("got:world"), "{write}");
        assert!(!write.contains("Terminal running with ID"), "{write}");
    }

    #[tokio::test]
    async fn terminal_commands_do_not_share_shell_state() {
        let manager = TerminalManager::new();
        let first = manager
            .execute_tool(
                temp_root("isolated-state"),
                TERMINAL_OPEN_TOOL_NAME.to_string(),
                "command: LEAKED=value; set +u; printf first".to_string(),
            )
            .await
            .unwrap();
        assert!(first.contains("Process exited with code 0"), "{first}");

        let next = manager
            .execute_tool(
                PathBuf::new(),
                TERMINAL_OPEN_TOOL_NAME.to_string(),
                "command: printf 'leaked=%s nounset=%s\\n' \"${LEAKED-unset}\" \"$(case $- in *u*) printf on;; *) printf off;; esac)\"".to_string(),
            )
            .await
            .unwrap();
        assert!(next.contains("leaked=unset nounset=on"), "{next}");
    }

    #[tokio::test]
    async fn terminal_commands_enforce_strict_bash_failures() {
        let manager = TerminalManager::new();
        let output = manager
            .execute_tool(
                temp_root("strict-command"),
                TERMINAL_OPEN_TOOL_NAME.to_string(),
                "command: printf before; false; printf scary".to_string(),
            )
            .await
            .unwrap();
        assert!(output.contains("Process exited with code 1"), "{output}");
        assert!(output.contains("before"), "{output}");
        assert!(!output.contains("scary"), "{output}");

        let pipeline = manager
            .execute_tool(
                PathBuf::new(),
                TERMINAL_OPEN_TOOL_NAME.to_string(),
                "command: true | false; printf scary".to_string(),
            )
            .await
            .unwrap();
        assert!(
            pipeline.contains("Process exited with code 1"),
            "{pipeline}"
        );
        assert!(!pipeline.contains("scary"), "{pipeline}");
    }

    #[tokio::test]
    async fn completed_terminal_cannot_receive_later_input() {
        let manager = TerminalManager::new();
        let open = manager
            .execute_tool(
                temp_root("completed-terminal"),
                TERMINAL_OPEN_TOOL_NAME.to_string(),
                "command: true".to_string(),
            )
            .await
            .unwrap();
        assert!(!open.contains("Terminal running with ID"), "{open}");

        let error = manager
            .execute_tool(
                PathBuf::new(),
                TERMINAL_WRITE_TOOL_NAME.to_string(),
                "terminal: 1\ninput: printf unsafe".to_string(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "terminal 1 does not exist");
    }

    #[tokio::test]
    async fn terminal_read_poll_returns_when_command_exits() {
        let manager = TerminalManager::new();
        let open = manager
            .execute_tool(
                temp_root("read-poll-exit"),
                TERMINAL_OPEN_TOOL_NAME.to_string(),
                "command: sleep 0.7; exit 7".to_string(),
            )
            .await
            .unwrap();
        assert!(open.contains("Terminal running with ID 1"), "{open}");

        let read = manager
            .execute_tool(
                PathBuf::new(),
                TERMINAL_READ_TOOL_NAME.to_string(),
                "terminal: 1\npoll_after: 2s".to_string(),
            )
            .await
            .unwrap();
        assert!(read.contains("Process exited with code 7"), "{read}");
        assert!(!read.contains("Terminal running with ID"), "{read}");
    }

    #[tokio::test]
    async fn terminal_read_poll_ignores_intermediate_output() {
        let manager = TerminalManager::new();
        manager
            .execute_tool(
                temp_root("read-poll-interval"),
                TERMINAL_OPEN_TOOL_NAME.to_string(),
                "command: sleep 0.6; printf tick; sleep 1".to_string(),
            )
            .await
            .unwrap();

        let started = Instant::now();
        let read = manager
            .execute_tool(
                PathBuf::new(),
                TERMINAL_READ_TOOL_NAME.to_string(),
                "terminal: 1\npoll_after: 300ms".to_string(),
            )
            .await
            .unwrap();

        assert!(started.elapsed() >= Duration::from_millis(270));
        assert!(read.contains("tick"), "{read}");
        manager.shutdown();
    }

    #[test]
    fn terminal_input_submission_keeps_control_input_raw() {
        assert_eq!(submitted_terminal_input("\u{3}"), "\u{3}");
        assert_eq!(submitted_terminal_input("printf ok\n"), "printf ok\n");
        assert_eq!(submitted_terminal_input("printf ok"), "printf ok\n");
    }

    #[test]
    fn terminal_output_strips_ansi_sequences_and_normalizes_carriage_returns() {
        let output =
            sanitize_terminal_output_for_model("plain\u{1b}[31m red\u{1b}[0m\r\nnext\rupdate");
        assert_eq!(output, "plain red\nupdate");
    }

    #[test]
    fn terminal_output_strips_charset_selection_without_leaking_final_byte() {
        let output = sanitize_terminal_output_for_model("ok\u{1b}(B");
        assert_eq!(output, "ok");
    }

    #[test]
    fn terminal_output_collapses_progress_carriage_returns_to_latest_line() {
        let output =
            sanitize_terminal_output_for_model("progress 10%\rprogress 20%\rprogress 100%\n");
        assert_eq!(output, "progress 100%\n");
    }

    #[test]
    fn terminal_output_strips_echoed_input_line() {
        let output = strip_echoed_input(
            "printf 'open-ok\\n'\nopen-ok\n",
            Some("printf 'open-ok\\n'\n"),
        );
        assert_eq!(output, "open-ok\n");
    }

    #[tokio::test]
    async fn terminal_open_runs_bash_commands() {
        let manager = TerminalManager::new();
        let open = manager
            .execute_tool(
                PathBuf::new(),
                TERMINAL_OPEN_TOOL_NAME.to_string(),
                "command: printf 'bash=%s\\n' \"$BASH_VERSION\"".to_string(),
            )
            .await
            .unwrap();
        assert!(open.contains("Process exited with code 0"), "{open}");
        assert!(open.contains("bash="), "{open}");
    }

    #[test]
    fn terminal_tools_reject_removed_shell_and_wait_headers() {
        let open_error = parse_terminal_open_args("shell: /bin/sh\ncommand: true").unwrap_err();
        assert_eq!(
            open_error.to_string(),
            "failed to parse `terminal_open` input: unsupported header `shell`"
        );

        let write_error =
            parse_terminal_write_args("terminal: 1\nwait_for: 1s\ninput: true").unwrap_err();
        assert_eq!(
            write_error.to_string(),
            "failed to parse `terminal_write` input: unsupported header `wait_for`"
        );

        let read_error = parse_terminal_read_args("terminal: 1\nwait_for: 1s").unwrap_err();
        assert_eq!(
            read_error.to_string(),
            "failed to parse `terminal_read` input: unsupported header `wait_for`"
        );
    }
}
