//! Codex endpoint fuzzing harness.
//!
//! Probes the ChatGPT Codex backend for:
//! - Server-side feature flags leaked through `x-codex-beta-features` responses
//! - Subagent routing differences via `x-openai-subagent`
//! - Version-gated model catalogs via `/models?client_version=`
//! - Error message leaks from malformed WebSocket frames
//! - Undocumented HTTP endpoints under the Codex base path
//! - Originator/client-identity spoofing effects
//!
//! Usage:
//!   cargo run --bin harness-fuzz -- [--quick] [--vector <name>]

use std::{
    collections::BTreeMap,
    env,
    fmt::Write,
    fs,
    io::Write as IoWrite,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{SinkExt, StreamExt};
use harness_responses_api::{
    ApiEndpoint, ApiProvider, CODEX_CLI_USER_AGENT, CodexHeaders, DEFAULT_CODEX_ORIGINATOR,
    lean_codex_default_headers,
};
use http::{HeaderMap, HeaderName, HeaderValue};
use http_body_util::BodyExt;
use serde::Deserialize;
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value, json};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig},
};

const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const FUZZ_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const FUZZ_WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const FUZZ_WARMUP_WAIT: Duration = Duration::from_secs(2);
const QUICK_MODE_MAX_PER_VECTOR: usize = 3;

static NEXT_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

// ── Auth mode ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    OwnState,
    CodexReadOnly,
}

// ── Fuzz vector registry ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum FuzzVector {
    BetaFeatures,
    Subagent,
    Originator,
    OpenAiBeta,
    ClientVersion,
    HttpEndpoints,
    FrameMutations,
    ExtraHeaders,
}

impl FuzzVector {
    fn all() -> Vec<Self> {
        use FuzzVector::*;
        vec![
            BetaFeatures,
            Subagent,
            Originator,
            OpenAiBeta,
            ClientVersion,
            HttpEndpoints,
            FrameMutations,
            ExtraHeaders,
        ]
    }

    fn name(&self) -> &'static str {
        match self {
            Self::BetaFeatures => "beta_features",
            Self::Subagent => "subagent",
            Self::Originator => "originator",
            Self::OpenAiBeta => "openai_beta",
            Self::ClientVersion => "client_version",
            Self::HttpEndpoints => "http_endpoints",
            Self::FrameMutations => "frame_mutations",
            Self::ExtraHeaders => "extra_headers",
        }
    }

    fn description(&self) -> &'static str {
        match self {
            Self::BetaFeatures => {
                "Probe x-codex-beta-features for server-side feature flag enumeration"
            }
            Self::Subagent => "Probe x-openai-subagent for routing/behavior differences",
            Self::Originator => "Spoof originator header to test client identity gating",
            Self::OpenAiBeta => "Probe openai-beta header for protocol version discovery",
            Self::ClientVersion => "Probe /models?client_version= for version-gated model catalogs",
            Self::HttpEndpoints => "Enumerate HTTP endpoints under the Codex base path",
            Self::FrameMutations => "Send malformed WebSocket frames to trigger error info leaks",
            Self::ExtraHeaders => {
                "Send Codex-specific extra headers to probe for undocumented behavior"
            }
        }
    }
}

// ── Observation types ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct FuzzObservation {
    vector: &'static str,
    probe: String,
    status: ObservationStatus,
    response_body: Option<String>,
    headers: BTreeMap<String, String>,
    elapsed_ms: u128,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ObservationStatus {
    Success,
    Error,
    Different,
    Forbidden,
    Unauthorized,
    Timeout,
    Unique,
}

fn emit(json: &serde_json::Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(
        stdout,
        "{}",
        serde_json::to_string(json).unwrap_or_default()
    );
    let _ = stdout.flush();
}

