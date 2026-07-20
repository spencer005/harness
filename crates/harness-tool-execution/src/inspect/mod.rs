//! Capability-rooted workspace inspection.

use std::{fs, path::PathBuf, pin::Pin, sync::Arc};

use harness_tool_api::{
    InvalidToolName, ToolCapabilities, ToolExecutionRequest, ToolExecutor, ToolFailure, ToolInput,
    ToolPresentation, ToolResult, ToolSpec,
};

use crate::WorkspaceRoot;

mod bytes;
mod cargo;
mod elf;
mod list;
mod process;
mod read;
mod search;
mod shell;
mod stat;
mod strings;
mod which;
pub use read::{format_read_display, format_read_output};
pub use shell::{ShellWord, parse_shell_words};

pub const NAME: &str = "inspect";
pub const DESCRIPTION: &str = "Batch compact inspection jobs. Commands: read, list (alias: ls), stat, bytes, byte-search, strings, elf, search, which, check, test, ps, and pwd. Read output includes stable line anchors for edit_file.";
pub const LARK_GRAMMAR: &str = "start: command\ncommand: /(?s).+/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectReadOutputRequest {
    pub path: String,
    pub start_line: usize,
    pub line_count: usize,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectReadNextRecord {
    pub start_line: usize,
    pub line_count: usize,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectReadDisplayRecord {
    pub path: String,
    pub start_line: usize,
    pub lines: Vec<String>,
    pub next: Option<InspectReadNextRecord>,
}

pub fn edit_line_hash(line: &str) -> u8 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in line.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    (hash & 0xff) as u8
}

pub fn line_anchor_word(hash: u8) -> &'static str {
    static WORDS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    WORDS.get_or_init(|| {
        include_str!("../../../../o200k_anchor_candidates.txt")
            .lines()
            .filter_map(|line| {
                let (_, value) = line.split_once("\": \"")?;
                value
                    .strip_suffix("\",")
                    .or_else(|| value.strip_suffix("\"}"))
            })
            .collect()
    })[usize::from(hash)]
}

pub fn spec() -> Result<ToolSpec, InvalidToolName> {
    Ok(ToolSpec::new(NAME)?
        .description(DESCRIPTION)
        .lark(LARK_GRAMMAR)
        .capabilities(ToolCapabilities {
            read_only: true,
            mutates_workspace: false,
            idempotent: true,
        }))
}

pub struct Executor {
    workspace: WorkspaceRoot,
}
impl Executor {
    pub fn new(workspace: WorkspaceRoot) -> Self {
        Self { workspace }
    }
}

::inventory::submit! {
    crate::inventory::ToolRegistration {
        spec,
        executor: |workspace| Arc::new(Executor::new(workspace)),
    }
}
impl ToolExecutor for Executor {
    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>>
    {
        let result = if request.tool.as_str() != NAME || request.route.identifier != NAME {
            Err(ToolFailure::Execution(
                "executor route does not match `inspect`".into(),
            ))
        } else {
            execute(&self.workspace, &request.input)
        };
        Box::pin(std::future::ready(result))
    }
}

pub fn execute(workspace: &WorkspaceRoot, input: &ToolInput) -> Result<ToolResult, ToolFailure> {
    let lines = input.as_str().lines().collect::<Vec<_>>();
    if lines.iter().all(|line| line.trim().is_empty()) {
        return Err(ToolFailure::InvalidInput(
            "failed to parse `inspect` input: at least one command is required".into(),
        ));
    }
    let mut output_text = String::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        if line.is_empty() {
            index += 1;
            continue;
        }
        if let Some(header) = line.strip_prefix("read ") {
            let mut header_words = parse_shell_words(header).map_err(|e| {
                ToolFailure::InvalidInput(format!("failed to parse `inspect` read input: {e}"))
            })?;
            if header_words.is_empty() {
                return Err(ToolFailure::InvalidInput(
                    "failed to parse `inspect` input: read path is required".into(),
                ));
            }
            let path = header_words.remove(0);
            let mut ranges = header_words
                .into_iter()
                .map(|word| parse_range(&word.value))
                .collect::<Result<Vec<_>, _>>()
                .map_err(ToolFailure::InvalidInput)?;
            index += 1;
            while index < lines.len() {
                let candidate = lines[index].trim();
                if candidate.is_empty() {
                    index += 1;
                    continue;
                }
                if command_start(candidate) {
                    break;
                }
                ranges.push(parse_range(candidate).map_err(ToolFailure::InvalidInput)?);
                index += 1;
            }
            if ranges.is_empty() {
                return Err(ToolFailure::InvalidInput(format!(
                    "failed to parse `inspect` input: read `{}` needs at least one range",
                    path.value
                )));
            }
            for (start, count) in ranges {
                output_text.push_str(
                    &read_file(
                        workspace,
                        &[
                            path.clone(),
                            ShellWord {
                                value: start.to_string(),
                                quoted: false,
                            },
                            ShellWord {
                                value: count.to_string(),
                                quoted: false,
                            },
                        ],
                    )
                    .map_err(ToolFailure::InvalidInput)?,
                );
            }
            continue;
        }
        let words = parse_shell_words(line).map_err(|e| {
            ToolFailure::InvalidInput(format!("failed to parse `inspect` input: {e}"))
        })?;
        output_text
            .push_str(&execute_command(workspace, &words).map_err(ToolFailure::InvalidInput)?);
        index += 1;
    }
    Ok(output(output_text, "inspect"))
}

