use std::{
    collections::HashMap,
    fmt, fs,
    fs::{File, OpenOptions},
    io,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::actors::{
    ActorHandle, ActorReceiver, ActorSender, DEFAULT_ACTOR_MAILBOX_CAPACITY,
    channel as actor_channel,
};

const FILE_MAGIC: &[u8] = b"NHSESS\0";
const FORMAT_VERSION: u16 = 1;
const ZSTD_LEVEL: i32 = 3;
const SESSION_FILE_EXTENSION: &str = "nhsession";
const INDEX_LOG_FILE: &str = "session_index.log";
const INDEX_SNAPSHOT_FILE: &str = "session_index.snapshot";
const CHUNK_HEADER_LEN: usize = 20;

/// Stable identifier for one root or child conversation session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    /// Construct a session id from the caller-owned durable id string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow this id as text for path and UI use.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Metadata record that must be the first persisted record in every session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Stable id for this session.
    pub id: SessionId,
    /// Parent actor session, when this is a child session.
    pub parent_session_id: Option<SessionId>,
    /// Source session id, when this session is a fork.
    pub forked_from_session_id: Option<SessionId>,
    /// Session creation time as Unix milliseconds.
    pub created_at_ms: u64,
    /// Last known update time as Unix milliseconds.
    pub updated_at_ms: u64,
    /// Working directory used when the session started.
    pub cwd: String,
    /// Model name selected for the session.
    pub model: String,
    /// Client originator persisted for debugging and replay.
    pub originator: String,
    /// Harness/client version persisted for debugging and replay.
    pub client_version: String,
    /// Source that created this session, for example `cli` or `tui`.
    pub source: String,
    /// Human title used by fast lookup.
    pub title: Option<String>,
    /// First user-facing preview used by session lists.
    pub preview: Option<String>,
}

impl SessionMeta {
    /// Build an index entry for this metadata and its relative session path.
    pub fn index_entry(&self, relative_path: impl Into<String>) -> SessionIndexEntry {
        SessionIndexEntry {
            session_id: self.id.clone(),
            path: relative_path.into(),
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            cwd: self.cwd.clone(),
            title: self.title.clone(),
            preview: self.preview.clone(),
            parent_session_id: self.parent_session_id.clone(),
            forked_from_session_id: self.forked_from_session_id.clone(),
        }
    }
}

/// Turn-local context needed to resume execution decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnContextRecord {
    /// Working directory for the turn.
    pub cwd: String,
    /// Local date string shown to the model for the turn.
    pub date: String,
    /// Local timezone identifier or offset shown to the model.
    pub timezone: String,
    /// Model selected for the turn.
    pub model: String,
    /// Sandbox policy selected for the turn.
    pub sandbox_policy: String,
    /// Approval policy selected for the turn.
    pub approval_policy: String,
}

/// User or assistant message payload stored in the durable history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRecord {
    /// Message text.
    pub text: String,
}

/// Freeform native tool call emitted by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreeformToolCallRecord {
    /// Responses API call id.
    pub call_id: String,
    /// Native tool name.
    pub name: String,
    /// Complete raw freeform input, when available.
    pub input: String,
}

/// Streaming input delta for a freeform native tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreeformToolInputDeltaRecord {
    /// Responses API output item id.
    pub item_id: String,
    /// Raw input fragment.
    pub delta: String,
}

/// Structured transcript-only tool output display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolOutputDisplayRecord {
    /// Read snippets returned by the unified inspect tool.
    InspectRead(Vec<InspectReadDisplayRecord>),
}

/// One transcript read snippet produced by `inspect read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectReadDisplayRecord {
    /// Root-relative file path that was read.
    pub path: String,
    /// First displayed line number.
    pub start_line: usize,
    /// Displayed source lines without edit anchors.
    pub lines: Vec<String>,
    #[serde(default)]
    pub next: Option<InspectReadNextRecord>,
}

/// Continuation range for a read snippet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectReadNextRecord {
    /// First unread line number.
    pub start_line: usize,
    /// Suggested line count.
    pub line_count: usize,
}

/// Result returned to a freeform native tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreeformToolOutputRecord {
    /// Responses API call id being answered.
    pub call_id: String,
    /// Tool output submitted back to the model.
    pub output: String,
    /// Transcript-only output when it differs from the model output.
    #[serde(default)]
    pub display_output: Option<String>,
    /// Structured transcript display when text output is not sufficient.
    #[serde(default)]
    pub display: Option<ToolOutputDisplayRecord>,
}

/// JSON/function native tool call emitted by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionToolCallRecord {
    /// Responses API call id.
    pub call_id: String,
    /// Native function tool name.
    pub name: String,
    /// Raw JSON arguments string.
    pub arguments: String,
}

/// Result returned to a JSON/function native tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionToolOutputRecord {
    /// Responses API call id being answered.
    pub call_id: String,
    /// Tool output submitted back to the model.
    pub output: String,
    /// Transcript-only output when it differs from the model output.
    #[serde(default)]
    pub display_output: Option<String>,
}

impl FreeformToolOutputRecord {
    /// Returns the transcript output for this tool result.
    pub fn transcript_output(&self) -> &str {
        self.display_output.as_deref().unwrap_or(&self.output)
    }
}

impl FunctionToolOutputRecord {
    /// Returns the transcript output for this tool result.
    pub fn transcript_output(&self) -> &str {
        self.display_output.as_deref().unwrap_or(&self.output)
    }
}
#[derive(Debug, Clone, Deserialize)]
struct LegacyFreeformToolOutputRecord {
    call_id: String,
    output: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyFunctionToolOutputRecord {
    call_id: String,
    output: String,
}

#[derive(Debug, Clone, Deserialize)]
enum LegacyToolOutputSessionRecordKind {
    SessionMeta(SessionMeta),
    TurnContext(TurnContextRecord),
    UserMessage(MessageRecord),
    DeveloperMessage(MessageRecord),
    AssistantMessage(MessageRecord),
    FreeformToolCall(FreeformToolCallRecord),
    FreeformToolInputDelta(FreeformToolInputDeltaRecord),
    FreeformToolOutput(LegacyFreeformToolOutputRecord),
    FunctionToolCall(FunctionToolCallRecord),
    FunctionToolOutput(LegacyFunctionToolOutputRecord),
    CompactionCheckpoint(CompactionCheckpointRecord),
    SessionClosed(SessionClosedRecord),
    ProviderSessionBinding(ProviderSessionBindingRecord),
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyToolOutputSessionRecord {
    seq: u64,
    kind: LegacyToolOutputSessionRecordKind,
}

impl LegacyToolOutputSessionRecord {
    fn into_current(self) -> SessionRecord {
        let kind = match self.kind {
            LegacyToolOutputSessionRecordKind::SessionMeta(record) => {
                SessionRecordKind::SessionMeta(record)
            }
            LegacyToolOutputSessionRecordKind::TurnContext(record) => {
                SessionRecordKind::TurnContext(record)
            }
            LegacyToolOutputSessionRecordKind::UserMessage(record) => {
                SessionRecordKind::UserMessage(record)
            }
            LegacyToolOutputSessionRecordKind::DeveloperMessage(record) => {
                SessionRecordKind::DeveloperMessage(record)
            }
            LegacyToolOutputSessionRecordKind::AssistantMessage(record) => {
                SessionRecordKind::AssistantMessage(record)
            }
            LegacyToolOutputSessionRecordKind::FreeformToolCall(record) => {
                SessionRecordKind::FreeformToolCall(record)
            }
            LegacyToolOutputSessionRecordKind::FreeformToolInputDelta(record) => {
                SessionRecordKind::FreeformToolInputDelta(record)
            }
            LegacyToolOutputSessionRecordKind::FreeformToolOutput(record) => {
                SessionRecordKind::FreeformToolOutput(FreeformToolOutputRecord {
                    call_id: record.call_id,
                    output: record.output,
                    display_output: None,
                    display: None,
                })
            }
            LegacyToolOutputSessionRecordKind::FunctionToolCall(record) => {
                SessionRecordKind::FunctionToolCall(record)
            }
            LegacyToolOutputSessionRecordKind::FunctionToolOutput(record) => {
                SessionRecordKind::FunctionToolOutput(FunctionToolOutputRecord {
                    call_id: record.call_id,
                    output: record.output,
                    display_output: None,
                })
            }
            LegacyToolOutputSessionRecordKind::CompactionCheckpoint(record) => {
                SessionRecordKind::CompactionCheckpoint(record)
            }
            LegacyToolOutputSessionRecordKind::SessionClosed(record) => {
                SessionRecordKind::SessionClosed(record)
            }
            LegacyToolOutputSessionRecordKind::ProviderSessionBinding(record) => {
                SessionRecordKind::ProviderSessionBinding(record)
            }
        };
        SessionRecord {
            seq: self.seq,
            kind,
        }
    }
}

/// History item that can be carried forward by a compaction checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryRecord {
    /// User message kept after compaction.
    UserMessage(MessageRecord),
    /// Developer-role message kept after compaction.
    DeveloperMessage(MessageRecord),
    /// Assistant message kept after compaction.
    AssistantMessage(MessageRecord),
    /// Freeform tool call kept after compaction.
    FreeformToolCall(FreeformToolCallRecord),
    /// Freeform tool result kept after compaction.
    FreeformToolOutput(FreeformToolOutputRecord),
    /// JSON/function tool call kept after compaction.
    FunctionToolCall(FunctionToolCallRecord),
    /// JSON/function tool result kept after compaction.
    FunctionToolOutput(FunctionToolOutputRecord),
}

/// Compaction checkpoint that replaces older history during resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionCheckpointRecord {
    /// Replacement history after compaction.
    pub replacement_history: Vec<HistoryRecord>,
}

/// Explicit close marker for a completed session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClosedRecord {
    /// Close time as Unix milliseconds.
    pub closed_at_ms: u64,
}

/// Provider binding used when the session was created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSessionBindingRecord {
    /// Provider binding snapshot.
    pub binding: crate::provider_runtime::ProviderSessionBinding,
}

/// Typed record payload stored in a session log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRecordKind {
    /// First record in each session file.
    SessionMeta(SessionMeta),
    /// Turn context record.
    TurnContext(TurnContextRecord),
    /// User message record.
    UserMessage(MessageRecord),
    /// Developer-role message record.
    DeveloperMessage(MessageRecord),
    /// Assistant message record.
    AssistantMessage(MessageRecord),
    /// Freeform native tool call record.
    FreeformToolCall(FreeformToolCallRecord),
    /// Freeform native tool input delta record.
    FreeformToolInputDelta(FreeformToolInputDeltaRecord),
    /// Freeform native tool output record.
    FreeformToolOutput(FreeformToolOutputRecord),
    /// JSON/function native tool call record.
    FunctionToolCall(FunctionToolCallRecord),
    /// JSON/function native tool output record.
    FunctionToolOutput(FunctionToolOutputRecord),
    /// Compaction checkpoint record.
    CompactionCheckpoint(CompactionCheckpointRecord),
    /// Explicit session close record.
    SessionClosed(SessionClosedRecord),
    /// Provider binding used by the session.
    ProviderSessionBinding(ProviderSessionBindingRecord),
}