fn emit_observation(obs: &FuzzObservation) {
    let mut headers_json = serde_json::Map::new();
    for (k, v) in &obs.headers {
        headers_json.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    let status = match obs.status {
        ObservationStatus::Success => "success",
        ObservationStatus::Error => "error",
        ObservationStatus::Different => "DIFFERENT",
        ObservationStatus::Forbidden => "forbidden",
        ObservationStatus::Unauthorized => "unauthorized",
        ObservationStatus::Timeout => "timeout",
        ObservationStatus::Unique => "UNIQUE",
    };
    emit(&serde_json::json!({
        "vector": obs.vector,
        "probe": obs.probe,
        "status": status,
        "response_body": obs.response_body,
        "response_headers": headers_json,
        "elapsed_ms": obs.elapsed_ms,
        "error": obs.error,
    }));
}

fn emit_banner(vector: &FuzzVector) {
    emit(&serde_json::json!({
        "banner": format!("━━━ {} ━━━", vector.description()),
        "vector": vector.name(),
    }));
}

fn emit_summary(observations: &[FuzzObservation]) {
    let total = observations.len();
    let unique = observations
        .iter()
        .filter(|o| o.status == ObservationStatus::Unique)
        .count();
    let different = observations
        .iter()
        .filter(|o| o.status == ObservationStatus::Different)
        .count();
    let errors = observations
        .iter()
        .filter(|o| o.status == ObservationStatus::Error)
        .count();
    let forbidden = observations
        .iter()
        .filter(|o| o.status == ObservationStatus::Forbidden)
        .count();

    emit(&serde_json::json!({
        "summary": {
            "total": total,
            "unique_responses": unique,
            "different_from_baseline": different,
            "errors": errors,
            "forbidden": forbidden,
        }
    }));
}

// ── Auth loading (vendored from CLI main.rs) ────────────────────────────────

fn codex_home() -> Result<PathBuf, String> {
    if let Ok(root) = env::var("CODEX_HOME") {
        return Ok(PathBuf::from(root));
    }
    let home = env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    Ok(PathBuf::from(home).join(".codex"))
}

fn harness_state_dir() -> Result<PathBuf, String> {
    if let Ok(root) = env::var("XDG_STATE_HOME")
        && !root.trim().is_empty()
    {
        return Ok(PathBuf::from(root).join("new_harness"));
    }
    let home = env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    Ok(PathBuf::from(home).join(".local/state/new_harness"))
}

#[derive(Debug, Deserialize)]
struct CodexAuthJson {
    tokens: Option<CodexAuthTokensJson>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthTokensJson {
    access_token: Option<String>,
}

#[derive(Debug)]
struct CodexAuthSnapshot {
    access_token: String,
}

fn read_codex_auth_snapshot(auth_path: &Path) -> Result<CodexAuthSnapshot, String> {
    let bytes =
        fs::read(auth_path).map_err(|e| format!("failed to read {}: {e}", auth_path.display()))?;
    let auth: CodexAuthJson = sonic_rs::from_slice(&bytes)
        .map_err(|e| format!("failed to parse {}: {e}", auth_path.display()))?;
    let tokens = auth
        .tokens
        .ok_or("missing tokens in auth.json".to_string())?;
    let access_token = tokens
        .access_token
        .filter(|t| !t.trim().is_empty())
        .ok_or("missing access_token".to_string())?;
    Ok(CodexAuthSnapshot { access_token })
}

#[derive(Debug, Deserialize)]
struct HarnessAuthJson {
    tokens: HarnessAuthTokensJson,
}

#[derive(Debug, Deserialize)]
struct HarnessAuthTokensJson {
    access_token: String,
}

fn read_harness_auth_snapshot(auth_path: &Path) -> Result<String, String> {
    let bytes =
        fs::read(auth_path).map_err(|e| format!("failed to read {}: {e}", auth_path.display()))?;
    let auth: HarnessAuthJson = sonic_rs::from_slice(&bytes)
        .map_err(|e| format!("failed to parse {}: {e}", auth_path.display()))?;
    Ok(auth.tokens.access_token)
}

fn load_auth_token(auth_mode: AuthMode) -> Result<String, String> {
    let codex_home = codex_home()?;
    let state_dir = harness_state_dir()?;
    match auth_mode {
        AuthMode::OwnState => {
            let auth_path = state_dir.join("auth.json");
            if auth_path.exists() {
                read_harness_auth_snapshot(&auth_path)
            } else {
                Ok(read_codex_auth_snapshot(&codex_home.join("auth.json"))?.access_token)
            }
        }
        AuthMode::CodexReadOnly => {
            Ok(read_codex_auth_snapshot(&codex_home.join("auth.json"))?.access_token)
        }
    }
}

fn provider_from_env() -> Result<ApiProvider, String> {
    let base_url =
        env::var("OPENAI_BASE_URL").unwrap_or_else(|_| CHATGPT_CODEX_BASE_URL.to_string());
    ApiProvider::new(&base_url).map_err(|err| err.describe())
}

// ── HTTP probe ──────────────────────────────────────────────────────────────

fn api_http_client() -> hyper_util::client::legacy::Client<
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

async fn http_get(
    url: &str,
    access_token: &str,
    extra_headers: HeaderMap,
) -> Result<(u16, HeaderMap, String), String> {
    let client = api_http_client();
    let mut req = http::Request::get(url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", CODEX_CLI_USER_AGENT)
        .header("originator", DEFAULT_CODEX_ORIGINATOR)
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .map_err(|e| e.to_string())?;

    for (name, value) in extra_headers.iter() {
        req.headers_mut().insert(name.clone(), value.clone());
    }

    let resp = tokio::time::timeout(FUZZ_REQUEST_TIMEOUT, client.request(req))
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| e.to_string())?;

    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = String::from_utf8_lossy(
        &resp
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes(),
    )
    .to_string();

    Ok((status, headers, body))
}

// ── WebSocket helpers ───────────────────────────────────────────────────────

fn generate_session_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = NEXT_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bits = now ^ (u128::from(std::process::id()) << 64) ^ u128::from(counter);
    bits ^= bits.rotate_left(31);
    bits = bits.wrapping_mul(0x9e37_79b9_7f4a_7c15_d1b5_4a32_d192_ed03);
    bits &= !(0xf_u128 << 76);
    bits |= 0x4_u128 << 76;
    bits &= !(0x3_u128 << 62);
    bits |= 0x2_u128 << 62;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (bits >> 96) as u32,
        (bits >> 80) as u16,
        (bits >> 64) as u16,
        (bits >> 48) as u16,
        bits & 0xffff_ffff_ffff_u128
    )
}

fn default_response_create_body() -> Value {
    json!({
        "type": "response.create",
        "model": "gpt-5.5",
        "instructions": "Reply with exactly one word: OK.",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "Reply with exactly one word: OK."}]
        }],
        "tools": [],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "store": false,
        "stream": true,
        "include": []
    })
}

struct WsStreamResult {
    handshake_headers: BTreeMap<String, String>,
    frames: Vec<(String, Value)>,
    error: Option<String>,
}

