//! Supervised OpenAI Chat Completions streaming transport.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use futures_util::{Stream, StreamExt};
use harness_model_api::{
    ModelAttempt, ModelAttemptHandle, ModelCancellation, ModelEvent, ModelFailure,
    ModelFailureKind, ModelInterruption, ModelTerminalOutcome, ModelTransport,
};
use harness_openai_chat_protocol::{ChatEventDecoder, encode_request};
use thiserror::Error;
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

/// One chunk returned by the injected HTTP streaming client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatStreamChunk {
    /// Response body bytes.
    Bytes(Vec<u8>),
    /// HTTP response body reaches EOF.
    End,
}

/// Failure returned by the injected Chat Completions HTTP client.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ChatStreamError {
    /// Authentication is rejected.
    #[error("OpenAI Chat authentication failed: {0}")]
    Authentication(String),
    /// Provider rate limit is reached.
    #[error("OpenAI Chat rate limit reached: {0}")]
    RateLimited(String),
    /// Provider rejects the request.
    #[error("OpenAI Chat request rejected: {0}")]
    ProviderRejected(String),
    /// Network transport fails.
    #[error("OpenAI Chat transport failed: {0}")]
    Transport(String),
    /// Request is cancelled.
    #[error("OpenAI Chat request cancelled")]
    Cancelled,
    /// Request or stream times out.
    #[error("OpenAI Chat request timed out")]
    Timeout,
}

/// HTTP boundary for `POST /v1/chat/completions`.
pub trait ChatStreamingClient: Send + Sync {
    /// Starts one streaming request from its fully encoded JSON body.
    fn start(
        &self,
        body: sonic_rs::Value,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Pin<
                            Box<dyn Stream<Item = Result<ChatStreamChunk, ChatStreamError>> + Send>,
                        >,
                        ChatStreamError,
                    >,
                > + Send
                + '_,
        >,
    >;
}

/// Bounded transport configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatTransportConfiguration {
    /// Number of decoded events buffered per attempt.
    pub event_capacity: usize,
    /// Maximum delay between response chunks.
    pub chunk_timeout: Duration,
    /// Maximum SSE event size.
    pub max_event_bytes: usize,
}

impl Default for ChatTransportConfiguration {
    fn default() -> Self {
        Self {
            event_capacity: 128,
            chunk_timeout: Duration::from_secs(300),
            max_event_bytes: harness_openai_chat_protocol::DEFAULT_MAX_EVENT_BYTES,
        }
    }
}

/// OpenAI Chat transport owning every attempt task through joined shutdown.
pub struct OpenAiChatTransport<C> {
    client: Arc<C>,
    configuration: ChatTransportConfiguration,
    state: Arc<Mutex<TransportState>>,
}

#[derive(Default)]
struct TransportState {
    accepting: bool,
    attempts: Vec<OwnedAttempt>,
}

struct OwnedAttempt {
    cancellation: CancellationToken,
    handle: JoinHandle<()>,
}

impl<C> OpenAiChatTransport<C>
where
    C: ChatStreamingClient + 'static,
{
    /// Creates a transport with explicit bounded stream policy.
    pub fn new(
        client: Arc<C>,
        configuration: ChatTransportConfiguration,
    ) -> Result<Self, ChatTransportError> {
        if configuration.event_capacity == 0 {
            return Err(ChatTransportError::ZeroCapacity);
        }
        Ok(Self {
            client,
            configuration,
            state: Arc::new(Mutex::new(TransportState {
                accepting: true,
                attempts: Vec::new(),
            })),
        })
    }
}

impl<C> ModelTransport for OpenAiChatTransport<C>
where
    C: ChatStreamingClient + 'static,
{
    fn start(
        &self,
        attempt: Arc<ModelAttempt>,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn ModelAttemptHandle>, ModelFailure>> + Send + '_>>
    {
        Box::pin(async move {
            let body = encode_request(&attempt).map_err(protocol_failure)?;
            let tools = Arc::clone(&attempt.request).tools.clone();
            let (sender, receiver) = mpsc::channel(self.configuration.event_capacity);
            let cancellation = CancellationToken::new();
            let task_cancellation = cancellation.clone();
            let client = Arc::clone(&self.client);
            let configuration = self.configuration;

            let mut state = self.state.lock().await;
            if !state.accepting {
                return Err(ModelFailure {
                    kind: ModelFailureKind::Transport,
                    message: "OpenAI Chat transport is shutting down".to_owned(),
                });
            }
            let handle = tokio::spawn(async move {
                run_attempt(
                    client,
                    body,
                    tools,
                    configuration,
                    sender,
                    task_cancellation,
                )
                .await;
            });
            state.attempts.push(OwnedAttempt {
                cancellation: cancellation.clone(),
                handle,
            });
            Ok(Box::new(ChatAttemptHandle {
                receiver,
                cancellation,
            }) as Box<dyn ModelAttemptHandle>)
        })
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = Result<(), ModelFailure>> + Send + '_>> {
        Box::pin(async move {
            let attempts = {
                let mut state = self.state.lock().await;
                state.accepting = false;
                std::mem::take(&mut state.attempts)
            };
            for attempt in &attempts {
                attempt.cancellation.cancel();
            }
            for attempt in attempts {
                attempt.handle.await.map_err(|error| ModelFailure {
                    kind: ModelFailureKind::Transport,
                    message: format!("OpenAI Chat attempt failed to join: {error}"),
                })?;
            }
            Ok(())
        })
    }
}

struct ChatAttemptHandle {
    receiver: mpsc::Receiver<ModelEvent>,
    cancellation: CancellationToken,
}

impl ModelAttemptHandle for ChatAttemptHandle {
    fn cancel(&mut self, _reason: ModelCancellation) {
        self.cancellation.cancel();
    }

    fn next_event(&mut self) -> Pin<Box<dyn Future<Output = Option<ModelEvent>> + Send + '_>> {
        Box::pin(self.receiver.recv())
    }
}

