use std::{future::Future, pin::Pin, sync::Arc};
use harness_tool_api::{InvalidToolName, ToolCapabilities, ToolExecutionRequest, ToolExecutor, ToolFailure, ToolResult, ToolSpec};
use crate::{inventory::ToolRegistration, WorkspaceRoot};
use super::{failure, manager, output, Manager, WRITE_NAME};

pub const DESCRIPTION: &str = "Write interactive input to a running terminal. Use `terminal:` and `input:`.";
pub const GRAMMAR: &str = include_str!("terminal_write.lark");
pub struct WriteExecutor { manager: Manager }
impl WriteExecutor { pub fn new(workspace: WorkspaceRoot) -> Self { Self { manager: manager(&workspace) } } }
impl ToolExecutor for WriteExecutor { fn execute(&self, request: ToolExecutionRequest) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>> { let manager = self.manager.clone(); let input = request.input.as_str().to_owned(); Box::pin(async move { let result = tokio::task::spawn_blocking(move || manager.write(&input)).await.map_err(|e| ToolFailure::Execution(e.to_string()))?; match result { Ok(text) => Ok(output(text, WRITE_NAME)), Err(error) => failure(error) } }) } }
pub fn spec() -> Result<ToolSpec, InvalidToolName> { Ok(ToolSpec::new(WRITE_NAME)?.description(DESCRIPTION).lark(GRAMMAR).capabilities(ToolCapabilities { read_only: false, mutates_workspace: true, idempotent: false })) }
pub fn registration(workspace: WorkspaceRoot) -> Arc<dyn ToolExecutor> { Arc::new(WriteExecutor::new(workspace)) }
::inventory::submit! { ToolRegistration { spec, executor: registration } }