async fn ws_stream_with_headers(
    provider: &ApiProvider,
    access_token: &str,
    codex_headers: CodexHeaders,
    body: &Value,
) -> WsStreamResult {
    let url = provider.websocket_endpoint_url(ApiEndpoint::Responses);

    let mut request_headers = lean_codex_default_headers();
    request_headers.extend(codex_headers.to_header_map().unwrap_or_default());
    request_headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access_token}")).unwrap(),
    );

    let mut request = match url.as_str().into_client_request() {
        Ok(r) => r,
        Err(e) => {
            return WsStreamResult {
                handshake_headers: BTreeMap::new(),
                frames: Vec::new(),
                error: Some(e.to_string()),
            };
        }
    };
    request.headers_mut().extend(request_headers);

    let (stream, response) = match tokio::time::timeout(
        FUZZ_WS_HANDSHAKE_TIMEOUT,
        connect_async_tls_with_config(request, Some(WebSocketConfig::default()), false, None),
    )
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return WsStreamResult {
                handshake_headers: BTreeMap::new(),
                frames: Vec::new(),
                error: Some(e.to_string()),
            };
        }
        Err(_) => {
            return WsStreamResult {
                handshake_headers: BTreeMap::new(),
                frames: Vec::new(),
                error: Some("handshake timeout".to_string()),
            };
        }
    };

    let mut hh = BTreeMap::new();
    for (name, value) in response.headers() {
        if let Ok(v) = value.to_str() {
            hh.insert(name.to_string(), v.to_string());
        }
    }

    let (mut ws_tx, mut ws_rx) = stream.split();

    let request_text = sonic_rs::to_string(body).unwrap_or_default();
    if let Err(e) = tokio::time::timeout(
        Duration::from_secs(5),
        ws_tx.send(tokio_tungstenite::tungstenite::Message::Text(
            request_text.into(),
        )),
    )
    .await
    {
        return WsStreamResult {
            handshake_headers: hh,
            frames: Vec::new(),
            error: Some(format!("send timeout: {e}")),
        };
    }

    let mut frames = Vec::new();
    loop {
        let msg = match tokio::time::timeout(FUZZ_REQUEST_TIMEOUT, ws_rx.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => {
                return WsStreamResult {
                    handshake_headers: hh,
                    frames,
                    error: Some(e.to_string()),
                };
            }
            Ok(None) => break,
            Err(_) => {
                return WsStreamResult {
                    handshake_headers: hh,
                    frames,
                    error: Some("stream timeout".to_string()),
                };
            }
        };

        match msg {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                let text = text.to_string();
                let value: Value =
                    sonic_rs::from_str(&text).unwrap_or_else(|_| Value::from(text.as_str()));
                let event_type = value
                    .as_object()
                    .and_then(|o| o.get(&"type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                frames.push((event_type.clone(), value));
                if event_type == "response.completed" || event_type == "response.done" {
                    break;
                }
            }
            tokio_tungstenite::tungstenite::Message::Close(close) => {
                return WsStreamResult {
                    handshake_headers: hh,
                    frames,
                    error: Some(format!("server close: {:?}", close)),
                };
            }
            _ => {}
        }
    }

    let _ = ws_tx.close().await;
    WsStreamResult {
        handshake_headers: hh,
        frames,
        error: None,
    }
}

async fn raw_ws_handshake(
    provider: &ApiProvider,
    access_token: &str,
    extra_headers: HeaderMap,
) -> Result<BTreeMap<String, String>, String> {
    let url = provider.websocket_endpoint_url(ApiEndpoint::Responses);

    let mut request_headers = lean_codex_default_headers();
    request_headers.insert(
        http::header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access_token}")).unwrap(),
    );
    for (name, value) in extra_headers.iter() {
        request_headers.insert(name.clone(), value.clone());
    }

    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| e.to_string())?;
    request.headers_mut().extend(request_headers);

    match tokio::time::timeout(
        FUZZ_WS_HANDSHAKE_TIMEOUT,
        connect_async_tls_with_config(request, Some(WebSocketConfig::default()), false, None),
    )
    .await
    {
        Ok(Ok((mut ws, resp))) => {
            let mut hh = BTreeMap::new();
            for (name, value) in resp.headers() {
                if let Ok(v) = value.to_str() {
                    hh.insert(name.to_string(), v.to_string());
                }
            }
            let _ = ws.close(None).await;
            Ok(hh)
        }
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("handshake timeout".to_string()),
    }
}

// ── Response comparison ─────────────────────────────────────────────────────

fn response_signature(obs: &FuzzObservation) -> String {
    let body = obs.response_body.as_deref().unwrap_or("<no-body>");
    let normalized: String = body
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let preview: String = normalized.chars().take(500).collect();
    format!("{}|{}", preview, body.len())
}

fn dedup_observations(observations: &mut Vec<FuzzObservation>) {
    let mut seen: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, obs) in observations.iter().enumerate() {
        seen.entry(response_signature(obs)).or_default().push(i);
    }
    let mut deduped = Vec::new();
    for indices in seen.values() {
        deduped.push(observations[indices[0]].clone());
        if indices.len() > 1 {
            deduped.last_mut().unwrap().status = ObservationStatus::Unique;
        }
    }
    *observations = deduped;
}

