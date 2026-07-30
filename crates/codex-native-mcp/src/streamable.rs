//! Stateless MCP Streamable HTTP request handling.
//!
//! Each POST contains one JSON-RPC request or notification. Requests receive a
//! JSON-RPC response; notifications receive HTTP 202 with no response body.

use std::sync::Arc;

use harness_tool_api::ToolRegistry;
use sonic_rs::{JsonValueTrait, json};
use tracing::{debug, warn};

use crate::{
    ExecutorMap, dispatch,
    jsonrpc::{self, Id, Response},
    oauth::Store as OauthStore,
};

pub struct Handler {
    pub registry: Arc<ToolRegistry>,
    pub executors: Arc<ExecutorMap>,
    pub oauth: Arc<OauthStore>,
}

impl Handler {
    pub async fn handle_request(&self, body: bytes::Bytes) -> Option<Response> {
        let raw: sonic_rs::Value = match sonic_rs::from_slice(&body) {
            Ok(value) => value,
            Err(error) => {
                warn!(%error, "failed to parse JSON-RPC message");
                return Some(jsonrpc::err(
                    Id::Null,
                    jsonrpc::code::PARSE_ERROR,
                    "parse error",
                ));
            }
        };

        if raw.get("id").is_none() {
            let notification: jsonrpc::Notification = match sonic_rs::from_slice(&body) {
                Ok(notification) => notification,
                Err(error) => {
                    warn!(%error, "invalid JSON-RPC notification");
                    return Some(jsonrpc::err(
                        Id::Null,
                        jsonrpc::code::INVALID_REQUEST,
                        "invalid request",
                    ));
                }
            };
            if notification.jsonrpc != "2.0" {
                return Some(jsonrpc::err(
                    Id::Null,
                    jsonrpc::code::INVALID_REQUEST,
                    "`jsonrpc` must be `2.0`",
                ));
            }
            debug!(method = %notification.method, "notification received");
            return None;
        }

        let request: jsonrpc::Request = match sonic_rs::from_slice(&body) {
            Ok(request) => request,
            Err(error) => {
                warn!(%error, "invalid JSON-RPC request");
                return Some(jsonrpc::err(
                    Id::Null,
                    jsonrpc::code::INVALID_REQUEST,
                    "invalid request",
                ));
            }
        };
        if request.jsonrpc != "2.0" {
            return Some(jsonrpc::err(
                request.id,
                jsonrpc::code::INVALID_REQUEST,
                "`jsonrpc` must be `2.0`",
            ));
        }

        Some(self.dispatch(request).await)
    }

    async fn dispatch(&self, request: jsonrpc::Request) -> Response {
        let id = request.id.clone();
        match request.method.as_str() {
            "initialize" => self.initialize(request),
            "ping" => jsonrpc::ok(id, json!({})),
            "tools/list" => jsonrpc::ok(id, dispatch::list(&self.registry)),
            "tools/call" => match dispatch::call(
                &self.registry,
                &self.executors,
                request.params.unwrap_or_default(),
            )
            .await
            {
                Ok(result) => jsonrpc::ok(id, result),
                Err((code, message)) => jsonrpc::err(id, code, message),
            },
            other => {
                debug!(method = other, "method not found");
                jsonrpc::err(
                    id,
                    jsonrpc::code::METHOD_NOT_FOUND,
                    format!("method not found: {other}"),
                )
            }
        }
    }

    fn initialize(&self, request: jsonrpc::Request) -> Response {
        const PROTOCOL_VERSION: &str = "2025-06-18";
        jsonrpc::ok(
            request.id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "serverInfo": {
                    "name": "codex-native-mcp",
                    "title": "Codex Native MCP",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "tools": { "listChanged": false },
                },
                "instructions": "Workspace-scoped tools: inspect files, edit files, run terminal commands.",
            }),
        )
    }
}
