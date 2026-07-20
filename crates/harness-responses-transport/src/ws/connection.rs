//! Single Responses WebSocket connection state machine.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use harness_responses_api::{
    ResponsesApiError, ResponsesStreamEvent, is_response_terminal_event,
    map_wrapped_websocket_error, protocol_error, protocol_source_error, response_completed_id,
    websocket_error, websocket_source_error,
};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

const IDLE_WEBSOCKET_CONTROL_POLL_TIMEOUT: Duration = Duration::from_millis(1);
const IDLE_WEBSOCKET_CONTROL_SEND_TIMEOUT: Duration = Duration::from_secs(1);

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Pool partition key for a live Responses WebSocket connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectionContext;

impl ConnectionContext {
    /// Build the pool partition key for one request.
    pub(crate) fn from_headers(_headers: &harness_responses_api::CodexHeaders) -> Self {
        // The pool itself is already scoped to one provider, auth source, and
        // default Codex identity header set. Request-specific Codex headers do
        // not partition idle sockets: current per-request metadata is stamped
        // into each WebSocket request frame, and the live backend accepts a
        // later request over an existing socket even when the next logical
        // session/thread/window metadata differs.
        Self
    }
}

/// One open Responses WebSocket connection.
#[derive(Debug)]
pub(crate) struct ResponsesConnection {
    pub(crate) context: ConnectionContext,
    stream: WsStream,
    idle_timeout: Duration,
    server_reasoning_included: bool,
    models_etag: Option<String>,
    server_model: Option<String>,
    pub(crate) closed: bool,
}

impl ResponsesConnection {
    /// Construct a live connection from a completed WebSocket handshake.
    pub(crate) fn new(
        context: ConnectionContext,
        stream: WsStream,
        idle_timeout: Duration,
        server_reasoning_included: bool,
        models_etag: Option<String>,
        server_model: Option<String>,
    ) -> Self {
        Self {
            context,
            stream,
            idle_timeout,
            server_reasoning_included,
            models_etag,
            server_model,
            closed: false,
        }
    }

    /// Send one request and stream frames until a terminal response event.
    ///
    /// The connection remains reusable after request-level HTTP errors wrapped
    /// in WebSocket frames. Transport failures before any response frame are
    /// promoted to retryable errors so the pool can retry exactly once on a new
    /// socket.
    pub(crate) async fn stream_request<F, Fut>(
        &mut self,
        body: &sonic_rs::Value,
        connection_reused: bool,
        on_event: &mut F,
    ) -> Result<(), ResponsesApiError>
    where
        F: FnMut(ResponsesStreamEvent) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        if let Some(model) = self.server_model.clone() {
            on_event(ResponsesStreamEvent::ServerModel(model)).await;
        }
        if let Some(etag) = self.models_etag.clone() {
            on_event(ResponsesStreamEvent::ModelsEtag(etag)).await;
        }
        if self.server_reasoning_included {
            on_event(ResponsesStreamEvent::ServerReasoningIncluded(true)).await;
        }

