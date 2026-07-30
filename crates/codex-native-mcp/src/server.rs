//! HTTP server: loopback listener, routing, body framing.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use sonic_rs::json;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

use harness_tool_api::ToolRegistry;

use crate::ExecutorMap;
use crate::oauth::Store as OauthStore;
use crate::streamable::Handler;

pub struct State {
    pub registry: ToolRegistry,
    pub executors: ExecutorMap,
    pub oauth: OauthStore,
}

pub async fn serve(listener: TcpListener, state: Arc<State>) -> anyhow::Result<()> {
    let handler = Arc::new(Handler {
        registry: Arc::new(state.registry.clone()),
        executors: Arc::new(state.executors.clone()),
        oauth: Arc::new(state.oauth.clone()),
    });

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (conn, peer) = match accept {
                    Ok(conn) => conn,
                    Err(error) => { error!(%error, "accept failed"); continue; }
                };
                debug!(%peer, "connection accepted");
                let handler = handler.clone();
                let io = TokioIo::new(conn);
                tokio::spawn(async move {
                    if let Err(error) = http1::Builder::new()
                        .serve_connection(io, service_fn(move |req| {
                            let handler = handler.clone();
                            async move { route(req, handler).await }
                        }))
                        .await
                    {
                        debug!(%error, "connection closed");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received, draining in-flight requests");
                break;
            }
        }
    }

    Ok(())
}

