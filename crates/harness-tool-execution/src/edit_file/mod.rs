//! Anchor-based workspace file editing.
//!
//! The tool accepts the native freeform `edit_file` format. Parsing is kept
//! separate from filesystem mutation so every operation can be validated and
//! planned before the file is written.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    fs::{self, File, OpenOptions},
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use harness_tool_api::{
    InvalidToolName, ToolCapabilities, ToolExecutionRequest, ToolExecutor, ToolFailure, ToolInput,
    ToolPresentation, ToolResult, ToolSpec,
};

use crate::WorkspaceRoot;

/// Stable name advertised to the model.
pub const NAME: &str = "edit_file";

/// Model-facing description of the native edit format.
pub const DESCRIPTION: &str = "Edit files using line anchors from inspect read output. Use raw lines, not JSON. Use section headers: `§ Edit <path>`, `§ Add <path>`, `§ Remove <path>`, and `§ Move <old_path>` followed by `§ To <new_path>`. Inside `§ Edit`, use segment headers: `§ Replace <start_anchor> <end_anchor>`, `§ Delete <start_anchor> <end_anchor>`, `§ Before <anchor>`, `§ After <anchor>`, and `§ Append <last_line_anchor>`. A segment body continues until the next `§` header or the end of input. Escape every literal `§` in a body as `\\§`; the escape is removed from written content. Anchors use a positive line number followed by one vocabulary word, such as `24 bucket`. Replace and delete ranges are inclusive. `***` patch delimiters are invalid. Anchors refer to the file state before this edit; re-read after mutating segments in the same call.";

/// Lark grammar advertised for the native freeform tool.
pub const LARK_GRAMMAR: &str = include_str!("edit_file.lark");

const TEMP_FILE_ATTEMPTS: usize = 8;
static NEXT_TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

/// Creates the provider-facing registration for this tool.
pub fn spec() -> Result<ToolSpec, InvalidToolName> {
    Ok(ToolSpec::new(NAME)?
        .description(DESCRIPTION)
        .lark(LARK_GRAMMAR)
        .capabilities(ToolCapabilities {
            read_only: false,
            mutates_workspace: true,
            idempotent: false,
        }))
}

/// Executor bound to one workspace capability.
pub struct Executor {
    workspace: WorkspaceRoot,
}

impl Executor {
    /// Creates an executor that can edit only files below `workspace`.
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
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>> {
        let result = if request.tool.as_str() != NAME || request.route.identifier != NAME {
            Err(ToolFailure::Execution(format!(
                "executor route does not match `{NAME}` for tool {}",
                request.tool.as_str()
            )))
        } else {
            execute(&self.workspace, &request.input)
        };
        Box::pin(std::future::ready(result))
    }
}

/// Parsed collection of file operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Operations are applied in the order they appear in the input.
    pub operations: Vec<Operation>,
}

/// One file operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Creates a new file and its missing parent directories.
    Add { path: String, body: String },
    /// Removes a file, symlink, or explicitly marked directory.
    Remove { path: String },
    /// Moves a file or directory to another workspace-relative path.
    Move { from: String, to: String },
    /// Applies one or more anchor-based edits to an existing text file.
    Edit {
        path: String,
        segments: Vec<Segment>,
    },
}

/// One anchor-based edit within an existing file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Replaces an inclusive range of lines.
    Replace {
        start: LineAnchor,
        end: LineAnchor,
        body: String,
    },
    /// Deletes an inclusive range of lines.
    Delete { start: LineAnchor, end: LineAnchor },
    /// Inserts text relative to one anchor.
    Insert {
        position: InsertPosition,
        anchor: LineAnchor,
        body: String,
    },
}

/// Position used by an insertion segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertPosition {
    /// Insert before the anchor line.
    Before,
    /// Insert after the anchor line.
    After,
    /// Insert after the current final line.
    Append,
}

/// Line number and vocabulary word identifying one source line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineAnchor {
    /// One-based source line number.
    pub line_number: usize,
    /// Compact vocabulary identifier for the source line.
    pub hash: u8,
}