fn classify_ws_result(result: &WsStreamResult, baseline: &WsStreamResult) -> ObservationStatus {
    if let Some(ref err) = result.error {
        if err.contains("401") || err.contains("Unauthorized") {
            return ObservationStatus::Unauthorized;
        }
        if err.contains("403") || err.contains("Forbidden") {
            return ObservationStatus::Forbidden;
        }
        return ObservationStatus::Error;
    }
    if result.handshake_headers != baseline.handshake_headers {
        return ObservationStatus::Different;
    }
    ObservationStatus::Success
}

fn headers_to_string(headers: &BTreeMap<String, String>) -> String {
    headers.iter().fold(String::new(), |mut s, (k, v)| {
        let _ = write!(s, "{k}: {v}\n");
        s
    })
}

// ── Fuzz vector implementations ─────────────────────────────────────────────

// ── 1. Beta Features ───────────────────────────────────────────────────────

async fn fuzz_beta_features(
    access_token: &str,
    baseline_headers: &CodexHeaders,
    body: &Value,
    quick: bool,
    provider: &ApiProvider,
) -> Vec<FuzzObservation> {
    let mut results = Vec::new();
    let baseline =
        ws_stream_with_headers(provider, access_token, baseline_headers.clone(), body).await;

    let probes: &[&str] = if quick {
        &[
            "auto_compact",
            "responses_websockets=2026-02-06",
            "thread_management_v2",
        ]
    } else {
        &[
            // Response/stream protocol
            "auto_compact",
            "auto_compact_v2",
            "responses_websockets=2026-02-06",
            "responses_websockets=2025-12-17",
            "responses_websockets=2025-06-01",
            // Thread/session management
            "thread_management_v2",
            "thread_management_v3",
            "thread_forking",
            "thread_summarization",
            // Tool features
            "parallel_tool_calls",
            "stream_tool_calls",
            "custom_tool_call_streaming",
            "function_call_streaming",
            // Reasoning
            "reasoning_effort",
            "extended_thinking",
            "thinking_tokens",
            // Context window
            "large_context",
            "extended_context",
            "context_window_1m",
            // Search/agents
            "web_search",
            "subagents",
            "agent_loop",
            // Vision/images
            "vision_input",
            "image_generation",
            "screenshot_tool",
            // Sandbox/security
            "sandbox_network",
            "sandbox_full",
            "approvals_v2",
            // Experimental/internal
            "experimental_features",
            "beta_features",
            "internal_testing",
            "debug_mode",
            // Codex-specific
            "codex_cli_rs",
            "codex_v2_backend",
            "codex_v3_backend",
            "codex_unified",
            // MCP
            "mcp_integration",
            "mcp_servers",
            "mcp_tools",
            // Freeform tools
            "freeform_tools",
            "freeform_parallel",
            "freeform_v2",
        ]
    };

    for probe in probes {
        let mut headers = baseline_headers.clone();
        headers.beta_features = Some(probe.to_string());
        let result = ws_stream_with_headers(provider, access_token, headers, body).await;

        let (status, error_msg) = if result.error.is_some() {
            if result.error.as_deref().unwrap().contains("401")
                || result.error.as_deref().unwrap().contains("403")
            {
                (ObservationStatus::Forbidden, result.error.clone())
            } else {
                (ObservationStatus::Error, result.error.clone())
            }
        } else if result.handshake_headers != baseline.handshake_headers
            || result.frames.len() != baseline.frames.len()
        {
            (ObservationStatus::Different, None)
        } else {
            (ObservationStatus::Success, None)
        };

        results.push(FuzzObservation {
            vector: "beta_features",
            probe: probe.to_string(),
            status,
            response_body: Some(headers_to_string(&result.handshake_headers)),
            headers: result.handshake_headers,
            elapsed_ms: 0,
            error: error_msg,
        });
    }

    dedup_observations(&mut results);
    results
}

// ── 2. Subagent ────────────────────────────────────────────────────────────

async fn fuzz_subagent(
    access_token: &str,
    baseline_headers: &CodexHeaders,
    body: &Value,
    quick: bool,
    provider: &ApiProvider,
) -> Vec<FuzzObservation> {
    let mut results = Vec::new();
    let baseline =
        ws_stream_with_headers(provider, access_token, baseline_headers.clone(), body).await;

    let probes: &[&str] = if quick {
        &["codex_cli_rs", "collab_spawn", ""]
    } else {
        &[
            "codex_cli_rs",
            "collab_spawn",
            "collab",
            "codex",
            "codex_cli",
            "chatgpt",
            "agent",
            "explore",
            "architect",
            "reviewer",
            "debugger",
            "test_writer",
            "code_reviewer",
            "planner",
            "executor",
            "researcher",
            "analyst",
            "builder",
            "refactor",
            "documentation",
            "security_auditor",
            "performance",
            "",
            "unknown_subagent",
            "__internal__",
            "system",
            "admin",
        ]
    };

    for probe in probes {
        let mut headers = baseline_headers.clone();
        headers.subagent = if probe.is_empty() {
            None
        } else {
            Some(probe.to_string())
        };
        let result = ws_stream_with_headers(provider, access_token, headers, body).await;
        let status = classify_ws_result(&result, &baseline);

        results.push(FuzzObservation {
            vector: "subagent",
            probe: probe.to_string(),
            status,
            response_body: Some(headers_to_string(&result.handshake_headers)),
            headers: result.handshake_headers,
            elapsed_ms: 0,
            error: result.error,
        });
    }

    dedup_observations(&mut results);
    results
}

