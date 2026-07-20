//! Supervised model-stream transport implementation.
//!
//! The transport owns cancellation and task joining. Wire decoding remains in
//! `harness-responses-protocol`, and conversation policy remains above this
//! crate.

use std::{collections::BTreeMap, pin::Pin, sync::Arc};

use futures_util::{Stream, StreamExt};
use harness_model_api::{
    ModelAttempt, ModelAttemptHandle, ModelCancellation, ModelEvent, ModelFailure,
    ModelFailureKind, ModelTerminalOutcome, ModelTransport,
};
use harness_responses_protocol::ResponsesEventDecoder;
use thiserror::Error;
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;

/// Maximum number of decoded events buffered for one model attempt.
pub const DEFAULT_EVENT_CAPACITY: usize = 128;

/// Bytes delivered by an injected streaming client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamChunk {
    /// Response-body bytes.
    Bytes(Vec<u8>),
    /// Stream reaches its transport-level end.
    End,
}

/// Failure returned by an injected streaming client.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// Network or connection failure.
    #[error("stream transport failed: {0}")]
    Transport(String),
    /// Request is cancelled.
    #[error("stream cancelled")]
    Cancelled,
    /// Request exceeds its deadline.
    #[error("stream timed out")]
    Timeout,
}

/// Injected client that starts one raw Responses byte stream.
pub trait StreamingClient: Send + Sync {
    /// Starts one stream for an immutable model attempt.
    fn start(
        &self,
        attempt: Arc<ModelAttempt>,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Pin<Box<dyn Stream<Item = Result<StreamChunk, StreamError>> + Send>>,
                        StreamError,
                    >,
                > + Send,
        >,
    >;
}

/// Transport configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportConfiguration {
    /// Bounded event queue capacity.
    pub event_capacity: usize,
    /// Maximum interval between stream chunks.
    pub chunk_timeout: Duration,
    /// Maximum one-event payload size.
    pub max_event_bytes: usize,
}

impl Default for TransportConfiguration {
    fn default() -> Self {
        Self {
            event_capacity: DEFAULT_EVENT_CAPACITY,
            chunk_timeout: Duration::from_secs(300),
            max_event_bytes: harness_responses_protocol::DEFAULT_MAX_EVENT_BYTES,
        }
    }
}

/// Transport supervisor owning all active attempts.
pub struct TransportSupervisor<C> {
    client: Arc<C>,
    configuration: TransportConfiguration,
    attempts: BTreeMap<u64, JoinHandle<()>>,
    cancellations: BTreeMap<u64, CancellationToken>,
    accepting: bool,
}