/// Sequence-numbered durable record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Monotonic sequence number within the session.
    pub seq: u64,
    /// Typed record payload.
    pub kind: SessionRecordKind,
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderBoundSessionMeta {
    id: SessionId,
    parent_session_id: Option<SessionId>,
    forked_from_session_id: Option<SessionId>,
    created_at_ms: u64,
    updated_at_ms: u64,
    cwd: String,
    model: String,
    provider_binding: Option<crate::provider_runtime::ProviderSessionBinding>,
    originator: String,
    client_version: String,
    source: String,
    title: Option<String>,
    preview: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
enum ProviderBoundSessionRecordKind {
    SessionMeta(ProviderBoundSessionMeta),
    TurnContext(TurnContextRecord),
    UserMessage(MessageRecord),
    DeveloperMessage(MessageRecord),
    AssistantMessage(MessageRecord),
    FreeformToolCall(FreeformToolCallRecord),
    FreeformToolInputDelta(FreeformToolInputDeltaRecord),
    FreeformToolOutput(FreeformToolOutputRecord),
    FunctionToolCall(FunctionToolCallRecord),
    FunctionToolOutput(FunctionToolOutputRecord),
    CompactionCheckpoint(CompactionCheckpointRecord),
    SessionClosed(SessionClosedRecord),
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderBoundSessionRecord {
    seq: u64,
    kind: ProviderBoundSessionRecordKind,
}

impl ProviderBoundSessionMeta {
    fn into_current(self) -> SessionMeta {
        SessionMeta {
            id: self.id,
            parent_session_id: self.parent_session_id,
            forked_from_session_id: self.forked_from_session_id,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
            cwd: self.cwd,
            model: self.model,
            originator: self.originator,
            client_version: self.client_version,
            source: self.source,
            title: self.title,
            preview: self.preview,
        }
    }
}

impl ProviderBoundSessionRecord {
    fn into_current(self) -> Vec<SessionRecord> {
        let seq = self.seq;
        match self.kind {
            ProviderBoundSessionRecordKind::SessionMeta(meta) => {
                let binding = meta.provider_binding.clone();
                let mut records = vec![SessionRecord {
                    seq,
                    kind: SessionRecordKind::SessionMeta(meta.into_current()),
                }];
                if let Some(binding) = binding {
                    records.push(SessionRecord {
                        seq,
                        kind: SessionRecordKind::ProviderSessionBinding(
                            ProviderSessionBindingRecord { binding },
                        ),
                    });
                }
                records
            }
            ProviderBoundSessionRecordKind::TurnContext(record) => vec![SessionRecord {
                seq,
                kind: SessionRecordKind::TurnContext(record),
            }],
            ProviderBoundSessionRecordKind::UserMessage(record) => vec![SessionRecord {
                seq,
                kind: SessionRecordKind::UserMessage(record),
            }],
            ProviderBoundSessionRecordKind::DeveloperMessage(record) => vec![SessionRecord {
                seq,
                kind: SessionRecordKind::DeveloperMessage(record),
            }],
            ProviderBoundSessionRecordKind::AssistantMessage(record) => vec![SessionRecord {
                seq,
                kind: SessionRecordKind::AssistantMessage(record),
            }],
            ProviderBoundSessionRecordKind::FreeformToolCall(record) => vec![SessionRecord {
                seq,
                kind: SessionRecordKind::FreeformToolCall(record),
            }],
            ProviderBoundSessionRecordKind::FreeformToolInputDelta(record) => vec![SessionRecord {
                seq,
                kind: SessionRecordKind::FreeformToolInputDelta(record),
            }],
            ProviderBoundSessionRecordKind::FreeformToolOutput(record) => vec![SessionRecord {
                seq,
                kind: SessionRecordKind::FreeformToolOutput(record),
            }],
            ProviderBoundSessionRecordKind::FunctionToolCall(record) => vec![SessionRecord {
                seq,
                kind: SessionRecordKind::FunctionToolCall(record),
            }],
            ProviderBoundSessionRecordKind::FunctionToolOutput(record) => vec![SessionRecord {
                seq,
                kind: SessionRecordKind::FunctionToolOutput(record),
            }],
            ProviderBoundSessionRecordKind::CompactionCheckpoint(record) => vec![SessionRecord {
                seq,
                kind: SessionRecordKind::CompactionCheckpoint(record),
            }],
            ProviderBoundSessionRecordKind::SessionClosed(record) => vec![SessionRecord {
                seq,
                kind: SessionRecordKind::SessionClosed(record),
            }],
        }
    }
}

/// Fast-lookup index entry stored in the uncompressed append-only index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndexEntry {
    /// Session id.
    pub session_id: SessionId,
    /// Session path relative to the store root.
    pub path: String,
    /// Session creation time as Unix milliseconds.
    pub created_at_ms: u64,
    /// Last indexed update time as Unix milliseconds.
    pub updated_at_ms: u64,
    /// Working directory summary.
    pub cwd: String,
    /// Current title, when known.
    pub title: Option<String>,
    /// First-message preview, when known.
    pub preview: Option<String>,
    /// Parent actor session, when this is a child session.
    pub parent_session_id: Option<SessionId>,
    /// Source session id, when this session is a fork.
    pub forked_from_session_id: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionIndexSnapshot {
    entries: Vec<SessionIndexEntry>,
    title_candidates: Vec<SessionTitleCandidates>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionTitleCandidates {
    title: String,
    session_ids: Vec<SessionId>,
}

/// Session summary returned by fast lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// Session id.
    pub session_id: SessionId,
    /// Absolute session file path.
    pub path: PathBuf,
    /// Session creation time as Unix milliseconds.
    pub created_at_ms: u64,
    /// Last indexed update time as Unix milliseconds.
    pub updated_at_ms: u64,
    /// Working directory summary.
    pub cwd: String,
    /// Current title, when known.
    pub title: Option<String>,
    /// First-message preview, when known.
    pub preview: Option<String>,
    /// Parent actor session, when this is a child session.
    pub parent_session_id: Option<SessionId>,
    /// Source session id, when this session is a fork.
    pub forked_from_session_id: Option<SessionId>,
}

impl SessionSummary {
    fn from_entry(root: &Path, entry: &SessionIndexEntry) -> Self {
        Self {
            session_id: entry.session_id.clone(),
            path: root.join(&entry.path),
            created_at_ms: entry.created_at_ms,
            updated_at_ms: entry.updated_at_ms,
            cwd: entry.cwd.clone(),
            title: entry.title.clone(),
            preview: entry.preview.clone(),
            parent_session_id: entry.parent_session_id.clone(),
            forked_from_session_id: entry.forked_from_session_id.clone(),
        }
    }
}

/// Resume view built from the latest compaction checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestSessionHistory {
    /// Session metadata from the first record.
    pub meta: SessionMeta,
    /// Replacement history from the latest compaction checkpoint.
    pub checkpoint_history: Vec<HistoryRecord>,
    /// Durable records written after the latest compaction checkpoint.
    pub records_after_checkpoint: Vec<SessionRecord>,
}

/// One transcript entry decoded from a persisted session record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptPageLine {
    /// Source session sequence number.
    pub seq: u64,
    /// Durable record kind used by structured UI projections.
    pub kind: SessionRecordKind,
}

/// Reverse-page of persisted transcript entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptPage {
    /// Entries in chronological display order.
    pub lines: Vec<TranscriptPageLine>,
    /// Sequence number to pass as `before_seq` for the next older page.
    pub next_before_seq: Option<u64>,
    /// Whether this page reached the oldest displayable transcript entry.
    pub reached_start: bool,
}

#[derive(Debug)]
struct SessionFileRead {
    records: Vec<SessionRecord>,
    complete_len: u64,
}

