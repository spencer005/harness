//! Deterministic Responses WebSocket pool.

use std::{
    collections::VecDeque,
    future::Future,
    num::{NonZeroU32, NonZeroUsize},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use http::HeaderMap;
use tokio::sync::Mutex;
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{client::IntoClientRequest, protocol::WebSocketConfig},
};

use super::connection::{ConnectionContext, ResponsesConnection};
use crate::{
    ApiProvider, Auth, CodexHeaders, OPENAI_MODEL, ResponsesApiError, ResponsesStreamEvent,
    ResponsesStreamRequest, X_CODEX_TURN_STATE, X_MODELS_ETAG, X_REASONING_INCLUDED, map_ws_error,
    merge_request_headers, stamp_prewarm_generate_false, websocket_source_error,
};

const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_IDLE_WEBSOCKET_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_IDLE_WEBSOCKET_GRACE_PERIOD: Duration = Duration::from_secs(60);

/// Runtime configuration for [`ResponsesWsPool`].
#[derive(Debug, Clone)]
pub struct WsPoolConfig {
    /// Maximum cold WebSocket handshakes the pool may reserve in a rolling minute.
    pub max_new_connections_per_minute: NonZeroU32,
    /// Minimum wall-clock spacing between cold WebSocket handshake attempts.
    pub min_new_connection_interval: Duration,
    /// Completed idle sockets kept ready for each active Responses connection.
    pub ready_connections_per_active_connection: NonZeroUsize,
    /// Delay before idle sockets above a reduced readiness target are closed.
    pub idle_connection_grace_period: Duration,
    /// Whether completed sockets are retained after a successful request.
    pub keep_connections_open: bool,
    /// Timeout applied to the WebSocket handshake.
    pub connect_timeout: Duration,
    /// Interval used by the background idle-maintenance loop.
    pub idle_maintenance_interval: Duration,
}

impl Default for WsPoolConfig {
    fn default() -> Self {
        Self {
            max_new_connections_per_minute: NonZeroU32::new(60).expect("60 is non-zero"),
            min_new_connection_interval: Duration::from_secs(1),
            ready_connections_per_active_connection: NonZeroUsize::new(25).expect("25 is non-zero"),
            idle_connection_grace_period: DEFAULT_IDLE_WEBSOCKET_GRACE_PERIOD,
            keep_connections_open: true,
            connect_timeout: DEFAULT_WEBSOCKET_CONNECT_TIMEOUT,
            idle_maintenance_interval: DEFAULT_IDLE_WEBSOCKET_MAINTENANCE_INTERVAL,
        }
    }
}

#[derive(Debug, Default)]
struct RateWindow {
    opened_at: VecDeque<Instant>,
    next_allowed_at: Option<Instant>,
}

#[derive(Debug)]
struct PoolInner {
    provider: ApiProvider,
    auth: Auth,
    default_headers: HeaderMap,
    config: WsPoolConfig,
    active_connections: AtomicUsize,
    idle_grace_target: AtomicUsize,
    idle_grace_expires_at_ms: AtomicU64,
    started_at: Instant,
    rate_window: Mutex<RateWindow>,
    idle: Mutex<VecDeque<ResponsesConnection>>,
    idle_top_up_running: Mutex<bool>,
    idle_maintenance_running: Mutex<bool>,
}

/// Deterministic live WebSocket pool for Responses requests.
///
/// The pool is scoped by construction to one provider, one authentication
/// source, and one default Codex identity header set. Per-request
/// [`CodexHeaders`] are sent on the handshake used to open a socket and stamped
/// into each request body, but they do not partition idle sockets.
///
/// Reuse is deterministic: acquisition scans the idle queue from the front,
/// leases the first matching socket, preserves the order of remaining matching
/// sockets, and returns completed sockets to the back of the queue. If the idle
/// queue is already at the current readiness target on release, the returned
/// socket is closed.
#[derive(Debug, Clone)]
pub struct ResponsesWsPool {
    inner: Arc<PoolInner>,
}

