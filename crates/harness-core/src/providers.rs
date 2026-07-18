//! Provider profile domain types used by the harness runtime and UI.

use std::{fmt, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

/// Stable provider profile identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderProfileId(String);

impl ProviderProfileId {
    /// Create a provider profile identifier from a validated string.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return the provider profile identifier as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// User-facing provider kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Codex provider using ChatGPT auth and WebSocket transport.
    Codex,
    /// Ollama Cloud provider using API-key auth and HTTPS request transport.
    OllamaCloud,
    /// HTTPS API provider using API-key auth and HTTPS request transport.
    HttpsApi,
}

impl ProviderKind {
    /// Return a short user-facing provider kind label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OllamaCloud => "ollama-cloud",
            Self::HttpsApi => "https-api",
        }
    }
}

/// User-facing provider transport kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTransportKind {
    /// WebSocket streaming transport.
    WebSocket,
    /// HTTPS streaming request transport.
    Https,
}

impl ProviderTransportKind {
    /// Return a short user-facing transport label.
    pub fn label(self) -> &'static str {
        match self {
            Self::WebSocket => "ws",
            Self::Https => "https",
        }
    }
}

/// Auth configuration for a provider profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderAuthConfig {
    /// ChatGPT auth managed by the harness.
    ChatGptHarness,
    /// API key stored by the harness.
    ApiKey { credential_id: String },
}

/// Driver configuration for a provider profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderDriverConfig {
    /// Codex wire protocol over WebSocket.
    CodexWsResponses {
        /// Base URL including provider path prefix.
        base_url: String,
        /// Stream idle timeout in milliseconds.
        stream_idle_timeout_ms: u64,
    },
    /// Responses-compatible wire protocol over HTTPS.
    HttpsResponses {
        /// Base URL including provider path prefix.
        base_url: String,
        /// Request timeout in milliseconds.
        request_timeout_ms: u64,
        /// Stream idle timeout in milliseconds.
        stream_idle_timeout_ms: u64,
    },
}

impl ProviderDriverConfig {
    /// Return the configured base URL.
    pub fn base_url(&self) -> &str {
        match self {
            Self::CodexWsResponses { base_url, .. } | Self::HttpsResponses { base_url, .. } => {
                base_url
            }
        }
    }

    /// Return the provider transport kind.
    pub fn transport(&self) -> ProviderTransportKind {
        match self {
            Self::CodexWsResponses { .. } => ProviderTransportKind::WebSocket,
            Self::HttpsResponses { .. } => ProviderTransportKind::Https,
        }
    }

    /// Return the stream idle timeout.
    pub fn stream_idle_timeout(&self) -> Duration {
        let millis = match self {
            Self::CodexWsResponses {
                stream_idle_timeout_ms,
                ..
            }
            | Self::HttpsResponses {
                stream_idle_timeout_ms,
                ..
            } => *stream_idle_timeout_ms,
        };
        Duration::from_millis(millis)
    }

    /// Return the HTTPS request timeout for HTTPS drivers.
    pub fn request_timeout(&self) -> Option<Duration> {
        match self {
            Self::CodexWsResponses { .. } => None,
            Self::HttpsResponses {
                request_timeout_ms, ..
            } => Some(Duration::from_millis(*request_timeout_ms)),
        }
    }
}

/// Provider profile persisted by the harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// Stable profile identifier.
    pub id: ProviderProfileId,
    /// User-facing provider display name.
    pub display_name: String,
    /// User-facing provider kind.
    pub kind: ProviderKind,
    /// Auth configuration.
    pub auth: ProviderAuthConfig,
    /// Driver configuration.
    pub driver: ProviderDriverConfig,
    /// Default model slug for new sessions.
    pub default_model: String,
    /// Default reasoning effort.
    pub default_reasoning_effort: Option<String>,
    /// Default service tier.
    pub default_service_tier: Option<String>,
    /// Locally configured model capability metadata, used when the provider's
    /// `/models` response does not include enough metadata.
    #[serde(default)]
    pub model_configs: Vec<ProviderModelConfig>,
    /// Model used for tool-output summary requests.
    #[serde(default = "default_tool_output_summary_model")]
    pub tool_output_summary_model: String,
}

fn default_tool_output_summary_model() -> String {
    "gpt-5.4".to_string()
}

/// Provider configuration document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Default provider profile id.
    pub default_profile_id: ProviderProfileId,
    /// Configured provider profiles.
    pub profiles: Vec<ProviderProfile>,
}

impl ProviderConfig {
    /// Return the default provider profile.
    pub fn default_profile(&self) -> Option<&ProviderProfile> {
        self.profile(&self.default_profile_id)
    }