fn execute_command(workspace: &WorkspaceRoot, words: &[ShellWord]) -> Result<String, String> {
    let Some(command) = words.first().map(|word| word.value.as_str()) else {
        return Ok(String::new());
    };
    match command {
        "pwd" if words.len() == 1 => Ok(format!("{}\n", workspace.path().display())),
        "list" | "ls" => list::execute(workspace, &words[1..]),
        "stat" => stat::execute(workspace, &words[1..]),
        "bytes" => bytes::execute(workspace, &words[1..]),
        "byte-search" => bytes::search(workspace, &words[1..]),
        "strings" => strings::execute(workspace, &words[1..]),
        "elf" => elf::execute(workspace, &words[1..]),
        "search" => search::execute(workspace, &words[1..]),
        "which" => which::execute(&words[1..]),
        "check" => cargo::check(workspace, &words[1..]),
        "test" => cargo::test(workspace, &words[1..]),
        "ps" => process::execute(&words[1..]),
        other => Err(format!(
            "failed to parse `inspect` input: unsupported command `{other}`"
        )),
    }
}

fn command_start(line: &str) -> bool {
    let command = line.split_whitespace().next().unwrap_or_default();
    matches!(
        command,
        "pwd"
            | "list"
            | "ls"
            | "stat"
            | "bytes"
            | "byte-search"
            | "strings"
            | "elf"
            | "search"
            | "which"
            | "check"
            | "test"
            | "ps"
            | "read"
    )
}

fn parse_range(value: &str) -> Result<(usize, usize), String> {
    if let Some((start, count)) = value.split_once('+') {
        let start = shell::parse_positive_usize_value(start).map_err(|_| {
            "failed to parse `inspect` input: range start must be a positive integer".to_string()
        })?;
        let count = shell::parse_positive_usize_value(count).map_err(|_| {
            "failed to parse `inspect` input: range count must be a positive integer".to_string()
        })?;
        return Ok((start, count));
    }
    if let Some((start, end)) = value.split_once('-') {
        let start = shell::parse_positive_usize_value(start).map_err(|_| {
            "failed to parse `inspect` input: range start must be a positive integer".to_string()
        })?;
        let end = shell::parse_positive_usize_value(end).map_err(|_| {
            "failed to parse `inspect` input: range end must be a positive integer".to_string()
        })?;
        if end < start {
            return Err("failed to parse `inspect` input: range end must be >= start".into());
        }
        return Ok((start, end - start + 1));
    }
    Err("failed to parse `inspect` input: range must be `start+count` or `start-end`".into())
}

fn output(text: impl Into<String>, label: &str) -> ToolResult {
    let text = text.into();
    ToolResult {
        model_output: text.clone(),
        presentation: Some(ToolPresentation {
            label: label.into(),
            display: Some(text),
        }),
        artifacts: Vec::new(),
    }
}
fn resolve(workspace: &WorkspaceRoot, value: &str) -> Result<(String, PathBuf), String> {
    let relative = workspace
        .relative_path(value)
        .map_err(|e| format!("inspect: invalid path `{value}`: {e}"))?;
    Ok((
        relative.as_str().to_owned(),
        workspace.path().join(relative.as_str()),
    ))
}
fn positive(value: &str, command: &str) -> Result<usize, String> {
    value
        .parse()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| format!("inspect {command}: expected a positive integer"))
}
fn read_file(workspace: &WorkspaceRoot, args: &[ShellWord]) -> Result<String, String> {
    if !(1..=3).contains(&args.len()) {
        return Err("inspect read: expected PATH [START_LINE] [LINE_COUNT]".into());
    }
    let (name, file) = resolve(workspace, &args[0].value)?;
    let start = args
        .get(1)
        .map(|v| positive(&v.value, "read"))
        .transpose()?
        .unwrap_or(1);
    let count = args
        .get(2)
        .map(|v| positive(&v.value, "read"))
        .transpose()?
        .unwrap_or(200)
        .min(400);
    let text = String::from_utf8(fs::read(&file).map_err(|e| format!("inspect read {name}: {e}"))?)
        .map_err(|_| format!("inspect read {name}: file is not UTF-8; use `bytes`"))?;
    Ok(format_read_output(
        &InspectReadOutputRequest {
            path: name,
            start_line: start,
            line_count: count,
        },
        &text,
    ))
}