/// Parses and executes one `edit_file` request.
pub fn execute(workspace: &WorkspaceRoot, input: &ToolInput) -> Result<ToolResult, ToolFailure> {
    let request = match parse_input(input.as_str()) {
        Ok(request) => request,
        Err(message) => return Ok(output(message.clone(), message)),
    };

    match apply_request(workspace, &request) {
        Ok(result) => Ok(output(result.model_output, result.display_output)),
        Err(message) => Ok(output(message.clone(), message)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppliedRequest {
    model_output: String,
    display_output: String,
}

fn output(model_output: String, display_output: String) -> ToolResult {
    ToolResult {
        model_output,
        presentation: Some(ToolPresentation {
            label: NAME.to_string(),
            display: Some(display_output),
        }),
        artifacts: Vec::new(),
    }
}

fn apply_request(workspace: &WorkspaceRoot, request: &Request) -> Result<AppliedRequest, String> {
    let mut errors = Vec::new();

    for (index, operation) in request.operations.iter().enumerate() {
        if let Err(message) = apply_operation(workspace, operation) {
            errors.push(format!("{} {}", index + 1, compact_error(&message)));
        }
    }

    if errors.is_empty() {
        return Ok(AppliedRequest {
            model_output: "ok".to_string(),
            display_output: "ok".to_string(),
        });
    }

    let mut model_output = String::from("edit errors\n");
    model_output.push_str(&errors.join("\n"));
    if !model_output.ends_with('\n') {
        model_output.push('\n');
    }
    Ok(AppliedRequest {
        model_output: model_output.clone(),
        display_output: model_output,
    })
}

fn compact_error(message: &str) -> String {
    message
        .strip_prefix("failed to edit ")
        .unwrap_or(message)
        .to_string()
}

fn apply_operation(workspace: &WorkspaceRoot, operation: &Operation) -> Result<(), String> {
    match operation {
        Operation::Add { path, body } => {
            reject_git_path(path)?;
            apply_add(workspace, path, body)
        }
        Operation::Remove { path } => {
            reject_git_path(path)?;
            apply_remove(workspace, path)
        }
        Operation::Move { from, to } => {
            reject_git_path(from)?;
            reject_git_path(to)?;
            apply_move(workspace, from, to)
        }
        Operation::Edit { path, segments } => {
            reject_git_path(path)?;
            apply_segments(workspace, path, segments)
        }
    }
}

fn reject_git_path(path: &str) -> Result<(), String> {
    if Path::new(path).components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(name) if name == ".git"
        )
    }) {
        return Err(format!(
            "failed to edit {path}: paths inside `.git` are sandboxed and cannot be modified"
        ));
    }
    Ok(())
}

fn resolve_path(workspace: &WorkspaceRoot, path: &str) -> Result<PathBuf, String> {
    let relative = workspace
        .relative_path(path)
        .map_err(|error| format!("failed to edit {path}: {error}"))?;
    Ok(workspace.path().join(relative.as_str()))
}

fn temporary_path(target: &Path) -> PathBuf {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let id = NEXT_TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        ".{name}.harness-edit-{}-{id}.tmp",
        std::process::id()
    ))
}

fn create_temporary_file(target: &Path) -> Result<(PathBuf, File), String> {
    for _attempt in 0..TEMP_FILE_ATTEMPTS {
        let temporary = temporary_path(target);
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "failed to create temporary file for {}: {error}",
                    target.display()
                ));
            }
        }
    }
    Err(format!(
        "failed to create a unique temporary file for {}",
        target.display()
    ))
}

fn cleanup_temporary(path: &Path, message: String) -> String {
    match fs::remove_file(path) {
        Ok(()) => message,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => message,
        Err(error) => format!("{message}; temporary cleanup failed: {error}"),
    }
}

