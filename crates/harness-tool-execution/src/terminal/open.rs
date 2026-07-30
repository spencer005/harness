use std::{future::Future, pin::Pin};

use harness_tool_api::{
    InvalidToolName, Prepared, ToolCapabilities, ToolExecutionRequest, ToolExecutor, ToolFailure,
    ToolInvocation, ToolOutcome, ToolPreparationRequest, ToolResult, ToolSpec,
};

use super::{Manager, OPEN_NAME, manager, prepare_open, rejected_result};
use crate::WorkspaceRoot;

pub const DESCRIPTION: &str = "Start a persistent Bash command attached to a PTY. Prefer `inspect` for workspace discovery, file reads, searches, compiler checks, and tests; use this tool when an interactive shell or runtime verification unavailable through `inspect` is required. Use `command:`, with optional `workdir:`, `rows:`, and `cols:`.";
pub const GRAMMAR: &str = include_str!("terminal_open.lark");

pub struct OpenExecutor {
    manager: Manager,
    workspace: WorkspaceRoot,
}
impl OpenExecutor {
    pub fn new(workspace: WorkspaceRoot) -> Self {
        Self {
            manager: manager(&workspace),
            workspace,
        }
    }
}
impl ToolExecutor for OpenExecutor {
    fn prepare(&self, request: ToolPreparationRequest) -> Result<ToolInvocation, ToolFailure> {
        if request.tool.as_str() != OPEN_NAME || request.route.identifier != OPEN_NAME {
            return Err(ToolFailure::Execution(format!(
                "executor route does not match `{OPEN_NAME}` for tool {}",
                request.tool.as_str()
            )));
        }
        let input = match request.input.decode_freeform() {
            Ok(input) => input,
            Err(error) => {
                return Ok(ToolInvocation::TerminalOpen(Prepared::Rejected(
                    harness_tool_api::ToolInputRejection {
                        message: error.to_string(),
                    },
                )));
            }
        };
        Ok(ToolInvocation::TerminalOpen(match prepare_open(&input) {
            Ok(prepared) => Prepared::Ready(prepared),
            Err(message) => Prepared::Rejected(harness_tool_api::ToolInputRejection { message }),
        }))
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>> {
        let manager = self.manager.clone();
        let workspace = self.workspace.clone();
        Box::pin(async move {
            let prepared = match request.invocation {
                ToolInvocation::TerminalOpen(Prepared::Ready(prepared)) => prepared,
                ToolInvocation::TerminalOpen(Prepared::Rejected(rejection)) => {
                    return Ok(rejected_result(rejection.message));
                }
                invocation => {
                    return Err(ToolFailure::Execution(format!(
                        "`{OPEN_NAME}` received prepared invocation for `{}`",
                        invocation.tool().name()
                    )));
                }
            };
            let result =
                tokio::task::spawn_blocking(move || manager.open(&workspace, &prepared))
                    .await
                    .map_err(|e| ToolFailure::Execution(e.to_string()))?
                    .map_err(ToolFailure::Execution)?;
            Ok(ToolResult {
                model_output: result.model,
                outcome: ToolOutcome::TerminalOpen(result.result),
            })
        })
    }
}

pub fn spec() -> Result<ToolSpec, InvalidToolName> {
    Ok(ToolSpec::new(OPEN_NAME)?
        .description(DESCRIPTION)
        .lark(GRAMMAR)
        .capabilities(ToolCapabilities {
            read_only: false,
            mutates_workspace: true,
            idempotent: false,
        }))
}
