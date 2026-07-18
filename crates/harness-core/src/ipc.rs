//! IPC wire protocol and framed transport helpers for UI sidecars.

use std::{
    future::Future,
    marker::PhantomData,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::sessions::{
    FreeformToolCallRecord, FreeformToolOutputRecord, FunctionToolCallRecord,
    FunctionToolOutputRecord, MessageRecord, SessionId, SessionIndex, SessionRecordKind,
    SessionStore,
};

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Error returned by the harness IPC protocol and framed transports.
#[derive(Debug, Error)]
pub enum IpcError {
    /// Underlying I/O operation failed.
    #[error("ipc io error: {0}")]
    Io(#[from] std::io::Error),
    /// Postcard encoding or decoding failed.
    #[error("ipc codec error: {0}")]
    Codec(#[from] postcard::Error),
    /// Incoming frame exceeds the configured maximum size.
    #[error("ipc frame too large: {actual} > {max}")]
    FrameTooLarge {
        /// Maximum accepted frame length in bytes.
        max: usize,
        /// Actual incoming frame length in bytes.
        actual: usize,
    },
    /// Requested session does not exist in the authoritative store.
    #[error("session not found: {0}")]
    SessionNotFound(String),
    /// Session persistence operation failed.
    #[error("session store error: {0}")]
    SessionStore(#[from] crate::sessions::SessionError),
}

/// UI-facing session summary transported over IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcSessionSummary {
    /// Stable session identifier.
    pub session_id: String,
    /// Absolute or root-relative durable session file path.
    pub path: PathBuf,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at_ms: u64,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
    /// Working directory captured for the session.
    pub cwd: PathBuf,
    /// Human-readable session title.
    pub title: String,
    /// Short preview used in session lists.
    pub preview: String,
    /// Parent session id when this is a child session.
    pub parent_session_id: Option<String>,
    /// Original session id when this session is a fork.
    pub forked_from_session_id: Option<String>,
}

/// Diagram sidecar attached to a transcript message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcDiagramSidecar {
    /// Source transcript message id or sequence identifier.
    pub message_id: String,
    /// Sidecar display name.
    pub name: String,
    /// Sidecar goal or purpose.
    pub goal: String,
    /// Focus labels highlighted by the analysis.
    pub focus: Vec<String>,
    /// Diagram nodes.
    pub nodes: Vec<IpcDiagramNode>,
    /// Diagram edges.
    pub edges: Vec<IpcDiagramEdge>,
    /// Source evidence entries.
    pub evidence: Vec<IpcDiagramEvidence>,
    /// Suggested repair text.
    pub repair: String,
}

/// Node in a message-associated sidecar diagram.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcDiagramNode {
    /// Stable node id.
    pub id: String,
    /// Display label.
    pub label: String,
    /// Detail text.
    pub detail: String,
    /// Severity classification.
    pub severity: IpcDiagramSeverity,
}

/// Directed edge in a sidecar diagram.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcDiagramEdge {
    /// Source node id.
    pub from: String,
    /// Destination node id.
    pub to: String,
    /// Edge label.
    pub label: String,
    /// Whether this transition is forbidden.
    pub forbidden: bool,
}

/// Evidence item backing a sidecar diagram.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcDiagramEvidence {
    /// Source location or artifact.
    pub source: String,
    /// Evidence note.
    pub note: String,
}

/// Severity used by sidecar diagrams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcDiagramSeverity {
    Critical,
    Warning,
    Info,
}