fn write_and_sync(
    temporary: &Path,
    mut file: File,
    contents: &[u8],
    existing: Option<&Path>,
) -> Result<(), String> {
    let result = (|| {
        if let Some(existing) = existing {
            copy_permissions(existing, &file)?;
        }
        file.write_all(contents)
            .map_err(|error| format!("failed to write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| format!("failed to sync {}: {error}", temporary.display()))?;
        Ok(())
    })();
    result.map_err(|message| cleanup_temporary(temporary, message))
}

#[cfg(unix)]
fn copy_permissions(existing: &Path, file: &File) -> Result<(), String> {
    let mode = fs::metadata(existing)
        .map_err(|error| format!("failed to read {} permissions: {error}", existing.display()))?
        .permissions()
        .mode();
    let mut permissions = file
        .metadata()
        .map_err(|error| format!("failed to read temporary file permissions: {error}"))?
        .permissions();
    permissions.set_mode(mode);
    file.set_permissions(permissions).map_err(|error| {
        format!(
            "failed to preserve {} permissions: {error}",
            existing.display()
        )
    })
}

#[cfg(not(unix))]
fn copy_permissions(_existing: &Path, _file: &File) -> Result<(), String> {
    Ok(())
}

fn sync_directory(directory: &Path) -> Result<(), String> {
    let handle = OpenOptions::new()
        .read(true)
        .open(directory)
        .map_err(|error| {
            format!(
                "failed to open {} for synchronization: {error}",
                directory.display()
            )
        })?;
    handle
        .sync_all()
        .map_err(|error| format!("failed to synchronize {}: {error}", directory.display()))
}

fn atomic_replace(target: &Path, contents: &[u8]) -> Result<(), String> {
    let (temporary, file) = create_temporary_file(target)?;
    write_and_sync(&temporary, file, contents, Some(target))?;
    if let Err(error) = fs::rename(&temporary, target) {
        return Err(cleanup_temporary(
            &temporary,
            format!("failed to commit {}: {error}", target.display()),
        ));
    }
    sync_directory(target.parent().ok_or_else(|| {
        format!(
            "failed to commit {}: target has no parent",
            target.display()
        )
    })?)
}

fn apply_add(workspace: &WorkspaceRoot, path: &str, body: &str) -> Result<(), String> {
    let resolved = resolve_path(workspace, path)?;
    if resolved.exists() {
        return Err(format!("failed to edit {path}: file already exists"));
    }
    if let Some(parent) = resolved.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let (temporary, file) = create_temporary_file(&resolved)?;
    write_and_sync(&temporary, file, body.as_bytes(), None)?;
    if let Err(error) = fs::rename(&temporary, &resolved) {
        return Err(cleanup_temporary(
            &temporary,
            format!("failed to commit {}: {error}", resolved.display()),
        ));
    }
    sync_directory(resolved.parent().ok_or_else(|| {
        format!(
            "failed to commit {}: target has no parent",
            resolved.display()
        )
    })?)
}

fn apply_remove(workspace: &WorkspaceRoot, path: &str) -> Result<(), String> {
    let resolved = resolve_path(workspace, path)?;
    let metadata = fs::symlink_metadata(&resolved)
        .map_err(|error| format!("failed to read {}: {error}", resolved.display()))?;
    let parent = resolved.parent().ok_or_else(|| {
        format!(
            "failed to remove {}: target has no parent",
            resolved.display()
        )
    })?;

    if metadata.is_dir() {
        if !path.ends_with('/') {
            return Err(format!(
                "failed to edit {path}: directory removal requires a trailing `/`"
            ));
        }
        let tombstone = temporary_path(&resolved);
        fs::rename(&resolved, &tombstone).map_err(|error| {
            format!(
                "failed to stage directory removal {}: {error}",
                resolved.display()
            )
        })?;
        if let Err(error) = sync_directory(parent) {
            let rollback = fs::rename(&tombstone, &resolved);
            return Err(match rollback {
                Ok(()) => format!("failed to commit directory removal: {error}"),
                Err(rollback_error) => format!(
                    "failed to commit directory removal: {error}; rollback failed: {rollback_error}"
                ),
            });
        }
        fs::remove_dir_all(&tombstone).map_err(|error| {
            format!(
                "directory removal committed but cleanup failed for {}: {error}",
                tombstone.display()
            )
        })?;
        sync_directory(parent)
    } else {
        fs::remove_file(&resolved)
            .map_err(|error| format!("failed to remove {}: {error}", resolved.display()))?;
        sync_directory(parent)
    }
}

fn apply_move(workspace: &WorkspaceRoot, from: &str, to: &str) -> Result<(), String> {
    let resolved_from = resolve_path(workspace, from)?;
    let resolved_to = resolve_path(workspace, to)?;
    let source_parent = resolved_from.parent().ok_or_else(|| {
        format!(
            "failed to move {}: source has no parent",
            resolved_from.display()
        )
    })?;
    let target_parent = resolved_to.parent().ok_or_else(|| {
        format!(
            "failed to move {}: target has no parent",
            resolved_to.display()
        )
    })?;
    if source_parent == target_parent {
        fs::rename(&resolved_from, &resolved_to).map_err(|error| {
            format!(
                "failed to move {} to {}: {error}",
                resolved_from.display(),
                resolved_to.display()
            )
        })?;
        return sync_directory(source_parent);
    }

    fs::create_dir_all(target_parent)
        .map_err(|error| format!("failed to create {}: {error}", target_parent.display()))?;
    fs::rename(&resolved_from, &resolved_to).map_err(|error| {
        format!(
            "failed to move {} to {}: {error}",
            resolved_from.display(),
            resolved_to.display()
        )
    })?;
    sync_directory(source_parent)?;
    sync_directory(target_parent)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedEdit {
    start_index: usize,
    end_index: usize,
    body: String,
}

fn apply_segments(
    workspace: &WorkspaceRoot,
    path: &str,
    segments: &[Segment],
) -> Result<(), String> {
    if segments.is_empty() {
        return Err(format!(
            "failed to edit {path}: EDIT requires at least one segment"
        ));
    }
    let resolved = resolve_path(workspace, path)?;
    let text = fs::read_to_string(&resolved)
        .map_err(|error| format!("failed to read {}: {error}", resolved.display()))?;
    let lines = text.lines().collect::<Vec<_>>();
    let mut planned = Vec::new();
    for segment in segments {
        planned.push(plan_segment(path, &lines, segment)?);
    }
    planned.sort_by_key(|edit| edit.start_index);
    for pair in planned.windows(2) {
        let previous = &pair[0];
        let next = &pair[1];
        if previous.end_index > next.start_index {
            return Err(format!("failed to edit {path}: edit segments overlap"));
        }
    }

    let mut output_lines = Vec::new();
    let mut cursor = 0;
    for edit in &planned {
        output_lines.extend(
            lines[cursor..edit.start_index]
                .iter()
                .map(|line| (*line).to_string()),
        );
        output_lines.extend(edit.body.lines().map(str::to_string));
        cursor = edit.end_index;
    }
    output_lines.extend(lines[cursor..].iter().map(|line| (*line).to_string()));

    let mut output = output_lines.join("\n");
    if text.ends_with('\n') || !output.is_empty() {
        output.push('\n');
    }
    atomic_replace(&resolved, output.as_bytes())?;
    Ok(())
}

fn plan_segment(path: &str, lines: &[&str], segment: &Segment) -> Result<PlannedEdit, String> {
    match segment {
        Segment::Replace { start, end, body } => {
            validate_anchor(path, lines, *start)?;
            validate_anchor(path, lines, *end)?;
            if end.line_number < start.line_number {
                return Err(format!(
                    "failed to edit {path}: end anchor precedes start anchor"
                ));
            }
            Ok(PlannedEdit {
                start_index: start.line_number - 1,
                end_index: end.line_number,
                body: body.clone(),
            })
        }
        Segment::Delete { start, end } => {
            validate_anchor(path, lines, *start)?;
            validate_anchor(path, lines, *end)?;
            if end.line_number < start.line_number {
                return Err(format!(
                    "failed to edit {path}: end anchor precedes start anchor"
                ));
            }
            Ok(PlannedEdit {
                start_index: start.line_number - 1,
                end_index: end.line_number,
                body: String::new(),
            })
        }
        Segment::Insert {
            position,
            anchor,
            body,
        } => {
            validate_anchor(path, lines, *anchor)?;
            if matches!(position, InsertPosition::Append) && anchor.line_number != lines.len() {
                return Err(format!(
                    "failed to edit {path}: APPEND anchor must be the current last line"
                ));
            }
            let insert_index = match position {
                InsertPosition::Before => anchor.line_number - 1,
                InsertPosition::After | InsertPosition::Append => anchor.line_number,
            };
            Ok(PlannedEdit {
                start_index: insert_index,
                end_index: insert_index,
                body: body.clone(),
            })
        }
    }
}

fn validate_anchor(path: &str, lines: &[&str], anchor: LineAnchor) -> Result<(), String> {
    if anchor.line_number == 0 || anchor.line_number > lines.len() {
        return Err(format!(
            "{path} anchor line {} outside 1..={}",
            anchor.line_number,
            lines.len()
        ));
    }
    let current_hash = line_hash(lines[anchor.line_number - 1]);
    if current_hash != anchor.hash {
        let rendered = format_line_anchor(anchor).ok_or_else(|| {
            "edit anchor vocabulary is missing an entry for the stale anchor".to_string()
        })?;
        return Err(format!("{path} stale anchor {rendered}"));
    }
    Ok(())
}

/// Parses the raw native tool input.
pub fn parse_input(input: &str) -> Result<Request, String> {
    Parser::new(input).parse()
}

struct Parser<'a> {
    input: &'a str,
    offset: usize,
    operations: Vec<Operation>,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            offset: 0,
            operations: Vec::new(),
        }
    }

    fn parse(mut self) -> Result<Request, String> {
        if self.input.lines().any(patch_delimiter) {
            return Err(
                "failed to parse `edit_file` input: `***` patch delimiters are not supported"
                    .to_string(),
            );
        }

        while self.offset < self.input.len() {
            let line = self.next_line().ok_or_else(|| {
                "failed to parse `edit_file` input: expected section header".to_string()
            })?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(path) = line.strip_prefix("§ Edit ") {
                let path = parse_path(path, "Edit")?;
                let segments = self.parse_segments()?;
                self.operations.push(Operation::Edit { path, segments });
            } else if let Some(path) = line.strip_prefix("§ Add ") {
                let path = parse_path(path, "Add")?;
                let body = self.take_body_until_header()?;
                self.operations.push(Operation::Add { path, body });
            } else if let Some(path) = line.strip_prefix("§ Remove ") {
                self.operations.push(Operation::Remove {
                    path: parse_path(path, "Remove")?,
                });
            } else if let Some(from) = line.strip_prefix("§ Move ") {
                let to_line = self.next_line().ok_or_else(|| {
                    "failed to parse `edit_file` input: Move requires `§ To <path>`".to_string()
                })?;
                let Some(to) = to_line.strip_prefix("§ To ") else {
                    return Err(
                        "failed to parse `edit_file` input: Move requires `§ To <path>`"
                            .to_string(),
                    );
                };
                self.operations.push(Operation::Move {
                    from: parse_path(from, "Move")?,
                    to: parse_path(to, "To")?,
                });
            } else {
                return Err(format!(
                    "failed to parse `edit_file` input: unsupported section header `{line}`"
                ));
            }
        }

        if self.operations.is_empty() {
            return Err(
                "failed to parse `edit_file` input: expected at least one section".to_string(),
            );
        }
        Ok(Request {
            operations: self.operations,
        })
    }

    fn parse_segments(&mut self) -> Result<Vec<Segment>, String> {
        let mut segments = Vec::new();
        while self.offset < self.input.len() {
            let Some(line) = self.peek_line() else {
                break;
            };
            if top_level_header(line) {
                break;
            }
            let Some(line) = self.next_line() else {
                return Err(
                    "failed to parse `edit_file` input: expected edit segment header".to_string(),
                );
            };
            if line.trim().is_empty() {
                continue;
            }
            if let Some(args) = line.strip_prefix("§ Replace ") {
                let (start, end) = parse_anchor_pair(args, "Replace")?;
                segments.push(Segment::Replace {
                    start,
                    end,
                    body: self.take_body_until_header()?,
                });
            } else if let Some(args) = line.strip_prefix("§ Delete ") {
                let (start, end) = parse_anchor_pair(args, "Delete")?;
                segments.push(Segment::Delete { start, end });
            } else if let Some(anchor) = line.strip_prefix("§ Before ") {
                segments.push(Segment::Insert {
                    position: InsertPosition::Before,
                    anchor: parse_single_anchor(anchor, "Before")?,
                    body: self.take_body_until_header()?,
                });
            } else if let Some(anchor) = line.strip_prefix("§ After ") {
                segments.push(Segment::Insert {
                    position: InsertPosition::After,
                    anchor: parse_single_anchor(anchor, "After")?,
                    body: self.take_body_until_header()?,
                });
            } else if let Some(anchor) = line.strip_prefix("§ Append ") {
                segments.push(Segment::Insert {
                    position: InsertPosition::Append,
                    anchor: parse_single_anchor(anchor, "Append")?,
                    body: self.take_body_until_header()?,
                });
            } else {
                return Err(format!(
                    "failed to parse `edit_file` input: unsupported edit header `{line}`"
                ));
            }
        }
        if segments.is_empty() {
            return Err(
                "failed to parse `edit_file` input: Edit requires at least one segment".to_string(),
            );
        }
        Ok(segments)
    }

    fn next_line(&mut self) -> Option<&'a str> {
        if self.offset >= self.input.len() {
            return None;
        }
        let rest = &self.input[self.offset..];
        if let Some(index) = rest.find('\n') {
            let line = &rest[..index];
            self.offset += index + 1;
            Some(line.strip_suffix('\r').unwrap_or(line))
        } else {
            self.offset = self.input.len();
            Some(rest.strip_suffix('\r').unwrap_or(rest))
        }
    }

    fn peek_line(&self) -> Option<&'a str> {
        if self.offset >= self.input.len() {
            return None;
        }
        let rest = &self.input[self.offset..];
        if let Some(index) = rest.find('\n') {
            Some(rest[..index].strip_suffix('\r').unwrap_or(&rest[..index]))
        } else {
            Some(rest.strip_suffix('\r').unwrap_or(rest))
        }
    }

    fn take_body_until_header(&mut self) -> Result<String, String> {
        let start = self.offset;
        while self.offset < self.input.len() {
            let Some(line) = self.peek_line() else {
                break;
            };
            if any_header(line) {
                break;
            }
            if self.next_line().is_none() {
                return Err(
                    "failed to parse `edit_file` input: failed to consume body line".to_string(),
                );
            }
        }
        decode_body(&self.input[start..self.offset])
    }
}