        self.send_request(body, connection_reused)
            .await
            .map_err(|error| {
                if error.can_retry_before_response_frame() {
                    error.into_retryable_websocket_error()
                } else {
                    error
                }
            })?;
        let mut response_frame_seen = false;
        loop {
            let message = match self.next_message().await {
                Ok(message) => message,
                Err(error) if !response_frame_seen && error.can_retry_before_response_frame() => {
                    return Err(error.into_retryable_websocket_error());
                }
                Err(_) => return Err(ResponsesApiError::StreamInterrupted),
            };
            match message {
                Message::Text(text) => {
                    let text = text.to_string();
                    if let Some(error) = map_wrapped_websocket_error(&text) {
                        if !error.can_keep_websocket_open_after_stream_error() {
                            self.closed = true;
                        }
                        return Err(error);
                    }
                    let frame: sonic_rs::Value = sonic_rs::from_str(&text).map_err(|err| {
                        protocol_source_error("failed to parse websocket event", err)
                    })?;
                    let completed = is_response_terminal_event(&frame);
                    let response_id = completed.then(|| response_completed_id(&frame)).flatten();
                    response_frame_seen = true;
                    on_event(ResponsesStreamEvent::Frame(frame)).await;
                    if completed {
                        on_event(ResponsesStreamEvent::Completed { response_id }).await;
                        return Ok(());
                    }
                }
                Message::Binary(_) => {
                    self.closed = true;
                    return Err(protocol_error("unexpected binary websocket event"));
                }
                Message::Close(close) => {
                    self.closed = true;
                    let error = websocket_error(match close {
                        Some(close) => format!(
                            "websocket closed by server before response.completed: code={} reason={}",
                            u16::from(close.code),
                            close.reason
                        ),
                        None => "websocket closed by server before response.completed".to_string(),
                    });
                    if !response_frame_seen {
                        return Err(error.into_retryable_websocket_error());
                    }
                    return Err(ResponsesApiError::StreamInterrupted);
                }
                Message::Frame(_) | Message::Pong(_) => {}
                Message::Ping(payload) => {
                    if let Err(err) = self.stream.send(Message::Pong(payload)).await {
                        let error = websocket_source_error("failed to send websocket pong", err);
                        if !response_frame_seen {
                            return Err(error.into_retryable_websocket_error());
                        }
                        return Err(ResponsesApiError::StreamInterrupted);
                    }
                }
            }
        }
    }

    async fn send_request(
        &mut self,
        body: &sonic_rs::Value,
        _connection_reused: bool,
    ) -> Result<(), ResponsesApiError> {
        let request_text = sonic_rs::to_string(body)
            .map_err(|err| protocol_source_error("failed to encode websocket request", err))?;
        tokio::time::timeout(
            self.idle_timeout,
            self.stream.send(Message::Text(request_text.into())),
        )
        .await
        .map_err(|_| ResponsesApiError::Timeout("send"))?
        .map_err(|err| {
            self.closed = true;
            websocket_source_error("failed to send websocket request", err)
        })
    }

    async fn next_message(&mut self) -> Result<Message, ResponsesApiError> {
        tokio::time::timeout(self.idle_timeout, self.stream.next())
            .await
            .map_err(|_| ResponsesApiError::Timeout("receive"))?
            .ok_or_else(|| {
                self.closed = true;
                websocket_error("stream closed before response.completed")
            })?
            .map_err(|err| {
                self.closed = true;
                websocket_source_error("websocket receive failed", err)
            })
    }

    /// Poll and service idle control frames without consuming request frames.
    ///
    /// This deterministic pool uses a short non-blocking timeout. Ping frames
    /// receive Pong replies, Pong and raw protocol frames are ignored, and any
    /// data frame while idle closes the connection because it cannot be assigned
    /// to a request.
    pub(crate) async fn service_idle_control_frames(&mut self) -> Result<(), ResponsesApiError> {
        loop {
            let Some(message) = (match tokio::time::timeout(
                IDLE_WEBSOCKET_CONTROL_POLL_TIMEOUT,
                self.stream.next(),
            )
            .await
            {
                Ok(message) => message,
                Err(_) => return Ok(()),
            }) else {
                self.closed = true;
                return Err(websocket_error("websocket closed while idle"));
            };

            match message.map_err(|err| {
                self.closed = true;
                websocket_source_error("websocket idle receive failed", err)
            })? {
                Message::Ping(payload) => {
                    tokio::time::timeout(
                        IDLE_WEBSOCKET_CONTROL_SEND_TIMEOUT,
                        self.stream.send(Message::Pong(payload)),
                    )
                    .await
                    .map_err(|_| ResponsesApiError::Timeout("idle pong"))?
                    .map_err(|err| {
                        self.closed = true;
                        websocket_source_error("failed to send idle websocket pong", err)
                    })?;
                }
                Message::Pong(_) | Message::Frame(_) => {}
                Message::Close(close) => {
                    self.closed = true;
                    return Err(websocket_error(match close {
                        Some(close) => format!(
                            "websocket closed by server while idle: code={} reason={}",
                            u16::from(close.code),
                            close.reason
                        ),
                        None => "websocket closed by server while idle".to_string(),
                    }));
                }
                Message::Text(_) => {
                    self.closed = true;
                    return Err(protocol_error("unexpected text websocket event while idle"));
                }
                Message::Binary(_) => {
                    self.closed = true;
                    return Err(protocol_error(
                        "unexpected binary websocket event while idle",
                    ));
                }
            }
        }
    }

    /// Close this connection and mark it unavailable for pool reuse.
    pub(crate) async fn close(&mut self) -> Result<(), ResponsesApiError> {
        self.closed = true;
        self.stream
            .close(None)
            .await
            .map_err(|err| websocket_source_error("failed to close websocket", err))
    }
}
