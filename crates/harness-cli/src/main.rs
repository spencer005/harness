//! Command-line entrypoint for interactive harness sessions.

use std::{
    env,
    ffi::OsString,
    fs,
    future::Future,
    io,
    io::{BufRead, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use harness_core::{
    UiSnapshot, UiTranscriptEntry,
    analytics::{CodexAnalytics, InstallationId},
    harness::{DEFAULT_DEVELOPER_MODE, HarnessActor, HarnessConfig},
    ipc::{IpcService, IpcTransport, UdsTransport},
    provider_runtime::{
        FileProviderCredentialStore, ProviderCredentialStore, ProviderRuntimeBuilder,
        ProviderSessionBinding,
    },
    providers::{
        FileProviderConfigStore, ProviderAuthConfig, ProviderConfig, ProviderConfigStore,
        ProviderDriverConfig, ProviderKind, ProviderProfile, ProviderProfileId, ProviderStoreError,
        ProviderUiInfo,
    },
    responses::{
        ApiEndpoint, ApiProvider, Auth, AuthError, ChatGptAuthSession, ChatGptAuthTokens,
        DEFAULT_CODEX_ORIGINATOR, DEFAULT_MODEL, DEFAULT_REASONING_EFFORT, FAST_SERVICE_TIER,
        ManagedChatGptAuth, ModelSettings, ResponsesApiError, ResponsesCreateRequest,
        ResponsesHeaders, ResponsesHttpsTransport, ResponsesModelCapabilities,
        ResponsesModelsClient, ResponsesRequestBuildError, ResponsesStreamEvent,
        ResponsesStreamRequest, ResponsesWsActor, ResponsesWsPool, WsPoolConfig,
        lean_codex_default_headers,
    },
    sessions::{SessionError, SessionId, SessionIndex, SessionStore, SessionSummary},
    tools::{
        FREEFORM_TOOL_FORMAT_SYNTAX_LARK, FREEFORM_TOOL_FORMAT_TYPE, FreeformTool,
        FreeformToolFormat, FunctionTool, NativeTool,
    },
};
use serde::{Deserialize, Serialize};
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};
use thiserror::Error;

static NEXT_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
const CHATGPT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const OLLAMA_CLOUD_BASE_URL: &str = "https://ollama.com/v1";
const INITIAL_TRANSCRIPT_PAGE_LINE_LIMIT: usize = 96;
const CLI_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
const RESUME_PICKER_DEFAULT_WIDTH: usize = 120;
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_BLUE: &str = "\x1b[34m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_MAGENTA: &str = "\x1b[35m";
const ANSI_GRAY: &str = "\x1b[90m";

type CliResult<T> = Result<T, CliError>;

#[derive(Debug, Error)]
enum CliError {
    #[error("failed to build async runtime")]
    RuntimeBuild {
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Responses(#[from] ResponsesApiError),
    #[error(transparent)]
    RequestBuild(#[from] ResponsesRequestBuildError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    ProviderStore(#[from] ProviderStoreError),
    #[error(transparent)]
    ProviderRuntime(#[from] harness_core::provider_runtime::ProviderError),
    #[error(transparent)]
    Ipc(#[from] harness_core::ipc::IpcError),
    #[error("CLI I/O failed")]
    Io {
        #[source]
        source: io::Error,
    },
    #[error("failed to encode experiment JSON")]
    ExperimentJson {
        #[source]
        source: sonic_rs::Error,
    },
    #[error("model `{model}` was not returned by /models")]
    MissingModel { model: String },
    #[error("HOME is required when {fallback_variable} is unset")]
    HomeRequired {
        fallback_variable: &'static str,
        #[source]
        source: env::VarError,
    },
    #[error("session `{id}` was not found")]
    SessionNotFound { id: String },
    #[error("no sessions available to resume")]
    NoSessionsAvailable,
    #[error("session id mismatch: index={index} file={file}")]
    SessionIdMismatch { index: String, file: String },
    #[error("no session selected")]
    NoSessionSelected,
    #[error(
        "invalid session selection `{selected}`; enter 1-{max}, a session id, or a cwd/latest-message filter"
    )]
    InvalidSessionSelection { selected: String, max: usize },
    #[error("failed to read current working directory")]
    CurrentDir {
        #[source]
        source: io::Error,
    },
    #[error(
        "unsupported arguments `{arguments}`; use [--norotate], resume [sessionid] [--norotate], inspect-session <sessionid>, probe-session-chunk <sessionid> <chunk-index>, repair-session <sessionid>, or ipc-uds <socket-path>"
    )]
    UnsupportedArguments { arguments: String },
    #[error("base instructions destination has no parent directory")]
    BaseInstructionsMissingParent,
    #[error("failed to create base instructions directory {path}")]
    CreateBaseInstructionsDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("missing base instructions: expected {source_path} or {destination}")]
    MissingBaseInstructions {
        source_path: PathBuf,
        destination: PathBuf,
    },
    #[error("failed to read harness base instructions {path}")]
    ReadBaseInstructions {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read resume startup binding from {path}")]
    ResumeStartupBinding {
        path: PathBuf,
        #[source]
        source: SessionError,
    },
    #[error("failed to read resume transcript page from {path}")]
    ResumeTranscriptPage {
        path: PathBuf,
        #[source]
        source: SessionError,
    },
}

impl From<io::Error> for CliError {
    fn from(source: io::Error) -> Self {
        Self::Io { source }
    }
}

impl From<sonic_rs::Error> for CliError {
    fn from(source: sonic_rs::Error) -> Self {
        Self::ExperimentJson { source }
    }
}

fn main() -> anyhow::Result<()> {
    configure_memory_allocator();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| CliError::RuntimeBuild { source })?;
    let result = runtime.block_on(run_cli());
    runtime.shutdown_timeout(CLI_RUNTIME_SHUTDOWN_TIMEOUT);
    result?;
    Ok(())
}

async fn run_cli() -> CliResult<()> {
    match parse_cli_args(env::args_os().skip(1))? {
        CliAction::Tui { auth_mode, resume } => run_tui(auth_mode, resume).await,
        CliAction::IpcUds { socket_path } => run_ipc_uds(socket_path).await,
        CliAction::RepairSession { session_id } => repair_session(session_id),
        CliAction::InspectSession { session_id } => inspect_session(session_id),
        CliAction::ProbeSessionChunk {
            session_id,
            chunk_index,
        } => probe_session_chunk(session_id, chunk_index),
    }
}

fn configure_memory_allocator() {
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }
}

async fn run_ipc_uds(socket_path: PathBuf) -> CliResult<()> {
    let session_root = session_root()?;
    let service = IpcService::new(session_root);
    UdsTransport::new(socket_path).serve(service).await?;
    Ok(())
}

fn repair_session(session_id: String) -> CliResult<()> {
    let root = session_root()?;
    let mut index = SessionIndex::load(&root)?;
    let session_id_value = SessionId::new(session_id.clone());
    let summary = index
        .summary_by_id(&session_id_value)?
        .ok_or(CliError::SessionNotFound { id: session_id })?;
    let old_len = fs::metadata(&summary.path)?.len();
    let backup_path = summary.path.with_extension("nhsession.bak");
    fs::copy(&summary.path, &backup_path)?;
    let store = SessionStore::new(&root);
    let new_len = store.repair_session_tail(&summary.path)?;
    println!(
        "repaired session tail: path={} backup={} old_len={} new_len={}",
        summary.path.display(),
        backup_path.display(),
        old_len,
        new_len
    );
    Ok(())
}

fn inspect_session(session_id: String) -> CliResult<()> {
    let root = session_root()?;
    let mut index = SessionIndex::load(&root)?;
    let session_id_value = SessionId::new(session_id.clone());
    let summary = index
        .summary_by_id(&session_id_value)?
        .ok_or(CliError::SessionNotFound { id: session_id })?;
    let file_len = fs::metadata(&summary.path)?.len();
    let store = SessionStore::new(&root);
    let reports = store.inspect_session_chunks(&summary.path)?;
    println!("session: {}", summary.path.display());
    println!("file_len: {}", file_len);
    let failing_indexes = reports
        .iter()
        .filter(|report| !report.decodes)
        .map(|report| report.index)
        .collect::<Vec<_>>();
    println!("failing_chunks: {}", failing_indexes.len());
    if failing_indexes.is_empty() {
        for report in reports {
            println!(
                "chunk={} header_offset={} payload_offset={} compressed_len={} uncompressed_len={} first_seq={} record_count={} decodes={}",
                report.index,
                report.header_offset,
                report.payload_offset,
                report.compressed_len,
                report.uncompressed_len,
                report.first_seq,
                report.record_count,
                report.decodes,
            );
        }
        return Ok(());
    }
    let mut selected = vec![false; reports.len()];
    for failing_index in &failing_indexes {
        let start = failing_index.saturating_sub(2);
        let end = (*failing_index + 2).min(reports.len().saturating_sub(1));
        for selected_index in start..=end {
            selected[selected_index] = true;
        }
    }
    for (index, report) in reports.into_iter().enumerate() {
        if !selected[index] {
            continue;
        }
        println!(
            "chunk={} header_offset={} payload_offset={} compressed_len={} uncompressed_len={} first_seq={} record_count={} decodes={}{}",
            report.index,
            report.header_offset,
            report.payload_offset,
            report.compressed_len,
            report.uncompressed_len,
            report.first_seq,
            report.record_count,
            report.decodes,
            report
                .error
                .as_deref()
                .map(|error| format!(" error={error}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}
fn probe_session_chunk(session_id: String, chunk_index: usize) -> CliResult<()> {
    let root = session_root()?;
    let mut index = SessionIndex::load(&root)?;
    let session_id_value = SessionId::new(session_id.clone());
    let summary = index
        .summary_by_id(&session_id_value)?
        .ok_or(CliError::SessionNotFound { id: session_id })?;
    let store = SessionStore::new(&root);
    let probe = store.probe_session_chunk(&summary.path, chunk_index)?;
    println!("session: {}", summary.path.display());
    println!(
        "chunk={} header_offset={} payload_offset={} compressed_len={} uncompressed_len={} first_seq={} record_count={}",
        probe.index,
        probe.header_offset,
        probe.payload_offset,
        probe.compressed_len,
        probe.uncompressed_len,
        probe.first_seq,
        probe.record_count,
    );
    println!("decompresses: {}", probe.decompresses);
    println!("current_decodes: {}", probe.current_decodes);
    println!("legacy_decodes: {}", probe.legacy_decodes);
    if let Some(error) = probe.current_error.as_deref() {
        println!("current_error: {error}");
    }
    if let Some(error) = probe.legacy_error.as_deref() {
        println!("legacy_error: {error}");
    }
    if let Some(prefix) = probe.payload_prefix_hex.as_deref() {
        println!("payload_prefix_hex: {prefix}");
    }
    if let Some(prefix) = probe.payload_prefix_ascii.as_deref() {
        println!("payload_prefix_ascii: {prefix}");
    }
    if let Some(count) = probe.current_prefix_record_count {
        println!("current_prefix_record_count: {count}");
    }
    if let Some(prefix) = probe.current_failure_remainder_hex.as_deref() {
        println!("current_failure_remainder_hex: {prefix}");
    }
    if let Some(prefix) = probe.current_failure_remainder_ascii.as_deref() {
        println!("current_failure_remainder_ascii: {prefix}");
    }
    Ok(())
}

async fn run_tui(auth_mode: AuthMode, resume: ResumeSelection) -> CliResult<()> {
    let session_root = session_root()?;
    let startup = resolve_session_startup(&session_root, resume)?;
    let state_dir = harness_state_dir()?;
    let provider_selection =
        load_or_create_provider_selection(&state_dir, auth_mode, startup.provider_binding.as_ref())
            .await?;
    let provider_profile = provider_selection.profile;
    let provider_info = ProviderUiInfo::from_profile(&provider_profile);
    let codex_home = codex_home()?;
    let credential_store: Arc<dyn ProviderCredentialStore> =
        Arc::new(FileProviderCredentialStore::new(&state_dir));
    let provider_store: Arc<dyn ProviderConfigStore> = Arc::new(FileProviderConfigStore::new(
        state_dir.join("providers.json"),
    ));

    let runtime_builder = ProviderRuntimeBuilder::new(provider_profile)
        .with_codex_home(&codex_home)
        .with_credential_store(credential_store.clone());

    let provider_runtime = runtime_builder
        .build_async_with_model(provider_selection.model_settings)
        .await?;
    let model_settings = provider_runtime.selected_model.clone();
    let model = model_settings.model.clone();
    let provider = provider_runtime.api.clone();
    let auth = provider_runtime.auth.clone().into_responses_auth();
    let default_headers = provider_runtime.default_headers.clone();

    let instructions = load_base_instructions(&model, &codex_home, &state_dir)?;

    let session_id = startup.session_id;
    let session_id_text = session_id.as_str().to_string();
    let window_id = format!("{}:0", session_id.as_str());
    let mut responses_headers =
        ResponsesHeaders::for_thread(session_id.as_str(), session_id.as_str(), window_id);

    let installation_id = InstallationId::load_or_generate(&state_dir)?;
    responses_headers.installation_id = Some(installation_id.as_str().to_string());

    let analytics = if provider_runtime.profile.kind == ProviderKind::Codex {
        match &provider_runtime.auth {
            harness_core::provider_runtime::ProviderAuthRuntime::ChatGpt(_) => {
                provider_runtime.auth.access_token().map(|token| {
                    CodexAnalytics::new(
                        provider
                            .endpoint_url(ApiEndpoint::AnalyticsEvents)
                            .to_string(),
                        token,
                        installation_id.clone(),
                    )
                })
            }
            harness_core::provider_runtime::ProviderAuthRuntime::ApiKey(_) => None,
        }
    } else {
        None
    };

    let responses = match &provider_runtime.profile.driver {
        ProviderDriverConfig::CodexWsResponses { .. } => {
            let pool =
                ResponsesWsPool::new(provider, auth, default_headers, WsPoolConfig::default());
            ResponsesWsActor::spawn(pool)
        }
        ProviderDriverConfig::HttpsResponses { .. } => {
            let request_timeout = provider_runtime
                .profile
                .driver
                .request_timeout()
                .expect("HTTPS provider driver has a request timeout");
            ResponsesWsActor::spawn_https(ResponsesHttpsTransport::new(
                provider,
                auth,
                default_headers,
                request_timeout,
            ))
        }
    };

    let runtime = HarnessActor::spawn(
        HarnessConfig {
            session_root,
            session_id: session_id.clone(),
            resume_session_path: startup.resume_session_path,
            initial_transcript_before_seq: startup.initial_transcript_before_seq,
            cwd: env::current_dir()
                .map_err(|source| CliError::CurrentDir { source })?
                .to_string_lossy()
                .into_owned(),
            provider_runtime: provider_runtime.clone(),
            provider: provider_info.clone(),
            provider_store: Some(provider_store),
            credential_store: Some(credential_store),
            codex_home: Some(codex_home),
            model: model.clone(),
            reasoning_effort: model_settings.reasoning_effort.clone(),
            service_tier: model_settings.service_tier.clone(),
            developer_mode: DEFAULT_DEVELOPER_MODE,
            instructions,
            source: "tui".to_string(),
            originator: DEFAULT_CODEX_ORIGINATOR.to_string(),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            sandbox_policy: "workspace-write".to_string(),
            approval_policy: "never".to_string(),
            responses_headers,
            model_capabilities: provider_runtime.selected_capabilities,
            model_catalog: provider_runtime.model_catalog.raw_models(),
            tool_registry: harness_core::provider_runtime::tool_registry_for_provider(
                provider_runtime.profile.kind,
            ),
            context_window_policy: Some(provider_runtime.context_window_policy),
            terminal_tools_enabled: true,
            analytics,
        },
        responses,
    );

    let snapshot = UiSnapshot {
        session_id: session_id_text,
        thread_title: format!("new_harness · {model}"),
        provider: Some(provider_info),
        model_settings,
        developer_mode: DEFAULT_DEVELOPER_MODE,
        response_streaming: false,
        last_ttft_ms: None,
        transcript_entries: startup.initial_transcript_entries,
        input: String::new(),
        input_cursor: 0,
        queued_steering_prompt: None,
        agents: Vec::new(),
        active_activities: Vec::new(),
    };

    let final_snapshot =
        harness_tui::run_with_runtime(snapshot, runtime.commands, runtime.events).await?;
    println!("resume session: {}", final_snapshot.session_id);
    Ok(())
}

fn provider_default_model_settings(profile: &ProviderProfile) -> ModelSettings {
    ModelSettings::new(
        profile.default_model.clone(),
        profile.default_reasoning_effort.clone(),
        profile.default_service_tier.clone(),
    )
}

#[derive(Debug, Clone)]
struct ProviderStartupSelection {
    profile: ProviderProfile,
    model_settings: Option<ModelSettings>,
}

async fn load_or_create_provider_profile(
    state_dir: &Path,
    auth_mode: AuthMode,
) -> CliResult<ProviderProfile> {
    Ok(
        load_or_create_provider_selection(state_dir, auth_mode, None)
            .await?
            .profile,
    )
}

async fn load_or_create_provider_selection(
    state_dir: &Path,
    _auth_mode: AuthMode,
    binding: Option<&ProviderSessionBinding>,
) -> CliResult<ProviderStartupSelection> {
    let store = FileProviderConfigStore::new(state_dir.join("providers.json"));
    if let Some(mut config) = store.load()? {
        if ensure_codex_provider_profile(&mut config) {
            store.save(&config)?;
        }
        if let Some(selection) = provider_startup_selection(&config, binding) {
            return Ok(selection);
        }
    }

    println!("{ANSI_BOLD}{ANSI_CYAN}No provider profile is configured.{ANSI_RESET}");
    println!("Create one now:");
    println!("  1) Codex");
    println!("  2) Ollama Cloud");
    println!("  3) HTTPS API");
    print!("choice> ");
    io::stdout().flush()?;

    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let choice = line.trim();

    let profile = match choice {
        "2" => create_ollama_cloud_provider_profile_interactive(state_dir).await?,
        "3" => create_https_api_provider_profile_interactive(state_dir).await?,
        _ => create_codex_provider_profile(),
    };

    store.save(&ProviderConfig {
        default_profile_id: profile.id.clone(),
        profiles: provider_profiles_with_codex(profile.clone()),
    })?;
    Ok(ProviderStartupSelection {
        profile,
        model_settings: None,
    })
}

fn provider_startup_selection(
    config: &ProviderConfig,
    binding: Option<&ProviderSessionBinding>,
) -> Option<ProviderStartupSelection> {
    if let Some(binding) = binding
        && let Some(profile) = config.profile(&binding.profile_id)
        && profile.kind == binding.kind
    {
        let model_settings = provider_can_select_bound_model(profile, &binding.model_settings)
            .then(|| binding.model_settings.clone());
        return Some(ProviderStartupSelection {
            profile: profile.clone(),
            model_settings,
        });
    }
    config
        .default_profile()
        .cloned()
        .map(|profile| ProviderStartupSelection {
            profile,
            model_settings: None,
        })
}

fn provider_can_select_bound_model(
    profile: &ProviderProfile,
    model_settings: &ModelSettings,
) -> bool {
    profile.kind == ProviderKind::Codex
        || profile
            .model_configs
            .iter()
            .any(|config| config.slug == model_settings.model)
}

fn provider_profiles_with_codex(profile: ProviderProfile) -> Vec<ProviderProfile> {
    if profile.id.as_str() == "codex" {
        vec![profile]
    } else {
        vec![profile, create_codex_provider_profile()]
    }
}

fn ensure_codex_provider_profile(config: &mut ProviderConfig) -> bool {
    if config.profile(&ProviderProfileId::new("codex")).is_none() {
        config.profiles.push(create_codex_provider_profile());
        true
    } else {
        false
    }
}

async fn create_https_api_provider_profile_interactive(
    state_dir: &Path,
) -> CliResult<ProviderProfile> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    writeln!(writer, "\nCreate HTTPS API provider")?;
    write!(writer, "Display name [ccapi]: ")?;
    writer.flush()?;
    let mut name = String::new();
    reader.read_line(&mut name)?;
    let name = {
        let trimmed = name.trim();
        if trimmed.is_empty() { "ccapi" } else { trimmed }
    };

    write!(writer, "Base URL [https://ccapi.us/v1]: ")?;
    writer.flush()?;
    let mut base_url = String::new();
    reader.read_line(&mut base_url)?;
    let base_url = {
        let trimmed = base_url.trim();
        if trimmed.is_empty() {
            "https://ccapi.us/v1"
        } else {
            trimmed
        }
    };

    writeln!(
        writer,
        "API key (will be saved to harness credential store):"
    )?;
    write!(writer, "key> ")?;
    writer.flush()?;
    let mut key = String::new();
    reader.read_line(&mut key)?;
    let key = key.trim().to_string();

    let store = FileProviderCredentialStore::new(state_dir);
    let credential_id = store
        .save_api_key(name.to_string(), key)
        .await
        .map_err(|err| CliError::Io {
            source: io::Error::new(io::ErrorKind::Other, err),
        })?;

    Ok(ProviderProfile {
        id: ProviderProfileId::new(name),
        display_name: name.to_string(),
        kind: ProviderKind::HttpsApi,
        auth: ProviderAuthConfig::ApiKey { credential_id },
        driver: ProviderDriverConfig::HttpsResponses {
            base_url: base_url.to_string(),
            request_timeout_ms: 300_000,
            stream_idle_timeout_ms: 300_000,
        },
        default_model: DEFAULT_MODEL.to_string(),
        default_reasoning_effort: Some(DEFAULT_REASONING_EFFORT.to_string()),
        default_service_tier: None,
        model_configs: Vec::new(),
        tool_output_summary_model: "gpt-5.4".to_string(),
    })
}
async fn create_ollama_cloud_provider_profile_interactive(
    state_dir: &Path,
) -> CliResult<ProviderProfile> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    writeln!(writer, "\nCreate Ollama Cloud provider")?;
    write!(writer, "Model name: ")?;
    writer.flush()?;
    let mut model = String::new();
    reader.read_line(&mut model)?;
    let model = model.trim().to_string();
    if model.is_empty() {
        return Err(CliError::MissingModel {
            model: String::new(),
        });
    }

    write!(
        writer,
        "API key (will be saved to harness credential store):"
    )?;
    writer.flush()?;
    writeln!(writer)?;
    write!(writer, "key> ")?;
    writer.flush()?;
    let mut key = String::new();
    reader.read_line(&mut key)?;
    let key = key.trim().to_string();

    let store = FileProviderCredentialStore::new(state_dir);
    let credential_id = store
        .save_api_key("ollama-cloud".to_string(), key)
        .await
        .map_err(|err| CliError::Io {
            source: io::Error::new(io::ErrorKind::Other, err),
        })?;

    Ok(create_ollama_cloud_provider_profile(credential_id, model))
}

/// Build an Ollama Cloud provider profile with a user-specified default model.
///
/// Model capabilities (context window, parallel tool call support) are discovered
/// at runtime from the provider's `/models` endpoint, so no static model config
/// is injected here.
fn create_ollama_cloud_provider_profile(credential_id: String, model: String) -> ProviderProfile {
    ProviderProfile {
        id: ProviderProfileId::new("ollama-cloud"),
        display_name: "Ollama Cloud".to_string(),
        kind: ProviderKind::OllamaCloud,
        auth: ProviderAuthConfig::ApiKey { credential_id },
        driver: ProviderDriverConfig::HttpsResponses {
            base_url: OLLAMA_CLOUD_BASE_URL.to_string(),
            request_timeout_ms: 300_000,
            stream_idle_timeout_ms: 300_000,
        },
        default_model: model.clone(),
        default_reasoning_effort: None,
        default_service_tier: None,
        model_configs: Vec::new(),
        tool_output_summary_model: model,
    }
}

fn create_codex_provider_profile() -> ProviderProfile {
    ProviderProfile {
        id: ProviderProfileId::new("codex"),
        display_name: "Codex".to_string(),
        kind: ProviderKind::Codex,
        auth: ProviderAuthConfig::ChatGptHarness,
        driver: ProviderDriverConfig::CodexWsResponses {
            base_url: CHATGPT_CODEX_BASE_URL.to_string(),
            stream_idle_timeout_ms: 300_000,
        },
        default_model: DEFAULT_MODEL.to_string(),
        default_reasoning_effort: Some(DEFAULT_REASONING_EFFORT.to_string()),
        default_service_tier: Some(FAST_SERVICE_TIER.to_string()),
        model_configs: Vec::new(),
        tool_output_summary_model: "gpt-5.4-mini".to_string(),
    }
}

fn load_chatgpt_auth(auth_mode: AuthMode, codex_home: &Path, state_dir: &Path) -> CliResult<Auth> {
    match auth_mode {
        AuthMode::OwnState => load_owned_chatgpt_auth(&harness_auth_path(state_dir), codex_home),
        AuthMode::CodexReadOnly => load_codex_readonly_chatgpt_auth(codex_home),
    }
}

fn session_root() -> CliResult<PathBuf> {
    if let Ok(root) = env::var("HARNESS_SESSION_ROOT") {
        return Ok(PathBuf::from(root));
    }
    harness_state_dir()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    OwnState,
    CodexReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeSelection {
    New,
    Pick,
    SessionId(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliAction {
    Tui {
        auth_mode: AuthMode,
        resume: ResumeSelection,
    },
    IpcUds {
        socket_path: PathBuf,
    },
    RepairSession {
        session_id: String,
    },
    InspectSession {
        session_id: String,
    },
    ProbeSessionChunk {
        session_id: String,
        chunk_index: usize,
    },
}

#[derive(Debug)]
struct SessionStartup {
    session_id: SessionId,
    resume_session_path: Option<PathBuf>,
    initial_transcript_before_seq: Option<u64>,
    initial_transcript_entries: Vec<UiTranscriptEntry>,
    provider_binding: Option<ProviderSessionBinding>,
}

#[derive(Debug, Clone)]
struct ResumeSessionOption {
    summary: SessionSummary,
    latest_message_preview: Option<String>,
    last_message_at_ms: u64,
}

fn resolve_session_startup(root: &Path, resume: ResumeSelection) -> CliResult<SessionStartup> {
    match resume {
        ResumeSelection::New => Ok(SessionStartup {
            session_id: SessionId::new(generate_session_id()),
            resume_session_path: None,
            initial_transcript_before_seq: None,
            initial_transcript_entries: Vec::new(),
            provider_binding: None,
        }),
        ResumeSelection::SessionId(id) => {
            let mut index = SessionIndex::load(root)?;
            let session_id = SessionId::new(id.clone());
            let summary = index
                .summary_by_id(&session_id)?
                .ok_or_else(|| CliError::SessionNotFound { id })?;
            resumed_session_startup(root, summary)
        }
        ResumeSelection::Pick => {
            let mut index = SessionIndex::load(root)?;
            let summaries = index.summaries()?;
            if summaries.is_empty() {
                return Err(CliError::NoSessionsAvailable);
            }
            let store = SessionStore::new(root);
            let options = resume_session_options(&store, summaries)?;
            let selected = {
                let stdin = io::stdin();
                let stdout = io::stdout();
                prompt_for_session_index(&options, stdin.lock(), stdout.lock())?
            };
            resumed_session_startup(root, options[selected].summary.clone())
        }
    }
}

fn resume_session_options(
    store: &SessionStore,
    summaries: Vec<SessionSummary>,
) -> CliResult<Vec<ResumeSessionOption>> {
    summaries
        .into_iter()
        .map(|summary| {
            let latest_message_preview = store
                .read_latest_message_preview(&summary.path)
                .ok()
                .flatten();
            let last_message_at_ms =
                session_file_modified_at_ms(&summary).unwrap_or(summary.updated_at_ms);
            Ok(ResumeSessionOption {
                summary,
                latest_message_preview,
                last_message_at_ms,
            })
        })
        .collect()
}

fn resumed_session_startup(root: &Path, summary: SessionSummary) -> CliResult<SessionStartup> {
    let store = SessionStore::new(root);
    let (meta, provider_binding) = store
        .read_startup_binding(&summary.path)
        .map_err(|source| CliError::ResumeStartupBinding {
            path: summary.path.clone(),
            source,
        })?;
    if meta.id != summary.session_id {
        return Err(CliError::SessionIdMismatch {
            index: summary.session_id.to_string(),
            file: meta.id.to_string(),
        });
    }

    let page = store
        .read_resume_transcript(&summary.path, 100, INITIAL_TRANSCRIPT_PAGE_LINE_LIMIT)
        .map_err(|source| CliError::ResumeTranscriptPage {
            path: summary.path.clone(),
            source,
        })?;

    Ok(SessionStartup {
        session_id: meta.id,
        resume_session_path: Some(summary.path),
        initial_transcript_before_seq: page.next_before_seq,
        initial_transcript_entries: page
            .lines
            .into_iter()
            .map(|line| UiTranscriptEntry::SessionRecord(line.kind))
            .collect(),
        provider_binding,
    })
}

fn session_file_modified_at_ms(summary: &SessionSummary) -> Option<u64> {
    fs::metadata(&summary.path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
}

fn prompt_for_session_index(
    options: &[ResumeSessionOption],
    mut input: impl BufRead,
    mut output: impl Write,
) -> CliResult<usize> {
    let terminal_width = terminal_width();
    writeln!(output, "{ANSI_BOLD}{ANSI_CYAN}Resume a session{ANSI_RESET}")?;
    writeln!(
        output,
        "{ANSI_DIM}Type a number to resume, or type text to filter cwd/latest message.{ANSI_RESET}"
    )?;
    writeln!(output)?;
    write_resume_session_options(options, terminal_width, &mut output)?;
    writeln!(output)?;
    write!(output, "{ANSI_GREEN}resume>{ANSI_RESET} ")?;
    output.flush()?;

    let mut line = String::new();
    let read = input.read_line(&mut line)?;
    if read == 0 {
        return Err(CliError::NoSessionSelected);
    }
    let selected = line.trim();
    if selected.is_empty() {
        return Err(CliError::NoSessionSelected);
    }
    if let Some(index) = resume_session_exact_selection(options, selected) {
        return Ok(index);
    }

    let matches = resume_session_filter_matches(options, selected);
    if matches.len() == 1 {
        return Ok(matches[0]);
    }
    if !matches.is_empty() {
        writeln!(output)?;
        writeln!(output, "{ANSI_BOLD}{ANSI_CYAN}Matches:{ANSI_RESET}")?;
        for index in matches {
            write_resume_session_option(index, &options[index], terminal_width, &mut output)?;
        }
        writeln!(output)?;
        write!(output, "{ANSI_GREEN}resume>{ANSI_RESET} ")?;
        output.flush()?;

        line.clear();
        let read = input.read_line(&mut line)?;
        if read == 0 {
            return Err(CliError::NoSessionSelected);
        }
        let selected = line.trim();
        if selected.is_empty() {
            return Err(CliError::NoSessionSelected);
        }
        if let Some(index) = resume_session_exact_selection(options, selected) {
            return Ok(index);
        }
        return Err(CliError::InvalidSessionSelection {
            selected: selected.to_string(),
            max: options.len(),
        });
    }

    Err(CliError::InvalidSessionSelection {
        selected: selected.to_string(),
        max: options.len(),
    })
}

fn resume_session_exact_selection(
    options: &[ResumeSessionOption],
    selected: &str,
) -> Option<usize> {
    if let Ok(index) = selected.parse::<usize>() {
        if (1..=options.len()).contains(&index) {
            return Some(index - 1);
        }
    }
    options
        .iter()
        .position(|option| option.summary.session_id.as_str() == selected)
}

fn write_resume_session_options(
    options: &[ResumeSessionOption],
    terminal_width: usize,
    mut output: impl Write,
) -> io::Result<()> {
    for (index, option) in options.iter().enumerate() {
        write_resume_session_option(index, option, terminal_width, &mut output)?;
    }
    Ok(())
}

fn write_resume_session_option(
    index: usize,
    option: &ResumeSessionOption,
    terminal_width: usize,
    mut output: impl Write,
) -> io::Result<()> {
    let age = relative_session_age(option.last_message_at_ms);
    let date = session_date_label(option.last_message_at_ms);
    writeln!(
        output,
        "{ANSI_GREEN}{:>2}){ANSI_RESET} {ANSI_BOLD}{}{ANSI_RESET}  {ANSI_DIM}{} · {}{ANSI_RESET}",
        index + 1,
        session_summary_primary_label(option),
        age,
        date
    )?;
    write_wrapped_field(
        &mut output,
        terminal_width,
        ANSI_BLUE,
        "cwd",
        &option.summary.cwd,
    )?;
    if let Some(prompt) = session_summary_prompt_label(option) {
        write_wrapped_field(&mut output, terminal_width, ANSI_YELLOW, "prompt", prompt)?;
    }
    if let Some(latest) = option.latest_message_preview.as_deref() {
        write_wrapped_field(&mut output, terminal_width, ANSI_MAGENTA, "latest", latest)?;
    }
    write_wrapped_field(
        &mut output,
        terminal_width,
        ANSI_GRAY,
        "id",
        option.summary.session_id.as_str(),
    )?;
    Ok(())
}

fn resume_session_filter_matches(options: &[ResumeSessionOption], filter: &str) -> Vec<usize> {
    let filter = filter.to_lowercase();
    options
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            let cwd_matches = option.summary.cwd.to_lowercase().contains(&filter);
            let latest_message_matches = option
                .latest_message_preview
                .as_deref()
                .is_some_and(|message| message.to_lowercase().contains(&filter));
            (cwd_matches || latest_message_matches).then_some(index)
        })
        .collect()
}

fn session_summary_primary_label(option: &ResumeSessionOption) -> &str {
    option
        .summary
        .title
        .as_deref()
        .or(option.summary.preview.as_deref())
        .or(option.latest_message_preview.as_deref())
        .unwrap_or(option.summary.cwd.as_str())
}

fn session_summary_prompt_label(option: &ResumeSessionOption) -> Option<&str> {
    option
        .summary
        .preview
        .as_deref()
        .filter(|preview| Some(*preview) != option.summary.title.as_deref())
}

fn write_wrapped_field(
    mut output: impl Write,
    terminal_width: usize,
    label_color: &str,
    label: &str,
    value: &str,
) -> io::Result<()> {
    let label_text = format!("    {label}: ");
    let continuation = " ".repeat(label_text.chars().count());
    let value_width = terminal_width.saturating_sub(continuation.len()).max(20);
    let mut lines = wrap_text(value, value_width).into_iter();

    let Some(first) = lines.next() else {
        writeln!(
            output,
            "{label_color}{label_text}{ANSI_RESET}{ANSI_DIM}<empty>{ANSI_RESET}"
        )?;
        return Ok(());
    };

    writeln!(output, "{label_color}{label_text}{ANSI_RESET}{first}")?;
    for line in lines {
        writeln!(output, "{continuation}{line}")?;
    }
    Ok(())
}

fn session_date_label(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let days = (total_seconds / 86_400) as i64;
    let seconds_of_day = total_seconds % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
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

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>();
    if normalized.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    for word in normalized {
        if current.is_empty() {
            push_wrapped_word(&mut lines, &mut current, word, width);
            continue;
        }
        let next_len = current.chars().count() + 1 + word.chars().count();
        if next_len <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            push_wrapped_word(&mut lines, &mut current, word, width);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn push_wrapped_word(lines: &mut Vec<String>, current: &mut String, word: &str, width: usize) {
    let mut chunk = String::new();
    for character in word.chars() {
        if chunk.chars().count() == width {
            lines.push(std::mem::take(&mut chunk));
        }
        chunk.push(character);
    }
    *current = chunk;
}

fn terminal_width() -> usize {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    if result == 0 && size.ws_col > 0 {
        usize::from(size.ws_col)
    } else {
        RESUME_PICKER_DEFAULT_WIDTH
    }
}

fn relative_session_age(updated_at_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(updated_at_ms, |duration| duration.as_millis() as u64);
    let elapsed_ms = now_ms.saturating_sub(updated_at_ms);
    let elapsed_seconds = elapsed_ms / 1000;

    if elapsed_seconds < 60 {
        return "just now".to_string();
    }
    let elapsed_minutes = elapsed_seconds / 60;
    if elapsed_minutes < 60 {
        return format!("{elapsed_minutes}m ago");
    }
    let elapsed_hours = elapsed_minutes / 60;
    if elapsed_hours < 48 {
        return format!("{elapsed_hours}h ago");
    }
    let elapsed_days = elapsed_hours / 24;
    format!("{elapsed_days}d ago")
}

fn parse_cli_args(args: impl IntoIterator<Item = OsString>) -> CliResult<CliAction> {
    let mut auth_mode = AuthMode::OwnState;
    let mut positional = Vec::new();
    for arg in args {
        if arg == "--norotate" {
            auth_mode = AuthMode::CodexReadOnly;
            continue;
        }
        positional.push(arg.to_string_lossy().into_owned());
    }
    match positional.as_slice() {
        [] => Ok(CliAction::Tui {
            auth_mode,
            resume: ResumeSelection::New,
        }),
        [command] if command == "resume" => Ok(CliAction::Tui {
            auth_mode,
            resume: ResumeSelection::Pick,
        }),
        [command, session_id] if command == "resume" => Ok(CliAction::Tui {
            auth_mode,
            resume: ResumeSelection::SessionId(session_id.clone()),
        }),
        [command, session_id] if command == "repair-session" => Ok(CliAction::RepairSession {
            session_id: session_id.clone(),
        }),
        [command, session_id] if command == "inspect-session" => Ok(CliAction::InspectSession {
            session_id: session_id.clone(),
        }),
        [command, session_id, chunk_index] if command == "probe-session-chunk" => {
            Ok(CliAction::ProbeSessionChunk {
                session_id: session_id.clone(),
                chunk_index: chunk_index.parse().map_err(|source| CliError::Io {
                    source: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid chunk index `{chunk_index}`: {source}"),
                    ),
                })?,
            })
        }
        [command, socket_path] if command == "ipc-uds" => Ok(CliAction::IpcUds {
            socket_path: PathBuf::from(socket_path),
        }),
        _ => Err(CliError::UnsupportedArguments {
            arguments: positional.join(" "),
        }),
    }
}

fn display_option<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn codex_home() -> CliResult<PathBuf> {
    if let Ok(root) = env::var("CODEX_HOME") {
        return Ok(PathBuf::from(root));
    }
    let home = env::var("HOME").map_err(|source| CliError::HomeRequired {
        fallback_variable: "CODEX_HOME",
        source,
    })?;
    Ok(PathBuf::from(home).join(".codex"))
}

fn harness_state_dir() -> CliResult<PathBuf> {
    if let Ok(root) = env::var("XDG_STATE_HOME")
        && !root.trim().is_empty()
    {
        return Ok(PathBuf::from(root).join("new_harness"));
    }
    let home = env::var("HOME").map_err(|source| CliError::HomeRequired {
        fallback_variable: "XDG_STATE_HOME",
        source,
    })?;
    Ok(PathBuf::from(home).join(".local/state/new_harness"))
}

fn harness_auth_path(state_dir: &Path) -> PathBuf {
    state_dir.join("auth.json")
}

fn load_base_instructions(model: &str, codex_home: &Path, state_dir: &Path) -> CliResult<String> {
    let file_name = format!("{model}-model-instructions.md");
    let source = codex_home.join(&file_name);
    let destination = state_dir.join("base-instructions.md");
    if source.exists() {
        let parent = destination
            .parent()
            .ok_or(CliError::BaseInstructionsMissingParent)?;
        fs::create_dir_all(parent).map_err(|source| CliError::CreateBaseInstructionsDir {
            path: parent.to_path_buf(),
            source,
        })?;
    } else if !destination.exists() {
        return Err(CliError::MissingBaseInstructions {
            source_path: source,
            destination,
        });
    }
    fs::read_to_string(&destination).map_err(|source| CliError::ReadBaseInstructions {
        path: destination,
        source,
    })
}

fn load_owned_chatgpt_auth(auth_path: &Path, codex_home: &Path) -> CliResult<Auth> {
    if auth_path.exists() {
        let _ = read_harness_auth_snapshot(auth_path)?;
    } else {
        import_codex_access_token(auth_path, codex_home)?;
    };
    let session = HarnessAuthFileSession {
        auth_path: auth_path.to_path_buf(),
        codex_auth_path: codex_home.join("auth.json"),
    };
    Ok(Auth::ChatGpt(Arc::new(session)))
}

fn load_codex_readonly_chatgpt_auth(codex_home: &Path) -> CliResult<Auth> {
    let auth_path = codex_home.join("auth.json");
    let session = CodexReadOnlyAuthSession::new(auth_path)?;
    Ok(Auth::ChatGpt(Arc::new(session)))
}

fn generate_session_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos();
    let counter = NEXT_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bits = now ^ (u128::from(std::process::id()) << 64) ^ u128::from(counter);
    bits ^= bits.rotate_left(31);
    bits = bits.wrapping_mul(0x9e37_79b9_7f4a_7c15_d1b5_4a32_d192_ed03);
    format_uuid_like(bits)
}

fn format_uuid_like(mut bits: u128) -> String {
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

#[derive(Debug)]
struct HarnessAuthFileSession {
    auth_path: PathBuf,
    codex_auth_path: PathBuf,
}

impl HarnessAuthFileSession {
    fn reload_snapshot(&self) -> Result<HarnessAuthSnapshot, AuthError> {
        read_harness_auth_snapshot(&self.auth_path)
    }

    async fn rotate_or_reimport_after_unauthorized(&self) -> Result<(), AuthError> {
        let snapshot = self.reload_snapshot()?;
        if let Some(refresh_token) = snapshot
            .refresh_token
            .as_ref()
            .filter(|token| !token.trim().is_empty())
        {
            let auth = ManagedChatGptAuth::new(ChatGptAuthTokens {
                access_token: snapshot.access_token,
                refresh_token: refresh_token.clone(),
                account_id: snapshot.account_id,
                fedramp: false,
            });
            auth.refresh_after_unauthorized().await?;
            let refreshed = auth.snapshot()?;
            let snapshot = HarnessAuthSnapshot {
                access_token: refreshed.access_token,
                refresh_token: Some(refreshed.refresh_token),
                account_id: refreshed.account_id,
            };
            write_harness_auth_snapshot(&self.auth_path, &snapshot)?;
            return Ok(());
        }

        let imported = read_codex_auth_snapshot(&self.codex_auth_path)?;
        if imported.access_token == snapshot.access_token {
            return Err(AuthError::refresh(
                "ChatGPT access token was rejected and the harness auth state has no harness-owned refresh token. Run `codex login` or use --norotate after Codex refreshes auth.json.",
            ));
        }

        let snapshot = HarnessAuthSnapshot {
            access_token: imported.access_token,
            refresh_token: None,
            account_id: imported.account_id,
        };
        write_harness_auth_snapshot(&self.auth_path, &snapshot)
    }
}

impl ChatGptAuthSession for HarnessAuthFileSession {
    fn access_token(&self) -> Result<String, AuthError> {
        Ok(self.reload_snapshot()?.access_token)
    }

    fn account_id(&self) -> Result<Option<String>, AuthError> {
        Ok(self.reload_snapshot()?.account_id)
    }

    fn refresh_after_unauthorized(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AuthError>> + Send + '_>> {
        Box::pin(self.rotate_or_reimport_after_unauthorized())
    }
}

#[derive(Debug)]
struct CodexReadOnlyAuthSession {
    auth_path: PathBuf,
    snapshot: RwLock<CodexAuthSnapshot>,
}

impl CodexReadOnlyAuthSession {
    fn new(auth_path: PathBuf) -> Result<Self, AuthError> {
        let snapshot = read_codex_auth_snapshot(&auth_path)?;
        Ok(Self {
            auth_path,
            snapshot: RwLock::new(snapshot),
        })
    }

    fn reload_snapshot(&self) -> Result<CodexAuthSnapshot, AuthError> {
        let snapshot = read_codex_auth_snapshot(&self.auth_path)?;
        let mut stored = self
            .snapshot
            .write()
            .map_err(|_| AuthError::load("Codex auth cache lock poisoned"))?;
        *stored = snapshot.clone();
        Ok(snapshot)
    }

    fn reload_after_unauthorized(&self) -> Result<(), AuthError> {
        let snapshot = read_codex_auth_snapshot(&self.auth_path)?;
        let mut stored = self
            .snapshot
            .write()
            .map_err(|_| AuthError::load("Codex auth cache lock poisoned"))?;
        if snapshot.access_token == stored.access_token {
            return Err(AuthError::refresh(
                "ChatGPT access token was rejected; --norotate will not rotate refresh tokens. Run `codex login` or let Codex refresh auth.json, then retry.",
            ));
        }
        *stored = snapshot;
        Ok(())
    }
}

impl ChatGptAuthSession for CodexReadOnlyAuthSession {
    fn access_token(&self) -> Result<String, AuthError> {
        Ok(self.reload_snapshot()?.access_token)
    }

    fn account_id(&self) -> Result<Option<String>, AuthError> {
        Ok(self.reload_snapshot()?.account_id)
    }

    fn refresh_after_unauthorized(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AuthError>> + Send + '_>> {
        Box::pin(async move { self.reload_after_unauthorized() })
    }
}

#[derive(Debug, Clone)]
struct CodexAuthSnapshot {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Debug, Clone)]
struct HarnessAuthSnapshot {
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthJson {
    tokens: Option<CodexAuthTokensJson>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthTokensJson {
    access_token: Option<String>,
    account_id: Option<String>,
}

fn read_codex_auth_snapshot(auth_path: &Path) -> Result<CodexAuthSnapshot, AuthError> {
    let bytes = fs::read(auth_path).map_err(|err| {
        AuthError::load_with_source(
            format!(
                "failed to read {}. Run `codex login` or set CODEX_HOME to a Codex auth directory.",
                auth_path.display()
            ),
            err,
        )
    })?;
    let auth = sonic_rs::from_slice::<CodexAuthJson>(&bytes).map_err(|err| {
        AuthError::load_with_source(format!("failed to parse {}", auth_path.display()), err)
    })?;
    let tokens = auth.tokens.ok_or(AuthError::Missing("tokens"))?;
    let access_token = tokens
        .access_token
        .filter(|token| !token.trim().is_empty())
        .ok_or(AuthError::Missing("tokens.access_token"))?;
    Ok(CodexAuthSnapshot {
        access_token,
        account_id: tokens
            .account_id
            .filter(|account_id| !account_id.trim().is_empty()),
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct HarnessAuthJson {
    tokens: HarnessAuthTokensJson,
}

#[derive(Debug, Serialize, Deserialize)]
struct HarnessAuthTokensJson {
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

fn import_codex_access_token(
    auth_path: &Path,
    codex_home: &Path,
) -> Result<HarnessAuthSnapshot, AuthError> {
    let codex = read_codex_auth_snapshot(&codex_home.join("auth.json"))?;
    let snapshot = HarnessAuthSnapshot {
        access_token: codex.access_token,
        refresh_token: None,
        account_id: codex.account_id,
    };
    write_harness_auth_snapshot(auth_path, &snapshot)?;
    Ok(snapshot)
}

fn read_harness_auth_snapshot(auth_path: &Path) -> Result<HarnessAuthSnapshot, AuthError> {
    let bytes = fs::read(auth_path).map_err(|err| {
        AuthError::load_with_source(format!("failed to read {}", auth_path.display()), err)
    })?;
    let auth = sonic_rs::from_slice::<HarnessAuthJson>(&bytes).map_err(|err| {
        AuthError::load_with_source(format!("failed to parse {}", auth_path.display()), err)
    })?;
    let access_token = if auth.tokens.access_token.trim().is_empty() {
        return Err(AuthError::Missing("tokens.access_token"));
    } else {
        auth.tokens.access_token
    };
    Ok(HarnessAuthSnapshot {
        access_token,
        refresh_token: auth
            .tokens
            .refresh_token
            .filter(|token| !token.trim().is_empty()),
        account_id: auth
            .tokens
            .account_id
            .filter(|account_id| !account_id.trim().is_empty()),
    })
}

fn write_harness_auth_snapshot(
    auth_path: &Path,
    snapshot: &HarnessAuthSnapshot,
) -> Result<(), AuthError> {
    let auth = HarnessAuthJson {
        tokens: HarnessAuthTokensJson {
            access_token: snapshot.access_token.clone(),
            refresh_token: snapshot.refresh_token.clone(),
            account_id: snapshot.account_id.clone(),
        },
    };
    let bytes = sonic_rs::to_vec(&auth)
        .map_err(|err| AuthError::load_with_source("failed to serialize harness auth", err))?;
    let parent = auth_path.parent().ok_or_else(|| {
        AuthError::load(format!(
            "auth path has no parent directory: {}",
            auth_path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|err| {
        AuthError::load_with_source(
            format!("failed to create auth state directory {}", parent.display()),
            err,
        )
    })?;
    let tmp_path = parent.join(format!(".auth.json.{}.tmp", std::process::id()));
    fs::write(&tmp_path, bytes).map_err(|err| {
        AuthError::load_with_source(
            format!(
                "failed to write temporary auth state {}",
                tmp_path.display()
            ),
            err,
        )
    })?;
    fs::rename(&tmp_path, auth_path).map_err(|err| {
        AuthError::load_with_source(
            format!("failed to install auth state {}", auth_path.display()),
            err,
        )
    })
}

#[cfg(test)]
mod tests {
    use sonic_rs::json;

    use super::*;

    #[test]
    fn cli_args_default_to_tui_owned_auth() {
        let action = parse_cli_args(Vec::<OsString>::new()).unwrap();

        assert_eq!(
            action,
            CliAction::Tui {
                auth_mode: AuthMode::OwnState,
                resume: ResumeSelection::New
            }
        );
    }

    #[test]
    fn ollama_cloud_provider_profile_uses_specified_model() {
        let profile =
            create_ollama_cloud_provider_profile("cred-test".to_string(), "glm-5.2".to_string());

        assert_eq!(profile.id.as_str(), "ollama-cloud");
        assert_eq!(profile.display_name, "Ollama Cloud");
        assert_eq!(profile.kind, ProviderKind::OllamaCloud);
        assert_eq!(profile.default_model, "glm-5.2");
        assert_eq!(profile.default_reasoning_effort, None);
        assert_eq!(profile.default_service_tier, None);
        assert_eq!(profile.tool_output_summary_model, "glm-5.2");
        assert_eq!(profile.model_configs, Vec::new());
        assert_eq!(
            profile.driver,
            ProviderDriverConfig::HttpsResponses {
                base_url: OLLAMA_CLOUD_BASE_URL.to_string(),
                request_timeout_ms: 300_000,
                stream_idle_timeout_ms: 300_000,
            }
        );
        assert_eq!(
            profile.auth,
            ProviderAuthConfig::ApiKey {
                credential_id: "cred-test".to_string(),
            }
        );
    }
    #[test]
    fn provider_profiles_with_codex_preserves_ollama_default_and_adds_codex() {
        let profile =
            create_ollama_cloud_provider_profile("cred-test".to_string(), "glm-5.2".to_string());
        let profiles = provider_profiles_with_codex(profile.clone());

        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0], profile);
        assert_eq!(profiles[1], create_codex_provider_profile());
    }

    #[test]
    fn ensure_codex_provider_profile_adds_codex_without_changing_default() {
        let default_profile =
            create_ollama_cloud_provider_profile("cred-test".to_string(), "glm-5.2".to_string());
        let mut config = ProviderConfig {
            default_profile_id: default_profile.id.clone(),
            profiles: vec![default_profile.clone()],
        };

        assert!(ensure_codex_provider_profile(&mut config));
        assert_eq!(config.default_profile(), Some(&default_profile));
        assert!(config.profile(&ProviderProfileId::new("codex")).is_some());
        assert!(!ensure_codex_provider_profile(&mut config));
    }

    #[test]
    fn provider_startup_selection_uses_bound_profile_and_configured_model() {
        let default_profile = create_codex_provider_profile();
        let mut bound_profile =
            create_ollama_cloud_provider_profile("cred-test".to_string(), "glm-5.2".to_string());
        bound_profile
            .model_configs
            .push(harness_core::providers::ProviderModelConfig {
                slug: "glm-5.2".to_string(),
                ..Default::default()
            });
        let config = ProviderConfig {
            default_profile_id: default_profile.id.clone(),
            profiles: vec![default_profile, bound_profile.clone()],
        };
        let binding = ProviderSessionBinding {
            profile_id: bound_profile.id.clone(),
            kind: ProviderKind::OllamaCloud,
            model_settings: ModelSettings::new("glm-5.2", Some("high".to_string()), None),
        };

        let selection = provider_startup_selection(&config, Some(&binding)).unwrap();

        assert_eq!(selection.profile, bound_profile);
        assert_eq!(selection.model_settings, Some(binding.model_settings));
    }

    #[test]
    fn provider_startup_selection_uses_bound_profile_default_for_unconfigured_model() {
        let bound_profile =
            create_ollama_cloud_provider_profile("cred-test".to_string(), "glm-5.2".to_string());
        let config = ProviderConfig {
            default_profile_id: bound_profile.id.clone(),
            profiles: vec![bound_profile.clone()],
        };
        let binding = ProviderSessionBinding {
            profile_id: bound_profile.id.clone(),
            kind: ProviderKind::OllamaCloud,
            model_settings: ModelSettings::new("glm-5.2", Some("high".to_string()), None),
        };

        let selection = provider_startup_selection(&config, Some(&binding)).unwrap();

        assert_eq!(selection.profile, bound_profile);
        assert_eq!(selection.model_settings, None);
    }

    #[test]
    fn cli_args_allow_resume_session_id() {
        let action =
            parse_cli_args([OsString::from("resume"), OsString::from("session-123")]).unwrap();

        assert_eq!(
            action,
            CliAction::Tui {
                auth_mode: AuthMode::OwnState,
                resume: ResumeSelection::SessionId("session-123".to_string())
            }
        );
    }

    #[test]
    fn cli_args_allow_bare_resume_with_norotate() {
        let action =
            parse_cli_args([OsString::from("resume"), OsString::from("--norotate")]).unwrap();

        assert_eq!(
            action,
            CliAction::Tui {
                auth_mode: AuthMode::CodexReadOnly,
                resume: ResumeSelection::Pick
            }
        );
    }

    #[test]
    fn cli_args_unsupported_usage_mentions_resume() {
        let error = parse_cli_args([OsString::from("bogus")]).unwrap_err();

        assert!(error.to_string().contains("resume [sessionid]"));
    }

    #[test]
    fn session_picker_selects_by_number() {
        let options = vec![
            test_resume_option("first-session", 1, Some("first"), Some("first latest")),
            test_resume_option("second-session", 2, Some("second"), Some("second latest")),
        ];
        let mut output = Vec::new();

        let selected =
            prompt_for_session_index(&options, std::io::Cursor::new(b"2\n"), &mut output).unwrap();

        assert_eq!(selected, 1);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Resume a session"));
        assert!(output.contains("Type a number to resume"));
        assert!(output.contains("1970-01-01 00:00 UTC"));
        assert!(output.contains("2)"));
        assert!(output.contains("second"));
        assert!(output.contains("cwd: "));
        assert!(output.contains("/tmp/project"));
        assert!(output.contains("latest: "));
        assert!(output.contains("second latest"));
        assert!(output.contains("id: "));
        assert!(output.contains("second-session"));
    }

    #[test]
    fn resume_picker_wraps_long_fields_to_width() {
        let wrapped = wrap_text("alpha beta gamma delta", 11);

        assert_eq!(wrapped, vec!["alpha beta", "gamma delta"]);
    }

    #[test]
    fn resume_picker_formats_session_date() {
        assert_eq!(
            session_date_label(1_717_171_717_000),
            "2024-05-31 16:08 UTC"
        );
    }

    #[test]
    fn session_picker_selects_by_session_id() {
        let options = vec![
            test_resume_option("first-session", 1, Some("first"), Some("first latest")),
            test_resume_option("second-session", 2, Some("second"), Some("second latest")),
        ];
        let mut output = Vec::new();

        let selected = prompt_for_session_index(
            &options,
            std::io::Cursor::new(b"second-session\n"),
            &mut output,
        )
        .unwrap();

        assert_eq!(selected, 1);
    }

    #[test]
    fn session_picker_filters_by_cwd() {
        let options = vec![
            test_resume_option_with_cwd(
                "first-session",
                1,
                "/tmp/first-project",
                Some("first"),
                Some("first latest"),
            ),
            test_resume_option_with_cwd(
                "second-session",
                2,
                "/tmp/second-project",
                Some("second"),
                Some("second latest"),
            ),
        ];
        let mut output = Vec::new();

        let selected = prompt_for_session_index(
            &options,
            std::io::Cursor::new(b"second-project\n"),
            &mut output,
        )
        .unwrap();

        assert_eq!(selected, 1);
    }

    #[test]
    fn session_picker_filters_by_latest_message() {
        let options = vec![
            test_resume_option("first-session", 1, Some("first"), Some("fix parser")),
            test_resume_option(
                "second-session",
                2,
                Some("second"),
                Some("wire resume filters"),
            ),
        ];
        let mut output = Vec::new();

        let selected = prompt_for_session_index(
            &options,
            std::io::Cursor::new(b"resume filters\n"),
            &mut output,
        )
        .unwrap();

        assert_eq!(selected, 1);
    }

    #[test]
    fn session_picker_filter_prompts_again_for_multiple_matches() {
        let options = vec![
            test_resume_option(
                "first-session",
                1,
                Some("first"),
                Some("resume filters one"),
            ),
            test_resume_option(
                "second-session",
                2,
                Some("second"),
                Some("resume filters two"),
            ),
        ];
        let mut output = Vec::new();

        let selected = prompt_for_session_index(
            &options,
            std::io::Cursor::new(b"resume filters\nsecond-session\n"),
            &mut output,
        )
        .unwrap();

        assert_eq!(selected, 1);
        assert!(String::from_utf8(output).unwrap().contains("Matches:"));
    }

    #[test]
    fn generated_session_ids_are_uuid_shaped() {
        let first = generate_session_id();
        let second = generate_session_id();

        assert_ne!(first, second);
        assert_uuid_shaped(&first);
        assert_uuid_shaped(&second);
    }

    fn assert_uuid_shaped(value: &str) {
        let parts = value.split('-').collect::<Vec<_>>();
        assert_eq!(parts.len(), 5);
        for (part, width) in parts.iter().zip([8, 4, 4, 4, 12]) {
            assert_eq!(part.len(), width);
            assert!(part
                .chars()
                .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character)));
        }
        assert_eq!(parts[2].chars().next(), Some('4'));
        assert!(matches!(
            parts[3].chars().next(),
            Some('8' | '9' | 'a' | 'b')
        ));
    }

    fn test_resume_option(
        id: &str,
        updated_at_ms: u64,
        title: Option<&str>,
        latest_message_preview: Option<&str>,
    ) -> ResumeSessionOption {
        test_resume_option_with_cwd(
            id,
            updated_at_ms,
            "/tmp/project",
            title,
            latest_message_preview,
        )
    }

    fn test_resume_option_with_cwd(
        id: &str,
        updated_at_ms: u64,
        cwd: &str,
        title: Option<&str>,
        latest_message_preview: Option<&str>,
    ) -> ResumeSessionOption {
        ResumeSessionOption {
            summary: SessionSummary {
                session_id: SessionId::new(id),
                path: PathBuf::from(format!("/tmp/{id}.nhsession")),
                created_at_ms: updated_at_ms,
                updated_at_ms,
                cwd: cwd.to_string(),
                title: title.map(str::to_string),
                preview: Some("preview".to_string()),
                parent_session_id: None,
                forked_from_session_id: None,
            },
            latest_message_preview: latest_message_preview.map(str::to_string),
            last_message_at_ms: updated_at_ms,
        }
    }
}