// ── 3. Originator ──────────────────────────────────────────────────────────

async fn fuzz_originator(
    access_token: &str,
    baseline_headers: &CodexHeaders,
    body: &Value,
    quick: bool,
    provider: &ApiProvider,
) -> Vec<FuzzObservation> {
    let mut results = Vec::new();
    let baseline =
        ws_stream_with_headers(provider, access_token, baseline_headers.clone(), body).await;

    let probes: &[&str] = if quick {
        &["codex_cli_rs", "chatgpt-web", "openai"]
    } else {
        &[
            "codex_cli_rs",
            "codex_cli",
            "codex",
            "chatgpt",
            "chatgpt-web",
            "chatgpt-mac",
            "chatgpt-ios",
            "chatgpt-android",
            "openai",
            "openai-api",
            "playground",
            "api",
            "cli",
            "vscode",
            "vscode-codex",
            "cursor",
            "windsurf",
            "",
            "internal",
            "codex-internal",
            "admin",
        ]
    };

    for probe in probes {
        let session_id = generate_session_id();
        let headers = CodexHeaders::for_thread(&session_id, &session_id, format!("{session_id}:0"));

        let mut extra = HeaderMap::new();
        extra.insert(
            "originator",
            HeaderValue::from_str(probe).unwrap_or(HeaderValue::from_static("unknown")),
        );
        for (k, v) in headers.to_header_map().unwrap_or_default() {
            extra.insert(k.unwrap(), v);
        }

        match raw_ws_handshake(provider, access_token, extra).await {
            Ok(hh) => {
                let status = if hh != baseline.handshake_headers {
                    ObservationStatus::Different
                } else {
                    ObservationStatus::Success
                };
                results.push(FuzzObservation {
                    vector: "originator",
                    probe: probe.to_string(),
                    status,
                    response_body: Some(headers_to_string(&hh)),
                    headers: hh,
                    elapsed_ms: 0,
                    error: None,
                });
            }
            Err(e) => {
                results.push(FuzzObservation {
                    vector: "originator",
                    probe: probe.to_string(),
                    status: ObservationStatus::Error,
                    response_body: None,
                    headers: BTreeMap::new(),
                    elapsed_ms: 0,
                    error: Some(e),
                });
            }
        }
    }

    dedup_observations(&mut results);
    results
}

// ── 4. OpenAI Beta ─────────────────────────────────────────────────────────

async fn fuzz_openai_beta(
    access_token: &str,
    baseline_headers: &CodexHeaders,
    _body: &Value,
    quick: bool,
    provider: &ApiProvider,
) -> Vec<FuzzObservation> {
    let mut results = Vec::new();
    let baseline_body = default_response_create_body();
    let baseline = ws_stream_with_headers(
        provider,
        access_token,
        baseline_headers.clone(),
        &baseline_body,
    )
    .await;

    let probes: &[&str] = if quick {
        &[
            "responses_websockets=2026-02-06",
            "responses_websockets=2025-12-17",
        ]
    } else {
        &[
            "responses_websockets=2026-02-06",
            "responses_websockets=2025-12-17",
            "responses_websockets=2025-06-01",
            "responses_websockets=2025-01-01",
            "responses_websockets=2024-10-01",
            "responses_websockets=v1",
            "responses_websockets=v2",
            "responses_websockets=v3",
            "responses=v1",
            "responses=v2",
            "assistants=v2",
            "",
        ]
    };

    for probe in probes {
        let session_id = generate_session_id();
        let headers = CodexHeaders::for_thread(&session_id, &session_id, format!("{session_id}:0"));

        let mut extra = HeaderMap::new();
        extra.insert(
            "openai-beta",
            HeaderValue::from_str(probe).unwrap_or(HeaderValue::from_static("unknown")),
        );
        extra.insert(
            "originator",
            HeaderValue::from_static(DEFAULT_CODEX_ORIGINATOR),
        );
        for (k, v) in headers.to_header_map().unwrap_or_default() {
            extra.insert(k.unwrap(), v);
        }

        match raw_ws_handshake(provider, access_token, extra).await {
            Ok(hh) => {
                let status = if hh != baseline.handshake_headers {
                    ObservationStatus::Different
                } else {
                    ObservationStatus::Success
                };
                results.push(FuzzObservation {
                    vector: "openai_beta",
                    probe: probe.to_string(),
                    status,
                    response_body: Some(headers_to_string(&hh)),
                    headers: hh,
                    elapsed_ms: 0,
                    error: None,
                });
            }
            Err(e) => {
                let status = if e.contains("401") {
                    ObservationStatus::Unauthorized
                } else if e.contains("403") {
                    ObservationStatus::Forbidden
                } else {
                    ObservationStatus::Error
                };
                results.push(FuzzObservation {
                    vector: "openai_beta",
                    probe: probe.to_string(),
                    status,
                    response_body: None,
                    headers: BTreeMap::new(),
                    elapsed_ms: 0,
                    error: Some(e),
                });
            }
        }
    }

    dedup_observations(&mut results);
    results
}

// ── 5. Client Version ──────────────────────────────────────────────────────

