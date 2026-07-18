//! Minimal local HTTP bridge for the browser frontend.

use std::{
    convert::Infallible,
    env,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
};

use bytes::Bytes;
use harness_core::ipc::{IpcRequest, IpcResponse, IpcService};
use http::{Method, Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use thiserror::Error;
use tokio::{net::TcpListener, task::LocalSet};

type ResponseBody = Full<Bytes>;

#[derive(Debug, Error)]
enum WebError {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("request path not found")]
    NotFound,
    #[error("request method not allowed")]
    MethodNotAllowed,
    #[error("invalid request query: {0}")]
    InvalidQuery(String),
    #[error("invalid static asset path")]
    InvalidAssetPath,
    #[error(transparent)]
    Ipc(#[from] harness_core::ipc::IpcError),
    #[error(transparent)]
    Json(#[from] sonic_rs::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Deserialize)]
struct TranscriptQuery {
    before_seq: Option<u64>,
    max_lines: Option<usize>,
}

#[derive(Debug)]
struct WebConfig {
    addr: SocketAddr,
    session_root: PathBuf,
    static_root: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let local = LocalSet::new();
    local.run_until(run()).await
}

async fn run() -> anyhow::Result<()> {
    let config = parse_args(env::args().skip(1))?;
    let listener = TcpListener::bind(config.addr).await?;
    let service = IpcService::new(config.session_root);
    let static_root = config.static_root;

    eprintln!("harness-web listening on http://{}", config.addr);

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let service = service.clone();
        let static_root = static_root.clone();

        tokio::task::spawn_local(async move {
            let connection = hyper::server::conn::http1::Builder::new().serve_connection(
                io,
                hyper::service::service_fn(move |request| {
                    let service = service.clone();
                    let static_root = static_root.clone();
                    async move {
                        Ok::<_, Infallible>(handle_request(service, static_root, request).await)
                    }
                }),
            );

            if let Err(error) = connection.await {
                eprintln!("harness-web connection error: {error}");
            }
        });
    }
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<WebConfig, WebError> {
    let mut addr = SocketAddr::from(([127, 0, 0, 1], 5175));
    let mut session_root = if let Ok(root) = env::var("HARNESS_SESSION_ROOT") {
        PathBuf::from(root)
    } else if let Ok(root) = env::var("XDG_STATE_HOME")
        && !root.trim().is_empty()
    {
        PathBuf::from(root).join("new_harness")
    } else {
        let home = env::var("HOME").map_err(|_| {
            WebError::InvalidArgument(
                "set --session-root, HARNESS_SESSION_ROOT, or XDG_STATE_HOME when HOME is unavailable"
                    .to_string(),
            )
        })?;
        PathBuf::from(home).join(".local/state/new_harness")
    };
    let mut static_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");

    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => {
                let value = args.next().ok_or_else(|| {
                    WebError::InvalidArgument("--addr requires a value".to_string())
                })?;
                addr = value
                    .parse()
                    .map_err(|_| WebError::InvalidArgument(format!("invalid --addr `{value}`")))?;
            }
            "--session-root" => {
                let value = args.next().ok_or_else(|| {
                    WebError::InvalidArgument("--session-root requires a value".to_string())
                })?;
                session_root = PathBuf::from(value);
            }
            "--static-root" => {
                let value = args.next().ok_or_else(|| {
                    WebError::InvalidArgument("--static-root requires a value".to_string())
                })?;
                static_root = PathBuf::from(value);
            }
            _ => return Err(WebError::InvalidArgument(arg)),
        }
    }

    Ok(WebConfig {
        addr,
        session_root,
        static_root,
    })
}

async fn handle_request(
    service: IpcService,
    static_root: PathBuf,
    request: Request<Incoming>,
) -> Response<ResponseBody> {
    match route_request(&service, &static_root, request).await {
        Ok(response) => response,
        Err(error) => error_response(error),
    }
}

async fn route_request(
    service: &IpcService,
    static_root: &Path,
    request: Request<Incoming>,
) -> Result<Response<ResponseBody>, WebError> {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let path = uri.path();

    match (method, path) {
        (Method::GET, "/api/sessions") => json_response(service.handle(IpcRequest::ListSessions)?),
        (Method::GET, path)
            if path.starts_with("/api/sessions/") && path.ends_with("/transcript") =>
        {
            let session_id = path
                .trim_start_matches("/api/sessions/")
                .trim_end_matches("/transcript")
                .trim_end_matches('/');
            if session_id.is_empty() {
                return Err(WebError::NotFound);
            }

            let query = parse_transcript_query(uri.query().unwrap_or(""))?;
            // TODO: Replace string transcript lines with data-oriented transcript item DTOs.
            json_response(service.handle(IpcRequest::LoadTranscriptPage {
                session_id: percent_decode(session_id),
                before_seq: query.before_seq,
                max_lines: query.max_lines.unwrap_or(200),
            })?)
        }
        (Method::GET, _) => static_response(static_root, path).await,
        _ => Err(WebError::MethodNotAllowed),
    }
}

fn parse_transcript_query(query: &str) -> Result<TranscriptQuery, WebError> {
    let mut parsed = TranscriptQuery {
        before_seq: None,
        max_lines: None,
    };

    if query.is_empty() {
        return Ok(parsed);
    }

    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(WebError::InvalidQuery(pair.to_string()));
        };
        match key {
            "before_seq" if !value.is_empty() => {
                parsed.before_seq = Some(value.parse().map_err(|_| {
                    WebError::InvalidQuery(format!("invalid before_seq `{value}`"))
                })?);
            }
            "max_lines" if !value.is_empty() => {
                parsed.max_lines =
                    Some(value.parse().map_err(|_| {
                        WebError::InvalidQuery(format!("invalid max_lines `{value}`"))
                    })?);
            }
            "before_seq" | "max_lines" => {}
            _ => return Err(WebError::InvalidQuery(key.to_string())),
        }
    }

    Ok(parsed)
}

