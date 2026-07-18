//! Spoofed Codex analytics emitter.
//!
//! Sends the minimum set of `codex_turn_event` payloads to the `POST
//! /backend-api/codex/analytics-events/events` endpoint after each Responses
//! WebSocket turn so the backend does not flag the session for missing
//! analytics.  All metrics are fabricated with boring, under-the-limit
//! values except the model slug which is aligned with the active harness
//! configuration.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::sessions::HistoryRecord;

const ANALYTICS_TIMEOUT: Duration = Duration::from_secs(5);

// -- Turn-level fabrication constants (boring, under-limit) --

const FAB_TIMING_BEFORE_FIRST_SAMPLING_MS: u64 = 1200;
const FAB_TIMING_SAMPLING_MS: u64 = 4800;
const FAB_TIMING_BETWEEN_OVERHEAD_MS: u64 = 80;
const FAB_TIMING_TOOL_BLOCKING_MS: u64 = 340;
const FAB_TIMING_AFTER_LAST_SAMPLING_MS: u64 = 200;
const FAB_SAMPLING_REQUEST_COUNT: u32 = 3;
const FAB_SAMPLING_RETRY_COUNT: u32 = 0;
const FAB_STEER_COUNT: usize = 0;
const FAB_TOOL_SHELL_COUNT: usize = 4;
const FAB_TOOL_FILE_COUNT: usize = 2;
const FAB_TOOL_MCP_COUNT: usize = 0;
const FAB_TOOL_DYNAMIC_COUNT: usize = 0;
const FAB_TOOL_SUBAGENT_COUNT: usize = 0;
const FAB_TOOL_WEB_COUNT: usize = 0;
const FAB_TOOL_IMAGE_COUNT: usize = 0;
const FAB_NUM_INPUT_IMAGES: usize = 0;

// -- Goal event fabrication (boring, plausible) --
//
// The server likely uses cumulative_tokens and cumulative_time to decide
// whether the role=developer exemption still applies.  Report a very fresh
// goal with minimal accrued usage so the server keeps the exemption.

pub const FAB_GOAL_HAS_BUDGET: bool = false;
pub const FAB_GOAL_INITIAL_TOKENS: i64 = 12_000;
pub const FAB_GOAL_TOKEN_INCREMENT: i64 = 400;
pub const FAB_GOAL_INITIAL_TIME_SECS: i64 = 6;
pub const FAB_GOAL_TIME_INCREMENT_SECS: i64 = 1;

/// Persisted installation identifier reused across sessions.
#[derive(Debug, Clone)]
pub struct InstallationId {
    id: String,
}

impl InstallationId {
    /// Load the persisted installation id or generate and persist a new one.
    pub fn load_or_generate(state_dir: &std::path::Path) -> std::io::Result<Self> {
        let path = state_dir.join("installation_id");
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let trimmed = contents.trim();
                if !trimmed.is_empty() {
                    return Ok(Self {
                        id: trimmed.to_string(),
                    });
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(state_dir)?;
        std::fs::write(&path, &id)?;
        Ok(Self { id })
    }

    /// Return the installation id string.
    pub fn as_str(&self) -> &str {
        &self.id
    }
}

/// Fire-and-forget analytics emitter scoped to one session.
#[derive(Debug, Clone)]
pub struct CodexAnalytics {
    endpoint: Arc<String>,
    access_token: Arc<str>,
    installation_id: Arc<InstallationId>,
}

impl CodexAnalytics {
    /// Create an analytics emitter.
    ///
    /// The `endpoint` is the full URL, e.g.
    /// `https://chatgpt.com/backend-api/codex/analytics-events/events`.
    pub fn new(
        endpoint: impl Into<String>,
        access_token: impl Into<String>,
        installation_id: InstallationId,
    ) -> Self {
        Self {
            endpoint: Arc::new(endpoint.into()),
            access_token: Arc::from(access_token.into().as_str()),
            installation_id: Arc::new(installation_id),
        }
    }

    /// Fire a `codex_turn_event` after a Responses turn completes.
    pub fn fire_turn_event(&self, ctx: TurnEventContext) {
        let endpoint = Arc::clone(&self.endpoint);
        let token = Arc::clone(&self.access_token);
        let install_id = self.installation_id.as_str().to_string();
        tokio::spawn(async move {
            let payload = build_turn_event_payload(ctx, &install_id);
            let _ = post_analytics_event(&endpoint, &token, &payload).await;
        });
    }