impl ResponsesWsPool {
    /// Create a pool for one Responses provider/auth/default-header scope.
    pub fn new(
        provider: ApiProvider,
        auth: Auth,
        default_headers: HeaderMap,
        config: WsPoolConfig,
    ) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                provider,
                auth,
                default_headers,
                config,
                active_connections: AtomicUsize::new(0),
                idle_grace_target: AtomicUsize::new(0),
                idle_grace_expires_at_ms: AtomicU64::new(0),
                started_at: Instant::now(),
                rate_window: Mutex::new(RateWindow::default()),
                idle: Mutex::new(VecDeque::new()),
                idle_top_up_running: Mutex::new(false),
                idle_maintenance_running: Mutex::new(false),
            }),
        }
    }

    /// Send a warmup request and retain the completed socket for later reuse.
    ///
    /// The body is cloned, stamped with Codex client metadata, and forced to
    /// `generate: false` before transmission. Unauthorized handshakes refresh
    /// ChatGPT auth once, and retryable transport failures reconnect once.
    pub async fn prewarm(&self, request: ResponsesStreamRequest) -> Result<(), ResponsesApiError> {
        let mut refreshed = false;
        let mut reconnected = false;
        loop {
            let mut body = request.body.clone();
            request.headers.stamp_client_metadata(&mut body)?;
            stamp_prewarm_generate_false(&mut body)?;
            let (mut connection, connection_reused) = match self.acquire(&request.headers).await {
                Ok(acquired) => acquired,
                Err(err) if err.requires_reconnect() && !reconnected => {
                    reconnected = true;
                    self.close_idle().await;
                    continue;
                }
                Err(err) => return Err(err),
            };
            self.start_active_connection(&request.headers).await;
            let mut ignore_event = |_event: ResponsesStreamEvent| std::future::ready(());
            let result = connection
                .stream_request(&body, connection_reused, &mut ignore_event)
                .await;
            match result {
                Ok(()) => {
                    self.release(connection, &request.headers).await;
                    self.finish_active_connection(&request.headers).await;
                    return Ok(());
                }
                Err(err) if err.is_unauthorized() && !refreshed => {
                    let _ = connection.close().await;
                    self.finish_active_connection(&request.headers).await;
                    if self.inner.auth.refresh_after_unauthorized().await? {
                        self.close_idle().await;
                        refreshed = true;
                        continue;
                    }
                    return Err(err);
                }
                Err(err) if err.requires_reconnect() && !reconnected => {
                    let _ = connection.close().await;
                    self.finish_active_connection(&request.headers).await;
                    self.close_idle().await;
                    reconnected = true;
                    continue;
                }
                Err(err) if err.can_keep_websocket_open_after_stream_error() => {
                    self.release(connection, &request.headers).await;
                    self.finish_active_connection(&request.headers).await;
                    return Err(err);
                }
                Err(err) => {
                    let _ = connection.close().await;
                    self.finish_active_connection(&request.headers).await;
                    return Err(err);
                }
            }
        }
    }

    /// Close all currently idle sockets.
    pub async fn close_idle(&self) {
        let mut idle = self.inner.idle.lock().await;
        let mut connections = Vec::new();
        while let Some(connection) = idle.pop_front() {
            connections.push(connection);
        }
        drop(idle);
        for mut connection in connections {
            let _ = connection.close().await;
        }
    }

    /// Schedule idle top-up and idle control-frame maintenance.
    ///
    /// Top-up only opens sockets when active Responses connections require
    /// ready idle capacity.
    pub fn warm_idle_connections(&self, headers: CodexHeaders) {
        self.schedule_idle_top_up(headers.clone());
        self.schedule_idle_maintenance(headers);
    }

    /// Stream one Responses request through a leased WebSocket connection.
    ///
    /// The pool sends the request over an idle socket when one is available,
    /// otherwise it opens a new socket subject to [`WsPoolConfig`] rate limits.
    /// Unauthorized requests refresh ChatGPT auth once, retryable pre-response
    /// transport failures reconnect once, and request-level HTTP error frames
    /// return the socket to the idle queue when the backend keeps it open.
    pub async fn stream_request<F, Fut>(
        &self,
        request: ResponsesStreamRequest,
        mut on_event: F,
    ) -> Result<(), ResponsesApiError>
    where
        F: FnMut(ResponsesStreamEvent) -> Fut,
        Fut: Future<Output = ()>,
    {
        let mut refreshed = false;
        let mut reconnected = false;
        loop {
            let mut body = request.body.clone();
            request.headers.stamp_client_metadata(&mut body)?;
            let (mut connection, connection_reused) = match self.acquire(&request.headers).await {
                Ok(acquired) => acquired,
                Err(err) if err.requires_reconnect() && !reconnected => {
                    reconnected = true;
                    self.close_idle().await;
                    continue;
                }
                Err(err) => return Err(err),
            };
            self.start_active_connection(&request.headers).await;
            let result = connection
                .stream_request(&body, connection_reused, &mut on_event)
                .await;
            match result {
                Ok(()) => {
                    self.release(connection, &request.headers).await;
                    self.finish_active_connection(&request.headers).await;
                    return Ok(());
                }
                Err(err) if err.is_unauthorized() && !refreshed => {
                    let _ = connection.close().await;
                    self.finish_active_connection(&request.headers).await;
                    if self.inner.auth.refresh_after_unauthorized().await? {
                        self.close_idle().await;
                        refreshed = true;
                        continue;
                    }
                    return Err(err);
                }
                Err(err) if err.requires_reconnect() && !reconnected => {
                    let _ = connection.close().await;
                    self.finish_active_connection(&request.headers).await;
                    self.close_idle().await;
                    reconnected = true;
                    continue;
                }
                Err(err) if err.can_keep_websocket_open_after_stream_error() => {
                    self.release(connection, &request.headers).await;
                    self.finish_active_connection(&request.headers).await;
                    return Err(err);
                }
                Err(err) => {
                    let _ = connection.close().await;
                    self.finish_active_connection(&request.headers).await;
                    return Err(err);
                }
            }
        }
    }

    async fn acquire(
        &self,
        headers: &CodexHeaders,
    ) -> Result<(ResponsesConnection, bool), ResponsesApiError> {
        let context = ConnectionContext::from_headers(headers);
        if self.inner.config.keep_connections_open {
            loop {
                let (connection, stale_connections) = {
                    let mut idle = self.inner.idle.lock().await;
                    let mut connection = None;
                    let mut retained = VecDeque::new();
                    let mut stale_connections = Vec::new();
                    while let Some(idle_connection) = idle.pop_front() {
                        if idle_connection.context == context {
                            if connection.is_none() {
                                connection = Some(idle_connection);
                            } else {
                                retained.push_back(idle_connection);
                            }
                        } else {
                            stale_connections.push(idle_connection);
                        }
                    }
                    *idle = retained;
                    (connection, stale_connections)
                };
                for mut stale_connection in stale_connections {
                    let _ = stale_connection.close().await;
                }
                let Some(mut connection) = connection else {
                    break;
                };
                match connection.service_idle_control_frames().await {
                    Ok(()) => return Ok((connection, true)),
                    Err(_) => {
                        let _ = connection.close().await;
                        self.schedule_idle_top_up(headers.clone());
                    }
                }
            }
        }

        let connection = self.connect_with_auth_rotation(headers, context).await?;
        Ok((connection, false))
    }

    async fn release(&self, connection: ResponsesConnection, headers: &CodexHeaders) {
        self.store_idle_connection(connection).await;
        self.schedule_idle_maintenance(headers.clone());
    }

    async fn start_active_connection(&self, headers: &CodexHeaders) {
        self.inner
            .active_connections
            .fetch_add(1, Ordering::Relaxed);
        self.clear_satisfied_idle_grace(self.active_connection_idle_target());
        self.schedule_idle_top_up(headers.clone());
        self.schedule_idle_maintenance(headers.clone());
    }

    async fn finish_active_connection(&self, headers: &CodexHeaders) {
        let previous_target = self.target_idle_connections();
        self.inner
            .active_connections
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                active.checked_sub(1)
            })
            .expect("active Responses WebSocket lease count is balanced");
        let current_target = self.active_connection_idle_target();
        if previous_target > current_target {
            let target = previous_target.max(self.grace_idle_target());
            self.inner
                .idle_grace_target
                .store(target, Ordering::Relaxed);
            self.inner.idle_grace_expires_at_ms.store(
                self.monotonic_ms()
                    .saturating_add(duration_ms(self.inner.config.idle_connection_grace_period)),
                Ordering::Relaxed,
            );
        } else {
            self.clear_satisfied_idle_grace(current_target);
        }
        self.trim_idle_to_target().await;
        if self.target_idle_connections() > 0 {
            self.schedule_idle_top_up(headers.clone());
            self.schedule_idle_maintenance(headers.clone());
        }
    }

    async fn store_idle_connection(&self, mut connection: ResponsesConnection) {
        if !self.inner.config.keep_connections_open || connection.closed {
            let _ = connection.close().await;
            return;
        }
        let mut idle = self.inner.idle.lock().await;
        if idle.len() < self.target_idle_connections() {
            idle.push_back(connection);
        } else {
            drop(idle);
            let _ = connection.close().await;
        }
    }

    fn schedule_idle_top_up(&self, headers: CodexHeaders) {
        if !self.inner.config.keep_connections_open {
            return;
        }
        let pool = self.clone();
        tokio::spawn(async move {
            pool.top_up_idle_connections(headers).await;
        });
    }

    fn schedule_idle_maintenance(&self, headers: CodexHeaders) {
        if !self.inner.config.keep_connections_open {
            return;
        }
        let weak_inner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let Some(inner) = weak_inner.upgrade() else {
                return;
            };
            {
                let mut running = inner.idle_maintenance_running.lock().await;
                if *running {
                    return;
                }
                *running = true;
            }
            drop(inner);

            loop {
                let Some(inner) = weak_inner.upgrade() else {
                    return;
                };
                let interval = inner.config.idle_maintenance_interval;
                drop(inner);
                tokio::time::sleep(interval).await;

                let Some(inner) = weak_inner.upgrade() else {
                    return;
                };
                let pool = Self { inner };
                pool.service_idle_connections().await;
                pool.top_up_idle_connections(headers.clone()).await;
                if pool.stop_idle_maintenance_if_empty().await {
                    return;
                }
            }
        });
    }

    async fn top_up_idle_connections(&self, headers: CodexHeaders) {
        {
            let mut running = self.inner.idle_top_up_running.lock().await;
            if *running {
                return;
            }
            *running = true;
        }

        let context = ConnectionContext::from_headers(&headers);
        loop {
            let missing = {
                let idle = self.inner.idle.lock().await;
                self.active_connection_idle_target()
                    .saturating_sub(idle.len())
            };
            if missing == 0 {
                break;
            }
            match self.connect_with_auth_rotation(&headers, context).await {
                Ok(connection) => self.store_idle_connection(connection).await,
                Err(_) => break,
            }
        }

        let mut running = self.inner.idle_top_up_running.lock().await;
        *running = false;
    }

    fn target_idle_connections(&self) -> usize {
        let current_target = self.active_connection_idle_target();
        if self.monotonic_ms() < self.inner.idle_grace_expires_at_ms.load(Ordering::Relaxed) {
            current_target.max(self.grace_idle_target())
        } else {
            current_target
        }
    }

    fn active_connection_idle_target(&self) -> usize {
        self.inner
            .active_connections
            .load(Ordering::Relaxed)
            .saturating_mul(
                self.inner
                    .config
                    .ready_connections_per_active_connection
                    .get(),
            )
    }

    fn clear_satisfied_idle_grace(&self, current_target: usize) {
        if self.grace_idle_target() <= current_target {
            self.inner.idle_grace_target.store(0, Ordering::Relaxed);
            self.inner
                .idle_grace_expires_at_ms
                .store(0, Ordering::Relaxed);
        }
    }

    fn grace_idle_target(&self) -> usize {
        self.inner.idle_grace_target.load(Ordering::Relaxed)
    }

    fn monotonic_ms(&self) -> u64 {
        let millis = self.inner.started_at.elapsed().as_millis();
        u64::try_from(millis).unwrap_or(u64::MAX)
    }

    async fn trim_idle_to_target(&self) {
        let target = self.target_idle_connections();
        let mut idle = self.inner.idle.lock().await;
        let mut connections = Vec::new();
        while idle.len() > target {
            let connection = idle.pop_back().expect("idle length is greater than target");
            connections.push(connection);
        }
        drop(idle);
        for mut connection in connections {
            let _ = connection.close().await;
        }
    }

    async fn service_idle_connections(&self) {
        let mut idle = self.inner.idle.lock().await;
        let mut retained = VecDeque::new();
        while let Some(mut connection) = idle.pop_front() {
            match connection.service_idle_control_frames().await {
                Ok(()) => retained.push_back(connection),
                Err(_) => {
                    let _ = connection.close().await;
                }
            }
        }
        *idle = retained;
        drop(idle);
        self.trim_idle_to_target().await;
    }

    async fn stop_idle_maintenance_if_empty(&self) -> bool {
        let mut maintenance_running = self.inner.idle_maintenance_running.lock().await;
        let top_up_running = *self.inner.idle_top_up_running.lock().await;
        let idle = self.inner.idle.lock().await;
        if self.target_idle_connections() == 0 && idle.is_empty() && !top_up_running {
            *maintenance_running = false;
            true
        } else {
            false
        }
    }

    async fn connect_with_auth_rotation(
        &self,
        headers: &CodexHeaders,
        context: ConnectionContext,
    ) -> Result<ResponsesConnection, ResponsesApiError> {
        let mut refreshed = false;
        loop {
            self.reserve_new_connection().await?;
            let result = self.connect_once(headers, context).await;
            match result {
                Ok(connection) => return Ok(connection),
                Err(err) if err.is_unauthorized() && !refreshed => {
                    if self.inner.auth.refresh_after_unauthorized().await? {
                        refreshed = true;
                        continue;
                    }
                    return Err(err);
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn reserve_new_connection(&self) -> Result<(), ResponsesApiError> {
        loop {
            let wait =
                {
                    let mut window = self.inner.rate_window.lock().await;
                    let now = Instant::now();
                    while window.opened_at.front().is_some_and(|opened| {
                        now.duration_since(*opened) >= Duration::from_secs(60)
                    }) {
                        window.opened_at.pop_front();
                    }

                    if let Some(next_allowed_at) = window.next_allowed_at
                        && now < next_allowed_at
                    {
                        Some(next_allowed_at.duration_since(now))
                    } else {
                        let limit = self.inner.config.max_new_connections_per_minute.get() as usize;
                        if window.opened_at.len() >= limit {
                            return Err(ResponsesApiError::ConnectionRateLimited { limit });
                        }
                        window.opened_at.push_back(now);
                        window.next_allowed_at =
                            Some(now + self.inner.config.min_new_connection_interval);
                        None
                    }
                };
            match wait {
                Some(duration) => tokio::time::sleep(duration).await,
                None => return Ok(()),
            }
        }
    }

    async fn connect_once(
        &self,
        headers: &CodexHeaders,
        context: ConnectionContext,
    ) -> Result<ResponsesConnection, ResponsesApiError> {
        let url = self
            .inner
            .provider
            .websocket_endpoint_url(crate::ApiEndpoint::Responses);
        let mut request_headers = merge_request_headers(
            self.inner.provider.headers(),
            headers.to_header_map()?,
            &self.inner.default_headers,
        );
        self.inner.auth.add_headers(&mut request_headers)?;

        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|err| websocket_source_error("failed to build websocket request", err))?;
        request.headers_mut().extend(request_headers);

        let response = tokio::time::timeout(
            self.inner.config.connect_timeout,
            connect_async_tls_with_config(request, Some(WebSocketConfig::default()), false, None),
        )
        .await
        .map_err(|_| ResponsesApiError::Timeout("connect"))?;

        let (stream, response) = response.map_err(|err| map_ws_error(err, &url))?;
        if let Some(turn_state) = &headers.turn_state
            && let Some(value) = response
                .headers()
                .get(X_CODEX_TURN_STATE)
                .and_then(|value| value.to_str().ok())
        {
            turn_state.capture(value.to_string());
        }
        let server_reasoning_included = response.headers().contains_key(X_REASONING_INCLUDED);
        let models_etag = response
            .headers()
            .get(X_MODELS_ETAG)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let server_model = response
            .headers()
            .get(OPENAI_MODEL)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);

        Ok(ResponsesConnection::new(
            context,
            stream,
            self.inner.provider.stream_idle_timeout(),
            server_reasoning_included,
            models_etag,
            server_model,
        ))
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