async fn fuzz_client_version(
    access_token: &str,
    quick: bool,
    provider: &ApiProvider,
) -> Vec<FuzzObservation> {
    let mut results = Vec::new();

    let probes: &[&str] = if quick {
        &["0.136.0", "0.1.0", "1.0.0"]
    } else {
        &[
            "0.136.0",
            "0.133.0",
            "0.130.0",
            "0.125.0",
            "0.120.0",
            "0.110.0",
            "0.100.0",
            "0.80.0",
            "0.50.0",
            "0.1.0",
            "0.200.0",
            "0.150.0",
            "0.140.0",
            "1.0.0",
            "2.0.0",
            "web-2025-01-15",
            "web-2025-06-01",
            "",
            "0.0.0",
        ]
    };

    for version in probes {
        let mut url = provider.endpoint_url(ApiEndpoint::Models);
        url.query_pairs_mut().append_pair("client_version", version);
        let (status, response_headers, body) =
            match http_get(url.as_str(), access_token, HeaderMap::new()).await {
                Ok(response) => response,
                Err(error) => {
                    results.push(FuzzObservation {
                        vector: "client_version",
                        probe: version.to_string(),
                        status: ObservationStatus::Error,
                        response_body: None,
                        headers: BTreeMap::new(),
                        elapsed_ms: 0,
                        error: Some(error),
                    });
                    continue;
                }
            };

        let status = match status {
            200 => ObservationStatus::Success,
            401 => ObservationStatus::Unauthorized,
            403 => ObservationStatus::Forbidden,
            404 => ObservationStatus::Error,
            429 => ObservationStatus::Error,
            _ => ObservationStatus::Different,
        };

        let header_map: BTreeMap<String, String> = response_headers
            .iter()
            .filter_map(|(n, v)| v.to_str().ok().map(|v| (n.to_string(), v.to_string())))
            .collect();

        results.push(FuzzObservation {
            vector: "client_version",
            probe: version.to_string(),
            status,
            response_body: Some(body),
            headers: header_map,
            elapsed_ms: 0,
            error: None,
        });
    }

    dedup_observations(&mut results);
    results
}

// ── 6. HTTP Endpoints ──────────────────────────────────────────────────────

async fn fuzz_http_endpoints(
    access_token: &str,
    quick: bool,
    provider: &ApiProvider,
) -> Vec<FuzzObservation> {
    let mut results = Vec::new();

    let paths: &[&str] = if quick {
        &[
            "/backend-api/codex/models",
            "/backend-api/codex/config",
            "/backend-api/conversation",
        ]
    } else {
        &[
            // Codex paths
            "/backend-api/codex/models",
            "/backend-api/codex/responses",
            "/backend-api/codex/config",
            "/backend-api/codex/settings",
            "/backend-api/codex/feature-flags",
            "/backend-api/codex/flags",
            "/backend-api/codex/features",
            "/backend-api/codex/capabilities",
            "/backend-api/codex/info",
            "/backend-api/codex/status",
            "/backend-api/codex/health",
            "/backend-api/codex/version",
            "/backend-api/codex/ping",
            "/backend-api/codex/session",
            "/backend-api/codex/sessions",
            "/backend-api/codex/thread",
            "/backend-api/codex/threads",
            "/backend-api/codex/user",
            "/backend-api/codex/account",
            "/backend-api/codex/billing",
            "/backend-api/codex/usage",
            "/backend-api/codex/limits",
            "/backend-api/codex/quotas",
            "/backend-api/codex/experiments",
            "/backend-api/codex/experimental",
            "/backend-api/codex/beta",
            "/backend-api/codex/internal",
            "/backend-api/codex/conversation",
            "/backend-api/codex/conversations",
            "/backend-api/codex/history",
            "/backend-api/codex/tools",
            "/backend-api/codex/tool-specs",
            "/backend-api/codex/native-tools",
            "/backend-api/codex/plugin",
            "/backend-api/codex/plugins",
            "/backend-api/codex/mcp",
            "/backend-api/codex/model",
            "/backend-api/codex/model-capabilities",
            "/backend-api/codex/model-list",
            "/backend-api/codex/workspace",
            "/backend-api/codex/sandbox",
            "/backend-api/codex/graphql",
            "/backend-api/codex/openapi.json",
            "/backend-api/codex/swagger.json",
            "/backend-api/codex/schema",
            // ChatGPT conversation API paths
            "/backend-api/conversation",
            "/backend-api/conversations",
            "/backend-api/me",
            "/backend-api/models",
        ]
    };

    for path in paths {
        let url = match provider.origin_url_for_path(path) {
            Ok(url) => url,
            Err(error) => {
                results.push(FuzzObservation {
                    vector: "http_endpoints",
                    probe: path.to_string(),
                    status: ObservationStatus::Error,
                    response_body: None,
                    headers: BTreeMap::new(),
                    elapsed_ms: 0,
                    error: Some(error.describe()),
                });
                continue;
            }
        };
        let (status_code, response_headers, body) =
            match http_get(url.as_str(), access_token, HeaderMap::new()).await {
                Ok(response) => response,
                Err(error) => {
                    results.push(FuzzObservation {
                        vector: "http_endpoints",
                        probe: path.to_string(),
                        status: ObservationStatus::Error,
                        response_body: None,
                        headers: BTreeMap::new(),
                        elapsed_ms: 0,
                        error: Some(error),
                    });
                    continue;
                }
            };

        let status = match status_code {
            200 => ObservationStatus::Unique, // 200 on unknown paths is the jackpot
            401 => ObservationStatus::Unauthorized,
            403 => ObservationStatus::Forbidden,
            404 => ObservationStatus::Success, // expected, not interesting
            405 => ObservationStatus::Different, // method not allowed = endpoint exists!
            _ => ObservationStatus::Different,
        };

        let header_map: BTreeMap<String, String> = response_headers
            .iter()
            .filter_map(|(n, v)| v.to_str().ok().map(|v| (n.to_string(), v.to_string())))
            .collect();

        results.push(FuzzObservation {
            vector: "http_endpoints",
            probe: path.to_string(),
            status,
            response_body: if body.is_empty() { None } else { Some(body) },
            headers: header_map,
            elapsed_ms: 0,
            error: None,
        });
    }

    dedup_observations(&mut results);
    results
}