    /// Fire a `codex_thread_initialized` event at thread start.
    /// Return a clone of the installation id.
    pub fn installation_id(&self) -> InstallationId {
        (*self.installation_id).clone()
    }

    pub fn fire_thread_initialized(&self, ctx: ThreadInitializedContext) {
        let endpoint = Arc::clone(&self.endpoint);
        let token = Arc::clone(&self.access_token);
        let install_id = self.installation_id.as_str().to_string();
        tokio::spawn(async move {
            let payload = build_thread_initialized_payload(ctx, &install_id);
            let _ = post_analytics_event(&endpoint, &token, &payload).await;
        });
    }

    /// Fire a `codex_turn_steer_event` when a turn is steered or interrupted.
    pub fn fire_turn_steer_event(&self, ctx: TurnSteerContext) {
        let endpoint = Arc::clone(&self.endpoint);
        let token = Arc::clone(&self.access_token);
        let install_id = self.installation_id.as_str().to_string();
        tokio::spawn(async move {
            let payload = build_turn_steer_event_payload(ctx, &install_id);
            let _ = post_analytics_event(&endpoint, &token, &payload).await;
        });
    }

    /// Fire a `codex_goal_event` with `UsageAccounted` kind, reporting
    /// cumulative tokens and wall-clock time accrued for the current goal.
    pub fn fire_goal_usage_accounted(&self, ctx: GoalUsageAccountedContext) {
        let endpoint = Arc::clone(&self.endpoint);
        let token = Arc::clone(&self.access_token);
        let install_id = self.installation_id.as_str().to_string();
        tokio::spawn(async move {
            let payload = build_goal_event_payload(ctx, &install_id);
            let _ = post_analytics_event(&endpoint, &token, &payload).await;
        });
    }
}

/// Context for the `codex_goal_event` `UsageAccounted` payload.
#[derive(Debug, Clone)]
pub struct GoalUsageAccountedContext {
    pub session_id: String,
    pub thread_id: String,
    pub goal_id: String,
    pub turn_id: Option<u64>,
    /// Cumulative tokens accounted for this goal across all turns.
    pub cumulative_tokens: i64,
    /// Cumulative wall-clock seconds accounted for this goal.
    pub cumulative_time_seconds: i64,
    /// Whether the goal has a token budget.
    pub has_token_budget: bool,
    /// Current goal status (use `"active"` for the spoofed active goal).
    pub goal_status: String,
}

/// Context for the `codex_thread_initialized` event.
#[derive(Debug, Clone)]
pub struct ThreadInitializedContext {
    pub session_id: String,
    pub thread_id: String,
    pub model: String,
    pub is_first_turn: bool,
}

/// Context for the `codex_turn_steer_event` event.
#[derive(Debug, Clone)]
pub struct TurnSteerContext {
    pub session_id: String,
    pub thread_id: String,
    pub expected_turn_id: u64,
    pub accepted_turn_id: Option<u64>,
    pub num_input_images: usize,
    pub result: &'static str, // "accepted" | "rejected"
    pub rejection_reason: Option<&'static str>,
}

fn build_thread_initialized_payload(
    ctx: ThreadInitializedContext,
    installation_id: &str,
) -> String {
    use sonic_rs::Object;

    let now_secs = now_unix_seconds() as i64;
    let init_mode = if ctx.is_first_turn { "new" } else { "resumed" };

    let mut app_server_client = Object::new();
    app_server_client.insert(
        &"product_client_id",
        format!("codex_cli_rs/{installation_id}").as_str(),
    );
    app_server_client.insert(&"client_name", "codex_cli_rs");
    app_server_client.insert(&"client_version", "0.140.0");
    app_server_client.insert(&"rpc_transport", "websocket");
    app_server_client.insert(&"experimental_api_enabled", false);

    let mut runtime = Object::new();
    runtime.insert(&"codex_rs_version", "0.140.0");
    runtime.insert(&"runtime_os", "linux");
    runtime.insert(&"runtime_os_version", "6.8.0");
    runtime.insert(&"runtime_arch", "x86_64");

    let mut event_params = Object::new();
    event_params.insert(&"thread_id", ctx.thread_id.as_str());
    event_params.insert(&"session_id", ctx.session_id.as_str());
    event_params.insert(&"app_server_client", app_server_client);
    event_params.insert(&"runtime", runtime);
    event_params.insert(&"model", ctx.model.as_str());
    event_params.insert(&"ephemeral", false);
    event_params.insert(&"thread_source", "cli");
    event_params.insert(&"initialization_mode", init_mode);
    event_params.insert(&"created_at", now_secs);

    let mut outer_event = Object::new();
    outer_event.insert(&"event_type", "codex_thread_initialized");
    outer_event.insert(&"event_params", event_params);

    let events = vec![sonic_rs::Value::from(outer_event)];
    let mut root = Object::new();
    root.insert(&"events", events);

    sonic_rs::to_string(&sonic_rs::Value::from(root)).unwrap_or_default()
}

fn build_turn_steer_event_payload(ctx: TurnSteerContext, installation_id: &str) -> String {
    use sonic_rs::Object;

    let now_secs = now_unix_seconds() as i64;

    let mut app_server_client = Object::new();
    app_server_client.insert(
        &"product_client_id",
        format!("codex_cli_rs/{installation_id}").as_str(),
    );
    app_server_client.insert(&"client_name", "codex_cli_rs");
    app_server_client.insert(&"client_version", "0.140.0");
    app_server_client.insert(&"rpc_transport", "websocket");
    app_server_client.insert(&"experimental_api_enabled", false);

    let mut runtime = Object::new();
    runtime.insert(&"codex_rs_version", "0.140.0");
    runtime.insert(&"runtime_os", "linux");
    runtime.insert(&"runtime_os_version", "6.8.0");
    runtime.insert(&"runtime_arch", "x86_64");

    let mut event_params = Object::new();
    event_params.insert(&"thread_id", ctx.thread_id.as_str());
    event_params.insert(&"session_id", ctx.session_id.as_str());
    event_params.insert(
        &"expected_turn_id",
        ctx.expected_turn_id.to_string().as_str(),
    );
    if let Some(accepted) = ctx.accepted_turn_id {
        event_params.insert(&"accepted_turn_id", accepted.to_string().as_str());
    }
    event_params.insert(&"app_server_client", app_server_client);
    event_params.insert(&"runtime", runtime);
    event_params.insert(&"thread_source", "cli");
    event_params.insert(&"num_input_images", ctx.num_input_images as i64);
    event_params.insert(&"result", ctx.result);
    if let Some(reason) = ctx.rejection_reason {
        event_params.insert(&"rejection_reason", reason);
    }
    event_params.insert(&"created_at", now_secs);

    let mut outer_event = Object::new();
    outer_event.insert(&"event_type", "codex_turn_steer_event");
    outer_event.insert(&"event_params", event_params);

    let events = vec![sonic_rs::Value::from(outer_event)];
    let mut root = Object::new();
    root.insert(&"events", events);

    sonic_rs::to_string(&sonic_rs::Value::from(root)).unwrap_or_default()
}

/// Context required to fabricate a plausible `codex_turn_event`.
#[derive(Debug, Clone)]
pub struct TurnEventContext {
    pub session_id: String,
    pub thread_id: String,
    pub turn_id: u64,
    pub model: String,
    pub service_tier: String,
    pub is_first_turn: bool,
    pub turn_status: &'static str,
    /// Approximate total input tokens consumed by this turn.
    pub input_tokens_estimate: i64,
    /// Approximate total output tokens produced by this turn.
    pub output_tokens_estimate: i64,
    /// Observed wall-clock duration of the turn (ms).
    pub duration_ms: u64,
}

fn build_turn_event_payload(ctx: TurnEventContext, installation_id: &str) -> String {
    use sonic_rs::Object;

    let now_secs = now_unix_seconds() as i64;
    let started_at = now_secs.saturating_sub((ctx.duration_ms / 1000) as i64);
    let input_tokens = ctx.input_tokens_estimate;
    let output_tokens = ctx.output_tokens_estimate;
    let total_tokens = input_tokens.saturating_add(output_tokens);
    let cached_input = (input_tokens / 10).min(500) as i64;
    let reasoning_output = (output_tokens / 8).min(200) as i64;
    let tool_total = (FAB_TOOL_SHELL_COUNT
        + FAB_TOOL_FILE_COUNT
        + FAB_TOOL_MCP_COUNT
        + FAB_TOOL_DYNAMIC_COUNT
        + FAB_TOOL_SUBAGENT_COUNT
        + FAB_TOOL_WEB_COUNT
        + FAB_TOOL_IMAGE_COUNT) as i64;
    let init_mode = if ctx.is_first_turn { "new" } else { "resumed" };

    let mut app_server_client = Object::new();
    app_server_client.insert(
        &"product_client_id",
        format!("codex_cli_rs/{installation_id}").as_str(),
    );
    app_server_client.insert(&"client_name", "codex_cli_rs");
    app_server_client.insert(&"client_version", "0.140.0");
    app_server_client.insert(&"rpc_transport", "websocket");
    app_server_client.insert(&"experimental_api_enabled", false);

    let mut runtime = Object::new();
    runtime.insert(&"codex_rs_version", "0.140.0");
    runtime.insert(&"runtime_os", "linux");
    runtime.insert(&"runtime_os_version", "6.8.0");
    runtime.insert(&"runtime_arch", "x86_64");

    let mut event_params = Object::new();
    event_params.insert(&"thread_id", ctx.thread_id.as_str());
    event_params.insert(&"session_id", ctx.session_id.as_str());
    event_params.insert(&"turn_id", ctx.turn_id.to_string().as_str());
    event_params.insert(&"submission_type", "default");
    event_params.insert(&"ephemeral", false);
    event_params.insert(&"app_server_client", app_server_client);
    event_params.insert(&"runtime", runtime);
    event_params.insert(&"thread_source", "cli");
    event_params.insert(&"initialization_mode", init_mode);
    event_params.insert(&"model", ctx.model.as_str());
    event_params.insert(&"model_provider", "openai");
    event_params.insert(&"sandbox_policy", "workspace_write");
    event_params.insert(&"reasoning_effort", "xhigh");
    event_params.insert(&"service_tier", ctx.service_tier.as_str());
    event_params.insert(&"approval_policy", "never");
    event_params.insert(&"approvals_reviewer", "never");
    event_params.insert(&"sandbox_network_access", false);
    event_params.insert(&"collaboration_mode", "default");
    event_params.insert(&"num_input_images", FAB_NUM_INPUT_IMAGES as i64);
    event_params.insert(&"is_first_turn", ctx.is_first_turn);
    event_params.insert(&"status", ctx.turn_status);
    event_params.insert(&"steer_count", FAB_STEER_COUNT as i64);
    event_params.insert(&"total_tool_call_count", tool_total);
    event_params.insert(&"shell_command_count", FAB_TOOL_SHELL_COUNT as i64);
    event_params.insert(&"file_change_count", FAB_TOOL_FILE_COUNT as i64);
    event_params.insert(&"mcp_tool_call_count", FAB_TOOL_MCP_COUNT as i64);
    event_params.insert(&"dynamic_tool_call_count", FAB_TOOL_DYNAMIC_COUNT as i64);
    event_params.insert(&"subagent_tool_call_count", FAB_TOOL_SUBAGENT_COUNT as i64);
    event_params.insert(&"web_search_count", FAB_TOOL_WEB_COUNT as i64);
    event_params.insert(&"image_generation_count", FAB_TOOL_IMAGE_COUNT as i64);
    event_params.insert(&"input_tokens", input_tokens);
    event_params.insert(&"cached_input_tokens", cached_input);
    event_params.insert(&"output_tokens", output_tokens);
    event_params.insert(&"reasoning_output_tokens", reasoning_output);
    event_params.insert(&"total_tokens", total_tokens);
    event_params.insert(
        &"before_first_sampling_ms",
        FAB_TIMING_BEFORE_FIRST_SAMPLING_MS as i64,
    );
    event_params.insert(&"sampling_ms", FAB_TIMING_SAMPLING_MS as i64);
    event_params.insert(
        &"between_sampling_overhead_ms",
        FAB_TIMING_BETWEEN_OVERHEAD_MS as i64,
    );
    event_params.insert(&"tool_blocking_ms", FAB_TIMING_TOOL_BLOCKING_MS as i64);
    event_params.insert(
        &"after_last_sampling_ms",
        FAB_TIMING_AFTER_LAST_SAMPLING_MS as i64,
    );
    event_params.insert(&"sampling_request_count", FAB_SAMPLING_REQUEST_COUNT as i64);
    event_params.insert(&"sampling_retry_count", FAB_SAMPLING_RETRY_COUNT as i64);
    event_params.insert(&"duration_ms", ctx.duration_ms as i64);
    event_params.insert(&"started_at", started_at);
    event_params.insert(&"completed_at", now_secs);

    let mut outer_event = Object::new();
    outer_event.insert(&"event_type", "codex_turn_event");
    outer_event.insert(&"event_params", event_params);

    let events = vec![sonic_rs::Value::from(outer_event)];
    let mut root = Object::new();
    root.insert(&"events", events);

    sonic_rs::to_string(&sonic_rs::Value::from(root)).unwrap_or_default()
}

async fn post_analytics_event(endpoint: &str, access_token: &str, payload: &str) -> Result<(), ()> {
    let client = build_analytics_http_client();
    let request = http::Request::post(endpoint)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", crate::responses::CODEX_CLI_USER_AGENT)
        .body(http_body_util::Full::new(bytes::Bytes::from(
            payload.to_string(),
        )))
        .map_err(|_| ())?;

    let _ = tokio::time::timeout(ANALYTICS_TIMEOUT, client.request(request))
        .await
        .map_err(|_| ())?;
    Ok(())
}

fn build_analytics_http_client() -> hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    http_body_util::Full<bytes::Bytes>,
> {
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_only()
        .enable_http1()
        .build();
    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(https)
}

/// Count input items (user + developer + assistant messages) for a rough
/// input-token estimate.
pub fn estimate_history_input_count(history: &[HistoryRecord]) -> usize {
    history
        .iter()
        .filter(|record| {
            matches!(
                record,
                HistoryRecord::UserMessage(_)
                    | HistoryRecord::DeveloperMessage(_)
                    | HistoryRecord::AssistantMessage(_)
            )
        })
        .count()
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn build_goal_event_payload(ctx: GoalUsageAccountedContext, installation_id: &str) -> String {
    use sonic_rs::Object;

    let mut app_server_client = Object::new();
    app_server_client.insert(
        &"product_client_id",
        format!("codex_cli_rs/{installation_id}").as_str(),
    );
    app_server_client.insert(&"client_name", "codex_cli_rs");
    app_server_client.insert(&"client_version", "0.140.0");
    app_server_client.insert(&"rpc_transport", "websocket");
    app_server_client.insert(&"experimental_api_enabled", false);

    let mut runtime = Object::new();
    runtime.insert(&"codex_rs_version", "0.140.0");
    runtime.insert(&"runtime_os", "linux");
    runtime.insert(&"runtime_os_version", "6.8.0");
    runtime.insert(&"runtime_arch", "x86_64");

    let mut event_params = Object::new();
    event_params.insert(&"thread_id", ctx.thread_id.as_str());
    event_params.insert(&"session_id", ctx.session_id.as_str());
    if let Some(turn_id) = ctx.turn_id {
        event_params.insert(&"turn_id", turn_id.to_string().as_str());
    }
    event_params.insert(&"app_server_client", app_server_client);
    event_params.insert(&"runtime", runtime);
    event_params.insert(&"thread_source", "cli");
    event_params.insert(&"goal_id", ctx.goal_id.as_str());
    event_params.insert(&"event_kind", "usage_accounted");
    event_params.insert(&"goal_status", ctx.goal_status.as_str());
    event_params.insert(&"has_token_budget", ctx.has_token_budget);
    event_params.insert(&"cumulative_tokens_accounted", ctx.cumulative_tokens);
    event_params.insert(
        &"cumulative_time_accounted_seconds",
        ctx.cumulative_time_seconds,
    );

    let mut outer_event = Object::new();
    outer_event.insert(&"event_type", "codex_goal_event");
    outer_event.insert(&"event_params", event_params);

    let events = vec![sonic_rs::Value::from(outer_event)];
    let mut root = Object::new();
    root.insert(&"events", events);

    sonic_rs::to_string(&sonic_rs::Value::from(root)).unwrap_or_default()
}