/// Structured transcript item transported over IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcTranscriptItem {
    /// User, developer, assistant, or system-like message.
    Message {
        /// Message role.
        role: IpcTranscriptRole,
        /// Message text.
        text: String,
    },
    /// Native freeform tool call.
    FreeformToolCall {
        /// Responses API call id.
        call_id: String,
        /// Native tool name.
        name: String,
        /// Complete raw freeform input.
        input: String,
    },
    /// Native freeform tool result.
    FreeformToolOutput {
        /// Responses API call id being answered.
        call_id: String,
        /// Transcript-facing output.
        output: String,
    },
    /// JSON/function tool call.
    FunctionToolCall {
        /// Responses API call id.
        call_id: String,
        /// Function tool name.
        name: String,
        /// Raw JSON arguments string.
        arguments: String,
    },
    /// JSON/function tool result.
    FunctionToolOutput {
        /// Responses API call id being answered.
        call_id: String,
        /// Transcript-facing output.
        output: String,
    },
    /// Session lifecycle marker.
    SessionClosed {
        /// Close timestamp in milliseconds since Unix epoch.
        closed_at_ms: u64,
    },
}

/// Transcript message role transported over IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcTranscriptRole {
    User,
    Developer,
    Assistant,
}

/// One transcript line transported over IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTranscriptLine {
    /// Source session sequence number.
    pub seq: u64,
    /// Structured transcript item.
    pub item: IpcTranscriptItem,
    /// Message-associated sidecar data.
    pub sidecar: Option<IpcDiagramSidecar>,
}

/// Reverse-page of persisted transcript lines transported over IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTranscriptPage {
    /// Entries in chronological display order.
    pub lines: Vec<IpcTranscriptLine>,
    /// Sequence number to pass as `before_seq` for the next older page.
    pub next_before_seq: Option<u64>,
    /// Whether this page reached the oldest displayable transcript entry.
    pub reached_start: bool,
}

/// Request messages accepted by the UI IPC server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcRequest {
    /// Return all indexed sessions sorted by newest update first.
    ListSessions,
    /// Return one reverse transcript page for a session.
    LoadTranscriptPage {
        /// Session to load.
        session_id: String,
        /// Sequence number before which older lines are requested.
        before_seq: Option<u64>,
        /// Maximum number of transcript lines to load.
        max_lines: usize,
    },
}

/// Response messages emitted by the UI IPC server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcResponse {
    /// Session summaries are available.
    Sessions(Vec<IpcSessionSummary>),
    /// Transcript page is available.
    TranscriptPage {
        /// Session that produced the page.
        session_id: String,
        /// Page payload.
        page: IpcTranscriptPage,
    },
}

/// Postcard length-prefixed frame codec.
///
/// Frames are encoded as a big-endian `u32` byte length followed by postcard
/// payload bytes.
#[derive(Debug)]
pub struct PostcardFrames<T> {
    max_frame_bytes: usize,
    _message: PhantomData<T>,
}

impl<T> Default for PostcardFrames<T> {
    fn default() -> Self {
        Self::new(MAX_FRAME_BYTES)
    }
}

impl<T> PostcardFrames<T> {
    /// Construct a frame codec with a maximum accepted frame length.
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            max_frame_bytes,
            _message: PhantomData,
        }
    }
}

impl<T> PostcardFrames<T>
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    /// Read one length-prefixed postcard message.
    pub async fn read_from<R>(&self, reader: &mut R) -> Result<T, IpcError>
    where
        R: AsyncRead + Unpin,
    {
        let len = reader.read_u32().await? as usize;
        if len > self.max_frame_bytes {
            return Err(IpcError::FrameTooLarge {
                max: self.max_frame_bytes,
                actual: len,
            });
        }

        let mut bytes = vec![0; len];
        reader.read_exact(&mut bytes).await?;
        Ok(postcard::from_bytes(&bytes)?)
    }

    /// Write one length-prefixed postcard message.
    pub async fn write_to<W>(&self, writer: &mut W, message: &T) -> Result<(), IpcError>
    where
        W: AsyncWrite + Unpin,
    {
        let bytes = postcard::to_allocvec(message)?;
        writer.write_u32(bytes.len() as u32).await?;
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok(())
    }
}

/// Request handler used by transports.
#[derive(Debug, Clone)]
pub struct IpcService {
    sessions_root: PathBuf,
}

impl IpcService {
    /// Construct an IPC service backed by the harness session root.
    pub fn new(sessions_root: impl Into<PathBuf>) -> Self {
        Self {
            sessions_root: sessions_root.into(),
        }
    }

