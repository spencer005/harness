//! Capability-rooted tool execution contracts.
//!
//! This crate keeps workspace paths and process lifecycles below the
//! provider-independent tool API. Concrete Linux relative-directory and PTY
//! backends are injected behind these contracts.

use std::path::{Path, PathBuf};

use harness_tool_api::{
    ToolExecutionRequest, ToolExecutor, ToolFailure, ToolResult,
};
use thiserror::Error;

/// Opened workspace capability used to scope tool operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRoot {
    root: PathBuf,
}

impl WorkspaceRoot {
    /// Creates a workspace capability from an existing directory.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, WorkspaceRootError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(WorkspaceRootError::NotAbsolute);
        }
        if !root.is_dir() {
            return Err(WorkspaceRootError::NotDirectory(root));
        }
        Ok(Self { root })
    }

    /// Returns the configured root for capability-specific backends.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Validates a user-supplied path without resolving it outside the capability.
    ///
    /// Concrete backends still perform directory-relative identity checks before
    /// opening components, so this value does not authorize path concatenation.
    pub fn relative_path<'a>(
        &self,
        value: &'a str,
    ) -> Result<WorkspaceRelativePath<'a>, WorkspacePathError> {
        let path = Path::new(value);
        if path.is_absolute() {
            return Err(WorkspacePathError::Absolute);
        }
        if value.is_empty() {
            return Err(WorkspacePathError::Empty);
        }
        if path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        }) {
            return Err(WorkspacePathError::Traversal);
        }
        Ok(WorkspaceRelativePath { value })
    }

/// Validated path relative to one workspace capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceRelativePath<'a> {
    value: &'a str,
}

impl WorkspaceRelativePath<'_> {
    /// Returns the validated relative path.
    pub fn as_str(self) -> &'_ str {
        self.value
    }
}

/// Provider-independent execution port bound to one workspace capability.
pub trait WorkspaceToolBackend: Send + Sync {
    /// Executes one request using the supplied capability and explicit policy.
    fn execute(
        &self,
        workspace: &WorkspaceRoot,
        request: ToolExecutionRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ToolResult, ToolFailure>> + Send + '_,
        >,
    >;
}

/// Marker backend that delegates execution to a workspace implementation.
pub struct WorkspaceToolExecutor<B> {
    workspace: WorkspaceRoot,
    backend: B,
}

impl<B> WorkspaceToolExecutor<B> {
    /// Creates an executor bound to one workspace capability.
    pub fn new(workspace: WorkspaceRoot, backend: B) -> Self {
        Self { workspace, backend }
    }
}

impl<B> ToolExecutor for WorkspaceToolExecutor<B>
where
    B: WorkspaceToolBackend + 'static,
{
    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>,
    > {
        self.backend.execute(&self.workspace, request)
    }
}

/// PTY lifecycle owned by one execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyState {
    /// Child and reader are starting.
    Starting,
    /// Child is running.
    Running,
    /// Child exits normally.
    Exited,
    /// Child or reader fails.
    Failed,
    /// Cancellation and close are in progress.
    Closing,
    /// Child and reader handles are joined.
    Closed,
}

/// Bounded recent output retained for a PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRing {
    capacity: usize,
    bytes: Vec<u8>,
    omitted_bytes: usize,
}

impl OutputRing {
    /// Creates a bounded output ring.
    pub fn new(capacity: usize) -> Result<Self, OutputRingError> {
        if capacity == 0 {
            return Err(OutputRingError::ZeroCapacity);
        }
        Ok(Self {
            capacity,
            bytes: Vec::new(),
            omitted_bytes: 0,
        })
    }

    /// Appends output and records an explicit omission boundary.
    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.capacity {
            let omitted = self
                .bytes
                .len()
                .saturating_add(bytes.len().saturating_sub(self.capacity));
            self.omitted_bytes = self.omitted_bytes.saturating_add(omitted);
            self.bytes.clear();
            self.bytes
                .extend_from_slice(&bytes[bytes.len() - self.capacity..]);
            return;
        }

        let overflow = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(self.capacity);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.omitted_bytes = self.omitted_bytes.saturating_add(overflow);
        }
        self.bytes.extend_from_slice(bytes);
    }

    /// Returns the bounded output and omitted-byte count.
    pub fn snapshot(&self) -> (&[u8], usize) {
        (&self.bytes, self.omitted_bytes)
    }
}

