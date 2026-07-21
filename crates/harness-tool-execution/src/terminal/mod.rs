//! Shared PTY session state for the terminal subtools.

use std::{collections::HashMap, io::{Read, Write}, path::PathBuf, sync::{Arc, Mutex, OnceLock}, time::Duration};
use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use harness_tool_api::{ToolFailure, ToolPresentation, ToolResult};
use crate::WorkspaceRoot;

mod open;
mod read;
mod write;
pub use open::OpenExecutor;
pub use read::ReadExecutor;
pub use write::WriteExecutor;

pub const OPEN_NAME: &str = "terminal_open";
pub const WRITE_NAME: &str = "terminal_write";
pub const READ_NAME: &str = "terminal_read";
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
const OUTPUT_LIMIT: usize = 1_048_576;

struct Session { child: Box<dyn Child + Send + Sync>, writer: Box<dyn Write + Send>, output: Arc<Mutex<Vec<u8>>> }
struct State { next_id: i32, sessions: HashMap<i32, Arc<Mutex<Session>>> }
#[derive(Clone)] pub(crate) struct Manager { state: Arc<Mutex<State>> }

static MANAGERS: OnceLock<Mutex<HashMap<PathBuf, Manager>>> = OnceLock::new();
pub(crate) fn manager(workspace: &WorkspaceRoot) -> Manager { let all = MANAGERS.get_or_init(|| Mutex::new(HashMap::new())); let mut all = all.lock().expect("terminal manager registry lock"); all.entry(workspace.path().to_owned()).or_insert_with(|| Manager { state: Arc::new(Mutex::new(State { next_id: 1, sessions: HashMap::new() })) }).clone() }

impl Manager {
    pub(crate) fn open(&self, workspace: &WorkspaceRoot, input: &str) -> Result<String, String> {
        let (fields, command) = open_parts(input)?;
                let rows = parse_dimension(fields.get("rows"), "rows", DEFAULT_ROWS)?; let cols = parse_dimension(fields.get("cols"), "cols", DEFAULT_COLS)?;
                        let workdir = fields.get("workdir").map(|value| if value.is_empty() { workspace.path().to_owned() } else { workspace.path().join(value) }).unwrap_or_else(|| workspace.path().to_owned());
        if !workdir.is_dir() { return Err(format!("terminal_open: workdir is not a directory: {}", workdir.display())); }
        let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).map_err(|e| format!("failed to open terminal pty: {e}"))?;
        let mut builder = CommandBuilder::new("/bin/bash"); builder.arg("-lc"); builder.arg(command); builder.cwd(&workdir);
        let child = pair.slave.spawn_command(builder).map_err(|e| format!("failed to spawn terminal command: {e}"))?;
        let writer = pair.master.take_writer().map_err(|e| format!("failed to open terminal input: {e}"))?;
        let mut reader = pair.master.try_clone_reader().map_err(|e| format!("failed to open terminal output: {e}"))?;
        let output = Arc::new(Mutex::new(Vec::new())); let sink = Arc::clone(&output);
        std::thread::spawn(move || { let mut chunk = [0_u8; 8192]; loop { let Ok(size) = reader.read(&mut chunk) else { break }; if size == 0 { break }; let mut output = sink.lock().expect("terminal output lock"); output.extend_from_slice(&chunk[..size]); if output.len() > OUTPUT_LIMIT { let drop = output.len() - OUTPUT_LIMIT; output.drain(..drop); } } });
        drop(pair.slave); drop(pair.master);
        let id = { let mut state = self.state.lock().expect("terminal state lock"); let id = state.next_id; state.next_id += 1; state.sessions.insert(id, Arc::new(Mutex::new(Session { child, writer, output: Arc::clone(&output) }))); id };
        std::thread::sleep(Duration::from_millis(100));
        Ok(format_output(id, &output, None, None))
    }
    pub(crate) fn write(&self, input: &str) -> Result<String, String> { let (fields, value) = write_parts(input)?; let id = parse_id(fields.get("terminal"), WRITE_NAME)?; let session = self.session(id)?; let mut session = session.lock().expect("terminal session lock"); if let Some(status) = session.child.try_wait().map_err(|e| format!("failed to poll terminal {id}: {e}"))? { let output = Arc::clone(&session.output); drop(session); self.remove(id); return Ok(format_output(id, &output, Some(&value), Some(status.exit_code()))); } session.writer.write_all(value.as_bytes()).and_then(|_| session.writer.flush()).map_err(|e| format!("failed to write terminal {id}: {e}"))?; let output = Arc::clone(&session.output); drop(session); std::thread::sleep(Duration::from_millis(100)); Ok(format_output(id, &output, Some(&value), None)) }
    pub(crate) fn read(&self, input: &str) -> Result<String, String> { let fields = read_parts(input)?; let id = parse_id(fields.get("terminal"), READ_NAME)?; let session = self.session(id)?; let mut session = session.lock().expect("terminal session lock"); let poll = fields.get("poll_after").map(|value| parse_duration(value)).transpose()?.unwrap_or(Duration::from_millis(100)); let output = Arc::clone(&session.output); std::thread::sleep(poll); let status = session.child.try_wait().map_err(|e| format!("failed to poll terminal {id}: {e}"))?; drop(session); if status.is_some() { self.remove(id); } Ok(format_output(id, &output, None, status.map(|status| status.exit_code()))) }
    fn session(&self, id: i32) -> Result<Arc<Mutex<Session>>, String> { self.state.lock().expect("terminal state lock").sessions.get(&id).cloned().ok_or_else(|| format!("terminal {id} does not exist")) }
    fn remove(&self, id: i32) { self.state.lock().expect("terminal state lock").sessions.remove(&id); }
}

