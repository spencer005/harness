//! Isolated session query service boundary.
//!
//! Request failures are returned to individual callers and never terminate
//! the listener. Storage remains behind `harness-session-store`; this crate
//! does not own or migrate `.nhsession` files.

use std::sync::Arc;

use harness_session_store::{
    PageSize, SessionId, SessionSequence, SessionStore, SessionStoreError, TranscriptPage,
};

use thiserror::Error;
use tokio::sync::Semaphore;


/// Query sent to the session service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionQuery {
    /// Loads older records using sequence identity.
    LoadOlder {
        /// Session identity.
        session_id: SessionId,
        /// Exclusive upper sequence bound.
        before: Option<SessionSequence>,
        /// Maximum persisted entries.
        maximum_entries: PageSize,
    },
    /// Returns the current session path for explicit inspection.
    SessionPath {
        /// Session identity.
        session_id: SessionId,
    },
}

/// Query response returned to one caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionQueryResponse {
    /// Paged session records.
    Page(TranscriptPage),
    /// Current-format session path.
    Path(std::path::PathBuf),
}

/// Read service that isolates storage execution from frontends.
pub struct SessionReadService {
    store: Arc<dyn SessionStore>,
    permits: Arc<Semaphore>,
}

impl SessionReadService {
    /// Creates a service over one session-store implementation.
    pub fn new(
        store: Arc<dyn SessionStore>,
        maximum_concurrent_reads: usize,
    ) -> Result<Self, SessionServiceError> {
        if maximum_concurrent_reads == 0 {
            return Err(SessionServiceError::ZeroConcurrency);
        }
        Ok(Self {
            store,
            permits: Arc::new(Semaphore::new(maximum_concurrent_reads)),
        })
    }

    /// Handles one query without changing listener lifetime.
    pub async fn handle(
        &self,
        query: SessionQuery,
    ) -> Result<SessionQueryResponse, SessionServiceError> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| SessionServiceError::Closed)?;
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            match query {
                SessionQuery::LoadOlder {
                    session_id,
                    before,
                    maximum_entries,
                } => {
                    let reader = store.reader()?;
                    Ok(SessionQueryResponse::Page(reader.load_older(
                        session_id,
                        before,
                        maximum_entries,
                    )?))
                }
                SessionQuery::SessionPath { session_id } => Ok(SessionQueryResponse::Path(
                    store.session_path(session_id)?,
                )),
            }
        })
        .await
        .map_err(|error| SessionServiceError::TaskJoin(error.to_string()))?
    }
}

/// Runs a bounded request stream and emits one result for every request.
pub async fn serve_requests(
    service: Arc<SessionReadService>,
    requests: impl futures_util::Stream<Item = SessionQuery> + Send + 'static,
    responses: tokio::sync::mpsc::Sender<
        Result<SessionQueryResponse, SessionServiceError>,
    >,
    maximum_in_flight: usize,
) -> Result<(), SessionServiceError> {
    if maximum_in_flight == 0 {
        return Err(SessionServiceError::ZeroConcurrency);
    }
    use futures_util::StreamExt;

    let mut requests = Box::pin(requests);
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            request = requests.next(), if tasks.len() < maximum_in_flight => {
                match request {
                    Some(request) => {
                        let service = Arc::clone(&service);
                        tasks.spawn(async move { service.handle(request).await });
                    }
                    None => break,
                }
            }
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(completed) = completed {
                    let result = completed
                        .map_err(|error| SessionServiceError::TaskJoin(error.to_string()))
                        .and_then(|response| response);
                    responses
                        .send(result)
                        .await
                        .map_err(|_| SessionServiceError::Closed)?;
                }
            }
        }
    }

    while let Some(completed) = tasks.join_next().await {
        let result = completed
            .map_err(|error| SessionServiceError::TaskJoin(error.to_string()))
            .and_then(|response| response);
        responses
            .send(result)
            .await
            .map_err(|_| SessionServiceError::Closed)?;
    }
    Ok(())
}


/// One request/response boundary for a long-lived session listener.
pub trait SessionRequestListener: Send + Sync {
    /// Handles one decoded request and returns its response or typed error.
    fn handle(
        &self,
        request: SessionQuery,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<SessionQueryResponse, SessionServiceError>,
                > + Send
                + '_,
        >,
    >;
}

impl SessionRequestListener for SessionReadService {
    fn handle(
        &self,
        request: SessionQuery,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<SessionQueryResponse, SessionServiceError>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(SessionReadService::handle(self, request))
    }
}

/// Error returned by one isolated session query.
#[derive(Debug, Error)]
pub enum SessionServiceError {
    /// Storage rejects this query.
    #[error(transparent)]
    Storage(#[from] SessionStoreError),
    /// Request payload is invalid.
    #[error("session query is invalid: {0}")]
    InvalidRequest(String),
    /// Service is closed before the query is scheduled.
    #[error("session service is closed")]
    Closed,
    /// Storage work task cannot be joined.
    #[error("session service task failed to join: {0}")]
    TaskJoin(String),
    /// Read concurrency is zero.
    #[error("session read concurrency must be greater than zero")]
    ZeroConcurrency,
}