async fn static_response(
    static_root: &Path,
    request_path: &str,
) -> Result<Response<ResponseBody>, WebError> {
    let asset_path = static_asset_path(static_root, request_path)?;
    let path = if tokio::fs::try_exists(&asset_path).await? {
        asset_path
    } else {
        static_root.join("index.html")
    };

    let body = tokio::fs::read(&path).await?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, content_type(&path))
        .body(Full::new(Bytes::from(body)))
        .expect("valid static response"))
}

fn static_asset_path(static_root: &Path, request_path: &str) -> Result<PathBuf, WebError> {
    let mut path = static_root.to_path_buf();
    for component in Path::new(request_path.trim_start_matches('/')).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(WebError::InvalidAssetPath);
            }
        }
    }

    if request_path.ends_with('/') || path == static_root {
        path.push("index.html");
    }

    Ok(path)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("css") => "text/css; charset=utf-8",
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn json_response(value: IpcResponse) -> Result<Response<ResponseBody>, WebError> {
    let body = sonic_rs::to_vec(&value)?;
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("valid JSON response"))
}

fn error_response(error: WebError) -> Response<ResponseBody> {
    let status = match error {
        WebError::NotFound => StatusCode::NOT_FOUND,
        WebError::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
        WebError::InvalidArgument(_) | WebError::InvalidQuery(_) | WebError::InvalidAssetPath => {
            StatusCode::BAD_REQUEST
        }
        WebError::Ipc(_) | WebError::Json(_) | WebError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };

    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(error.to_string())))
        .expect("valid error response")
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hi = hex_value(bytes[index + 1]);
            let lo = hex_value(bytes[index + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                decoded.push((hi << 4) | lo);
                index += 3;
                continue;
            }
        }

        decoded.push(bytes[index]);
        index += 1;
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