// ── 7. Frame Mutations ─────────────────────────────────────────────────────

async fn fuzz_frame_mutations(
    access_token: &str,
    baseline_headers: &CodexHeaders,
    baseline_body: &Value,
    quick: bool,
    provider: &ApiProvider,
) -> Vec<FuzzObservation> {
    let mut results = Vec::new();
    let baseline = ws_stream_with_headers(
        provider,
        access_token,
        baseline_headers.clone(),
        baseline_body,
    )
    .await;

    let mutations: Vec<(&str, Value)> = if quick {
        vec![
            (
                "empty_model",
                json!({
                    "type": "response.create", "model": "",
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            (
                "store_true",
                json!({
                    "type": "response.create", "model": "gpt-5.5", "store": true,
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
        ]
    } else {
        vec![
            // Model field mutations
            (
                "empty_model",
                json!({
                    "type": "response.create", "model": "",
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            (
                "missing_model",
                json!({
                    "type": "response.create",
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            (
                "nonexistent_model",
                json!({
                    "type": "response.create", "model": "gpt-nonexistent-999",
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            // Bad type
            (
                "wrong_event_type",
                json!({
                    "type": "response.update",
                    "model": "gpt-5.5",
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            (
                "no_type",
                json!({
                    "model": "gpt-5.5",
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            // Extra unknown top-level fields
            (
                "unknown_top_field",
                json!({
                    "type": "response.create", "model": "gpt-5.5",
                    "unknown_flag": true, "internal_mode": "debug",
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            // Store mutation
            (
                "store_true",
                json!({
                    "type": "response.create", "model": "gpt-5.5", "store": true,
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            // Reasoning mutations
            (
                "reasoning_xhigh",
                json!({
                    "type": "response.create", "model": "gpt-5.5",
                    "reasoning": {"effort": "xhigh", "summary": "auto"},
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            (
                "reasoning_summary_verbose",
                json!({
                    "type": "response.create", "model": "gpt-5.5",
                    "reasoning": {"effort": "xhigh", "summary": "detailed"},
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            // Service tier mutations
            (
                "service_tier_internal",
                json!({
                    "type": "response.create", "model": "gpt-5.5", "service_tier": "internal",
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            (
                "service_tier_auto",
                json!({
                    "type": "response.create", "model": "gpt-5.5", "service_tier": "auto",
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            // Tool choice edge
            (
                "tool_choice_required",
                json!({
                    "type": "response.create", "model": "gpt-5.5", "tool_choice": "required",
                    "tools": [{"type": "custom", "name": "test", "format": {"type": "text", "syntax": "lark", "definition": "start: /.*/"}}],
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            // Include mutation
            (
                "include_thinking",
                json!({
                    "type": "response.create", "model": "gpt-5.5",
                    "include": ["message.output_text", "reasoning", "thinking"],
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
            // Top-level debug fields
            (
                "top_debug_flags",
                json!({
                    "type": "response.create", "model": "gpt-5.5",
                    "debug": true, "log_level": "debug",
                    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
                }),
            ),
        ]
    };

    for (mutation_name, body) in mutations {
        let result =
            ws_stream_with_headers(provider, access_token, baseline_headers.clone(), &body).await;
        let status = classify_ws_result(&result, &baseline);

        // Collect any error frames from the stream
        let error_frames: String = result.frames.iter().filter(|(et, _)| et == "error").fold(
            String::new(),
            |mut s, (_, frame)| {
                let _ = writeln!(s, "{}", sonic_rs::to_string(frame).unwrap_or_default());
                s
            },
        );

        let body_str = if error_frames.is_empty() {
            headers_to_string(&result.handshake_headers)
        } else {
            error_frames
        };

        results.push(FuzzObservation {
            vector: "frame_mutations",
            probe: mutation_name.to_string(),
            status,
            response_body: Some(body_str),
            headers: result.handshake_headers,
            elapsed_ms: 0,
            error: result.error,
        });
    }

    dedup_observations(&mut results);
    results
}

// ── 8. Extra Headers ───────────────────────────────────────────────────────

async fn fuzz_extra_headers(
    access_token: &str,
    baseline_headers: &CodexHeaders,
    _body: &Value,
    quick: bool,
    provider: &ApiProvider,
) -> Vec<FuzzObservation> {
    let mut results = Vec::new();
    let baseline_body = default_response_create_body();
    let baseline = ws_stream_with_headers(
        provider,
        access_token,
        baseline_headers.clone(),
        &baseline_body,
    )
    .await;

    let probes: &[(&str, &str)] = if quick {
        &[
            ("x-codex-session-type", "interactive"),
            ("x-codex-approvals", "never"),
        ]
    } else {
        &[
            // Session/config
            ("x-codex-session-type", "interactive"),
            ("x-codex-session-type", "background"),
            ("x-codex-session-type", "automated"),
            ("x-codex-compact-mode", "true"),
            ("x-codex-compact-mode", "auto"),
            // Sandbox
            ("x-codex-sandbox", "workspace-write"),
            ("x-codex-sandbox", "workspace-read"),
            ("x-codex-sandbox", "danger-full"),
            // Approvals
            ("x-codex-approvals", "never"),
            ("x-codex-approvals", "always"),
            ("x-codex-approvals", "on-request"),
            // Model override
            ("x-codex-model-override", "gpt-5.5"),
            // Experiment IDs
            ("x-codex-experiment", "exp_001"),
            ("x-codex-experiment-id", "test"),
            // Debug/trace
            ("x-debug", "true"),
            ("x-trace-id", "trace-001"),
            ("x-span-id", "span-001"),
        ]
    };

    for (name, value) in probes {
        let session_id = generate_session_id();
        let headers = CodexHeaders::for_thread(&session_id, &session_id, format!("{session_id}:0"));

        let mut extra = HeaderMap::new();
        for (k, v) in headers.to_header_map().unwrap_or_default() {
            extra.insert(k.unwrap(), v);
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            extra.insert(n, v);
        }

        match raw_ws_handshake(provider, access_token, extra).await {
            Ok(hh) => {
                let status = if hh != baseline.handshake_headers {
                    ObservationStatus::Different
                } else {
                    ObservationStatus::Success
                };
                results.push(FuzzObservation {
                    vector: "extra_headers",
                    probe: format!("{name}={value}"),
                    status,
                    response_body: Some(headers_to_string(&hh)),
                    headers: hh,
                    elapsed_ms: 0,
                    error: None,
                });
            }
            Err(e) => {
                let status = if e.contains("401") {
                    ObservationStatus::Unauthorized
                } else if e.contains("403") {
                    ObservationStatus::Forbidden
                } else {
                    ObservationStatus::Error
                };
                results.push(FuzzObservation {
                    vector: "extra_headers",
                    probe: format!("{name}={value}"),
                    status,
                    response_body: None,
                    headers: BTreeMap::new(),
                    elapsed_ms: 0,
                    error: Some(e),
                });
            }
        }
    }

    dedup_observations(&mut results);
    results
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let mut quick = false;
    let mut vectors: Vec<FuzzVector> = Vec::new();
    let mut auth_mode = AuthMode::OwnState;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--quick" => quick = true,
            "--norotate" => auth_mode = AuthMode::CodexReadOnly,
            "--vector" => {
                i += 1;
                if i < args.len() {
                    match FuzzVector::all().into_iter().find(|v| v.name() == args[i]) {
                        Some(v) => vectors.push(v),
                        None => {
                            eprintln!("Unknown vector: {}", args[i]);
                            eprintln!("Available vectors:");
                            for v in FuzzVector::all() {
                                eprintln!("  {}", v.name());
                            }
                            return;
                        }
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }

    if vectors.is_empty() {
        vectors = FuzzVector::all();
    }

    let access_token = match load_auth_token(auth_mode) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to load auth: {e}");
            eprintln!("Make sure you've run `codex login` first.");
            return;
        }
    };

    let provider = match provider_from_env() {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("Invalid OPENAI_BASE_URL: {error}");
            return;
        }
    };

    if quick {
        eprintln!("Quick mode: {} vectors", vectors.len());
    } else {
        eprintln!("Full mode: {} vectors", vectors.len());
    }

    let mut total_observations = 0usize;

    for vector in &vectors {
        eprintln!("Running: {} — {}", vector.name(), vector.description());
        emit_banner(vector);

        let observations = run_vector(*vector, &access_token, quick, &provider).await;

        for obs in &observations {
            emit_observation(obs);
        }
        emit_summary(&observations);

        total_observations += observations.len();
        eprintln!("  {} observations", observations.len());

        tokio::time::sleep(FUZZ_WARMUP_WAIT).await;
    }

    eprintln!("Done. {} total observations.", total_observations);
}

async fn run_vector(
    vector: FuzzVector,
    access_token: &str,
    quick: bool,
    provider: &ApiProvider,
) -> Vec<FuzzObservation> {
    let session_id = generate_session_id();
    let window_id = format!("{session_id}:fuzz");
    let baseline_headers = CodexHeaders::for_thread(&session_id, &session_id, window_id);
    let body = default_response_create_body();

    match vector {
        FuzzVector::BetaFeatures => {
            fuzz_beta_features(access_token, &baseline_headers, &body, quick, provider).await
        }
        FuzzVector::Subagent => {
            fuzz_subagent(access_token, &baseline_headers, &body, quick, provider).await
        }
        FuzzVector::Originator => {
            fuzz_originator(access_token, &baseline_headers, &body, quick, provider).await
        }
        FuzzVector::OpenAiBeta => {
            fuzz_openai_beta(access_token, &baseline_headers, &body, quick, provider).await
        }
        FuzzVector::ClientVersion => fuzz_client_version(access_token, quick, provider).await,
        FuzzVector::HttpEndpoints => fuzz_http_endpoints(access_token, quick, provider).await,
        FuzzVector::FrameMutations => {
            fuzz_frame_mutations(access_token, &baseline_headers, &body, quick, provider).await
        }
        FuzzVector::ExtraHeaders => {
            fuzz_extra_headers(access_token, &baseline_headers, &body, quick, provider).await
        }
    }
}