pub(crate) fn output(text: String, label: &str) -> ToolResult { ToolResult { model_output: text.clone(), presentation: Some(ToolPresentation { label: label.to_owned(), display: Some(text) }), artifacts: Vec::new() } }
pub(crate) fn failure(error: String) -> Result<ToolResult, ToolFailure> { Err(ToolFailure::InvalidInput(error)) }
fn format_output(id: i32, output: &Arc<Mutex<Vec<u8>>>, input: Option<&str>, exit: Option<u32>) -> String { let bytes = output.lock().expect("terminal output lock").clone(); let mut text = String::from_utf8_lossy(&bytes).into_owned(); if let Some(input) = input { text.push_str(&format!("echoed input: {input}")); } let mut result = format!("terminal: {id}\n{text}"); if let Some(exit) = exit { result.push_str(&format!("exit code: {exit}\n")); } result }
fn fields(input: &str, tool: &str) -> Result<HashMap<String, String>, String> { let mut fields = HashMap::new(); let mut current = None; for line in input.lines() { if let Some((key, value)) = line.split_once(':') { let key = key.trim(); if !matches!(key, "workdir" | "rows" | "cols" | "command" | "terminal" | "input" | "poll_after") { return Err(format!("failed to parse `{tool}` input: unknown field `{key}`")); } current = Some(key.to_owned()); fields.insert(key.to_owned(), value.trim_start().to_owned()); } else if let Some(key) = current.as_ref() { fields.entry(key.clone()).and_modify(|value| { value.push('\n'); value.push_str(line); }); } else if !line.trim().is_empty() { return Err(format!("failed to parse `{tool}` input: expected `key: value`")); } } Ok(fields) }
fn parse_dimension(value: Option<&String>, name: &str, default: u16) -> Result<u16, String> { value.map(|value| value.parse::<u16>().map_err(|_| format!("terminal_open: {name} must be a positive integer")).and_then(|value| if value == 0 { Err(format!("terminal_open: {name} must be a positive integer")) } else { Ok(value) })).transpose().map(|value| value.unwrap_or(default)) }
fn parse_id(value: Option<&String>, tool: &str) -> Result<i32, String> { value.ok_or_else(|| format!("failed to parse `{tool}` input: terminal is required"))?.parse().map_err(|_| format!("failed to parse `{tool}` input: terminal must be an integer")) }


fn open_parts(input: &str) -> Result<(HashMap<String, String>, String), String> {
    let marker = "command:";
    let Some(index) = input.find(marker) else { return Err("failed to parse `terminal_open` input: command is required".into()); };
    let fields = header_fields(&input[..index], OPEN_NAME)?;
    let command = input[index + marker.len()..].trim_start_matches([' ', '\n', '\r']).to_string();
    if command.trim().is_empty() { return Err("failed to parse `terminal_open` input: command is required".into()); }
    Ok((fields, command))
}

fn write_parts(input: &str) -> Result<(HashMap<String, String>, String), String> {
    let marker = "input:";
    let Some(index) = input.find(marker) else { return Err("failed to parse `terminal_write` input: input is required".into()); };
    let fields = header_fields(&input[..index], WRITE_NAME)?;
    let value = input[index + marker.len()..].trim_start_matches([' ', '\n', '\r']).to_string();
    Ok((fields, value))
}

fn read_parts(input: &str) -> Result<HashMap<String, String>, String> { header_fields(input, READ_NAME) }

fn header_fields(input: &str, tool: &str) -> Result<HashMap<String, String>, String> {
    let mut fields = HashMap::new();
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        let Some((key, value)) = line.split_once(':') else { return Err(format!("failed to parse `{tool}` input: expected `key: value`")); };
        let key = key.trim();
        if !matches!(key, "workdir" | "rows" | "cols" | "terminal" | "poll_after") { return Err(format!("failed to parse `{tool}` input: unknown field `{key}`")); }
        if fields.insert(key.to_string(), value.trim().to_string()).is_some() { return Err(format!("failed to parse `{tool}` input: duplicate field `{key}`")); }
    }
    Ok(fields)
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") { (value, 1) }
        else if let Some(value) = value.strip_suffix('s') { (value, 1_000) }
        else if let Some(value) = value.strip_suffix('m') { (value, 60_000) }
        else { (value, 1) };
    let number = number.parse::<u64>().map_err(|_| "poll_after must be a duration such as 250ms or 30s".to_string())?;
    Ok(Duration::from_millis(number.saturating_mul(multiplier)))
}
