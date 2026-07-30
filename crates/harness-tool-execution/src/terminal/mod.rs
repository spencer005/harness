//! Shared PTY session state for the terminal subtools.

use std::{
    collections::HashMap,
    io::{Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use harness_tool_api::{
    TerminalOpenRequest, TerminalProcessState, TerminalReadRequest, TerminalResult,
    TerminalWriteRequest, ToolExecutionFailure, ToolFailureCategory, ToolOutcome, ToolResult,
};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};

use crate::WorkspaceRoot;

mod open;
mod read;
mod write;
mod screen;
pub use open::OpenExecutor;
pub use read::ReadExecutor;
pub use write::WriteExecutor;
pub use open::spec as open_spec;
pub use read::spec as read_spec;
pub use write::spec as write_spec;

pub const OPEN_NAME: &str = "terminal_open";
pub const WRITE_NAME: &str = "terminal_write";
pub const READ_NAME: &str = "terminal_read";
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
const OUTPUT_LIMIT: usize = 1_048_576;
const DEFAULT_POLL_AFTER: Duration = Duration::from_secs(8);
const POLL_AFTER_GRACE: Duration = Duration::from_millis(250);
#[derive(Default)]
struct TerminalBuffer {
    bytes: Vec<u8>,
    first_offset: u64,
    next_offset: u64,
    model_cursor: u64,
    model_snapshot: String,
    model_first_offset: u64,
}

impl TerminalBuffer {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        self.next_offset = self.next_offset.saturating_add(chunk.len() as u64);
        if self.bytes.len() > OUTPUT_LIMIT {
            let dropped = self.bytes.len() - OUTPUT_LIMIT;
            self.bytes.drain(..dropped);
            self.first_offset = self.first_offset.saturating_add(dropped as u64);
        }
    }

    fn consume_model_delta(&mut self) -> (String, u64) {
        let omitted = self.first_offset.saturating_sub(self.model_cursor);
        let rendered = screen::render(&self.bytes);
        let reset = self.first_offset != self.model_first_offset;
        let delta = screen::delta(&self.model_snapshot, &rendered, reset);
        self.model_snapshot = rendered;
        self.model_first_offset = self.first_offset;
        self.model_cursor = self.next_offset;
        (delta, omitted)
    }
}

pub(crate) struct TerminalCommandOutput {
    pub(crate) model: String,
    pub(crate) result: TerminalResult,
}

struct Session {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<TerminalBuffer>>,
}
struct State {
    next_id: i32,
    sessions: HashMap<i32, Arc<Mutex<Session>>>,
}
#[derive(Clone)]
pub(crate) struct Manager {
    state: Arc<Mutex<State>>,
}

static MANAGERS: OnceLock<Mutex<HashMap<PathBuf, Manager>>> = OnceLock::new();
pub(crate) fn manager(workspace: &WorkspaceRoot) -> Manager {
    let all = MANAGERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut all = all.lock().expect("terminal manager registry lock");
    all.entry(workspace.path().to_owned())
        .or_insert_with(|| Manager {
            state: Arc::new(Mutex::new(State {
                next_id: 1,
                sessions: HashMap::new(),
            })),
        })
        .clone()
}