    /// Handle one UI IPC request against authoritative session storage.
    pub fn handle(&self, request: IpcRequest) -> Result<IpcResponse, IpcError> {
        match request {
            IpcRequest::ListSessions => {
                let mut index = SessionIndex::load(self.sessions_root.clone())?;
                let sessions = index
                    .summaries()?
                    .into_iter()
                    .map(IpcSessionSummary::from)
                    .collect();
                Ok(IpcResponse::Sessions(sessions))
            }
            IpcRequest::LoadTranscriptPage {
                session_id,
                before_seq,
                max_lines,
            } => {
                let session_id_value = SessionId::new(session_id);
                let mut index = SessionIndex::load(self.sessions_root.clone())?;
                let summary = index
                    .summary_by_id(&session_id_value)?
                    .ok_or_else(|| IpcError::SessionNotFound(session_id_value.to_string()))?;

                let store = SessionStore::new(self.sessions_root.clone());
                let page = store.read_transcript_page(&summary.path, before_seq, max_lines)?;
                Ok(IpcResponse::TranscriptPage {
                    session_id: session_id_value.to_string(),
                    page: page.into(),
                })
            }
        }
    }
}

/// Static-dispatch transport server contract.
pub trait IpcTransport {
    /// Serve IPC requests using the supplied service.
    fn serve(self, service: IpcService) -> impl Future<Output = Result<(), IpcError>> + Send;
}

/// Unix-domain-socket IPC transport.
#[derive(Debug)]
pub struct UdsTransport {
    socket_path: PathBuf,
}

impl UdsTransport {
    /// Construct a UDS transport bound to `socket_path`.
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    fn remove_existing_socket(path: &Path) -> Result<(), IpcError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(IpcError::Io(error)),
        }
    }
}

/// Unix-domain-socket IPC client.
#[derive(Debug)]
pub struct UdsClient {
    socket_path: PathBuf,
}

impl UdsClient {
    /// Construct a UDS client connected to `socket_path`.
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// Send one request and read one response.
    pub async fn request(&self, request: &IpcRequest) -> Result<IpcResponse, IpcError> {
        let mut stream = tokio::net::UnixStream::connect(&self.socket_path).await?;
        let request_codec = PostcardFrames::<IpcRequest>::default();
        let response_codec = PostcardFrames::<IpcResponse>::default();
        request_codec.write_to(&mut stream, request).await?;
        response_codec.read_from(&mut stream).await
    }
}

impl IpcTransport for UdsTransport {
    fn serve(self, service: IpcService) -> impl Future<Output = Result<(), IpcError>> + Send {
        async move {
            let socket_path = self.socket_path;
            Self::remove_existing_socket(&socket_path)?;
            let listener = tokio::net::UnixListener::bind(socket_path)?;

            loop {
                let (mut stream, _) = listener.accept().await?;
                let request_codec = PostcardFrames::<IpcRequest>::default();
                let response_codec = PostcardFrames::<IpcResponse>::default();
                let request = request_codec.read_from(&mut stream).await?;
                let response = service.handle(request)?;
                response_codec.write_to(&mut stream, &response).await?;
            }
        }
    }
}

impl From<crate::sessions::SessionSummary> for IpcSessionSummary {
    fn from(summary: crate::sessions::SessionSummary) -> Self {
        Self {
            session_id: summary.session_id.to_string(),
            path: summary.path,
            created_at_ms: summary.created_at_ms,
            updated_at_ms: summary.updated_at_ms,
            cwd: PathBuf::from(summary.cwd),
            title: summary
                .title
                .unwrap_or_else(|| summary.session_id.to_string()),
            preview: summary.preview.unwrap_or_default(),
            parent_session_id: summary.parent_session_id.map(|id| id.to_string()),
            forked_from_session_id: summary.forked_from_session_id.map(|id| id.to_string()),
        }
    }
}

impl IpcTranscriptItem {
    fn from_message(role: IpcTranscriptRole, message: MessageRecord) -> Self {
        Self::Message {
            role,
            text: message.text,
        }
    }