async fn run_attempt<C>(
    client: Arc<C>,
    body: sonic_rs::Value,
    tools: Arc<[harness_tool_api::ToolDefinition]>,
    configuration: ChatTransportConfiguration,
    sender: mpsc::Sender<ModelEvent>,
    cancellation: CancellationToken,
) where
    C: ChatStreamingClient + 'static,
{
    let result = run_stream(client, body, &tools, configuration, &sender, &cancellation).await;
    if let Err(failure) = result {
        let outcome = match failure {
            AttemptFailure::Cancelled => ModelTerminalOutcome::Cancelled(ModelCancellation {
                reason: "OpenAI Chat attempt cancelled".to_owned(),
            }),
            AttemptFailure::Interrupted(reason) => {
                ModelTerminalOutcome::Interrupted(ModelInterruption { reason })
            }
            AttemptFailure::Failed(failure) => ModelTerminalOutcome::Failed(failure),
        };
        let _ = sender.send(ModelEvent::Terminal(outcome)).await;
    }
}

async fn run_stream<C>(
    client: Arc<C>,
    body: sonic_rs::Value,
    tools: &[harness_tool_api::ToolDefinition],
    configuration: ChatTransportConfiguration,
    sender: &mpsc::Sender<ModelEvent>,
    cancellation: &CancellationToken,
) -> Result<(), AttemptFailure>
where
    C: ChatStreamingClient + 'static,
{
    let mut stream = client.start(body).await.map_err(map_stream_error)?;
    let mut decoder =
        ChatEventDecoder::with_tools(configuration.max_event_bytes, tools).map_err(|error| {
            AttemptFailure::Failed(ModelFailure {
                kind: ModelFailureKind::Protocol,
                message: error.to_string(),
            })
        })?;

    loop {
        let next = tokio::select! {
            () = cancellation.cancelled() => return Err(AttemptFailure::Cancelled),
            next = timeout(configuration.chunk_timeout, stream.next()) => next
                .map_err(|_| AttemptFailure::Failed(ModelFailure {
                    kind: ModelFailureKind::Timeout,
                    message: "OpenAI Chat stream chunk timed out".to_owned(),
                }))?,
        };
        let Some(chunk) = next else {
            return Err(AttemptFailure::Interrupted(
                "OpenAI Chat stream ended without [DONE]".to_owned(),
            ));
        };
        match chunk.map_err(map_stream_error)? {
            ChatStreamChunk::Bytes(bytes) => {
                let events = decoder
                    .push(&bytes)
                    .map_err(|error| AttemptFailure::Failed(protocol_failure(error)))?;
                if send_events(sender, events, cancellation).await? {
                    return Ok(());
                }
            }
            ChatStreamChunk::End => {
                let events = decoder
                    .finish()
                    .map_err(|error| AttemptFailure::Failed(protocol_failure(error)))?;
                if send_events(sender, events, cancellation).await? {
                    return Ok(());
                }
                return Err(AttemptFailure::Interrupted(
                    "OpenAI Chat stream ended without a terminal outcome".to_owned(),
                ));
            }
        }
    }
}

async fn send_events(
    sender: &mpsc::Sender<ModelEvent>,
    events: Vec<ModelEvent>,
    cancellation: &CancellationToken,
) -> Result<bool, AttemptFailure> {
    let mut terminal = false;
    for event in events {
        terminal |= matches!(event, ModelEvent::Terminal(_));
        tokio::select! {
            () = cancellation.cancelled() => return Err(AttemptFailure::Cancelled),
            result = sender.send(event) => {
                result.map_err(|_| AttemptFailure::Cancelled)?;
            }
        }
    }
    Ok(terminal)
}

fn protocol_failure(error: impl std::fmt::Display) -> ModelFailure {
    ModelFailure {
        kind: ModelFailureKind::Protocol,
        message: error.to_string(),
    }
}

fn map_stream_error(error: ChatStreamError) -> AttemptFailure {
    let kind = match error {
        ChatStreamError::Authentication(_) => ModelFailureKind::Authentication,
        ChatStreamError::RateLimited(_) => ModelFailureKind::RateLimited,
        ChatStreamError::ProviderRejected(_) => ModelFailureKind::ProviderRejected,
        ChatStreamError::Transport(_) => ModelFailureKind::Transport,
        ChatStreamError::Cancelled => return AttemptFailure::Cancelled,
        ChatStreamError::Timeout => ModelFailureKind::Timeout,
    };
    AttemptFailure::Failed(ModelFailure {
        kind,
        message: error.to_string(),
    })
}

#[derive(Debug)]
enum AttemptFailure {
    Cancelled,
    Interrupted(String),
    Failed(ModelFailure),
}

/// Transport construction failure.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ChatTransportError {
    /// Event queue capacity is zero.
    #[error("OpenAI Chat event capacity must be greater than zero")]
    ZeroCapacity,
}