impl Manager {
    pub(crate) fn open(
        &self,
        workspace: &WorkspaceRoot,
        request: &TerminalOpenRequest,
    ) -> Result<TerminalCommandOutput, String> {
        let workdir = request
            .workdir
            .as_deref()
            .map(|value| workspace.path().join(value))
            .unwrap_or_else(|| workspace.path().to_owned());
        if !workdir.is_dir() {
            return Err(format!(
                "terminal_open: workdir is not a directory: {}",
                workdir.display()
            ));
        }
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: request.rows,
                cols: request.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("failed to open terminal pty: {e}"))?;
        let mut builder = CommandBuilder::new("/bin/bash");
        builder.arg("-lc");
        builder.arg(&request.command);
        builder.cwd(&workdir);
        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| format!("failed to spawn terminal command: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("failed to open terminal input: {e}"))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("failed to open terminal output: {e}"))?;
        let output = Arc::new(Mutex::new(TerminalBuffer::default()));
        let sink = Arc::clone(&output);
        std::thread::spawn(move || {
            let mut chunk = [0_u8; 8192];
            loop {
                let Ok(size) = reader.read(&mut chunk) else {
                    break;
                };
                if size == 0 {
                    break;
                };
                let mut output = sink.lock().expect("terminal output lock");
                output.push(&chunk[..size]);
            }
        });
        drop(pair.slave);
        drop(pair.master);
        let id = {
            let mut state = self.state.lock().expect("terminal state lock");
            let id = state.next_id;
            state.next_id += 1;
            state.sessions.insert(
                id,
                Arc::new(Mutex::new(Session {
                    child,
                    writer,
                    output: Arc::clone(&output),
                })),
            );
            id
        };
        std::thread::sleep(Duration::from_millis(100));
        Ok(format_output(id, &output, None, None))
    }
    pub(crate) fn write(
        &self,
        request: &TerminalWriteRequest,
    ) -> Result<TerminalCommandOutput, String> {
        let id = request.terminal_id;
        let session = self.session(id)?;
        let mut session = session.lock().expect("terminal session lock");
        if let Some(status) = session
            .child
            .try_wait()
            .map_err(|e| format!("failed to poll terminal {id}: {e}"))?
        {
            let output = Arc::clone(&session.output);
            drop(session);
            self.remove(id);
            return Ok(format_output(
                id,
                &output,
                Some(&request.input),
                Some(status.exit_code()),
            ));
        }
        session
            .writer
            .write_all(request.input.as_bytes())
            .and_then(|_| session.writer.flush())
            .map_err(|e| format!("failed to write terminal {id}: {e}"))?;
        let output = Arc::clone(&session.output);
        drop(session);
        std::thread::sleep(Duration::from_millis(100));
        Ok(format_output(id, &output, Some(&request.input), None))
    }
    pub(crate) fn read(
        &self,
        request: &TerminalReadRequest,
    ) -> Result<TerminalCommandOutput, String> {
        let id = request.terminal_id;
        let session = self.session(id)?;
        let mut session = session.lock().expect("terminal session lock");
        let output = Arc::clone(&session.output);
        std::thread::sleep(Duration::from_millis(request.poll_after_ms));
        let status = session
            .child
            .try_wait()
            .map_err(|e| format!("failed to poll terminal {id}: {e}"))?;
        drop(session);
        if status.is_some() {
            self.remove(id);
        }
        Ok(format_output(
            id,
            &output,
            None,
            status.map(|status| status.exit_code()),
        ))
    }
    fn session(&self, id: i32) -> Result<Arc<Mutex<Session>>, String> {
        self.state
            .lock()
            .expect("terminal state lock")
            .sessions
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("terminal {id} does not exist"))
    }
    fn remove(&self, id: i32) {
        self.state
            .lock()
            .expect("terminal state lock")
            .sessions
            .remove(&id);
    }
}

pub(crate) fn rejected_result(message: String) -> ToolResult {
    ToolResult {
        model_output: message.clone(),
        outcome: ToolOutcome::Failed(ToolExecutionFailure {
            category: ToolFailureCategory::InvalidInput,
            message,
        }),
    }
}
fn format_output(
    id: i32,
    output: &Arc<Mutex<TerminalBuffer>>,
    input: Option<&str>,
    exit: Option<u32>,
) -> TerminalCommandOutput {
    let (delta, unread_omitted, retained, historical_omitted) = {
        let mut output = output.lock().expect("terminal output lock");
        let (delta, unread_omitted) = output.consume_model_delta();
        (
            delta,
            unread_omitted,
            output.bytes.clone(),
            output.first_offset,
        )
    };

    let mut model = format!("terminal: {id}\n");
    if unread_omitted > 0 {
        model.push_str(&format!(
            "[terminal output truncated: {unread_omitted} unread bytes omitted]\n"
        ));
    }
    model.push_str(&delta);
    append_command_status(&mut model, input, exit);

    TerminalCommandOutput {
        model,
        result: TerminalResult {
            terminal_id: id,
            output: screen::render(&retained),
            earlier_output_omitted: historical_omitted,
            state: exit.map_or(TerminalProcessState::Running, |code| {
                TerminalProcessState::Exited { code }
            }),
        },
    }
}