    fn from_freeform_tool_call(call: FreeformToolCallRecord) -> Self {
        Self::FreeformToolCall {
            call_id: call.call_id,
            name: call.name,
            input: call.input,
        }
    }

    fn from_freeform_tool_output(output: FreeformToolOutputRecord) -> Self {
        let transcript_output = output.transcript_output().to_string();
        Self::FreeformToolOutput {
            call_id: output.call_id,
            output: transcript_output,
        }
    }

    fn from_function_tool_call(call: FunctionToolCallRecord) -> Self {
        Self::FunctionToolCall {
            call_id: call.call_id,
            name: call.name,
            arguments: call.arguments,
        }
    }

    fn from_function_tool_output(output: FunctionToolOutputRecord) -> Self {
        let transcript_output = output.transcript_output().to_string();
        Self::FunctionToolOutput {
            call_id: output.call_id,
            output: transcript_output,
        }
    }

    fn from_session_record_kind(kind: SessionRecordKind) -> Option<Self> {
        match kind {
            SessionRecordKind::UserMessage(message) => {
                Some(Self::from_message(IpcTranscriptRole::User, message))
            }
            SessionRecordKind::DeveloperMessage(message) => {
                Some(Self::from_message(IpcTranscriptRole::Developer, message))
            }
            SessionRecordKind::AssistantMessage(message) => {
                Some(Self::from_message(IpcTranscriptRole::Assistant, message))
            }
            SessionRecordKind::FreeformToolCall(call) => Some(Self::from_freeform_tool_call(call)),
            SessionRecordKind::FreeformToolOutput(output) => {
                Some(Self::from_freeform_tool_output(output))
            }
            SessionRecordKind::FunctionToolCall(call) => Some(Self::from_function_tool_call(call)),
            SessionRecordKind::FunctionToolOutput(output) => {
                Some(Self::from_function_tool_output(output))
            }
            SessionRecordKind::SessionClosed(record) => Some(Self::SessionClosed {
                closed_at_ms: record.closed_at_ms,
            }),
            SessionRecordKind::SessionMeta(_)
            | SessionRecordKind::TurnContext(_)
            | SessionRecordKind::FreeformToolInputDelta(_)
            | SessionRecordKind::CompactionCheckpoint(_)
            | SessionRecordKind::ProviderSessionBinding(_) => None,
        }
    }
}

impl From<crate::sessions::TranscriptPageLine> for IpcTranscriptLine {
    fn from(line: crate::sessions::TranscriptPageLine) -> Self {
        let item = IpcTranscriptItem::from_session_record_kind(line.kind)
            .expect("transcript page lines are displayable session records");
        Self {
            seq: line.seq,
            item,
            sidecar: None,
        }
    }
}

