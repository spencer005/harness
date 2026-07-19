//! Core runtime, storage, protocol, and tool orchestration for the harness.

#![recursion_limit = "256"]

/// Actor mailbox types and runtime command/event contracts.
pub mod actors;
/// Spoofed Codex analytics emitter for backend telemetry compliance.
pub mod analytics;
/// Native apply-patch implementation and errors.
pub(crate) mod apply_patch;
/// Command registration, validation, and runtime lookup.
pub mod commands;
/// Context-window compaction planning and token estimation.
pub mod compact;
/// Top-level harness actor and session orchestration.
pub mod harness;
/// Framed IPC protocol for structured frontend integration.
pub mod ipc;
/// Provider runtime builder and validated runtime.
pub mod provider_runtime;
/// Provider transport actor abstraction.
pub mod provider_transport;
/// Provider profile domain and persistence types.
pub mod providers;
/// Responses API request, model, and WebSocket actor types.
pub mod responses;
/// Durable session identifiers, records, indexes, and storage.
pub mod sessions;
/// User steering queue and delivery modes.
pub mod steering;
/// Subagent scheduling state and events.
pub mod subagents;
/// PTY-backed terminal tool manager.
pub(crate) mod terminal;
/// Native tool definitions and parsing helpers.
pub mod tools;

use serde::{Deserialize, Serialize};

/// Renderable transcript entry consumed and returned by a UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiTranscriptEntry {
    /// Plain UI transcript text with optional persisted source identity.
    Text {
        /// Persisted session sequence when this text originates from storage.
        source_sequence: Option<u64>,
        /// Plain transcript text.
        text: String,
    },
    /// Typed durable session record with optional persisted source identity.
    SessionRecord {
        /// Persisted session sequence when known.
        source_sequence: Option<u64>,
        /// Structured session record.
        record: sessions::SessionRecordKind,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Renderable state consumed by the TUI.
pub struct UiSnapshot {
    /// Stable session identifier shown to the user.
    pub session_id: String,
    /// Human-readable thread title.
    pub thread_title: String,
    /// Current provider profile summary.
    pub provider: Option<providers::ProviderUiInfo>,
    /// Active model and request settings.
    pub model_settings: responses::ModelSettings,
    /// Whether user input is submitted as developer text.
    pub developer_mode: bool,
    /// Whether a response stream is currently active.
    pub response_streaming: bool,
    /// Time to first token for the most recent stream, in milliseconds.
    pub last_ttft_ms: Option<u64>,
    /// In-memory transcript entries rendered by the TUI.
    pub transcript_entries: Vec<UiTranscriptEntry>,
    /// Current prompt input buffer.
    pub input: String,
    /// Byte cursor position inside `input`.
    pub input_cursor: usize,
    /// Steering prompt queued for the next tool call.
    pub queued_steering_prompt: Option<String>,
    /// Subagent summaries shown in the activity panel.
    pub agents: Vec<subagents::AgentSummary>,
    /// Subagent and background activity IDs currently running.
    pub active_activities: Vec<String>,
}

impl Default for UiSnapshot {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            thread_title: String::new(),
            provider: None,
            model_settings: Default::default(),
            developer_mode: harness::DEFAULT_DEVELOPER_MODE,
            response_streaming: false,
            last_ttft_ms: None,
            transcript_entries: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            queued_steering_prompt: None,
            agents: Vec::new(),
            active_activities: Vec::new(),
        }
    }
}