fn decode_body(body: &str) -> Result<String, String> {
    let mut decoded = String::with_capacity(body.len());
    let mut characters = body.chars();
    while let Some(character) = characters.next() {
        if character == '\\' && characters.clone().next() == Some('§') {
            let _ = characters.next();
            decoded.push('§');
        } else if character == '§' {
            return Err(
                "failed to parse `edit_file` input: literal `§` in body must be escaped as `\\§`"
                    .to_string(),
            );
        } else {
            decoded.push(character);
        }
    }
    Ok(decoded)
}

fn parse_path(value: &str, header: &str) -> Result<String, String> {
    let path = value.trim();
    if path.is_empty() {
        return Err(format!(
            "failed to parse `edit_file` input: {header} path must not be empty"
        ));
    }
    Ok(path.to_string())
}

fn parse_anchor_pair(value: &str, header: &str) -> Result<(LineAnchor, LineAnchor), String> {
    let mut parts = value.split_whitespace();
    let start_line = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires start anchor")
    })?;
    let start_word = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires start anchor")
    })?;
    let end_line = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires end anchor")
    })?;
    let end_word = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires end anchor")
    })?;
    if parts.next().is_some() {
        return Err(format!(
            "failed to parse `edit_file` input: {header} accepts exactly two anchors"
        ));
    }
    Ok((
        parse_anchor(&format!("{start_line} {start_word}"))?,
        parse_anchor(&format!("{end_line} {end_word}"))?,
    ))
}

