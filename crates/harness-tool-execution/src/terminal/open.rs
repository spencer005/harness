use std::{future::Future, pin::Pin, sync::Arc};
use harness_tool_api::{InvalidToolName, ToolCapabilities, ToolExecutionRequest, ToolExecutor, ToolFailure, ToolResult, ToolSpec};
use crate::{inventory::ToolRegistration, WorkspaceRoot};
use super::{failure, manager, output, Manager, OPEN_NAME};

pub const DESCRIPTION: &str = "Start a persistent Bash command attached to a PTY. Use `command:`, with optional `workdir:`, `rows:`, and `cols:`.";
pub const GRAMMAR: &str = include_str!("terminal_open.lark");

pub struct OpenExecutor { manager: Manager, workspace: WorkspaceRoot }
impl OpenExecutor { pub fn new(workspace: WorkspaceRoot) -> Self { Self { manager: manager(&workspace), workspace } } }
impl ToolExecutor for OpenExecutor {
    fn execute(&self, request: ToolExecutionRequest) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>> { let manager = self.manager.clone(); let workspace = self.workspace.clone(); let input = request.input.as_str().to_owned(); Box::pin(async move { let result = tokio::task::spawn_blocking(move || manager.open(&workspace, &input)).await.map_err(|e| ToolFailure::Execution(e.to_string()))?; match result { Ok(text) => Ok(output(text, OPEN_NAME)), Err(error) => failure(error) } }) }
}
pub fn spec() -> Result<ToolSpec, InvalidToolName> { Ok(ToolSpec::new(OPEN_NAME)?.description(DESCRIPTION).lark(GRAMMAR).capabilities(ToolCapabilities { read_only: false, mutates_workspace: true, idempotent: false })) }
pub fn registration(workspace: WorkspaceRoot) -> Arc<dyn ToolExecutor> { Arc::new(OpenExecutor::new(workspace)) }
::inventory::submit! { ToolRegistration { spec, executor: registration } }