async fn route(
    mut req: Request<Incoming>,
    handler: Arc<Handler>,
) -> Result<Response<BoxBody<Bytes, std::convert::Infallible>>, std::convert::Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let auth = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let origin = req
        .headers()
        .get(hyper::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // CORS preflight: ChatGPT's connector page is on chatgpt.com and makes
    // cross-origin requests to this server. Without CORS the browser blocks
    // the fetch before it even reaches our handler.
    if method == hyper::Method::OPTIONS {
        tracing::info!(%path, "CORS preflight");
        return Ok(cors_response(&origin));
    }

    let response: Result<Response<BoxBody<Bytes, std::convert::Infallible>>, std::convert::Infallible> = match (method.as_ref(), path.as_str()) {
        ("GET", "/healthz") => Ok(text_response(StatusCode::OK, "ok")),
        ("GET", "/.well-known/openid-configuration") => {
            let base = base_url(&req);
            Ok(json_response(StatusCode::OK, &json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/oauth/authorize"),
                "token_endpoint": format!("{base}/oauth/token"),
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code"],
                "subject_types_supported": ["public"],
                "id_token_signing_alg_values_supported": ["none"],
                "code_challenge_methods_supported": ["S256"],
            })))
        }
        ("GET", "/") => {
            // ChatGPT opens a GET connection to the MCP endpoint for SSE
            // streaming. We don't implement SSE streaming but must accept the
            // connection so ChatGPT doesn't see a 404.
            let auth_header = auth.as_deref();
            match handler.oauth.authorize(auth_header) {
                crate::oauth::AuthResult::Unauthorized => {
                    let base = base_url(&req);
                    let resource_url = format!("{base}/.well-known/oauth-protected-resource");
                    let mut resp = json_response(StatusCode::UNAUTHORIZED, &json!({ "error": "unauthorized" }));
                    resp.headers_mut().insert(
                        hyper::header::WWW_AUTHENTICATE,
                        format!("Bearer resource_metadata=\"{resource_url}\"").parse().expect("valid header"),
                    );
                    Ok(resp)
                }
                crate::oauth::AuthResult::Disabled | crate::oauth::AuthResult::Authorized => {
                    // Return an empty SSE stream to keep the connection alive.
                    Ok(sse_response())
                }
            }
        }
        ("GET", "/.well-known/oauth-protected-resource") => {
            let base = base_url(&req);
            let resource_url = format!("{base}/mcp");
            Ok(json_response(StatusCode::OK, &crate::oauth::protected_resource_metadata(&resource_url)))
        }
        ("GET", "/.well-known/oauth-authorization-server") => {
            let base = base_url(&req);
            Ok(json_response(StatusCode::OK, &crate::oauth::authorization_server_metadata(&base)))
        }
        ("POST", "/oauth/register") => {
            let body = match read_body(&mut req).await {
                Ok(bytes) => bytes,
                Err(error) => return Ok(body_error_response(error)),
            };
            let value = match parse_json_or_form(&body, req.headers()) {
                Ok(value) => value,
                Err(error) => return Ok(text_response(StatusCode::BAD_REQUEST, &error)),
            };
            match handler.oauth.register_client(&value) {
                Ok(registration) => Ok(json_response(StatusCode::CREATED, &registration)),
                Err(message) => Ok(json_response(
                    StatusCode::BAD_REQUEST,
                    &json!({ "error": "invalid_client_metadata", "error_description": message }),
                )),
            }
        }
        ("GET", "/oauth/authorize") => {
            let value = parse_form(req.uri().query().unwrap_or("").as_bytes());
            match handler.oauth.begin_authorization(&value) {
                Ok(page) => Ok(html_response(StatusCode::OK, &page)),
                Err(message) => Ok(text_response(StatusCode::BAD_REQUEST, &message)),
            }
        }
        ("POST", "/oauth/authorize") => {
            let body = match read_body(&mut req).await {
                Ok(bytes) => bytes,
                Err(error) => return Ok(body_error_response(error)),
            };
            let value = parse_form(&body);
            match handler.oauth.complete_authorization(&value) {
                Ok(crate::oauth::AuthorizationOutcome::Page(page)) => {
                    Ok(html_response(StatusCode::UNAUTHORIZED, &page))
                }
                Ok(crate::oauth::AuthorizationOutcome::Redirect(location)) => {
                    tracing::info!(target: "codex_native_mcp::auth", "connector authorized");
                    Ok(redirect_response(&location))
                }
                Err(message) => Ok(text_response(StatusCode::BAD_REQUEST, &message)),
            }
        }
        ("POST", "/oauth/token") => {
            let issuer = base_url(&req);
            let body = match read_body(&mut req).await {
                Ok(bytes) => bytes,
                Err(error) => return Ok(body_error_response(error)),
            };
            let value = match parse_json_or_form(&body, req.headers()) {
                Ok(value) => value,
                Err(error) => return Ok(text_response(StatusCode::BAD_REQUEST, &error)),
            };
            match handler.oauth.exchange_code(&value, &issuer) {
                Ok(token_response) => {
                    tracing::info!(target: "codex_native_mcp::auth", "token issued");
                    Ok(json_response(StatusCode::OK, &token_response))
                }
                Err(message) => {
                    tracing::warn!(target: "codex_native_mcp::auth", %message, "token request failed");
                    Ok(json_response(
                        StatusCode::BAD_REQUEST,
                        &json!({ "error": "invalid_grant", "error_description": message }),
                    ))
                }
            }
        }
        ("POST", "/mcp") | ("POST", "/") => {
            let auth_header = auth.as_deref();
            // Check auth before reading the body: an unauthorized request gets
            // HTTP 401 + WWW-Authenticate so ChatGPT starts the OAuth flow.
            match handler.oauth.authorize(auth_header) {
                crate::oauth::AuthResult::Unauthorized => {
                    let base = base_url(&req);
                    let resource_url = format!("{base}/.well-known/oauth-protected-resource");
                    let mut resp = json_response(
                        StatusCode::UNAUTHORIZED,
                        &json!({ "error": "unauthorized" }),
                    );
                    resp.headers_mut().insert(
                        hyper::header::WWW_AUTHENTICATE,
                        format!("Bearer resource_metadata=\"{resource_url}\"")
                            .parse()
                            .expect("valid header value"),
                    );
                    Ok(resp)
                }
                crate::oauth::AuthResult::Disabled | crate::oauth::AuthResult::Authorized => {
                    let body = match read_body(&mut req).await {
                        Ok(bytes) => bytes,
                        Err(error) => return Ok(body_error_response(error)),
                    };
                    match handler.handle_request(body).await {
                        Some(rpc) => {
                            let value = sonic_rs::to_value(&rpc)
                                .unwrap_or_else(|_| json!({ "jsonrpc": "2.0", "id": null, "error": {
                                    "code": -32603,
                                    "message": "failed to serialize JSON-RPC response"
                                }}));
                            Ok(json_response(StatusCode::OK, &value))
                        }
                        None => Ok(empty_response(StatusCode::ACCEPTED)),
                    }
                }
            }
        }
        _ => Ok(text_response(StatusCode::NOT_FOUND, "not found")),
    };

    let mut resp = match response {
        Ok(r) => r,
        Err(never) => match never {},
    };
    add_cors_headers(&mut resp, &origin);
    Ok(resp)
}

/// Derives the public base URL from the request. Behind cloudflared the Host
/// header carries the public domain and the scheme is HTTPS, so the OAuth
/// metadata advertises endpoints ChatGPT can actually reach.
fn base_url(req: &Request<Incoming>) -> String {
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().host())
        .unwrap_or("127.0.0.1");
    // Loopback (no proxy): use http + the port from the Host header (which
    // includes the port when curl/browser targets a non-standard port).
    if host.starts_with("127.0.0.1") || host.starts_with("localhost") {
        if host.contains(':') {
            return format!("http://{host}");
        }
        return format!("http://{host}:8472");
    }
    // Behind cloudflared: Host is the public domain, scheme is HTTPS, port 443.
    // The Host header may or may not include a port; strip it if present since
    // cloudflared terminates TLS on 443.
    let host = host.split(':').next().unwrap_or(host);
    format!("https://{host}")
}

