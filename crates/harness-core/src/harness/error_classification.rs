//! Pure response-error classifiers used by retry and compaction routing.

/// Return whether an error message indicates a context-window failure.
pub(super) fn is_context_window_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    (lower.contains("context") || lower.contains("token") || lower.contains("too large"))
        && (lower.contains("length")
            || lower.contains("window")
            || lower.contains("maximum")
            || lower.contains("exceed")
            || lower.contains("too large")
            || lower.contains("too many"))
}

/// Return whether a transient response error can be retried.
pub(super) fn is_retryable_response_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.starts_with("retryable websocket error:")
        || lower.starts_with("http transport error:")
        || lower.starts_with("websocket timeout during ")
        || lower.contains("websocket closed by server before response.completed")
        || lower.contains("stream closed before response.completed")
        || lower.contains("keepalive ping timeout")
        || lower.contains("error reading a body from connection")
        || lower.contains("http error 502 bad gateway")
        || lower.contains("http error 503 service unavailable")
        || lower.contains("client error")
}

/// Return whether a response error requires rotating pooled WebSocket state before retrying.
pub(super) fn is_reconnect_required_response_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.starts_with("retryable websocket error:")
        || lower.contains("responses websocket connection limit reached")
        || lower.contains("websocket_connection_limit_reached")
        || lower.contains("cf ray:")
        || lower.contains("error reading a body from connection")
        || lower.contains("http error 502 bad gateway")
        || lower.contains("http error 503 service unavailable")
        || lower.contains("client error")
}
#[cfg(test)]
mod tests {
    use super::{is_reconnect_required_response_error, is_retryable_response_error};

    #[test]
    fn ccapi_gateway_and_connect_failures_are_retryable_after_reconnect() {
        for message in [
            "HTTP error 502 Bad Gateway: <html><title>502 Bad Gateway</title></html>",
            "HTTP error 503 Service Unavailable: {\"error\":{\"message\":\"Service temporarily unavailable\",\"type\":\"api_error\",\"param\":\"\",\"code\":null}}",
            "client error (Connect)",
        ] {
            assert!(is_retryable_response_error(message));
            assert!(is_reconnect_required_response_error(message));
        }
    }
}
