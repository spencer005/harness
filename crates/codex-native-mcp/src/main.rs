//! Codex Native MCP server.
//!
//! Exposes the harness's workspace-scoped tools over MCP Streamable HTTP so a
//! ChatGPT custom connector can invoke them. Bind to loopback and proxy to a
//! public HTTPS URL with cloudflared; the connector sees one MCP endpoint.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use harness_tool_api::{ToolExecutor, ToolRegistry, ToolRegistryBuilder};
use harness_tool_execution::{WorkspaceRoot, edit_file, inspect, terminal};
use tracing_subscriber::EnvFilter;

mod dispatch;
mod jsonrpc;
mod oauth;
mod server;
mod streamable;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args = parse_args()?;
    let workspace = args.workspace.clone();
    let registry = build_registry()?;
    let executors = build_executors(workspace);

    let oauth = oauth::Store::open(args.password.as_deref(), Some(args.key_file.as_path()))?;
    let state = Arc::new(server::State {
        registry,
        executors,
        oauth,
    });

    let addr: SocketAddr = ([127, 0, 0, 1], args.port).into();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;

    eprintln!("codex-native-mcp listening on {addr}");
    eprintln!("  workspace:  {}", args.workspace.path().display());
    eprintln!("  key file:   {}", args.key_file.display());
    eprintln!("  auth:       {}", if args.password.is_some() { "password" } else { "DISABLED (no-auth)" });
    eprintln!("  tools:      edit_file, inspect, terminal_open, terminal_read, terminal_write");
    eprintln!();
    eprintln!("proxy to HTTPS via cloudflared:");
    eprintln!("  cloudflared tunnel --url http://{addr} --no-tls-verify");
    eprintln!();

    server::serve(listener, state).await
}

struct Args {
    workspace: WorkspaceRoot,
    port: u16,
    password: Option<String>,
    key_file: PathBuf,
}

fn default_key_file() -> PathBuf {
    if let Some(dir) = dirs_config_dir() {
        return dir.join("codex-native-mcp").join("signing.key");
    }
    PathBuf::from(".codex-native-mcp-signing.key")
}

fn dirs_config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }
}

fn parse_args() -> Result<Args> {
    let mut workspace: Option<PathBuf> = None;
    let mut port: u16 = 8472;
    let mut key_file: Option<PathBuf> = None;
    let mut no_auth = false;

    let raw = std::env::args().collect::<Vec<_>>();
    let mut iter = raw.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" | "-w" => {
                workspace = Some(PathBuf::from(
                    iter.next().context("--workspace requires a path")?,
                ));
            }
            "--port" | "-p" => {
                port = iter
                    .next()
                    .context("--port requires a value")?
                    .parse()
                    .context("--port must be a number")?;
            }
            "--password" => {
                bail!(
                    "--password is not accepted because command-line values can be exposed; set \
                     CODEX_NATIVE_MCP_PASSWORD in the server process environment instead"
                );
            }
            "--key-file" => {
                key_file = Some(PathBuf::from(
                    iter.next().context("--key-file requires a path")?,
                ));
            }
            "--no-auth" => {
                no_auth = true;
            }
            "--help" | "-h" => {
                eprintln!("codex-native-mcp — expose harness tools as a ChatGPT MCP connector\n");
                eprintln!("usage: codex-native-mcp --workspace <abs-path> [options]\n");
                eprintln!("required:");
                eprintln!("  --workspace <path>   Absolute path to the workspace tools operate on");
                eprintln!();
                eprintln!("options:");
                eprintln!("  --port <n>           Loopback port (default: 8472)");
                eprintln!("  --no-auth           Disable auth (NOT recommended for public URLs)");
                eprintln!("  --key-file <path>   HMAC signing key file (default: platform config dir)");
                eprintln!("  --help              Show this message");
                eprintln!();
                eprintln!("environment:");
                eprintln!("  CODEX_NATIVE_MCP_PASSWORD");
                eprintln!("                       Password for the OAuth consent page (recommended)");
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other}"),
        }
    }

    let workspace_path = match workspace {
        Some(p) => p,
        None => {
            eprintln!("error: --workspace is required (absolute path to your project directory)");
            eprintln!("  example: codex-native-mcp --workspace /home/me/myproject");
            bail!("--workspace is required");
        }
    };

    let workspace = WorkspaceRoot::open(workspace_path)
        .map_err(|error| anyhow!("invalid workspace: {error}"))?;

    let key_file = key_file.unwrap_or_else(default_key_file);

    let password = if no_auth {
        None
    } else {
        match std::env::var("CODEX_NATIVE_MCP_PASSWORD") {
            Ok(password) if password.is_empty() => {
                eprintln!("error: CODEX_NATIVE_MCP_PASSWORD is empty");
                eprintln!("  Set it to a non-empty OAuth consent-page password.");
                eprintln!("  Alternatively, use --no-auth only for loopback testing.");
                bail!("CODEX_NATIVE_MCP_PASSWORD must not be empty");
            }
            Ok(password) => Some(password),
            Err(std::env::VarError::NotPresent) => {
                eprintln!(
                    "error: set CODEX_NATIVE_MCP_PASSWORD or specify --no-auth"
                );
                eprintln!();
                eprintln!("  CODEX_NATIVE_MCP_PASSWORD");
                eprintln!("      Sets the OAuth consent-page password.");
                eprintln!("      Anyone connecting via ChatGPT must type this password.");
                eprintln!("      STRONGLY RECOMMENDED — your tools can run arbitrary commands.");
                eprintln!();
                eprintln!("  --no-auth");
                eprintln!("      Disables auth entirely. Only safe for loopback testing.");
                eprintln!("      NEVER use --no-auth with cloudflared or a public URL.");
                bail!("authentication is not configured");
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!(
                    "CODEX_NATIVE_MCP_PASSWORD contains non-Unicode data; set it to a non-empty \
                     Unicode password or use --no-auth only for loopback testing"
                );
            }
        }
    };

    Ok(Args { workspace, port, password, key_file })
}

fn build_registry() -> Result<ToolRegistry> {
    let builder = ToolRegistryBuilder::default()
        .tool(edit_file::spec()?)
        .tool(inspect::spec()?)
        .tool(terminal::open_spec()?)
        .tool(terminal::read_spec()?)
        .tool(terminal::write_spec()?);
    builder.build().map_err(|error| anyhow!("duplicate tool: {error}"))
}

fn build_executors(workspace: WorkspaceRoot) -> ExecutorMap {
    let mut map = ExecutorMap::new();
    insert(&mut map, edit_file::NAME, edit_file::Executor::new(workspace.clone()));
    insert(&mut map, inspect::NAME, inspect::Executor::new(workspace.clone()));
    insert(&mut map, terminal::OPEN_NAME, terminal::OpenExecutor::new(workspace.clone()));
    insert(&mut map, terminal::READ_NAME, terminal::ReadExecutor::new(workspace.clone()));
    insert(&mut map, terminal::WRITE_NAME, terminal::WriteExecutor::new(workspace));
    map
}

fn insert<E: ToolExecutor + 'static>(map: &mut ExecutorMap, name: &str, executor: E) {
    map.insert(name.to_owned(), Arc::new(executor));
}

pub type ExecutorMap = BTreeMap<String, Arc<dyn ToolExecutor>>;
