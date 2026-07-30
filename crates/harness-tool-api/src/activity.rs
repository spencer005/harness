//! Closed user-facing contracts for built-in tool activity.

use serde::{Deserialize, Serialize};

/// Built-in tool identity. Frontends must handle every variant exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuiltInTool {
    Inspect,
    EditFile,
    TerminalOpen,
    TerminalRead,
    TerminalWrite,
    Goal,
}

impl BuiltInTool {
    pub const ALL: [Self; 6] = [
        Self::Inspect,
        Self::EditFile,
        Self::TerminalOpen,
        Self::TerminalRead,
        Self::TerminalWrite,
        Self::Goal,
    ];

    /// Resolves an advertised built-in name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "inspect" => Some(Self::Inspect),
            "edit_file" => Some(Self::EditFile),
            "terminal_open" => Some(Self::TerminalOpen),
            "terminal_read" => Some(Self::TerminalRead),
            "terminal_write" => Some(Self::TerminalWrite),
            "goal" => Some(Self::Goal),
            _ => None,
        }
    }

    /// Returns the advertised tool name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::EditFile => "edit_file",
            Self::TerminalOpen => "terminal_open",
            Self::TerminalRead => "terminal_read",
            Self::TerminalWrite => "terminal_write",
            Self::Goal => "goal",
        }
    }
}

/// A parsed request or an explicit input rejection for one known tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Prepared<T> {
    Ready(T),
    Rejected(ToolInputRejection),
}

/// Invalid model-provided input retained for an expandable failure view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInputRejection {
    pub message: String,
}

/// Parsed invocation displayed as soon as a tool call is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolInvocation {
    Inspect(Prepared<InspectRequest>),
    EditFile(Prepared<EditFileRequest>),
    TerminalOpen(Prepared<TerminalOpenRequest>),
    TerminalRead(Prepared<TerminalReadRequest>),
    TerminalWrite(Prepared<TerminalWriteRequest>),
    Goal(Prepared<GoalRequest>),
}

impl ToolInvocation {
    /// Returns the built-in tool represented by this invocation.
    pub fn tool(&self) -> BuiltInTool {
        match self {
            Self::Inspect(_) => BuiltInTool::Inspect,
            Self::EditFile(_) => BuiltInTool::EditFile,
            Self::TerminalOpen(_) => BuiltInTool::TerminalOpen,
            Self::TerminalRead(_) => BuiltInTool::TerminalRead,
            Self::TerminalWrite(_) => BuiltInTool::TerminalWrite,
            Self::Goal(_) => BuiltInTool::Goal,
        }
    }

    /// Returns an input rejection when preparation failed.
    pub fn rejection(&self) -> Option<&ToolInputRejection> {
        match self {
            Self::Inspect(Prepared::Rejected(rejection))
            | Self::EditFile(Prepared::Rejected(rejection))
            | Self::TerminalOpen(Prepared::Rejected(rejection))
            | Self::TerminalRead(Prepared::Rejected(rejection))
            | Self::TerminalWrite(Prepared::Rejected(rejection))
            | Self::Goal(Prepared::Rejected(rejection)) => Some(rejection),
            _ => None,
        }
    }

    /// Builds the explicit rejected variant for a known tool.
    pub fn rejected(tool: BuiltInTool, message: String) -> Self {
        let rejection = ToolInputRejection { message };
        match tool {
            BuiltInTool::Inspect => Self::Inspect(Prepared::Rejected(rejection)),
            BuiltInTool::EditFile => Self::EditFile(Prepared::Rejected(rejection)),
            BuiltInTool::TerminalOpen => Self::TerminalOpen(Prepared::Rejected(rejection)),
            BuiltInTool::TerminalRead => Self::TerminalRead(Prepared::Rejected(rejection)),
            BuiltInTool::TerminalWrite => Self::TerminalWrite(Prepared::Rejected(rejection)),
            BuiltInTool::Goal => Self::Goal(Prepared::Rejected(rejection)),
        }
    }
}

/// Fully parsed inspect batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectRequest {
    pub jobs: Vec<InspectJobRequest>,
}