#[derive(Debug, Clone, Copy)]
struct SessionChunkIndexEntry {
    header_offset: u64,
    payload_offset: u64,
    compressed_len: usize,
    uncompressed_len: usize,
    first_seq: u64,
    record_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChunkReport {
    pub index: usize,
    pub header_offset: u64,
    pub payload_offset: u64,
    pub compressed_len: usize,
    pub uncompressed_len: usize,
    pub first_seq: u64,
    pub record_count: u32,
    pub decodes: bool,
    pub error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionChunkProbe {
    pub index: usize,
    pub header_offset: u64,
    pub payload_offset: u64,
    pub compressed_len: usize,
    pub uncompressed_len: usize,
    pub first_seq: u64,
    pub record_count: u32,
    pub decompresses: bool,
    pub current_decodes: bool,
    pub legacy_decodes: bool,
    pub current_error: Option<String>,
    pub legacy_error: Option<String>,
    pub payload_prefix_hex: Option<String>,
    pub payload_prefix_ascii: Option<String>,
    pub current_prefix_record_count: Option<usize>,
    pub current_failure_remainder_hex: Option<String>,
    pub current_failure_remainder_ascii: Option<String>,
}

/// Error produced by session storage, indexing, and decoding.
#[derive(Debug, Error)]
pub enum SessionError {
    /// File-system I/O failed.
    #[error("session I/O failed: {0}")]
    Io(#[from] io::Error),
    /// Postcard encoding or decoding failed.
    #[error("session postcard codec failed: {0}")]
    Postcard(#[from] postcard::Error),
    /// Sonic JSON encoding or decoding failed.
    #[error("session index JSON codec failed: {0}")]
    Json(#[from] sonic_rs::Error),
    /// Session file magic did not match this harness format.
    #[error("invalid session file magic")]
    InvalidMagic,
    /// Session file format version is not supported by this build.
    #[error("unsupported session file format version {0}")]
    UnsupportedVersion(u16),
    /// A complete compressed frame decoded to the wrong record count.
    #[error("session chunk declared {declared} records but decoded {decoded}")]
    ChunkRecordCount {
        /// Record count stored in the chunk header.
        declared: u32,
        /// Record count decoded from the chunk payload.
        decoded: usize,
    },
    /// A complete compressed frame decoded to the wrong first sequence number.
    #[error("session chunk declared first seq {declared} but decoded {decoded}")]
    ChunkFirstSeq {
        /// First sequence number stored in the chunk header.
        declared: u64,
        /// First sequence number decoded from the chunk payload.
        decoded: u64,
    },
    /// Session file did not start with metadata.
    #[error("session file does not start with SessionMeta")]
    MissingSessionMeta,
    /// A pending chunk cannot fit in the on-disk length fields.
    #[error("session chunk is too large: {len} bytes")]
    ChunkTooLarge {
        /// Oversized length.
        len: usize,
    },
}

/// Rooted session store for compressed binary session files.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    /// Create a store rooted at the supplied directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Borrow the store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create a session file, write the metadata record, and append an index entry.
    pub fn create_session(&self, meta: SessionMeta) -> Result<SessionWriter, SessionError> {
        let relative_path = session_relative_path(&meta.id, meta.created_at_ms);
        let path = self.root.join(&relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let mut writer = SessionWriter::create(path, file)?;
        writer.append(SessionRecordKind::SessionMeta(meta.clone()));
        writer.flush()?;

        SessionIndex::append_entry_to_log(
            &self.root,
            &meta.index_entry(path_to_index_string(&relative_path)),
        )?;
        Ok(writer)
    }

    /// Open an existing session for append after dropping any incomplete final chunk.
    ///
    /// This is the resume path after a clean stop or crash. Complete corrupt chunks
    /// still error; only an incomplete final chunk is discarded before new writes.
    pub fn resume_session(&self, path: impl AsRef<Path>) -> Result<SessionWriter, SessionError> {
        let path = path.as_ref().to_path_buf();
        let read = read_session_file(&path)?;
        first_meta(&read.records)?;
        let next_seq = read.records.last().map_or(0, |record| record.seq + 1);

        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        file.set_len(read.complete_len)?;
        file.seek(SeekFrom::Start(read.complete_len))?;
        Ok(SessionWriter::resume(path, file, next_seq))
    }

    /// Read all complete records from a session path.
    ///
    /// An incomplete final chunk is ignored. A complete corrupt chunk is an error.
    pub fn read_records(&self, path: impl AsRef<Path>) -> Result<Vec<SessionRecord>, SessionError> {
        read_session_records(path.as_ref())
    }
    /// Truncate a session file to the last complete readable prefix.
    ///
    /// This permanently discards any unreadable tail bytes after the final
    /// complete chunk so resume and transcript loading can proceed.
    pub fn repair_session_tail(&self, path: impl AsRef<Path>) -> Result<u64, SessionError> {
        let path = path.as_ref();
        let chunks = read_session_chunk_index(path)?;
        let mut complete_len = FILE_MAGIC.len() as u64 + 2;
        for chunk in chunks {
            match read_indexed_chunk(path, chunk) {
                Ok(_) => {
                    complete_len = chunk.payload_offset + chunk.compressed_len as u64;
                }
                Err(_) => break,
            }
        }
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(complete_len)?;
        file.sync_data()?;
        return Ok(complete_len);
    }

    /// Return a read-only chunk decode report for one session file.
    pub fn inspect_session_chunks(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Vec<SessionChunkReport>, SessionError> {
        inspect_session_chunks(path.as_ref())
    }

    /// Probe one indexed chunk and report which decode stages succeed.
    pub fn probe_session_chunk(
        &self,
        path: impl AsRef<Path>,
        chunk_index: usize,
    ) -> Result<SessionChunkProbe, SessionError> {
        probe_session_chunk(path.as_ref(), chunk_index)
    }
    /// Read the metadata from a session path and verify it is the first record.
    pub fn read_meta(&self, path: impl AsRef<Path>) -> Result<SessionMeta, SessionError> {
        let records = read_first_chunk_records(path.as_ref())?;
        first_meta(&records).cloned()
    }

    /// Read metadata and provider binding records needed to initialize a resumed session.
    pub fn read_startup_binding(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<
        (
            SessionMeta,
            Option<crate::provider_runtime::ProviderSessionBinding>,
        ),
        SessionError,
    > {
        read_startup_binding_records(path.as_ref())
    }

    /// Export complete records from a binary session as newline-delimited JSON.
    pub fn export_jsonl(
        &self,
        path: impl AsRef<Path>,
        mut writer: impl Write,
    ) -> Result<(), SessionError> {
        for record in self.read_records(path)? {
            let bytes = sonic_rs::to_vec(&record)?;
            writer.write_all(&bytes)?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        Ok(())
    }

    /// Load history from the latest compaction checkpoint in a session file.
    pub fn load_latest_history(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<LatestSessionHistory, SessionError> {
        let records = self.read_records(path)?;
        let meta = first_meta(&records)?.clone();
        let mut checkpoint_history = Vec::new();
        let mut checkpoint_index = 0usize;

        for (index, record) in records.iter().enumerate() {
            if let SessionRecordKind::CompactionCheckpoint(checkpoint) = &record.kind {
                checkpoint_history = checkpoint.replacement_history.clone();
                checkpoint_index = index;
            }
        }

        Ok(LatestSessionHistory {
            meta,
            checkpoint_history,
            records_after_checkpoint: records.into_iter().skip(checkpoint_index + 1).collect(),
        })
    }

    /// Read one reverse page of transcript entries from a single session file.
    ///
    /// The reader first indexes complete chunks in the requested session file by
    /// scanning chunk headers only, then walks those chunks newest-to-oldest and
    /// decompresses only chunks needed to fill the page. It never scans sibling
    /// session files and it preserves the postcard+zstd session payload format.
    pub fn read_transcript_page(
        &self,
        path: impl AsRef<Path>,
        before_seq: Option<u64>,
        max_lines: usize,
    ) -> Result<TranscriptPage, SessionError> {
        read_transcript_page(path.as_ref(), before_seq, max_lines)
    }
    /// Read enough recent transcript entries to cover the requested minimum
    /// number of persisted chat messages while preserving transcript order.
    pub fn read_resume_transcript(
        &self,
        path: impl AsRef<Path>,
        minimum_message_count: usize,
        page_line_limit: usize,
    ) -> Result<TranscriptPage, SessionError> {
        read_resume_transcript(path.as_ref(), minimum_message_count, page_line_limit)
    }

    /// Read an eighty-character preview of the newest persisted message in a session file.
    pub fn read_latest_message_preview(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Option<String>, SessionError> {
        read_latest_message_preview(path.as_ref())
    }
}

/// Append writer for one session file.
#[derive(Debug)]
pub struct SessionWriter {
    path: PathBuf,
    file: File,
    next_seq: u64,
    pending: Vec<SessionRecord>,
}

impl SessionWriter {
    fn create(path: PathBuf, file: File) -> Result<Self, SessionError> {
        let mut writer = Self {
            path,
            file,
            next_seq: 0,
            pending: Vec::new(),
        };
        writer.write_header()?;
        Ok(writer)
    }

    fn resume(path: PathBuf, file: File, next_seq: u64) -> Self {
        Self {
            path,
            file,
            next_seq,
            pending: Vec::new(),
        }
    }

    /// Return the path being written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queue one record payload for the next flush and return its sequence number.
    pub fn append(&mut self, kind: SessionRecordKind) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.push(SessionRecord { seq, kind });
        seq
    }

    /// Queue several record payloads for the next flush.
    pub fn append_many(
        &mut self,
        records: impl IntoIterator<Item = SessionRecordKind>,
    ) -> Vec<u64> {
        records
            .into_iter()
            .map(|record| self.append(record))
            .collect()
    }

    /// Write all queued records as one zstd-compressed postcard chunk and sync it.
    pub fn flush(&mut self) -> Result<(), SessionError> {
        if self.pending.is_empty() {
            self.file.sync_data()?;
            return Ok(());
        }

        let chunk = encode_chunk(&self.pending)?;
        let chunk_start = self.file.stream_position()?;
        if let Err(error) = write_chunk_bytes(&mut self.file, &chunk) {
            let _ = self.file.set_len(chunk_start);
            let _ = self.file.seek(SeekFrom::Start(chunk_start));
            return Err(SessionError::Io(error));
        }
        self.pending.clear();
        Ok(())
    }

    fn write_header(&mut self) -> Result<(), SessionError> {
        self.file.write_all(FILE_MAGIC)?;
        self.file.write_all(&FORMAT_VERSION.to_le_bytes())?;
        Ok(())
    }
}

/// In-memory fast lookup index loaded from snapshot plus append log.
#[derive(Debug, Clone)]
pub struct SessionIndex {
    root: PathBuf,
    by_id: HashMap<SessionId, SessionIndexEntry>,
    by_title: HashMap<String, Vec<SessionId>>,
}

impl SessionIndex {
    /// Load the index snapshot and append log without scanning session files.
    pub fn load(root: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let root = root.into();
        let mut index = Self {
            root,
            by_id: HashMap::new(),
            by_title: HashMap::new(),
        };
        index.load_snapshot()?;
        index.load_log()?;
        Ok(index)
    }

    /// Append an entry to the index log and update this in-memory index.
    pub fn insert(&mut self, entry: SessionIndexEntry) -> Result<(), SessionError> {
        Self::append_entry_to_log(&self.root, &entry)?;
        self.apply_entry(entry);
        Ok(())
    }

    /// Return a verified summary by id.
    pub fn summary_by_id(
        &mut self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, SessionError> {
        let Some(entry) = self.by_id.get(session_id).cloned() else {
            return Ok(None);
        };
        if !self.verify_entry(session_id, &entry)? {
            self.remove_entry(session_id);
            return Ok(None);
        }
        Ok(Some(SessionSummary::from_entry(&self.root, &entry)))
    }

    /// Return the newest verified summary by title.
    pub fn summary_by_title(
        &mut self,
        title: &str,
    ) -> Result<Option<SessionSummary>, SessionError> {
        let Some(session_ids) = self.by_title.get(title).cloned() else {
            return Ok(None);
        };
        for session_id in session_ids.into_iter().rev() {
            if let Some(summary) = self.summary_by_id(&session_id)? {
                return Ok(Some(summary));
            }
        }
        Ok(None)
    }

    /// Return all verified session summaries sorted by newest indexed update first.
    pub fn summaries(&mut self) -> Result<Vec<SessionSummary>, SessionError> {
        let mut session_ids = self.by_id.keys().cloned().collect::<Vec<_>>();
        session_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let mut summaries = Vec::new();
        for session_id in session_ids {
            if let Some(summary) = self.summary_by_id(&session_id)? {
                summaries.push(summary);
            }
        }
        summaries.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
                .then_with(|| left.session_id.as_str().cmp(right.session_id.as_str()))
        });
        Ok(summaries)
    }

    /// Write a compact snapshot of the current in-memory index.
    pub fn write_snapshot(&self) -> Result<(), SessionError> {
        fs::create_dir_all(&self.root)?;
        let snapshot_path = self.root.join(INDEX_SNAPSHOT_FILE);
        let tmp_path = self.root.join(format!("{INDEX_SNAPSHOT_FILE}.tmp"));
        let snapshot = SessionIndexSnapshot {
            entries: self.by_id.values().cloned().collect(),
            title_candidates: self
                .by_title
                .iter()
                .map(|(title, session_ids)| SessionTitleCandidates {
                    title: title.clone(),
                    session_ids: session_ids.clone(),
                })
                .collect(),
        };
        let bytes = sonic_rs::to_vec(&snapshot)?;
        {
            let mut file = File::create(&tmp_path)?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_data()?;
        }
        fs::rename(tmp_path, snapshot_path)?;
        Ok(())
    }

    fn append_entry_to_log(root: &Path, entry: &SessionIndexEntry) -> Result<(), SessionError> {
        fs::create_dir_all(root)?;
        let path = root.join(INDEX_LOG_FILE);
        let bytes = sonic_rs::to_vec(entry)?;
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_data()?;
        Ok(())
    }

    fn load_snapshot(&mut self) -> Result<(), SessionError> {
        let path = self.root.join(INDEX_SNAPSHOT_FILE);
        if !path.exists() {
            return Ok(());
        }
        let bytes = fs::read(path)?;
        let snapshot: SessionIndexSnapshot = sonic_rs::from_slice(&bytes)?;
        for entry in snapshot.entries {
            self.by_id.insert(entry.session_id.clone(), entry);
        }
        for candidates in snapshot.title_candidates {
            let session_ids: Vec<_> = candidates
                .session_ids
                .into_iter()
                .filter(|session_id| self.by_id.contains_key(session_id))
                .collect();
            if !session_ids.is_empty() {
                self.by_title.insert(candidates.title, session_ids);
            }
        }
        Ok(())
    }

    fn load_log(&mut self) -> Result<(), SessionError> {
        let path = self.root.join(INDEX_LOG_FILE);
        if !path.exists() {
            return Ok(());
        }
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        loop {
            line.clear();
            let len = reader.read_until(b'\n', &mut line)?;
            if len == 0 {
                break;
            }
            let complete_line = line.last() == Some(&b'\n');
            if !complete_line {
                break;
            }
            while line
                .last()
                .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
            {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            let entry: SessionIndexEntry = sonic_rs::from_slice(&line)?;
            self.apply_entry(entry);
        }
        Ok(())
    }

    fn apply_entry(&mut self, entry: SessionIndexEntry) {
        if let Some(previous) = self.by_id.get(&entry.session_id) {
            if previous.title != entry.title {
                if let Some(previous_title) = &previous.title {
                    remove_title_candidate(&mut self.by_title, previous_title, &entry.session_id);
                }
            }
        }
        if let Some(title) = &entry.title {
            let session_ids = self.by_title.entry(title.clone()).or_default();
            session_ids.retain(|session_id| session_id != &entry.session_id);
            session_ids.push(entry.session_id.clone());
        }
        self.by_id.insert(entry.session_id.clone(), entry);
    }

    fn verify_entry(
        &self,
        session_id: &SessionId,
        entry: &SessionIndexEntry,
    ) -> Result<bool, SessionError> {
        let path = self.root.join(&entry.path);
        if !path.exists() {
            return Ok(false);
        }
        let store = SessionStore::new(&self.root);
        let Ok(meta) = store.read_meta(path) else {
            return Ok(false);
        };
        if &meta.id != session_id {
            return Ok(false);
        }
        Ok(true)
    }

    fn remove_entry(&mut self, session_id: &SessionId) {
        let Some(entry) = self.by_id.remove(session_id) else {
            return;
        };
        if let Some(title) = entry.title {
            remove_title_candidate(&mut self.by_title, &title, session_id);
        }
    }
}

/// Command accepted by the single-writer session actor.
#[derive(Debug)]
pub enum SessionCommand {
    /// Create a new session file and make it the active writer.
    Create {
        /// Metadata persisted as the first session record.
        meta: SessionMeta,
        /// Per-command reply channel.
        reply: ActorSender<SessionEvent>,
    },
    /// Resume an existing session file and make it the active writer.
    Resume {
        /// Session path to resume.
        path: PathBuf,
        /// Per-command reply channel.
        reply: ActorSender<SessionEvent>,
    },
    /// Append records to the active writer.
    Append {
        /// Record payloads to append.
        records: Vec<SessionRecordKind>,
        /// Whether to flush and sync after appending.
        flush: bool,
        /// Per-command reply channel.
        reply: ActorSender<SessionEvent>,
    },
    /// Flush and sync the active writer.
    Flush {
        /// Per-command reply channel.
        reply: ActorSender<SessionEvent>,
    },
    /// Load resume history from a session path.
    LoadLatestHistory {
        /// Session path to read.
        path: PathBuf,
        /// Per-command reply channel.
        reply: ActorSender<SessionEvent>,
    },
    /// Load one lazy reverse transcript page from a session path.
    LoadTranscriptPage {
        /// Session path to read.
        path: PathBuf,
        /// Exclusive sequence bound for the page. `None` starts at the newest entry.
        before_seq: Option<u64>,
        /// Maximum display entries to return.
        max_lines: usize,
        /// Per-command reply channel.
        reply: ActorSender<SessionEvent>,
    },
    /// Write a compact index snapshot.
    WriteIndexSnapshot {
        /// Per-command reply channel.
        reply: ActorSender<SessionEvent>,
    },
    /// Stop the actor.
    Shutdown,
}

/// Event returned by the session actor.
#[derive(Debug)]
pub enum SessionEvent {
    /// A session file was created and opened for append.
    Created {
        /// Metadata persisted for the session.
        meta: SessionMeta,
        /// Absolute session file path.
        path: PathBuf,
    },
    /// A session file was resumed and opened for append.
    Resumed {
        /// Resume history loaded from the session.
        history: LatestSessionHistory,
        /// Absolute session file path.
        path: PathBuf,
    },
    /// Records were appended to the active writer.
    Appended {
        /// Sequence numbers assigned by the writer.
        seqs: Vec<u64>,
    },
    /// The active writer was flushed.
    Flushed,
    /// Resume history was loaded without changing the active writer.
    LoadedLatestHistory {
        /// Resume history loaded from the session.
        history: LatestSessionHistory,
    },
    /// A lazy transcript page was loaded.
    TranscriptPageLoaded {
        /// Transcript page loaded from the session file.
        page: TranscriptPage,
    },
    /// The index snapshot was written.
    IndexSnapshotWritten,
    /// A session operation failed.
    Error {
        /// Human-readable error message.
        message: String,
    },
    /// The session actor stopped.
    ShutdownComplete,
}

/// Single-writer actor that isolates blocking session storage.
#[derive(Debug)]
pub struct SessionActor {
    store: SessionStore,
    writer: Option<SessionWriter>,
    rx: ActorReceiver<SessionCommand>,
}

impl SessionActor {
    /// Spawn a session actor rooted at the supplied store directory.
    pub fn spawn(root: impl Into<PathBuf>) -> ActorHandle<SessionCommand> {
        let (handle, rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
        let actor = Self {
            store: SessionStore::new(root),
            writer: None,
            rx,
        };
        tokio::spawn(actor.run());
        handle
    }

    async fn run(mut self) {
        while let Ok(command) = self.rx.recv().await {
            match command {
                SessionCommand::Create { meta, reply } => {
                    let event = match self.store.create_session(meta.clone()) {
                        Ok(writer) => {
                            let path = writer.path().to_path_buf();
                            self.writer = Some(writer);
                            SessionEvent::Created { meta, path }
                        }
                        Err(err) => SessionEvent::Error {
                            message: err.to_string(),
                        },
                    };
                    let _ = reply.send(event).await;
                }
                SessionCommand::Resume { path, reply } => {
                    let event = match self.store.load_latest_history(&path) {
                        Ok(history) => match self.store.resume_session(&path) {
                            Ok(writer) => {
                                self.writer = Some(writer);
                                SessionEvent::Resumed { history, path }
                            }
                            Err(err) => SessionEvent::Error {
                                message: err.to_string(),
                            },
                        },
                        Err(err) => SessionEvent::Error {
                            message: err.to_string(),
                        },
                    };
                    let _ = reply.send(event).await;
                }
                SessionCommand::Append {
                    records,
                    flush,
                    reply,
                } => {
                    let event = match self.writer.as_mut() {
                        Some(writer) => {
                            let seqs = writer.append_many(records);
                            if flush {
                                match writer.flush() {
                                    Ok(()) => SessionEvent::Appended { seqs },
                                    Err(err) => SessionEvent::Error {
                                        message: err.to_string(),
                                    },
                                }
                            } else {
                                SessionEvent::Appended { seqs }
                            }
                        }
                        None => SessionEvent::Error {
                            message: "no active session writer".to_string(),
                        },
                    };
                    let _ = reply.send(event).await;
                }
                SessionCommand::Flush { reply } => {
                    let event = match self.writer.as_mut() {
                        Some(writer) => match writer.flush() {
                            Ok(()) => SessionEvent::Flushed,
                            Err(err) => SessionEvent::Error {
                                message: err.to_string(),
                            },
                        },
                        None => SessionEvent::Error {
                            message: "no active session writer".to_string(),
                        },
                    };
                    let _ = reply.send(event).await;
                }
                SessionCommand::LoadLatestHistory { path, reply } => {
                    let event = match self.store.load_latest_history(path) {
                        Ok(history) => SessionEvent::LoadedLatestHistory { history },
                        Err(err) => SessionEvent::Error {
                            message: err.to_string(),
                        },
                    };
                    let _ = reply.send(event).await;
                }
                SessionCommand::LoadTranscriptPage {
                    path,
                    before_seq,
                    max_lines,
                    reply,
                } => {
                    let event = match self.store.read_transcript_page(path, before_seq, max_lines) {
                        Ok(page) => SessionEvent::TranscriptPageLoaded { page },
                        Err(err) => SessionEvent::Error {
                            message: err.to_string(),
                        },
                    };
                    let _ = reply.send(event).await;
                }
                SessionCommand::WriteIndexSnapshot { reply } => {
                    let event = match SessionIndex::load(self.store.root()) {
                        Ok(index) => match index.write_snapshot() {
                            Ok(()) => SessionEvent::IndexSnapshotWritten,
                            Err(err) => SessionEvent::Error {
                                message: err.to_string(),
                            },
                        },
                        Err(err) => SessionEvent::Error {
                            message: err.to_string(),
                        },
                    };
                    let _ = reply.send(event).await;
                }
                SessionCommand::Shutdown => break,
            }
        }
    }
}

impl Drop for SessionActor {
    fn drop(&mut self) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.flush();
        }
    }
}

fn encode_chunk(records: &[SessionRecord]) -> Result<Vec<u8>, SessionError> {
    let first_seq = records.first().map_or(0, |record| record.seq);
    let record_count = u32::try_from(records.len())
        .map_err(|_| SessionError::ChunkTooLarge { len: records.len() })?;
    let payload = postcard::to_allocvec(records)?;
    let compressed = zstd::bulk::compress(&payload, ZSTD_LEVEL)?;
    let compressed_len =
        u32::try_from(compressed.len()).map_err(|_| SessionError::ChunkTooLarge {
            len: compressed.len(),
        })?;
    let uncompressed_len = u32::try_from(payload.len())
        .map_err(|_| SessionError::ChunkTooLarge { len: payload.len() })?;

    let mut chunk = Vec::with_capacity(CHUNK_HEADER_LEN + compressed.len());
    chunk.extend_from_slice(&compressed_len.to_le_bytes());
    chunk.extend_from_slice(&uncompressed_len.to_le_bytes());
    chunk.extend_from_slice(&first_seq.to_le_bytes());
    chunk.extend_from_slice(&record_count.to_le_bytes());
    chunk.extend_from_slice(&compressed);
    Ok(chunk)
}

fn write_chunk_bytes(file: &mut File, chunk: &[u8]) -> Result<(), io::Error> {
    file.write_all(chunk)?;
    file.flush()?;
    file.sync_data()?;
    Ok(())
}

fn read_session_records(path: &Path) -> Result<Vec<SessionRecord>, SessionError> {
    Ok(read_session_file(path)?.records)
}

fn read_first_chunk_records(path: &Path) -> Result<Vec<SessionRecord>, SessionError> {
    let mut file = File::open(path)?;
    read_header(&mut file)?;
    read_chunk_at_current_position(&mut file)?.ok_or(SessionError::MissingSessionMeta)
}

fn read_session_file(path: &Path) -> Result<SessionFileRead, SessionError> {
    let mut records = Vec::new();
    let mut complete_len = FILE_MAGIC.len() as u64 + 2;
    let chunks = read_session_chunk_index(path)?;
    for (index, chunk) in chunks.iter().copied().enumerate() {
        let Some(chunk_records) =
            read_indexed_chunk_allow_incomplete_final(path, chunk, index + 1 == chunks.len())?
        else {
            break;
        };
        records.extend(chunk_records);
        complete_len = chunk.payload_offset + chunk.compressed_len as u64;
    }
    Ok(SessionFileRead {
        records,
        complete_len,
    })
}
fn read_startup_binding_records(
    path: &Path,
) -> Result<
    (
        SessionMeta,
        Option<crate::provider_runtime::ProviderSessionBinding>,
    ),
    SessionError,
> {
    let chunks = read_session_chunk_index(path)?;
    let mut meta = None;
    let mut binding = None;

    for (index, chunk) in chunks.iter().copied().enumerate() {
        let Some(records) =
            read_indexed_chunk_allow_incomplete_final(path, chunk, index + 1 == chunks.len())?
        else {
            break;
        };
        for record in records {
            match record.kind {
                SessionRecordKind::SessionMeta(record_meta) if meta.is_none() => {
                    meta = Some(record_meta);
                }
                SessionRecordKind::ProviderSessionBinding(record) if binding.is_none() => {
                    binding = Some(record.binding);
                }
                _ => {}
            }
            if meta.is_some() && binding.is_some() {
                break;
            }
        }
        if meta.is_some() && binding.is_some() {
            break;
        }
    }

    let Some(meta) = meta else {
        return Err(SessionError::MissingSessionMeta);
    };
    Ok((meta, binding))
}

fn read_latest_message_preview(path: &Path) -> Result<Option<String>, SessionError> {
    let chunks = read_session_chunk_index(path)?;
    for (index, chunk) in chunks.iter().copied().enumerate().rev() {
        let Some(records) =
            read_indexed_chunk_allow_incomplete_final(path, chunk, index + 1 == chunks.len())?
        else {
            continue;
        };
        if let Some(preview) = records.into_iter().rev().find_map(|record| {
            session_record_message_text(&record.kind).map(session_message_preview)
        }) {
            return Ok(Some(preview));
        }
    }
    Ok(None)
}

fn read_transcript_page(
    path: &Path,
    before_seq: Option<u64>,
    max_lines: usize,
) -> Result<TranscriptPage, SessionError> {
    let chunk_index = read_session_chunk_index(path)?;
    let mut lines = Vec::new();
    let mut reached_start = true;

    for (index, chunk) in chunk_index.iter().copied().enumerate().rev() {
        if before_seq.is_some_and(|before_seq| chunk.first_seq >= before_seq) {
            continue;
        }

        let records = match read_indexed_chunk_allow_incomplete_final(
            path,
            chunk,
            index + 1 == chunk_index.len(),
        ) {
            Ok(Some(records)) => records,
            Ok(None) => continue,
            Err(_) => {
                reached_start = false;
                continue;
            }
        };
        for record in records.into_iter().rev() {
            if before_seq.is_some_and(|before_seq| record.seq >= before_seq) {
                continue;
            }
            if !session_record_is_transcript_entry(&record.kind) {
                continue;
            }
            if lines.len() == max_lines {
                reached_start = false;
                break;
            }
            lines.push(TranscriptPageLine {
                seq: record.seq,
                kind: record.kind,
            });
        }

        if !reached_start {
            break;
        }
    }

    lines.reverse();
    let next_before_seq = lines.first().map(|line| line.seq);
    Ok(TranscriptPage {
        lines,
        next_before_seq,
        reached_start,
    })
}
fn read_resume_transcript(
    path: &Path,
    minimum_message_count: usize,
    page_line_limit: usize,
) -> Result<TranscriptPage, SessionError> {
    let mut before_seq = None;
    let mut lines = Vec::new();

    loop {
        let page = read_transcript_page(path, before_seq, page_line_limit)?;
        let page_reached_start = page.reached_start;
        let page_next_before_seq = page.next_before_seq;
        lines.splice(0..0, page.lines);
        let message_count = lines
            .iter()
            .filter(|line| session_record_is_message(&line.kind))
            .count();
        if page_reached_start
            || message_count >= minimum_message_count
            || page_next_before_seq.is_none()
        {
            return Ok(TranscriptPage {
                lines,
                next_before_seq: page_next_before_seq,
                reached_start: page_reached_start,
            });
        }
        before_seq = page_next_before_seq;
    }
}

fn read_session_chunk_index(path: &Path) -> Result<Vec<SessionChunkIndexEntry>, SessionError> {
    let mut file = File::open(path)?;
    read_header(&mut file)?;
    let mut chunks = Vec::new();
    loop {
        let header_offset = file.stream_position()?;
        let mut chunk_header = [0u8; CHUNK_HEADER_LEN];
        if !read_exact_or_ignore_tail(&mut file, &mut chunk_header)? {
            break;
        }
        let compressed_len =
            u32::from_le_bytes(chunk_header[0..4].try_into().expect("len")) as usize;
        let uncompressed_len =
            u32::from_le_bytes(chunk_header[4..8].try_into().expect("len")) as usize;
        let first_seq = u64::from_le_bytes(chunk_header[8..16].try_into().expect("len"));
        let record_count = u32::from_le_bytes(chunk_header[16..20].try_into().expect("len"));
        let payload_offset = file.stream_position()?;
        let remaining_len = file.metadata()?.len().saturating_sub(payload_offset);
        if compressed_len as u64 > remaining_len {
            break;
        }
        chunks.push(SessionChunkIndexEntry {
            header_offset,
            payload_offset,
            compressed_len,
            uncompressed_len,
            first_seq,
            record_count,
        });
        file.seek(SeekFrom::Start(payload_offset + compressed_len as u64))?;
    }
    Ok(chunks)
}

fn read_indexed_chunk(
    path: &Path,
    chunk: SessionChunkIndexEntry,
) -> Result<Vec<SessionRecord>, SessionError> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(chunk.payload_offset))?;
    let mut compressed = vec![0u8; chunk.compressed_len];
    file.read_exact(&mut compressed)?;
    let payload = zstd::bulk::decompress(&compressed, chunk.uncompressed_len)?;
    decode_chunk_records(&payload, chunk.record_count, chunk.first_seq)
}
fn read_indexed_chunk_allow_incomplete_final(
    path: &Path,
    chunk: SessionChunkIndexEntry,
    allow_incomplete_final_chunk: bool,
) -> Result<Option<Vec<SessionRecord>>, SessionError> {
    match read_indexed_chunk(path, chunk) {
        Ok(records) => Ok(Some(records)),
        Err(error)
            if allow_incomplete_final_chunk
                && matches!(error, SessionError::Postcard(_) | SessionError::Io(_)) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
fn inspect_session_chunks(path: &Path) -> Result<Vec<SessionChunkReport>, SessionError> {
    let chunks = read_session_chunk_index(path)?;
    let mut reports = Vec::with_capacity(chunks.len());
    for (index, chunk) in chunks.iter().copied().enumerate() {
        match read_indexed_chunk(path, chunk) {
            Ok(_) => reports.push(SessionChunkReport {
                index,
                header_offset: chunk.header_offset,
                payload_offset: chunk.payload_offset,
                compressed_len: chunk.compressed_len,
                uncompressed_len: chunk.uncompressed_len,
                first_seq: chunk.first_seq,
                record_count: chunk.record_count,
                decodes: true,
                error: None,
            }),
            Err(error) => reports.push(SessionChunkReport {
                index,
                header_offset: chunk.header_offset,
                payload_offset: chunk.payload_offset,
                compressed_len: chunk.compressed_len,
                uncompressed_len: chunk.uncompressed_len,
                first_seq: chunk.first_seq,
                record_count: chunk.record_count,
                decodes: false,
                error: Some(error.to_string()),
            }),
        }
    }
    Ok(reports)
}
fn probe_session_chunk(path: &Path, chunk_index: usize) -> Result<SessionChunkProbe, SessionError> {
    let chunks = read_session_chunk_index(path)?;
    let chunk = chunks.get(chunk_index).copied().ok_or_else(|| {
        SessionError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk index out of range",
        ))
    })?;
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(chunk.payload_offset))?;
    let mut compressed = vec![0u8; chunk.compressed_len];
    file.read_exact(&mut compressed)?;
    let payload = zstd::bulk::decompress(&compressed, chunk.uncompressed_len);
    let (
        decompresses,
        current_decodes,
        legacy_decodes,
        current_error,
        legacy_error,
        payload_prefix_hex,
        payload_prefix_ascii,
        current_prefix_record_count,
        current_failure_remainder_hex,
        current_failure_remainder_ascii,
    ) = match payload {
        Ok(payload) => {
            let prefix = &payload[..payload.len().min(128)];
            let payload_prefix_hex = prefix
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            let payload_prefix_ascii = prefix
                .iter()
                .map(|byte| {
                    let character = char::from(*byte);
                    if character.is_ascii_graphic() || character == ' ' {
                        character
                    } else {
                        '.'
                    }
                })
                .collect::<String>();
            let (
                current_prefix_record_count,
                current_failure_remainder_hex,
                current_failure_remainder_ascii,
            ) = current_decode_prefix(&payload);
            let current = postcard::from_bytes::<Vec<SessionRecord>>(&payload)
                .map_err(|error| error.to_string())
                .and_then(|records| {
                    validate_chunk_records(&records, chunk.record_count, chunk.first_seq)
                        .map_err(|error| error.to_string())?;
                    Ok(records)
                });
            let legacy = postcard::from_bytes::<Vec<ProviderBoundSessionRecord>>(&payload)
                .map_err(|error| error.to_string())
                .and_then(|records| {
                    if records.len() != chunk.record_count as usize {
                        return Err(format!(
                            "legacy record count mismatch: declared={} decoded={}",
                            chunk.record_count,
                            records.len()
                        ));
                    }
                    if let Some(first_record) = records.first()
                        && first_record.seq != chunk.first_seq
                    {
                        return Err(format!(
                            "legacy first seq mismatch: declared={} decoded={}",
                            chunk.first_seq, first_record.seq
                        ));
                    }
                    Ok(records)
                });
            (
                true,
                current.is_ok(),
                legacy.is_ok(),
                current.as_ref().err().map(ToString::to_string),
                legacy.as_ref().err().map(ToString::to_string),
                Some(payload_prefix_hex),
                Some(payload_prefix_ascii),
                Some(current_prefix_record_count),
                current_failure_remainder_hex,
                current_failure_remainder_ascii,
            )
        }
        Err(error) => (
            false,
            false,
            false,
            Some(error.to_string()),
            Some(error.to_string()),
            None,
            None,
            None,
            None,
            None,
        ),
    };
    Ok(SessionChunkProbe {
        index: chunk_index,
        header_offset: chunk.header_offset,
        payload_offset: chunk.payload_offset,
        compressed_len: chunk.compressed_len,
        uncompressed_len: chunk.uncompressed_len,
        first_seq: chunk.first_seq,
        record_count: chunk.record_count,
        decompresses,
        current_decodes,
        legacy_decodes,
        current_error,
        legacy_error,
        payload_prefix_hex,
        payload_prefix_ascii,
        current_prefix_record_count,
        current_failure_remainder_hex,
        current_failure_remainder_ascii,
    })
}

fn current_decode_prefix(payload: &[u8]) -> (usize, Option<String>, Option<String>) {
    let Ok((record_count, mut remaining)) = postcard::take_from_bytes::<usize>(payload) else {
        return (0, None, None);
    };
    let mut decoded = 0usize;
    while decoded < record_count {
        match postcard::take_from_bytes::<SessionRecord>(remaining) {
            Ok((_, next)) => {
                decoded += 1;
                remaining = next;
            }
            Err(_) => {
                let prefix = &remaining[..remaining.len().min(128)];
                let hex = prefix
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                let ascii = prefix
                    .iter()
                    .map(|byte| {
                        let character = char::from(*byte);
                        if character.is_ascii_graphic() || character == ' ' {
                            character
                        } else {
                            '.'
                        }
                    })
                    .collect::<String>();
                return (decoded, Some(hex), Some(ascii));
            }
        }
    }
    (decoded, None, None)
}
fn read_chunk_at_current_position(
    file: &mut File,
) -> Result<Option<Vec<SessionRecord>>, SessionError> {
    let mut chunk_header = [0u8; CHUNK_HEADER_LEN];
    if !read_exact_or_ignore_tail(file, &mut chunk_header)? {
        return Ok(None);
    }
    let compressed_len = u32::from_le_bytes(chunk_header[0..4].try_into().expect("len")) as usize;
    let uncompressed_len = u32::from_le_bytes(chunk_header[4..8].try_into().expect("len")) as usize;
    let first_seq = u64::from_le_bytes(chunk_header[8..16].try_into().expect("len"));
    let record_count = u32::from_le_bytes(chunk_header[16..20].try_into().expect("len"));
    let current_position = file.stream_position()?;
    let remaining_len = file.metadata()?.len().saturating_sub(current_position);
    if compressed_len as u64 > remaining_len {
        return Ok(None);
    }
    let mut compressed = vec![0u8; compressed_len];
    if !read_exact_or_ignore_tail(file, &mut compressed)? {
        return Ok(None);
    }
    let payload = zstd::bulk::decompress(&compressed, uncompressed_len)?;
    let chunk_records = decode_chunk_records(&payload, record_count, first_seq)?;
    Ok(Some(chunk_records))
}

fn session_record_is_message(kind: &SessionRecordKind) -> bool {
    matches!(
        kind,
        SessionRecordKind::UserMessage(_)
            | SessionRecordKind::DeveloperMessage(_)
            | SessionRecordKind::AssistantMessage(_)
    )
}
fn decode_chunk_records(
    payload: &[u8],
    record_count: u32,
    first_seq: u64,
) -> Result<Vec<SessionRecord>, SessionError> {
    match postcard::from_bytes::<Vec<SessionRecord>>(payload) {
        Ok(records) => {
            validate_chunk_records(&records, record_count, first_seq)?;
            Ok(records)
        }
        Err(current_error) => {
            if let Ok(provider_bound_records) =
                postcard::from_bytes::<Vec<ProviderBoundSessionRecord>>(payload)
            {
                if provider_bound_records.len() != record_count as usize {
                    return Err(SessionError::ChunkRecordCount {
                        declared: record_count,
                        decoded: provider_bound_records.len(),
                    });
                }
                if let Some(first_record) = provider_bound_records.first()
                    && first_record.seq != first_seq
                {
                    return Err(SessionError::ChunkFirstSeq {
                        declared: first_seq,
                        decoded: first_record.seq,
                    });
                }
                return Ok(provider_bound_records
                    .into_iter()
                    .flat_map(ProviderBoundSessionRecord::into_current)
                    .collect());
            }

            let legacy_tool_output_records =
                postcard::from_bytes::<Vec<LegacyToolOutputSessionRecord>>(payload)
                    .map_err(|_| SessionError::Postcard(current_error))?;
            if legacy_tool_output_records.len() != record_count as usize {
                return Err(SessionError::ChunkRecordCount {
                    declared: record_count,
                    decoded: legacy_tool_output_records.len(),
                });
            }
            if let Some(first_record) = legacy_tool_output_records.first()
                && first_record.seq != first_seq
            {
                return Err(SessionError::ChunkFirstSeq {
                    declared: first_seq,
                    decoded: first_record.seq,
                });
            }
            Ok(legacy_tool_output_records
                .into_iter()
                .map(LegacyToolOutputSessionRecord::into_current)
                .collect())
        }
    }
}

fn validate_chunk_records(
    records: &[SessionRecord],
    record_count: u32,
    first_seq: u64,
) -> Result<(), SessionError> {
    if records.len() != record_count as usize {
        return Err(SessionError::ChunkRecordCount {
            declared: record_count,
            decoded: records.len(),
        });
    }
    if let Some(first_record) = records.first()
        && first_record.seq != first_seq
    {
        return Err(SessionError::ChunkFirstSeq {
            declared: first_seq,
            decoded: first_record.seq,
        });
    }
    Ok(())
}

fn session_record_message_text(kind: &SessionRecordKind) -> Option<&str> {
    match kind {
        SessionRecordKind::UserMessage(message)
        | SessionRecordKind::DeveloperMessage(message)
        | SessionRecordKind::AssistantMessage(message) => Some(&message.text),
        SessionRecordKind::SessionMeta(_)
        | SessionRecordKind::TurnContext(_)
        | SessionRecordKind::FreeformToolCall(_)
        | SessionRecordKind::FreeformToolInputDelta(_)
        | SessionRecordKind::FreeformToolOutput(_)
        | SessionRecordKind::FunctionToolCall(_)
        | SessionRecordKind::FunctionToolOutput(_)
        | SessionRecordKind::CompactionCheckpoint(_)
        | SessionRecordKind::SessionClosed(_)
        | SessionRecordKind::ProviderSessionBinding(_) => None,
    }
}

fn session_message_preview(message: &str) -> String {
    let mut preview = message.chars().take(80).collect::<String>();
    if message.chars().count() > 80 {
        preview.push('…');
    }
    preview
}

fn session_record_is_transcript_entry(kind: &SessionRecordKind) -> bool {
    match kind {
        SessionRecordKind::UserMessage(_)
        | SessionRecordKind::DeveloperMessage(_)
        | SessionRecordKind::AssistantMessage(_)
        | SessionRecordKind::FreeformToolCall(_)
        | SessionRecordKind::FreeformToolOutput(_)
        | SessionRecordKind::FunctionToolCall(_)
        | SessionRecordKind::FunctionToolOutput(_)
        | SessionRecordKind::SessionClosed(_) => true,
        SessionRecordKind::SessionMeta(_)
        | SessionRecordKind::TurnContext(_)
        | SessionRecordKind::FreeformToolInputDelta(_)
        | SessionRecordKind::CompactionCheckpoint(_)
        | SessionRecordKind::ProviderSessionBinding(_) => false,
    }
}

fn read_header(reader: &mut File) -> Result<(), SessionError> {
    let mut magic = vec![0u8; FILE_MAGIC.len()];
    reader.read_exact(&mut magic)?;
    if magic != FILE_MAGIC {
        return Err(SessionError::InvalidMagic);
    }
    let mut version = [0u8; 2];
    reader.read_exact(&mut version)?;
    let version = u16::from_le_bytes(version);
    if version != FORMAT_VERSION {
        return Err(SessionError::UnsupportedVersion(version));
    }
    Ok(())
}

fn read_exact_or_ignore_tail(
    reader: &mut File,
    mut buffer: &mut [u8],
) -> Result<bool, SessionError> {
    let mut read_any = false;
    while !buffer.is_empty() {
        match reader.read(buffer) {
            Ok(0) => return Ok(false),
            Ok(read) => {
                read_any = true;
                let remaining = buffer;
                buffer = &mut remaining[read..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(SessionError::Io(error)),
        }
    }
    Ok(read_any)
}

fn first_meta(records: &[SessionRecord]) -> Result<&SessionMeta, SessionError> {
    match records.first() {
        Some(SessionRecord {
            kind: SessionRecordKind::SessionMeta(meta),
            ..
        }) => Ok(meta),
        _ => Err(SessionError::MissingSessionMeta),
    }
}

fn session_relative_path(session_id: &SessionId, created_at_ms: u64) -> PathBuf {
    let (year, month, day) = ymd_from_unix_ms(created_at_ms);
    PathBuf::from("sessions")
        .join(format!("{year:04}"))
        .join(format!("{month:02}"))
        .join(format!("{day:02}"))
        .join(format!(
            "{}.{}",
            session_id.as_str(),
            SESSION_FILE_EXTENSION
        ))
}

fn path_to_index_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn remove_title_candidate(
    by_title: &mut HashMap<String, Vec<SessionId>>,
    title: &str,
    session_id: &SessionId,
) {
    if let Some(session_ids) = by_title.get_mut(title) {
        session_ids.retain(|indexed_session_id| indexed_session_id != session_id);
        if session_ids.is_empty() {
            by_title.remove(title);
        }
    }
}

fn ymd_from_unix_ms(ms: u64) -> (i32, u32, u32) {
    let days = (ms / 1000 / 86_400) as i64;
    civil_from_days(days)
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    #[ignore]
    fn inspect_nhsession_model_visible_sizes() {
        let path = std::env::var("NHSESSION_PATH").expect("NHSESSION_PATH");
        let records = read_session_records(Path::new(&path)).expect("read session");
        let mut sizes = Vec::new();
        for record in &records {
            let Some((kind, size)) = (match &record.kind {
                SessionRecordKind::UserMessage(message) => {
                    Some(("user", message.text.chars().count()))
                }
                SessionRecordKind::DeveloperMessage(message) => {
                    Some(("developer", message.text.chars().count()))
                }
                SessionRecordKind::AssistantMessage(message) => {
                    Some(("assistant", message.text.chars().count()))
                }
                SessionRecordKind::FreeformToolCall(call) => {
                    Some(("freeform-call", call.input.chars().count()))
                }
                SessionRecordKind::FreeformToolOutput(output) => {
                    Some(("freeform-output", output.output.chars().count()))
                }
                SessionRecordKind::FunctionToolCall(call) => {
                    Some(("function-call", call.arguments.chars().count()))
                }
                SessionRecordKind::FunctionToolOutput(output) => {
                    Some(("function-output", output.output.chars().count()))
                }
                _ => None,
            }) else {
                continue;
            };
            sizes.push((size, record.seq, kind));
        }
        let total: usize = sizes.iter().map(|(size, _, _)| *size).sum();
        sizes.sort_by_key(|(size, _, _)| *size);
        eprintln!("visible records: {}", sizes.len());
        eprintln!("visible chars: {total}");
        for (size, seq, kind) in sizes.iter().rev().take(20) {
            eprintln!("{seq} {kind} chars={size}");
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "new-harness-session-{name}-{}-{now}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temp root");
        root
    }

    fn test_meta(id: &str, title: Option<&str>) -> SessionMeta {
        SessionMeta {
            id: SessionId::new(id),
            parent_session_id: None,
            forked_from_session_id: None,
            created_at_ms: 1_717_171_717_000,
            updated_at_ms: 1_717_171_717_000,
            cwd: "/tmp/project".to_string(),
            model: "gpt-test".to_string(),
            originator: "new_harness".to_string(),
            client_version: "0.1.0".to_string(),
            source: "test".to_string(),
            title: title.map(str::to_string),
            preview: Some("preview".to_string()),
        }
    }

    fn append_complete_corrupt_chunk(path: &Path) {
        append_complete_corrupt_chunk_with_first_seq(path, 1);
    }

    fn append_complete_corrupt_chunk_with_first_seq(path: &Path, first_seq: u64) {
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(&1u32.to_le_bytes()).unwrap();
        file.write_all(&1u32.to_le_bytes()).unwrap();
        file.write_all(&first_seq.to_le_bytes()).unwrap();
        file.write_all(&1u32.to_le_bytes()).unwrap();
        file.write_all(&[0]).unwrap();
        file.flush().unwrap();
    }
    fn append_complete_postcard_truncated_chunk(path: &Path, first_seq: u64) {
        let records = vec![SessionRecord {
            seq: first_seq,
            kind: SessionRecordKind::UserMessage(MessageRecord {
                text: "truncated".to_string(),
            }),
        }];
        let mut payload = postcard::to_allocvec(&records).unwrap();
        payload.pop().unwrap();
        let compressed = zstd::bulk::compress(&payload, ZSTD_LEVEL).unwrap();

        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(&(compressed.len() as u32).to_le_bytes())
            .unwrap();
        file.write_all(&(payload.len() as u32).to_le_bytes())
            .unwrap();
        file.write_all(&first_seq.to_le_bytes()).unwrap();
        file.write_all(&1u32.to_le_bytes()).unwrap();
        file.write_all(&compressed).unwrap();
        file.flush().unwrap();
    }

    fn write_serialized_session_chunk<T: Serialize>(
        path: &Path,
        records: &T,
        first_seq: u64,
        record_count: u32,
    ) {
        let payload = postcard::to_allocvec(records).unwrap();
        let compressed = zstd::bulk::compress(&payload, ZSTD_LEVEL).unwrap();
        let mut file = File::create(path).unwrap();
        file.write_all(FILE_MAGIC).unwrap();
        file.write_all(&FORMAT_VERSION.to_le_bytes()).unwrap();
        file.write_all(&(compressed.len() as u32).to_le_bytes())
            .unwrap();
        file.write_all(&(payload.len() as u32).to_le_bytes())
            .unwrap();
        file.write_all(&first_seq.to_le_bytes()).unwrap();
        file.write_all(&record_count.to_le_bytes()).unwrap();
        file.write_all(&compressed).unwrap();
        file.flush().unwrap();
    }

    #[test]
    fn session_store_writes_and_reads_compressed_session() {
        let root = temp_root("roundtrip");
        let store = SessionStore::new(&root);
        let mut writer = store
            .create_session(test_meta("s1", Some("title")))
            .unwrap();
        writer.append_many([
            SessionRecordKind::UserMessage(MessageRecord {
                text: "hello".to_string(),
            }),
            SessionRecordKind::AssistantMessage(MessageRecord {
                text: "world".to_string(),
            }),
        ]);
        writer.flush().unwrap();

        let records = store.read_records(writer.path()).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].seq, 0);
        assert_eq!(records[1].seq, 1);
        assert_eq!(records[2].seq, 2);
        assert!(matches!(records[0].kind, SessionRecordKind::SessionMeta(_)));
        assert_eq!(
            records[1].kind,
            SessionRecordKind::UserMessage(MessageRecord {
                text: "hello".to_string()
            })
        );
    }

    #[test]
    fn session_store_exports_jsonl() {
        let root = temp_root("export-jsonl");
        let store = SessionStore::new(&root);
        let mut writer = store.create_session(test_meta("export", None)).unwrap();
        writer.append(SessionRecordKind::UserMessage(MessageRecord {
            text: "exported".to_string(),
        }));
        writer.flush().unwrap();

        let mut jsonl = Vec::new();
        store.export_jsonl(writer.path(), &mut jsonl).unwrap();
        let records: Vec<SessionRecord> = jsonl
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| sonic_rs::from_slice(line).unwrap())
            .collect();

        assert_eq!(records.len(), 2);
        assert_eq!(
            records[1].kind,
            SessionRecordKind::UserMessage(MessageRecord {
                text: "exported".to_string()
            })
        );
    }

    #[test]
    fn session_store_reads_latest_message_preview() {
        let root = temp_root("latest-message-preview");
        let store = SessionStore::new(&root);
        let mut writer = store.create_session(test_meta("latest", None)).unwrap();
        writer.append(SessionRecordKind::UserMessage(MessageRecord {
            text: "first message".to_string(),
        }));
        writer.append(SessionRecordKind::FunctionToolOutput(
            FunctionToolOutputRecord {
                call_id: "call-1".to_string(),
                output: "newer tool output".to_string(),
                display_output: None,
            },
        ));
        writer.append(SessionRecordKind::AssistantMessage(MessageRecord {
            text: "latest message".to_string(),
        }));
        writer.flush().unwrap();

        let preview = store.read_latest_message_preview(writer.path()).unwrap();

        assert_eq!(preview, Some("latest message".to_string()));
    }

    #[test]
    fn session_index_finds_session_without_scanning_sessions() {
        let root = temp_root("index-by-id");
        let store = SessionStore::new(&root);
        let writer = store
            .create_session(test_meta("indexed", Some("lookup")))
            .unwrap();

        let mut index = SessionIndex::load(&root).unwrap();
        let summary = index
            .summary_by_id(&SessionId::new("indexed"))
            .unwrap()
            .unwrap();
        assert_eq!(summary.session_id, SessionId::new("indexed"));
        assert_eq!(summary.path, writer.path());
    }

    #[test]
    fn session_index_verifies_meta_without_reading_later_chunks() {
        let root = temp_root("index-meta-only");
        let store = SessionStore::new(&root);
        let writer = store
            .create_session(test_meta("meta-only", Some("lookup")))
            .unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);
        append_complete_corrupt_chunk(&path);

        let mut index = SessionIndex::load(&root).unwrap();
        let summary = index
            .summary_by_id(&SessionId::new("meta-only"))
            .unwrap()
            .unwrap();
        assert_eq!(summary.session_id, SessionId::new("meta-only"));
        assert!(store.read_records(&path).is_err());
    }

    #[test]
    fn session_index_title_lookup_uses_newest_entry() {
        let root = temp_root("title-newest");
        let store = SessionStore::new(&root);
        store
            .create_session(test_meta("old-session", Some("same title")))
            .unwrap();
        let new_writer = store
            .create_session(test_meta("new-session", Some("same title")))
            .unwrap();

        let mut index = SessionIndex::load(&root).unwrap();
        let summary = index.summary_by_title("same title").unwrap().unwrap();
        assert_eq!(summary.session_id, SessionId::new("new-session"));
        assert_eq!(summary.path, new_writer.path());
    }

    #[test]
    fn session_index_summaries_return_verified_newest_first() {
        let root = temp_root("summaries-newest");
        let store = SessionStore::new(&root);
        let old_writer = store
            .create_session(test_meta("old-session", Some("old title")))
            .unwrap();
        let new_writer = store
            .create_session(test_meta("new-session", Some("new title")))
            .unwrap();
        let old_relative_path = old_writer
            .path()
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let new_relative_path = new_writer
            .path()
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let mut index = SessionIndex::load(&root).unwrap();
        index
            .insert(SessionIndexEntry {
                session_id: SessionId::new("old-session"),
                path: old_relative_path.clone(),
                created_at_ms: 1,
                updated_at_ms: 10,
                cwd: "/tmp/project".to_string(),
                title: Some("old title".to_string()),
                preview: None,
                parent_session_id: None,
                forked_from_session_id: None,
            })
            .unwrap();
        index
            .insert(SessionIndexEntry {
                session_id: SessionId::new("new-session"),
                path: new_relative_path,
                created_at_ms: 2,
                updated_at_ms: 20,
                cwd: "/tmp/project".to_string(),
                title: Some("new title".to_string()),
                preview: None,
                parent_session_id: None,
                forked_from_session_id: None,
            })
            .unwrap();
        index
            .insert(SessionIndexEntry {
                session_id: SessionId::new("invalid-session"),
                path: old_relative_path,
                created_at_ms: 3,
                updated_at_ms: 30,
                cwd: "/tmp/project".to_string(),
                title: Some("invalid title".to_string()),
                preview: None,
                parent_session_id: None,
                forked_from_session_id: None,
            })
            .unwrap();

        let summaries = index.summaries().unwrap();

        assert_eq!(
            summaries
                .iter()
                .map(|summary| summary.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["new-session", "old-session"]
        );
    }

    #[test]
    fn session_index_snapshot_preserves_title_candidate_order() {
        let root = temp_root("snapshot-title-order");
        let store = SessionStore::new(&root);
        store
            .create_session(test_meta("old-session", Some("same title")))
            .unwrap();
        store
            .create_session(test_meta("new-session", Some("same title")))
            .unwrap();

        let index = SessionIndex::load(&root).unwrap();
        index.write_snapshot().unwrap();
        fs::remove_file(root.join(INDEX_LOG_FILE)).unwrap();

        let mut reloaded = SessionIndex::load(&root).unwrap();
        let summary = reloaded.summary_by_title("same title").unwrap().unwrap();
        assert_eq!(summary.session_id, SessionId::new("new-session"));
    }

    #[test]
    fn session_index_title_update_removes_old_title() {
        let root = temp_root("title-update");
        let store = SessionStore::new(&root);
        let writer = store
            .create_session(test_meta("renamed-session", Some("old title")))
            .unwrap();
        let relative_path = writer
            .path()
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut index = SessionIndex::load(&root).unwrap();
        index
            .insert(SessionIndexEntry {
                session_id: SessionId::new("renamed-session"),
                path: relative_path,
                created_at_ms: 1,
                updated_at_ms: 2,
                cwd: "/tmp/project".to_string(),
                title: Some("new title".to_string()),
                preview: None,
                parent_session_id: None,
                forked_from_session_id: None,
            })
            .unwrap();

        assert_eq!(index.summary_by_title("old title").unwrap(), None);
        let summary = index.summary_by_title("new title").unwrap().unwrap();
        assert_eq!(summary.session_id, SessionId::new("renamed-session"));
    }

    #[test]
    fn session_reader_ignores_incomplete_tail() {
        let root = temp_root("tail");
        let store = SessionStore::new(&root);
        let mut writer = store.create_session(test_meta("tail", None)).unwrap();
        writer.append(SessionRecordKind::UserMessage(MessageRecord {
            text: "durable".to_string(),
        }));
        writer.flush().unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[1, 2, 3]).unwrap();
        file.flush().unwrap();

        let records = store.read_records(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[1].kind,
            SessionRecordKind::UserMessage(MessageRecord {
                text: "durable".to_string()
            })
        );
    }

    #[test]
    fn resume_session_appends_after_existing_records() {
        let root = temp_root("resume");
        let store = SessionStore::new(&root);
        let mut writer = store.create_session(test_meta("resume", None)).unwrap();
        writer.append(SessionRecordKind::UserMessage(MessageRecord {
            text: "before resume".to_string(),
        }));
        writer.flush().unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);

        let mut resumed = store.resume_session(&path).unwrap();
        let seq = resumed.append(SessionRecordKind::AssistantMessage(MessageRecord {
            text: "after resume".to_string(),
        }));
        resumed.flush().unwrap();

        assert_eq!(seq, 2);
        let records = store.read_records(&path).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(
            records[2].kind,
            SessionRecordKind::AssistantMessage(MessageRecord {
                text: "after resume".to_string()
            })
        );
    }

    #[test]
    fn resume_session_truncates_incomplete_tail_before_append() {
        let root = temp_root("resume-tail");
        let store = SessionStore::new(&root);
        let mut writer = store
            .create_session(test_meta("resume-tail", None))
            .unwrap();
        writer.append(SessionRecordKind::UserMessage(MessageRecord {
            text: "durable".to_string(),
        }));
        writer.flush().unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);

        let complete_len = fs::metadata(&path).unwrap().len();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[1, 2, 3]).unwrap();
        file.flush().unwrap();
        drop(file);
        assert!(fs::metadata(&path).unwrap().len() > complete_len);

        let mut resumed = store.resume_session(&path).unwrap();
        resumed.append(SessionRecordKind::AssistantMessage(MessageRecord {
            text: "reachable after truncated tail".to_string(),
        }));
        resumed.flush().unwrap();

        let records = store.read_records(&path).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(
            records[2].kind,
            SessionRecordKind::AssistantMessage(MessageRecord {
                text: "reachable after truncated tail".to_string()
            })
        );
    }

