use std::{future::Future, pin::Pin};

use harness_tool_api::{
    InvalidToolName, Prepared, ToolCapabilities, ToolExecutionRequest, ToolExecutor, ToolFailure,
    ToolInvocation, ToolOutcome, ToolPreparationRequest, ToolResult, ToolSpec,
};

use super::{Manager, READ_NAME, manager, prepare_read, rejected_result};
use crate::WorkspaceRoot;

pub const DESCRIPTION: &str = "Read recent output from a running terminal. Use `terminal:` and an optional `poll_after:` duration; values below 8s are treated as 8s, and the default is 8s.";
pub const GRAMMAR: &str = include_str!("terminal_read.lark");
pub struct ReadExecutor {
    manager: Manager,
}
impl ReadExecutor {
    pub fn new(workspace: WorkspaceRoot) -> Self {
        Self {
            manager: manager(&workspace),
        }
    }
}
impl ToolExecutor for ReadExecutor {
    fn prepare(&self, request: ToolPreparationRequest) -> Result<ToolInvocation, ToolFailure> {
        if request.tool.as_str() != READ_NAME || request.route.identifier != READ_NAME {
            return Err(ToolFailure::Execution(format!(
                "executor route does not match `{READ_NAME}` for tool {}",
                request.tool.as_str()
            )));
        }
        let input = match request.input.decode_freeform() {
            Ok(input) => input,
            Err(error) => {
                return Ok(ToolInvocation::TerminalRead(Prepared::Rejected(
                    harness_tool_api::ToolInputRejection {
                        message: error.to_string(),
                    },
                )));
            }
        };
        Ok(ToolInvocation::TerminalRead(match prepare_read(&input) {
            Ok(prepared) => Prepared::Ready(prepared),
            Err(message) => Prepared::Rejected(harness_tool_api::ToolInputRejection { message }),
        }))
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>> {
        let manager = self.manager.clone();
        Box::pin(async move {
            let prepared = match request.invocation {
                ToolInvocation::TerminalRead(Prepared::Ready(prepared)) => prepared,
                ToolInvocation::TerminalRead(Prepared::Rejected(rejection)) => {
                    return Ok(rejected_result(rejection.message));
                }
                invocation => {
                    return Err(ToolFailure::Execution(format!(
                        "`{READ_NAME}` received prepared invocation for `{}`",
                        invocation.tool().name()
                    )));
                }
            };
            let result = tokio::task::spawn_blocking(move || manager.read(&prepared))
                .await
                .map_err(|e| ToolFailure::Execution(e.to_string()))?
                .map_err(ToolFailure::Execution)?;
            Ok(ToolResult {
                model_output: result.model,
                outcome: ToolOutcome::TerminalRead(result.result),
            })
        })
    }
}

pub fn spec() -> Result<ToolSpec, InvalidToolName> {
    Ok(ToolSpec::new(READ_NAME)?
        .description(DESCRIPTION)
        .lark(GRAMMAR)
        .capabilities(ToolCapabilities {
            read_only: true,
            mutates_workspace: false,
            idempotent: true,
        }))
}