/// One fully parsed inspect job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InspectJobRequest {
    Read(InspectReadRequest),
    List(InspectListRequest),
    Stat(InspectStatRequest),
    Bytes(InspectBytesRequest),
    ByteSearch(InspectByteSearchRequest),
    Strings(InspectStringsRequest),
    Elf(InspectElfRequest),
    Search(InspectSearchRequest),
    Which(InspectWhichRequest),
    Check(InspectCheckRequest),
    Test(InspectTestRequest),
    Ps(InspectPsRequest),
    Pwd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectReadRequest {
    pub path: String,
    pub ranges: Vec<LineRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start_line: usize,
    pub line_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectListRequest {
    pub paths: Vec<String>,
    pub depth: usize,
    pub exact: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectStatRequest {
    pub paths: Vec<String>,
    pub metadata: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectBytesRequest {
    pub path: String,
    pub offset: u64,
    pub length: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectByteSearchRequest {
    pub path: String,
    pub pattern: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectStringsRequest {
    pub path: String,
    pub literal: Option<String>,
    pub maximum_results: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectElfRequest {
    pub path: String,
    pub query: InspectElfQuery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InspectElfQuery {
    Summary,
    Sections,
    Segments,
    Symbols(Option<String>),
    Relocations(Option<String>),
    Dynamic(Option<String>),
    Address(u64),
    Offset(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectSearchRequest {
    pub mode: InspectSearchMode,
    pub roots: Vec<String>,
    pub maximum_results: usize,
    pub includes: Vec<String>,
    pub excludes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InspectSearchMode {
    Content {
        patterns: Vec<String>,
        literal: bool,
        case: SearchCase,
        files_with_matches: bool,
    },
    Files,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchCase {
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectWhichRequest {
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectCheckRequest {
    pub cargo_arguments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectTestRequest {
    pub cargo_arguments: Vec<String>,
    pub filters: Vec<String>,
    pub libtest_arguments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectPsRequest {
    pub filter: Option<String>,
}

/// Parsed edit batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditFileRequest {
    pub operations: Vec<EditOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditOperation {
    Add {
        path: String,
        body: String,
    },
    Remove {
        path: String,
    },
    Move {
        from: String,
        to: String,
    },
    Edit {
        path: String,
        segments: Vec<EditSegment>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditSegment {
    Replace {
        start: EditLineAnchor,
        end: EditLineAnchor,
        body: String,
    },
    Delete {
        start: EditLineAnchor,
        end: EditLineAnchor,
    },
    Insert {
        position: EditInsertPosition,
        anchor: EditLineAnchor,
        body: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditInsertPosition {
    Before,
    After,
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditLineAnchor {
    pub line_number: usize,
    pub hash: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalOpenRequest {
    pub command: String,
    pub workdir: Option<String>,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalReadRequest {
    pub terminal_id: i32,
    pub poll_after_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalWriteRequest {
    pub terminal_id: i32,
    pub input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalRequest;

/// Mandatory structured outcome for one completed invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolOutcome {
    Inspect(InspectResult),
    EditFile(EditFileResult),
    TerminalOpen(TerminalResult),
    TerminalRead(TerminalResult),
    TerminalWrite(TerminalResult),
    Goal(GoalResult),
    Failed(ToolExecutionFailure),
    Interrupted(ToolInterruption),
}
/// Lifecycle state shown for one persistent tool activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolActivityStatus {
    Running,
    Successful,
    PartiallySuccessful,
    Failed,
    Interrupted,
}


impl ToolOutcome {
    /// Returns the built-in tool for a successful typed outcome.
    pub fn tool(&self) -> Option<BuiltInTool> {
        match self {
            Self::Inspect(_) => Some(BuiltInTool::Inspect),
            Self::EditFile(_) => Some(BuiltInTool::EditFile),
            Self::TerminalOpen(_) => Some(BuiltInTool::TerminalOpen),
            Self::TerminalRead(_) => Some(BuiltInTool::TerminalRead),
            Self::TerminalWrite(_) => Some(BuiltInTool::TerminalWrite),
            Self::Goal(_) => Some(BuiltInTool::Goal),
            Self::Failed(_) | Self::Interrupted(_) => None,
        }
    }
    /// Derives the user-facing activity state from the complete typed outcome.
    pub fn status(&self) -> ToolActivityStatus {
        match self {
            Self::Inspect(result) => {
                let succeeded = result
                    .jobs
                    .iter()
                    .filter(|job| matches!(job, InspectJobOutcome::Succeeded(_)))
                    .count();
                let failed = result.jobs.len().saturating_sub(succeeded);
                match (succeeded > 0, failed > 0) {
                    (true, true) => ToolActivityStatus::PartiallySuccessful,
                    (false, true) => ToolActivityStatus::Failed,
                    _ => ToolActivityStatus::Successful,
                }
            }
            Self::EditFile(result) => {
                let succeeded = result
                    .operations
                    .iter()
                    .any(|operation| matches!(operation, EditOperationOutcome::Succeeded(_)));
                let incomplete = result.operations.iter().any(|operation| {
                    matches!(
                        operation,
                        EditOperationOutcome::PartiallySucceeded { .. }
                            | EditOperationOutcome::Failed { .. }
                    )
                });
                match (succeeded, incomplete) {
                    (true, true) => ToolActivityStatus::PartiallySuccessful,
                    (false, true)
                        if result.operations.iter().any(|operation| {
                            matches!(
                                operation,
                                EditOperationOutcome::PartiallySucceeded { .. }
                            )
                        }) =>
                    {
                        ToolActivityStatus::PartiallySuccessful
                    }
                    (false, true) => ToolActivityStatus::Failed,
                    _ => ToolActivityStatus::Successful,
                }
            }
            Self::TerminalOpen(_)
            | Self::TerminalRead(_)
            | Self::TerminalWrite(_)
            | Self::Goal(_) => ToolActivityStatus::Successful,
            Self::Failed(_) => ToolActivityStatus::Failed,
            Self::Interrupted(_) => ToolActivityStatus::Interrupted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionFailure {
    pub category: ToolFailureCategory,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolFailureCategory {
    InvalidInput,
    TimedOut,
    Cancelled,
    Execution,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInterruption {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectResult {
    pub jobs: Vec<InspectJobOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InspectJobOutcome {
    Succeeded(InspectJobSuccess),
    Failed(ToolExecutionFailure),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InspectJobSuccess {
    Read(InspectReadResult),
    List(InspectListResult),
    Stat(InspectStatResult),
    Bytes(InspectBytesResult),
    ByteSearch(InspectByteSearchResult),
    Strings(InspectStringsResult),
    Elf(InspectElfResult),
    Search(InspectSearchResult),
    Which(InspectWhichResult),
    Check(InspectCheckResult),
    Test(InspectTestResult),
    Ps(InspectPsResult),
    Pwd { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectReadResult {
    pub excerpts: Vec<SourceExcerpt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceExcerpt {
    pub path: String,
    pub start_line: usize,
    pub lines: Vec<String>,
    pub next: Option<LineRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectListResult {
    pub roots: Vec<String>,
    pub entries: Vec<InspectListEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectListEntry {
    pub path: String,
    pub depth: usize,
    pub kind: InspectPathKind,
    pub line_count: Option<usize>,
    pub byte_count: Option<u64>,
    pub symlink_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InspectPathKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectStatResult {
    pub entries: Vec<InspectStatEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectStatEntry {
    pub path: String,
    pub kind: InspectPathKind,
    pub byte_count: u64,
    pub modified_unix_seconds: i64,
    pub permissions: u32,
    pub metadata: Option<InspectUnixMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectUnixMetadata {
    pub uid: u32,
    pub gid: u32,
    pub inode: u64,
    pub device: u64,
    pub links: u64,
    pub blocks: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectBytesResult {
    pub path: String,
    pub file_size: u64,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub next_offset: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectByteSearchResult {
    pub path: String,
    pub offsets: Vec<u64>,
    pub total_matches: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectStringsResult {
    pub path: String,
    pub matches: Vec<InspectStringMatch>,
    pub total_matches: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectStringMatch {
    pub offset: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectElfResult {
    pub path: String,
    pub query: InspectElfQuery,
    pub summary: Option<InspectElfSummary>,
    pub entries: Vec<InspectElfEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectElfSummary {
    pub bits: u8,
    pub architecture: String,
    pub endianness: String,
    pub kind: String,
    pub entry: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InspectElfEntry {
    Section {
        name: String,
        file_offset: Option<u64>,
        file_size: Option<u64>,
        virtual_address: u64,
        size: u64,
    },
    Segment {
        name: Option<String>,
        file_offset: u64,
        file_size: u64,
        virtual_address: u64,
        size: u64,
    },
    Symbol {
        name: String,
        virtual_address: u64,
        size: u64,
    },
    Relocation {
        section: String,
        offset: u64,
        target: String,
        kind: String,
    },
    Mapping {
        file_offset: u64,
        virtual_address: u64,
    },
    Dynamic {
        name: String,
        value: String,
    },
    Notice(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectSearchResult {
    pub files: Vec<InspectSearchFile>,
    pub total_matches: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectSearchFile {
    pub path: String,
    pub matches: Vec<InspectSearchMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectSearchMatch {
    pub line_number: Option<usize>,
    pub line: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectWhichResult {
    pub matches: Vec<InspectCommandMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectCommandMatch {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectCheckResult {
    pub succeeded: bool,
    pub diagnostics: Vec<InspectDiagnostic>,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectDiagnostic {
    pub level: InspectDiagnosticLevel,
    pub code: Option<String>,
    pub message: String,
    pub path: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InspectDiagnosticLevel {
    Error,
    Warning,
    Note,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectTestResult {
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
    pub failures: Vec<InspectTestFailure>,
    pub execution_failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectTestFailure {
    pub name: Option<String>,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectPsResult {
    pub processes: Vec<InspectProcess>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectProcess {
    pub user: String,
    pub pid: u32,
    pub cpu_percent: String,
    pub memory_percent: String,
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditFileResult {
    pub operations: Vec<EditOperationOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditOperationOutcome {
    Succeeded(FileChange),
    PartiallySucceeded {
        operation_index: usize,
        change: FileChange,
        message: String,
    },
    Failed {
        operation_index: usize,
        message: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub kind: FileChangeKind,
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub hunks: Vec<DiffHunk>,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChangeKind {
    Added,
    Removed,
    Modified,
    Moved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalResult {
    pub terminal_id: i32,
    pub output: String,
    pub earlier_output_omitted: u64,
    pub state: TerminalProcessState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalProcessState {
    Running,
    Exited { code: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalResult;