impl<C> TransportSupervisor<C>
where
    C: StreamingClient + 'static,
{
    /// Creates an empty transport supervisor.
    pub fn new(
        client: Arc<C>,
        configuration: TransportConfiguration,
    ) -> Result<Self, TransportSupervisorError> {
        if configuration.event_capacity == 0 {
            return Err(TransportSupervisorError::ZeroCapacity);
        }
        Ok(Self {
            client,
            configuration,
            attempts: BTreeMap::new(),
            cancellations: BTreeMap::new(),
            accepting: true,
        })
    }

    /// Starts one supervised attempt and returns its event handle.
    pub fn start(
        &mut self,
        attempt: Arc<ModelAttempt>,
    ) -> Result<TransportAttemptHandle, TransportSupervisorError> {
        if !self.accepting {
            return Err(TransportSupervisorError::ShuttingDown);
        }
        let attempt_id = attempt.attempt_id.0;
        if self.attempts.contains_key(&attempt_id) {
            return Err(TransportSupervisorError::DuplicateAttempt);
        }

        let (sender, receiver) = mpsc::channel(self.configuration.event_capacity);
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let client = Arc::clone(&self.client);
        let configuration = self.configuration;
        let task = tokio::spawn(async move {
            run_attempt(client, configuration, attempt, sender, task_cancellation).await;
        });

        self.attempts.insert(attempt_id, task);
        self.cancellations.insert(attempt_id, cancellation.clone());
        Ok(TransportAttemptHandle {
            receiver,
            cancellation,
            attempt_id,
            terminal_seen: false,
        })
    }

    /// Cancels one attempt and retains its join handle for shutdown.
    pub fn cancel(&self, attempt_id: u64) -> Result<(), TransportSupervisorError> {
        self.cancellations
            .get(&attempt_id)
            .ok_or(TransportSupervisorError::UnknownAttempt)?
            .cancel();
        Ok(())
    }

    /// Retires one completed attempt and joins its owned task exactly once.
    pub async fn retire(&mut self, attempt_id: u64) -> Result<(), TransportSupervisorError> {
        self.cancellations
            .remove(&attempt_id)
            .ok_or(TransportSupervisorError::UnknownAttempt)?;
        let handle = self
            .attempts
            .remove(&attempt_id)
            .ok_or(TransportSupervisorError::UnknownAttempt)?;
        handle
            .await
            .map_err(|error| TransportSupervisorError::Join(error.to_string()))
    }

    /// Cancels and joins every active attempt.
    pub async fn shutdown(&mut self) -> Result<(), TransportSupervisorError> {
        self.accepting = false;
        for token in self.cancellations.values() {
            token.cancel();
        }
        self.cancellations.clear();

        let attempts = std::mem::take(&mut self.attempts);
        let mut first_error = None;
        for (_, handle) in attempts {
            if let Err(error) = handle.await
                && first_error.is_none()
            {
                first_error = Some(TransportSupervisorError::Join(error.to_string()));
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// Model transport implementation backed by a supervised transport supervisor.
pub struct SupervisedModelTransport<C> {
    supervisor: Arc<tokio::sync::Mutex<TransportSupervisor<C>>>,
}

impl<C> SupervisedModelTransport<C>
where
    C: StreamingClient + 'static,
{
    /// Creates an async model transport with explicit stream configuration.
    pub fn new(
        client: Arc<C>,
        configuration: TransportConfiguration,
    ) -> Result<Self, TransportSupervisorError> {
        Ok(Self {
            supervisor: Arc::new(tokio::sync::Mutex::new(TransportSupervisor::new(
                client,
                configuration,
            )?)),
        })
    }

    /// Cancels and joins every registered model attempt.
    pub async fn shutdown(&self) -> Result<(), TransportSupervisorError> {
        self.supervisor.lock().await.shutdown().await
    }
}

impl<C> ModelTransport for SupervisedModelTransport<C>
where
    C: StreamingClient + 'static,
{
    fn start(
        &self,
        attempt: Arc<ModelAttempt>,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Box<dyn ModelAttemptHandle>, ModelFailure>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let handle = self
                .supervisor
                .lock()
                .await
                .start(attempt)
                .map_err(|error| ModelFailure {
                    kind: ModelFailureKind::Transport,
                    message: error.to_string(),
                })?;
            Ok(Box::new(handle) as Box<dyn ModelAttemptHandle>)
        })
    }
    fn shutdown(
        &self,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), ModelFailure>> + Send + '_>> {
        Box::pin(async move {
            SupervisedModelTransport::shutdown(self)
                .await
                .map_err(|error| ModelFailure {
                    kind: ModelFailureKind::Transport,
                    message: error.to_string(),
                })
        })
    }
}
/// Handle for one supervised model attempt.
pub struct TransportAttemptHandle {
    receiver: mpsc::Receiver<ModelEvent>,
    cancellation: CancellationToken,
    attempt_id: u64,
    terminal_seen: bool,
}

impl TransportAttemptHandle {
    /// Returns the attempt identity.
    pub fn attempt_id(&self) -> u64 {
        self.attempt_id
    }

    /// Returns whether a terminal model event has been received.
    pub fn terminal_seen(&self) -> bool {
        self.terminal_seen
    }
}

impl ModelAttemptHandle for TransportAttemptHandle {
    fn cancel(&mut self, reason: ModelCancellation) {
        let _ = reason;
        self.cancellation.cancel();
    }

    fn next_event(
        &mut self,
    ) -> Pin<Box<dyn std::future::Future<Output = Option<ModelEvent>> + Send + '_>> {
        Box::pin(async move {
            let event = self.receiver.recv().await;
            if event
                .as_ref()
                .is_some_and(|event| matches!(event, ModelEvent::Terminal(_)))
            {
                self.terminal_seen = true;
            }
            event
        })
    }
}

async fn run_attempt<C>(
    client: Arc<C>,
    configuration: TransportConfiguration,
    attempt: Arc<ModelAttempt>,
    sender: mpsc::Sender<ModelEvent>,
    cancellation: CancellationToken,
) where
    C: StreamingClient + 'static,
{
    let result = run_attempt_inner(
        client,
        configuration,
        Arc::clone(&attempt),
        sender.clone(),
        cancellation.clone(),
    )
    .await;

    if let Err(failure) = result {
        let outcome = match failure {
            AttemptFailure::Cancelled => ModelTerminalOutcome::Cancelled(ModelCancellation {
                reason: "model attempt cancelled".to_owned(),
            }),
            AttemptFailure::Interrupted(reason) => {
                ModelTerminalOutcome::Interrupted(harness_model_api::ModelInterruption { reason })
            }
            AttemptFailure::Failure(failure) => ModelTerminalOutcome::Failed(failure),
        };
        let _ = sender.send(ModelEvent::Terminal(outcome)).await;
    }
}

