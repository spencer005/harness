//! Durable session-store contracts independent of the on-disk format.
//!
//! The current `.nhsession` format remains behind an adapter. This crate does
//! not encode, migrate, or transparently reinterpret session files.

use std::{
    ops::RangeInclusive,
    path::{Path, PathBuf},

};

use thiserror::Error;

/// Stable identity of a persisted session.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Creates a session identity from a nonempty value.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidSessionId> {
        let value = value.into();
        if value.is_empty() {
            return Err(InvalidSessionId);
        }
        Ok(Self(value))
    }

    /// Returns the session identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Monotonic identity assigned to a durable session record.
pub type SessionSequence = u64;

/// Bounded number of entries requested in one page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PageSize(u32);

impl PageSize {
    /// Creates a nonzero page size.
    pub fn new(value: u32) -> Result<Self, InvalidPageSize> {
        if value == 0 {
            return Err(InvalidPageSize);
        }
        Ok(Self(value))
    }

    /// Returns the requested entry count.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// Durability level acknowledged by an append operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Records are accepted by the writer but not synchronized.
    Buffered,
    /// Records are written and synchronized to stable storage.
    Durable,
}

/// Receipt for an append operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendReceipt {
    /// Sequence range assigned to the appended records.
    pub sequences: RangeInclusive<SessionSequence>,
    /// Durability achieved before the receipt is returned.
    pub durability: Durability,
}

/// Provider-independent record envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// Session identity.
    pub session_id: SessionId,
    /// Record sequence.
    pub sequence: SessionSequence,
    /// Typed record payload.
    pub payload: SessionPayload,
}

/// Durable session payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPayload {
    /// Session metadata.
    Metadata(SessionMetadata),
    /// Provider binding owned by storage.
    ProviderBinding(SessionProviderBinding),
    /// Turn begins.
    TurnStarted { turn_id: u64 },
    /// User input is durably accepted.
    InputMessage { turn_id: u64, text: String },
    /// Model attempt begins.
    ModelAttemptStarted {
        turn_id: u64,
        attempt_id: u64,
    },
    /// Assistant output is durably committed.
    AssistantMessage { turn_id: u64, text: String },
    /// Tool call is durably accepted.
    ToolCallAccepted {
        turn_id: u64,
        call_id: String,
        name: String,
        input: String,
    },
    /// Tool execution begins.
    ToolExecutionStarted { turn_id: u64, call_id: String },
    /// Tool execution finishes.
    ToolExecutionFinished {
        turn_id: u64,
        call_id: String,
        output: String,
    },
    /// Turn reaches a terminal outcome.
    TurnFinished { turn_id: u64, outcome: TurnOutcome },
    /// Session closes.
    SessionClosed,
}

/// Session metadata owned by the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadata {
    /// Human-readable session title.
    pub title: String,
}

/// Provider binding persisted without depending on provider implementation types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionProviderBinding {
    /// Provider identity.
    pub provider: String,
    /// Selected model.
    pub model: String,
}

/// Terminal turn outcome persisted by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Model and tools complete the turn.
    Completed,
    /// User interrupts the turn.
    Interrupted { reason: String },
    /// Runtime cancels the turn.
    Cancelled { reason: String },
    /// Turn fails.
    Failed { message: String },
}

/// A page of persisted records ordered from newer to older.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptPage {
    /// Records returned by the page.
    pub entries: Vec<SessionRecord>,
    /// Cursor for the next older page.
    pub next_before: Option<SessionSequence>,
    /// Whether the beginning of the session is reached.
    pub reached_start: bool,
}

/// Read-only session handle.
pub trait SessionReader: Send + Sync {
    /// Loads a page before the supplied sequence.
    fn load_older(
        &self,
        session_id: SessionId,
        before: Option<SessionSequence>,
        maximum_entries: PageSize,
    ) -> Result<TranscriptPage, SessionStoreError>;
}

/// Unique writer lease for one session.
pub trait SessionWriter: Send {
    /// Appends records and reports achieved durability.
    fn append<'a>(
        &'a mut self,
        records: &'a [SessionPayload],
        durability: Durability,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<AppendReceipt, SessionStoreError>,
                > + Send
                + 'a,
        >,
    >;

    /// Closes the writer after flushing and joining its owned work.
    fn close(
        self: Box<Self>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), SessionStoreError>> + Send,
        >,
    >;
}

/// Storage backend boundary used by composition.
pub trait SessionStore: Send + Sync {
    /// Opens a read-only session handle.
    fn reader(&self) -> Result<Box<dyn SessionReader>, SessionStoreError>;

    /// Acquires the unique writer lease for a session.
    fn writer(
        &self,
        session_id: SessionId,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Box<dyn SessionWriter>, SessionStoreError>,
                > + Send
                + '_,
        >,
    >;

    /// Returns the current-format session path for explicit inspection.
    fn session_path(&self, session_id: SessionId) -> Result<PathBuf, SessionStoreError>;
}

/// Adapter boundary for the existing `.nhsession` format.
///
/// Implementations preserve current files and never perform migration during
/// normal runtime startup. A replacement format is introduced only by the
/// final user-run migration phase.
pub trait CurrentFormatAdapter: Send + Sync {
    /// Opens the current-format store without rewriting session files.
    fn open(&self, root: &Path) -> Result<Box<dyn SessionStore>, SessionStoreError>;
}

/// Storage failures exposed without format-specific fallback behavior.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// A session is not present.
    #[error("session {0:?} was not found")]
    NotFound(SessionId),
    /// Another process owns the writer lease.
    #[error("session {0:?} is already open for writing")]
    WriterBusy(SessionId),
    /// The current-format adapter rejects the session.
    #[error("session format is invalid: {0}")]
    InvalidFormat(String),
    /// Storage I/O fails.
    #[error("session storage I/O failed: {0}")]
    Io(#[source] std::io::Error),
    /// The requested page size is invalid.
    #[error("page size is invalid")]
    InvalidPageSize,
    /// The current-format adapter is not available.
    #[error("current session-format adapter is unavailable")]
    AdapterUnavailable,
}

/// Failure returned when a session identity is empty.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("session identity must be nonempty")]
pub struct InvalidSessionId;
/// Failure returned when a page size is zero.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("page size must be greater than zero")]
pub struct InvalidPageSize;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_rejects_zero() {
        assert_eq!(PageSize::new(0), Err(InvalidPageSize));
    }

    #[test]
    fn append_receipt_preserves_requested_durability() {
        let receipt = AppendReceipt {
            sequences: 4..=6,
            durability: Durability::Durable,
        };
        assert_eq!(receipt.durability, Durability::Durable);
        assert_eq!(receipt.sequences, 4..=6);
    }
}