/// Parses JSON or an OAuth form body without repairing malformed input.
fn parse_json_or_form(
    body: &[u8],
    headers: &hyper::HeaderMap,
) -> Result<sonic_rs::Value, String> {
    let content_type = headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if content_type.contains("application/json") {
        return sonic_rs::from_slice(body).map_err(|error| format!("invalid JSON body: {error}"));
    }
    Ok(parse_form(body))
}

fn parse_form(body: &[u8]) -> sonic_rs::Value {
    let object = url::form_urlencoded::parse(body)
        .into_owned()
        .map(|(key, value)| (key, serde_json::Value::String(value)))
        .collect::<serde_json::Map<_, _>>();
    sonic_rs::to_value(&serde_json::Value::Object(object))
        .expect("form object serializes")
}

const MAX_REQUEST_BYTES: usize = 1024 * 1024;

enum BodyReadError {
    TooLarge,
    Transport(String),
}

async fn read_body(req: &mut Request<Incoming>) -> Result<Bytes, BodyReadError> {
    let mut body = Vec::new();
    while let Some(frame) = req.body_mut().frame().await {
        let frame = frame.map_err(|error| BodyReadError::Transport(error.to_string()))?;
        if let Ok(data) = frame.into_data() {
            if body.len().saturating_add(data.len()) > MAX_REQUEST_BYTES {
                return Err(BodyReadError::TooLarge);
            }
            body.extend_from_slice(&data);
        }
    }
    Ok(Bytes::from(body))
}

fn body_error_response(
    error: BodyReadError,
) -> Response<BoxBody<Bytes, std::convert::Infallible>> {
    match error {
        BodyReadError::TooLarge => text_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body exceeds the 1 MiB limit",
        ),
        BodyReadError::Transport(message) => text_response(
            StatusCode::BAD_REQUEST,
            &format!("failed to read request body: {message}"),
        ),
    }
}

fn redirect_response(location: &str) -> Response<BoxBody<Bytes, std::convert::Infallible>> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header("location", location)
        .body(boxed(Vec::new()))
        .expect("valid response")
}

fn html_response(status: StatusCode, body: &str) -> Response<BoxBody<Bytes, std::convert::Infallible>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/html; charset=utf-8")
        .body(boxed(body.as_bytes().to_owned()))
        .expect("valid response")
}

fn sse_response() -> Response<BoxBody<Bytes, std::convert::Infallible>> {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(boxed(Vec::new()))
        .expect("valid response")
}

fn cors_response(origin: &Option<String>) -> Response<BoxBody<Bytes, std::convert::Infallible>> {
    let mut resp = Response::builder()
        .status(StatusCode::OK)
        .body(boxed(Vec::new()))
        .expect("valid response");
    add_cors_headers(&mut resp, origin);
    resp
}

fn add_cors_headers(resp: &mut Response<BoxBody<Bytes, std::convert::Infallible>>, origin: &Option<String>) {
    let headers = resp.headers_mut();
    headers.insert(
        "access-control-allow-origin",
        origin.as_deref().unwrap_or("*").parse().expect("valid header"),
    );
    headers.insert(
        "access-control-allow-methods",
        "GET, POST, OPTIONS".parse().expect("valid header"),
    );
    headers.insert(
        "access-control-allow-headers",
        "Authorization, Content-Type".parse().expect("valid header"),
    );
    headers.insert(
        "access-control-max-age",
        "86400".parse().expect("valid header"),
    );
}

fn empty_response(
    status: StatusCode,
) -> Response<BoxBody<Bytes, std::convert::Infallible>> {
    Response::builder()
        .status(status)
        .body(boxed(Vec::new()))
        .expect("valid response")
}

fn text_response(status: StatusCode, body: &str) -> Response<BoxBody<Bytes, std::convert::Infallible>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(boxed(body.as_bytes().to_owned()))
        .expect("valid response")
}

fn json_response(status: StatusCode, value: &sonic_rs::Value) -> Response<BoxBody<Bytes, std::convert::Infallible>> {
    let body = sonic_rs::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(boxed(body))
        .expect("valid response")
}

fn boxed(bytes: Vec<u8>) -> BoxBody<Bytes, std::convert::Infallible> {
    use http_body_util::BodyExt;
    http_body_util::Full::new(Bytes::from(bytes)).map_err(|never| match never {}).boxed()
}
