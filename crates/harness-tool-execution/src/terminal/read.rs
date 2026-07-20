use std::{future::Future, pin::Pin, sync::Arc};
use harness_tool_api::{InvalidToolName, ToolCapabilities, ToolExecutionRequest, ToolExecutor, ToolFailure, ToolResult, ToolSpec};
use crate::{inventory::ToolRegistration, WorkspaceRoot};
use super::{failure, manager, output, Manager, READ_NAME};

pub const DESCRIPTION: &str = "Read recent output from a running terminal. Use `terminal:` and optional `poll_after:` milliseconds.";
pub const GRAMMAR: &str = include_str!("terminal_read.lark");
pub struct ReadExecutor { manager: Manager }
impl ReadExecutor { pub fn new(workspace: WorkspaceRoot) -> Self { Self { manager: manager(&workspace) } } }
impl ToolExecutor for ReadExecutor { fn execute(&self, request: ToolExecutionRequest) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>> { let manager = self.manager.clone(); let input = request.input.as_str().to_owned(); Box::pin(async move { let result = tokio::task::spawn_blocking(move || manager.read(&input)).await.map_err(|e| ToolFailure::Execution(e.to_string()))?; match result { Ok(text) => Ok(output(text, READ_NAME)), Err(error) => failure(error) } }) } }
pub fn spec() -> Result<ToolSpec, InvalidToolName> { Ok(ToolSpec::new(READ_NAME)?.description(DESCRIPTION).lark(GRAMMAR).capabilities(ToolCapabilities { read_only: true, mutates_workspace: false, idempotent: true })) }
pub fn registration(workspace: WorkspaceRoot) -> Arc<dyn ToolExecutor> { Arc::new(ReadExecutor::new(workspace)) }
::inventory::submit! { ToolRegistration { spec, executor: registration } }