impl From<crate::sessions::TranscriptPage> for IpcTranscriptPage {
    fn from(page: crate::sessions::TranscriptPage) -> Self {
        Self {
            lines: page
                .lines
                .into_iter()
                .map(IpcTranscriptLine::from)
                .collect(),
            next_before_seq: page.next_before_seq,
            reached_start: page.reached_start,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::sessions::{MessageRecord, SessionMeta, SessionRecordKind};

    fn temp_root(name: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "new-harness-ipc-{name}-{}-{now}",
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

    #[tokio::test]
    async fn postcard_frames_roundtrip_request() {
        let request = IpcRequest::LoadTranscriptPage {
            session_id: "session-a".to_string(),
            before_seq: Some(42),
            max_lines: 128,
        };
        let codec = PostcardFrames::<IpcRequest>::default();
        let (mut client, mut server) = tokio::io::duplex(1024);

        codec.write_to(&mut client, &request).await.unwrap();
        let decoded = codec.read_from(&mut server).await.unwrap();

        assert_eq!(decoded, request);
    }

    #[tokio::test]
    async fn postcard_frames_reject_oversized_frame() {
        let codec = PostcardFrames::<IpcRequest>::new(4);
        let (mut client, mut server) = tokio::io::duplex(1024);

        client.write_u32(5).await.unwrap();
        client.write_all(&[0; 5]).await.unwrap();

        let error = codec.read_from(&mut server).await.unwrap_err();
        assert!(matches!(
            error,
            IpcError::FrameTooLarge { max: 4, actual: 5 }
        ));
    }

    #[test]
    fn ipc_service_lists_sessions_from_index() {
        let root = temp_root("list-sessions");
        let store = SessionStore::new(&root);
        let mut writer = store
            .create_session(test_meta("ipc-list", Some("IPC List")))
            .unwrap();
        writer.append(SessionRecordKind::UserMessage(MessageRecord {
            text: "hello over ipc".to_string(),
        }));
        writer.flush().unwrap();

        let response = IpcService::new(&root)
            .handle(IpcRequest::ListSessions)
            .unwrap();

        let IpcResponse::Sessions(sessions) = response else {
            panic!("expected sessions response");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "ipc-list");
        assert_eq!(sessions[0].title, "IPC List");
        assert_eq!(sessions[0].preview, "preview");
    }

    #[test]
    fn ipc_service_loads_transcript_page() {
        let root = temp_root("load-transcript");
        let store = SessionStore::new(&root);
        let mut writer = store
            .create_session(test_meta("ipc-transcript", Some("IPC Transcript")))
            .unwrap();
        writer.append(SessionRecordKind::UserMessage(MessageRecord {
            text: "first".to_string(),
        }));
        writer.append(SessionRecordKind::AssistantMessage(MessageRecord {
            text: "second".to_string(),
        }));
        writer.append(SessionRecordKind::FunctionToolCall(
            FunctionToolCallRecord {
                call_id: "call-1".to_string(),
                name: "run".to_string(),
                arguments: "{\"cmd\":\"true\"}".to_string(),
            },
        ));
        writer.append(SessionRecordKind::FunctionToolOutput(
            FunctionToolOutputRecord {
                call_id: "call-1".to_string(),
                output: "ok".to_string(),
                display_output: None,
            },
        ));
        writer.flush().unwrap();

        let response = IpcService::new(&root)
            .handle(IpcRequest::LoadTranscriptPage {
                session_id: "ipc-transcript".to_string(),
                before_seq: None,
                max_lines: 16,
            })
            .unwrap();

        let IpcResponse::TranscriptPage { session_id, page } = response else {
            panic!("expected transcript page response");
        };
        assert_eq!(session_id, "ipc-transcript");
        assert_eq!(page.lines.len(), 4);
        assert_eq!(page.lines[0].seq, 1);
        assert_eq!(
            page.lines[0].item,
            IpcTranscriptItem::Message {
                role: IpcTranscriptRole::User,
                text: "first".to_string(),
            }
        );
        assert_eq!(page.lines[1].seq, 2);
        assert_eq!(
            page.lines[1].item,
            IpcTranscriptItem::Message {
                role: IpcTranscriptRole::Assistant,
                text: "second".to_string(),
            }
        );
        assert_eq!(page.lines[2].seq, 3);
        assert_eq!(
            page.lines[2].item,
            IpcTranscriptItem::FunctionToolCall {
                call_id: "call-1".to_string(),
                name: "run".to_string(),
                arguments: "{\"cmd\":\"true\"}".to_string(),
            }
        );
        assert_eq!(page.lines[3].seq, 4);
        assert_eq!(
            page.lines[3].item,
            IpcTranscriptItem::FunctionToolOutput {
                call_id: "call-1".to_string(),
                output: "ok".to_string(),
            }
        );
        assert!(page.reached_start);
    }

    #[test]
    fn ipc_service_reports_missing_session() {
        let root = temp_root("missing-session");

        let error = IpcService::new(&root)
            .handle(IpcRequest::LoadTranscriptPage {
                session_id: "missing".to_string(),
                before_seq: None,
                max_lines: 16,
            })
            .unwrap_err();

        assert!(matches!(error, IpcError::SessionNotFound(id) if id == "missing"));
    }
}