fn parse_single_anchor(value: &str, header: &str) -> Result<LineAnchor, String> {
    let mut parts = value.split_whitespace();
    let line_number = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires one anchor")
    })?;
    let word = parts.next().ok_or_else(|| {
        format!("failed to parse `edit_file` input: {header} requires one anchor")
    })?;
    if parts.next().is_some() {
        return Err(format!(
            "failed to parse `edit_file` input: {header} accepts exactly one anchor"
        ));
    }
    parse_anchor(&format!("{line_number} {word}"))
}

fn patch_delimiter(line: &str) -> bool {
    line.starts_with("*** ")
}

fn top_level_header(line: &str) -> bool {
    line.starts_with("§ Edit ")
        || line.starts_with("§ Add ")
        || line.starts_with("§ Remove ")
        || line.starts_with("§ Move ")
}

fn any_header(line: &str) -> bool {
    line.starts_with("§ ")
}

fn parse_anchor(value: &str) -> Result<LineAnchor, String> {
    let mut parts = value.split_whitespace();
    let Some(line_number) = parts.next() else {
        return Err(
            "failed to parse `edit_file` input: anchor requires a line number and word".to_string(),
        );
    };
    let Some(word) = parts.next() else {
        return Err(
            "failed to parse `edit_file` input: anchor requires a line number and word".to_string(),
        );
    };
    if parts.next().is_some() {
        return Err(
            "failed to parse `edit_file` input: anchor accepts exactly a line number and word"
                .to_string(),
        );
    }

    let line_number = line_number.parse::<usize>().map_err(|_| {
        "failed to parse `edit_file` input: anchor line number must be positive".to_string()
    })?;
    if line_number == 0 {
        return Err(
            "failed to parse `edit_file` input: anchor line number must be positive".to_string(),
        );
    }

    let hash = (0..=u8::MAX)
        .find(|&hash| edit_anchor_word(hash) == Some(word))
        .ok_or_else(|| {
            "failed to parse `edit_file` input: anchor word is not in the vocabulary".to_string()
        })?;
    Ok(LineAnchor { line_number, hash })
}