/// Owned PTY lifecycle handles.
pub struct PtySession {
    state: PtyState,
    cancellation: tokio_util::sync::CancellationToken,
    child: tokio::task::JoinHandle<()>,
    reader: tokio::task::JoinHandle<()>,
}

impl PtySession {
    /// Creates a running PTY session from owned child and reader tasks.
    pub fn new(
        cancellation: tokio_util::sync::CancellationToken,
        child: tokio::task::JoinHandle<()>,
        reader: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            state: PtyState::Running,
            cancellation,
            child,
            reader,
        }
    }

    /// Requests cancellation and joins child and reader tasks.
    pub async fn close(mut self) -> Result<(), PtyError> {
        self.state = PtyState::Closing;
        self.cancellation.cancel();
        let child = self.child.await;
        let reader = self.reader.await;
        self.state = PtyState::Closed;

        if let Err(error) = child {
            return Err(PtyError::Join(error.to_string()));
        }
        if let Err(error) = reader {
            return Err(PtyError::Join(error.to_string()));
        }
        Ok(())
    }

    /// Returns the current PTY state.
    pub fn state(&self) -> PtyState {
        self.state
    }
}

/// PTY lifecycle failure.
#[derive(Debug, Error)]
pub enum PtyError {
    /// Output-ring capacity is invalid.
    #[error("PTY output capacity must be greater than zero")]
    ZeroCapacity,
    /// Child or reader task cannot be joined.
    #[error("PTY task failed to join: {0}")]
    Join(String),
}
/// Identity recorded before one planned file mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFileIdentity {
    /// Validated relative path.
    pub path: String,
    /// Hash or metadata fingerprint observed during planning.
    pub fingerprint: String,
}

/// Recoverable patch transaction journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchJournal {
    /// Transaction identity.
    pub transaction_id: u64,
    /// Planned file identities.
    pub files: Vec<PlannedFileIdentity>,
    /// Journal phase.
    pub phase: PatchJournalPhase,
}

/// Recoverable patch transaction phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchJournalPhase {
    /// Plan is written and awaits staging.
    Planned,
    /// Replacement files are staged and synchronized.
    Staged,
    /// Intent is synchronized and ready for apply.
    IntentSynchronized,
    /// Source identities are revalidated.
    Revalidated,
    /// Mutations are applied and directory sync is pending.
    Applied,
    /// Completion marker is synchronized.
    Completed,
}

/// Patch transaction promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchTransactionGuarantee {
    /// Recovery can identify and resume or repair the transaction.
    Recoverable,
    /// The operation is complete.
    Completed,
}
/// Stable identifier for one subagent message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentMessageId(pub u64);

/// Delivery mode for a subagent message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDeliveryMode {
    /// Deliver after the current child turn.
    Queue,
    /// Request immediate child interruption and deliver the text.
    Interrupt,
}

/// Message retained by a child mailbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessage {
    /// Stable message identity.
    pub message_id: AgentMessageId,
    /// Exact user/runtime text.
    pub text: String,
    /// Delivery mode.
    pub delivery_mode: AgentDeliveryMode,
}

/// Mailbox contract for one subagent.
pub trait AgentMailbox: Send + Sync {
    /// Enqueues a message without discarding its text.
    fn send(
        &self,
        message: AgentMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MailboxError>> + Send + '_>>;
}

/// Mailbox failure.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MailboxError {
    /// Child mailbox is closed.
    #[error("subagent mailbox is closed")]
    Closed,
}
/// Failure returned while creating a workspace capability.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WorkspaceRootError {
    /// Workspace path is not absolute.
    #[error("workspace root must be absolute")]
    NotAbsolute,
    /// Workspace path is not an existing directory.
    #[error("workspace root is not a directory: {0}")]
    NotDirectory(PathBuf),
}

/// Failure returned while validating a workspace-relative path.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum WorkspacePathError {
    /// Path is empty.
    #[error("workspace path is empty")]
    Empty,
    /// Path is absolute.
    #[error("workspace path must be relative")]
    Absolute,
    /// Path traverses or aliases the current/parent directory.
    #[error("workspace path contains traversal")]
    Traversal,
}