async fn run_attempt_inner<C>(
    client: Arc<C>,
    configuration: TransportConfiguration,
    attempt: Arc<ModelAttempt>,
    sender: mpsc::Sender<ModelEvent>,
    cancellation: CancellationToken,
) -> Result<(), AttemptFailure>
where
    C: StreamingClient + 'static,
{
    send_event(&sender, ModelEvent::Started, &cancellation).await?;
    let mut stream = client.start(attempt).await.map_err(map_stream_error)?;
    let mut decoder = ResponsesEventDecoder::with_max_event_bytes(configuration.max_event_bytes)
        .map_err(|error| {
            AttemptFailure::Failure(ModelFailure {
                kind: ModelFailureKind::Protocol,
                message: error.to_string(),
            })
        })?;

    loop {
        let next = tokio::select! {
            _ = cancellation.cancelled() => return Err(AttemptFailure::Cancelled),
            next = timeout(configuration.chunk_timeout, stream.next()) => next
                .map_err(|_| AttemptFailure::Failure(ModelFailure {
                    kind: ModelFailureKind::Timeout,
                    message: "model stream chunk timed out".to_owned(),
                }))?,
        };

        let Some(next) = next else {
            return Err(AttemptFailure::Interrupted(
                "model stream ended without a terminal outcome".to_owned(),
            ));
        };
        let chunk = next.map_err(map_stream_error)?;
        match chunk {
            StreamChunk::Bytes(bytes) => {
                let events = decoder.push(&bytes).map_err(|error| {
                    AttemptFailure::Failure(ModelFailure {
                        kind: ModelFailureKind::Protocol,
                        message: error.to_string(),
                    })
                })?;
                for event in events {
                    let terminal = matches!(event, ModelEvent::Terminal(_));
                    send_event(&sender, event, &cancellation).await?;
                    if terminal {
                        return Ok(());
                    }
                }
            }
            StreamChunk::End => {
                let events = decoder.finish().map_err(|error| {
                    AttemptFailure::Failure(ModelFailure {
                        kind: ModelFailureKind::Protocol,
                        message: error.to_string(),
                    })
                })?;
                let terminal_seen = events
                    .iter()
                    .any(|event| matches!(event, ModelEvent::Terminal(_)));
                for event in events {
                    send_event(&sender, event, &cancellation).await?;
                }
                if !terminal_seen {
                    return Err(AttemptFailure::Interrupted(
                        "model stream ended without a terminal outcome".to_owned(),
                    ));
                }
                return Ok(());
            }
        }
    }
}

async fn send_event(
    sender: &mpsc::Sender<ModelEvent>,
    event: ModelEvent,
    cancellation: &CancellationToken,
) -> Result<(), AttemptFailure> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(AttemptFailure::Cancelled),
        result = sender.send(event) => result.map_err(|_| AttemptFailure::Cancelled),
    }
}

fn map_stream_error(error: StreamError) -> AttemptFailure {
    match error {
        StreamError::Transport(message) => AttemptFailure::Failure(ModelFailure {
            kind: ModelFailureKind::Transport,
            message,
        }),
        StreamError::Cancelled => AttemptFailure::Cancelled,
        StreamError::Timeout => AttemptFailure::Failure(ModelFailure {
            kind: ModelFailureKind::Timeout,
            message: "model stream timed out".to_owned(),
        }),
    }
}

#[derive(Debug)]
enum AttemptFailure {
    Cancelled,
    Interrupted(String),
    Failure(ModelFailure),
}

/// Failure returned by transport supervisor operations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TransportSupervisorError {
    /// Attempt identity is already registered.
    #[error("model attempt is already registered")]
    DuplicateAttempt,
    /// Attempt identity is not registered.
    #[error("model attempt is not registered")]
    UnknownAttempt,
    /// A supervised task cannot be joined.
    #[error("model attempt task failed to join: {0}")]
    Join(String),
    /// Event queue capacity is zero.
    #[error("transport event capacity must be greater than zero")]
    ZeroCapacity,
    /// Transport no longer accepts attempts because shutdown started.
    #[error("transport is shutting down")]
    ShuttingDown,
}

pub mod ws;