    /// Return a provider profile by id.
    pub fn profile(&self, id: &ProviderProfileId) -> Option<&ProviderProfile> {
        self.profiles.iter().find(|profile| &profile.id == id)
    }
}

/// Provider summary shown in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUiInfo {
    /// Provider display name.
    pub display_name: String,
    /// Provider kind.
    pub kind: ProviderKind,
    /// Provider transport.
    pub transport: ProviderTransportKind,
}

impl ProviderUiInfo {
    /// Build UI info from a profile.
    pub fn from_profile(profile: &ProviderProfile) -> Self {
        Self {
            display_name: profile.display_name.clone(),
            kind: profile.kind,
            transport: profile.driver.transport(),
        }
    }

    /// Return a compact provider/transport label.
    pub fn compact_label(&self) -> String {
        format!("{}/{}", self.display_name, self.transport.label())
    }
}

/// Locally-configured capability metadata for a model, used when the provider's
/// `/models` endpoint does not return enough metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModelConfig {
    /// Model slug this config describes.
    pub slug: String,
    /// Total context window in tokens, when known.
    pub context_window: Option<u64>,
    /// Effective input window as a percent of `context_window`.
    pub effective_context_window_percent: u64,
    /// Whether the model supports native tools.
    pub supports_tools: bool,
    /// Whether the model supports parallel tool calls.
    pub supports_parallel_tool_calls: bool,
    /// Supported service tiers.
    pub service_tiers: Vec<String>,
}

impl Default for ProviderModelConfig {
    fn default() -> Self {
        Self {
            slug: String::new(),
            context_window: None,
            effective_context_window_percent: 95,
            supports_tools: true,
            supports_parallel_tool_calls: false,
            service_tiers: Vec::new(),
        }
    }
}

impl ProviderModelConfig {
    /// Return effective context window tokens.
    pub fn effective_context_window(&self) -> Option<u64> {
        self.context_window
            .map(|window| (window * self.effective_context_window_percent) / 100)
    }
}

/// Provider persistence backend.
pub trait ProviderConfigStore: Send + Sync {
    /// Load provider configuration.
    fn load(&self) -> Result<Option<ProviderConfig>, ProviderStoreError>;

    /// Save provider configuration.
    fn save(&self, config: &ProviderConfig) -> Result<(), ProviderStoreError>;
}

/// File-backed provider config store.
#[derive(Debug, Clone)]
pub struct FileProviderConfigStore {
    path: PathBuf,
}

impl FileProviderConfigStore {
    /// Create a file-backed provider config store.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Return the provider config file path.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl ProviderConfigStore for FileProviderConfigStore {
    fn load(&self) -> Result<Option<ProviderConfig>, ProviderStoreError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let config =
                    sonic_rs::from_slice(&bytes).map_err(|source| ProviderStoreError::Decode {
                        path: self.path.clone(),
                        source,
                    })?;
                Ok(Some(config))
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ProviderStoreError::Read {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn save(&self, config: &ProviderConfig) -> Result<(), ProviderStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ProviderStoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let bytes =
            sonic_rs::to_vec_pretty(config).map_err(|source| ProviderStoreError::Encode {
                path: self.path.clone(),
                source,
            })?;
        std::fs::write(&self.path, bytes).map_err(|source| ProviderStoreError::Write {
            path: self.path.clone(),
            source,
        })
    }
}

/// Provider config store error.
#[derive(Debug, thiserror::Error)]
pub enum ProviderStoreError {
    /// Provider config read failed.
    #[error("failed to read provider config `{path}`")]
    Read {
        /// Provider config path.
        path: PathBuf,
        /// Source I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Provider config decode failed.
    #[error("failed to decode provider config `{path}`")]
    Decode {
        /// Provider config path.
        path: PathBuf,
        /// Source JSON error.
        #[source]
        source: sonic_rs::Error,
    },
    /// Provider config encode failed.
    #[error("failed to encode provider config `{path}`")]
    Encode {
        /// Provider config path.
        path: PathBuf,
        /// Source JSON error.
        #[source]
        source: sonic_rs::Error,
    },
    /// Provider config parent directory creation failed.
    #[error("failed to create provider config directory `{path}`")]
    CreateDir {
        /// Directory path.
        path: PathBuf,
        /// Source I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Provider config write failed.
    #[error("failed to write provider config `{path}`")]
    Write {
        /// Provider config path.
        path: PathBuf,
        /// Source I/O error.
        #[source]
        source: std::io::Error,
    },
}