fn format_line_anchor(anchor: LineAnchor) -> Option<String> {
    Some(format!(
        "{} {}",
        anchor.line_number,
        edit_anchor_word(anchor.hash)?
    ))
}

fn line_hash(line: &str) -> u8 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in line.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    (hash & 0xff) as u8
}

const EDIT_ANCHOR_VOCABULARY: &str = include_str!("../../../../o200k_anchor_candidates.txt");

fn edit_anchor_word(hash: u8) -> Option<&'static str> {
    let line = EDIT_ANCHOR_VOCABULARY.lines().nth(hash as usize)?;
    line.split_once("\": \"")
        .and_then(|(_, value)| value.strip_suffix("\","))
        .or_else(|| {
            line.split_once("\": \"")
                .and_then(|(_, value)| value.strip_suffix("\"}"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_preserves_escaped_section_markers_in_bodies() {
        let request = parse_input("§ Add file.txt\nbody \\§ marker\n").unwrap();
        assert_eq!(
            request.operations,
            vec![Operation::Add {
                path: "file.txt".to_string(),
                body: "body § marker\n".to_string(),
            }]
        );
    }

    #[test]
    fn parser_rejects_patch_syntax() {
        let error = parse_input("*** Begin Patch\n").unwrap_err();
        assert!(error.contains("patch delimiters are not supported"));
    }
}