    #[test]
    fn resume_session_truncates_postcard_truncated_final_chunk() {
        let root = temp_root("resume-truncated-postcard-tail");
        let store = SessionStore::new(&root);
        let mut writer = store
            .create_session(test_meta("resume-postcard-tail", None))
            .unwrap();
        writer.append(SessionRecordKind::UserMessage(MessageRecord {
            text: "kept".to_string(),
        }));
        writer.flush().unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);

        append_complete_postcard_truncated_chunk(&path, 2);

        let history = store.load_latest_history(&path).unwrap();
        assert_eq!(history.records_after_checkpoint.len(), 1);

        let mut resumed = store.resume_session(&path).unwrap();
        resumed.append(SessionRecordKind::AssistantMessage(MessageRecord {
            text: "after resume".to_string(),
        }));
        resumed.flush().unwrap();

        let records = store.read_records(resumed.path()).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(
            records[1].kind,
            SessionRecordKind::UserMessage(MessageRecord {
                text: "kept".to_string()
            })
        );
        assert_eq!(
            records[2].kind,
            SessionRecordKind::AssistantMessage(MessageRecord {
                text: "after resume".to_string()
            })
        );
    }
    #[test]
    fn latest_history_uses_latest_compaction_checkpoint() {
        let root = temp_root("checkpoint");
        let store = SessionStore::new(&root);
        let mut writer = store.create_session(test_meta("checkpoint", None)).unwrap();
        writer.append(SessionRecordKind::UserMessage(MessageRecord {
            text: "old".to_string(),
        }));
        writer.append(SessionRecordKind::CompactionCheckpoint(
            CompactionCheckpointRecord {
                replacement_history: vec![HistoryRecord::UserMessage(MessageRecord {
                    text: "summary".to_string(),
                })],
            },
        ));
        writer.append(SessionRecordKind::AssistantMessage(MessageRecord {
            text: "after".to_string(),
        }));
        writer.flush().unwrap();

        let latest = store.load_latest_history(writer.path()).unwrap();
        assert_eq!(
            latest.checkpoint_history,
            vec![HistoryRecord::UserMessage(MessageRecord {
                text: "summary".to_string()
            })]
        );
        assert_eq!(latest.records_after_checkpoint.len(), 1);
        assert_eq!(
            latest.records_after_checkpoint[0].kind,
            SessionRecordKind::AssistantMessage(MessageRecord {
                text: "after".to_string()
            })
        );
    }

    #[test]
    fn transcript_page_reads_tail_entries_in_display_order() {
        let root = temp_root("transcript-tail");
        let store = SessionStore::new(&root);
        let mut writer = store
            .create_session(test_meta("transcript-tail", None))
            .unwrap();
        writer.append_many([
            SessionRecordKind::TurnContext(TurnContextRecord {
                cwd: "/tmp/project".to_string(),
                date: "2026-06-03".to_string(),
                timezone: "UTC".to_string(),
                model: "gpt-test".to_string(),
                sandbox_policy: "workspace-write".to_string(),
                approval_policy: "never".to_string(),
            }),
            SessionRecordKind::DeveloperMessage(MessageRecord {
                text: "build it".to_string(),
            }),
        ]);
        writer.flush().unwrap();
        writer.append_many([
            SessionRecordKind::AssistantMessage(MessageRecord {
                text: "working".to_string(),
            }),
            SessionRecordKind::FunctionToolCall(FunctionToolCallRecord {
                call_id: "call-1".to_string(),
                name: "function_tool".to_string(),
                arguments: "{\"cmd\":\"true\"}".to_string(),
            }),
            SessionRecordKind::FunctionToolOutput(FunctionToolOutputRecord {
                call_id: "call-1".to_string(),
                output: "ok".to_string(),
                display_output: None,
            }),
        ]);
        writer.flush().unwrap();

        let page = store.read_transcript_page(writer.path(), None, 3).unwrap();

        assert_eq!(page.next_before_seq, Some(3));
        assert_eq!(page.reached_start, false);
        assert_eq!(
            page.lines,
            vec![
                TranscriptPageLine {
                    seq: 3,
                    kind: SessionRecordKind::AssistantMessage(MessageRecord {
                        text: "working".to_string(),
                    }),
                },
                TranscriptPageLine {
                    seq: 4,
                    kind: SessionRecordKind::FunctionToolCall(FunctionToolCallRecord {
                        call_id: "call-1".to_string(),
                        name: "function_tool".to_string(),
                        arguments: "{\"cmd\":\"true\"}".to_string(),
                    }),
                },
                TranscriptPageLine {
                    seq: 5,
                    kind: SessionRecordKind::FunctionToolOutput(FunctionToolOutputRecord {
                        call_id: "call-1".to_string(),
                        output: "ok".to_string(),
                        display_output: None,
                    }),
                },
            ]
        );
    }

    #[test]
    fn transcript_page_uses_before_seq_for_older_page() {
        let root = temp_root("transcript-before");
        let store = SessionStore::new(&root);
        let mut writer = store
            .create_session(test_meta("transcript-before", None))
            .unwrap();
        for index in 0..6 {
            writer.append(SessionRecordKind::UserMessage(MessageRecord {
                text: format!("message-{index}"),
            }));
            writer.flush().unwrap();
        }

        let tail = store.read_transcript_page(writer.path(), None, 2).unwrap();
        assert_eq!(
            tail.lines
                .iter()
                .map(|line| match &line.kind {
                    SessionRecordKind::UserMessage(message) => message.text.as_str(),
                    _ => panic!("expected user message transcript line"),
                })
                .collect::<Vec<_>>(),
            vec!["message-4", "message-5"]
        );

        let older = store
            .read_transcript_page(writer.path(), tail.next_before_seq, 3)
            .unwrap();
        assert_eq!(older.next_before_seq, Some(2));
        assert_eq!(older.reached_start, false);
        assert_eq!(
            older
                .lines
                .iter()
                .map(|line| match &line.kind {
                    SessionRecordKind::UserMessage(message) => message.text.as_str(),
                    _ => panic!("expected user message transcript line"),
                })
                .collect::<Vec<_>>(),
            vec!["message-1", "message-2", "message-3"]
        );
    }

    #[test]
    fn transcript_page_does_not_decompress_skipped_newer_corrupt_chunk() {
        let root = temp_root("transcript-skip-corrupt");
        let store = SessionStore::new(&root);
        let mut writer = store
            .create_session(test_meta("transcript-skip-corrupt", None))
            .unwrap();
        writer.append(SessionRecordKind::UserMessage(MessageRecord {
            text: "older".to_string(),
        }));
        writer.flush().unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);
        append_complete_corrupt_chunk_with_first_seq(&path, 99);

        let page = store.read_transcript_page(&path, Some(3), 10).unwrap();

        assert_eq!(page.reached_start, true);
        assert_eq!(
            page.lines,
            vec![TranscriptPageLine {
                seq: 1,
                kind: SessionRecordKind::UserMessage(MessageRecord {
                    text: "older".to_string(),
                }),
            }]
        );
        assert!(store.read_transcript_page(&path, None, 10).is_err());
    }

    #[test]
    fn lookup_verifies_session_id_matches_index() {
        let root = temp_root("id-mismatch");
        let store = SessionStore::new(&root);
        let writer = store
            .create_session(test_meta("actual-session", Some("actual")))
            .unwrap();
        let relative_path = writer
            .path()
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut index = SessionIndex::load(&root).unwrap();
        index
            .insert(SessionIndexEntry {
                session_id: SessionId::new("fake-session"),
                path: relative_path,
                created_at_ms: 1,
                updated_at_ms: 1,
                cwd: "/tmp/project".to_string(),
                title: Some("fake".to_string()),
                preview: None,
                parent_session_id: None,
                forked_from_session_id: None,
            })
            .unwrap();

        let summary = index
            .summary_by_id(&SessionId::new("fake-session"))
            .unwrap();
        assert_eq!(summary, None);
    }

    #[test]
    fn title_lookup_skips_invalid_newest_entry() {
        let root = temp_root("title-invalid-newest");
        let store = SessionStore::new(&root);
        let old_writer = store
            .create_session(test_meta("old-valid", Some("shared")))
            .unwrap();
        let relative_path = old_writer
            .path()
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut index = SessionIndex::load(&root).unwrap();
        index
            .insert(SessionIndexEntry {
                session_id: SessionId::new("invalid-newest"),
                path: relative_path,
                created_at_ms: 2,
                updated_at_ms: 2,
                cwd: "/tmp/project".to_string(),
                title: Some("shared".to_string()),
                preview: None,
                parent_session_id: None,
                forked_from_session_id: None,
            })
            .unwrap();

        let summary = index.summary_by_title("shared").unwrap().unwrap();
        assert_eq!(summary.session_id, SessionId::new("old-valid"));
    }

    #[test]
    fn read_startup_binding_reads_separate_provider_binding_record() {
        let root = temp_root("startup-binding");
        let store = SessionStore::new(&root);
        let mut writer = store
            .create_session(test_meta("startup-binding", None))
            .unwrap();
        let binding = crate::provider_runtime::ProviderSessionBinding {
            profile_id: crate::providers::ProviderProfileId::new("ollama-cloud"),
            kind: crate::providers::ProviderKind::OllamaCloud,
            model_settings: crate::responses::ModelSettings::new("glm-5.2", None, None),
        };
        writer.append(SessionRecordKind::ProviderSessionBinding(
            ProviderSessionBindingRecord {
                binding: binding.clone(),
            },
        ));
        writer.flush().unwrap();

        let (meta, decoded_binding) = store.read_startup_binding(writer.path()).unwrap();

        assert_eq!(meta.id.as_str(), "startup-binding");
        assert_eq!(decoded_binding, Some(binding));
    }

    #[test]
    fn read_startup_binding_reads_provider_bound_metadata() {
        #[derive(Serialize)]
        struct ProviderBoundSessionMeta {
            id: SessionId,
            parent_session_id: Option<SessionId>,
            forked_from_session_id: Option<SessionId>,
            created_at_ms: u64,
            updated_at_ms: u64,
            cwd: String,
            model: String,
            provider_binding: Option<crate::provider_runtime::ProviderSessionBinding>,
            originator: String,
            client_version: String,
            source: String,
            title: Option<String>,
            preview: Option<String>,
        }

        #[derive(Serialize)]
        enum ProviderBoundSessionRecordKind {
            SessionMeta(ProviderBoundSessionMeta),
        }

        #[derive(Serialize)]
        struct ProviderBoundSessionRecord {
            seq: u64,
            kind: ProviderBoundSessionRecordKind,
        }

        let root = temp_root("provider-bound-metadata");
        let path = root.join("provider-bound.nhsession");
        let binding = crate::provider_runtime::ProviderSessionBinding {
            profile_id: crate::providers::ProviderProfileId::new("ollama-cloud"),
            kind: crate::providers::ProviderKind::OllamaCloud,
            model_settings: crate::responses::ModelSettings::new("glm-5.2", None, None),
        };
        let record = ProviderBoundSessionRecord {
            seq: 0,
            kind: ProviderBoundSessionRecordKind::SessionMeta(ProviderBoundSessionMeta {
                id: SessionId::new("provider-bound"),
                parent_session_id: None,
                forked_from_session_id: None,
                created_at_ms: 1_717_171_717_000,
                updated_at_ms: 1_717_171_717_000,
                cwd: "/tmp/project".to_string(),
                model: "gpt-test".to_string(),
                provider_binding: Some(binding.clone()),
                originator: "new_harness".to_string(),
                client_version: "0.1.0".to_string(),
                source: "test".to_string(),
                title: Some("provider-bound".to_string()),
                preview: Some("preview".to_string()),
            }),
        };
        write_serialized_session_chunk(&path, &vec![record], 0, 1);
        let store = SessionStore::new(&root);

        let (meta, decoded_binding) = store.read_startup_binding(&path).unwrap();

        assert_eq!(meta.id.as_str(), "provider-bound");
        assert_eq!(decoded_binding, Some(binding));
    }

    #[test]
    fn date_path_uses_utc_day_from_unix_millis() {
        assert_eq!(ymd_from_unix_ms(0), (1970, 1, 1));
        assert_eq!(ymd_from_unix_ms(1_717_171_717_000), (2024, 5, 31));
    }
}