fn append_command_status(output: &mut String, input: Option<&str>, exit: Option<u32>) {
    if let Some(input) = input {
        output.push_str(&format!("echoed input: {input}"));
    }
    if let Some(exit) = exit {
        output.push_str(&format!("exit code: {exit}\n"));
    }
}
fn parse_dimension(value: Option<&String>, name: &str, default: u16) -> Result<u16, String> {
    value
        .map(|value| {
            value
                .parse::<u16>()
                .map_err(|_| format!("terminal_open: {name} must be a positive integer"))
                .and_then(|value| {
                    if value == 0 {
                        Err(format!("terminal_open: {name} must be a positive integer"))
                    } else {
                        Ok(value)
                    }
                })
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}
fn parse_id(value: Option<&String>, tool: &str) -> Result<i32, String> {
    value
        .ok_or_else(|| format!("failed to parse `{tool}` input: terminal is required"))?
        .parse()
        .map_err(|_| format!("failed to parse `{tool}` input: terminal must be an integer"))
}

pub(crate) fn prepare_open(input: &str) -> Result<TerminalOpenRequest, String> {
    let marker = "command:";
    let Some(index) = input.find(marker) else {
        return Err("failed to parse `terminal_open` input: command is required".into());
    };
    let fields = header_fields(&input[..index], OPEN_NAME)?;
    let command = input[index + marker.len()..]
        .trim_start_matches([' ', '\n', '\r'])
        .to_string();
    if command.trim().is_empty() {
        return Err("failed to parse `terminal_open` input: command is required".into());
    }
    Ok(TerminalOpenRequest {
        command,
        workdir: fields.get("workdir").cloned().filter(|value| !value.is_empty()),
        rows: parse_dimension(fields.get("rows"), "rows", DEFAULT_ROWS)?,
        cols: parse_dimension(fields.get("cols"), "cols", DEFAULT_COLS)?,
    })
}

pub(crate) fn prepare_write(input: &str) -> Result<TerminalWriteRequest, String> {
    let marker = "input:";
    let Some(index) = input.find(marker) else {
        return Err("failed to parse `terminal_write` input: input is required".into());
    };
    let fields = header_fields(&input[..index], WRITE_NAME)?;
    let value = input[index + marker.len()..]
        .trim_start_matches([' ', '\n', '\r'])
        .to_string();
    Ok(TerminalWriteRequest {
        terminal_id: parse_id(fields.get("terminal"), WRITE_NAME)?,
        input: value,
    })
}

pub(crate) fn prepare_read(input: &str) -> Result<TerminalReadRequest, String> {
    let fields = header_fields(input, READ_NAME)?;
    let poll = fields
        .get("poll_after")
        .map(|value| parse_poll_after(value))
        .transpose()?
        .unwrap_or(DEFAULT_POLL_AFTER);
    Ok(TerminalReadRequest {
        terminal_id: parse_id(fields.get("terminal"), READ_NAME)?,
        poll_after_ms: poll.as_millis().min(u128::from(u64::MAX)) as u64,
    })
}

fn header_fields(input: &str, tool: &str) -> Result<HashMap<String, String>, String> {
    let mut fields = HashMap::new();
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!(
                "failed to parse `{tool}` input: expected `key: value`"
            ));
        };
        let key = key.trim();
        if !matches!(key, "workdir" | "rows" | "cols" | "terminal" | "poll_after") {
            return Err(format!(
                "failed to parse `{tool}` input: unknown field `{key}`"
            ));
        }
        if fields
            .insert(key.to_string(), value.trim().to_string())
            .is_some()
        {
            return Err(format!(
                "failed to parse `{tool}` input: duplicate field `{key}`"
            ));
        }
    }
    Ok(fields)
}

fn parse_poll_after(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60_000)
    } else {
        (value, 1)
    };
    let number = number
        .parse::<u64>()
        .map_err(|_| "poll_after must be a duration such as 250ms or 30s".to_string())?;
    let requested = Duration::from_millis(number.saturating_mul(multiplier));
    Ok(requested
        .max(DEFAULT_POLL_AFTER)
        .saturating_add(POLL_AFTER_GRACE))
}
