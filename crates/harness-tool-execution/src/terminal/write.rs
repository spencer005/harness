use std::{future::Future, pin::Pin};

use harness_tool_api::{
    InvalidToolName, Prepared, ToolCapabilities, ToolExecutionRequest, ToolExecutor, ToolFailure,
    ToolInvocation, ToolOutcome, ToolPreparationRequest, ToolResult, ToolSpec,
};

use super::{Manager, WRITE_NAME, manager, prepare_write, rejected_result};
use crate::WorkspaceRoot;

pub const DESCRIPTION: &str =
    "Write interactive input to a running terminal. Use `terminal:` and `input:`.";
pub const GRAMMAR: &str = include_str!("terminal_write.lark");
pub struct WriteExecutor {
    manager: Manager,
}
impl WriteExecutor {
    pub fn new(workspace: WorkspaceRoot) -> Self {
        Self {
            manager: manager(&workspace),
        }
    }
}
impl ToolExecutor for WriteExecutor {
    fn prepare(&self, request: ToolPreparationRequest) -> Result<ToolInvocation, ToolFailure> {
        if request.tool.as_str() != WRITE_NAME || request.route.identifier != WRITE_NAME {
            return Err(ToolFailure::Execution(format!(
                "executor route does not match `{WRITE_NAME}` for tool {}",
                request.tool.as_str()
            )));
        }
        let input = match request.input.decode_freeform() {
            Ok(input) => input,
            Err(error) => {
                return Ok(ToolInvocation::TerminalWrite(Prepared::Rejected(
                    harness_tool_api::ToolInputRejection {
                        message: error.to_string(),
                    },
                )));
            }
        };
        Ok(ToolInvocation::TerminalWrite(match prepare_write(&input) {
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
                ToolInvocation::TerminalWrite(Prepared::Ready(prepared)) => prepared,
                ToolInvocation::TerminalWrite(Prepared::Rejected(rejection)) => {
                    return Ok(rejected_result(rejection.message));
                }
                invocation => {
                    return Err(ToolFailure::Execution(format!(
                        "`{WRITE_NAME}` received prepared invocation for `{}`",
                        invocation.tool().name()
                    )));
                }
            };
            let result = tokio::task::spawn_blocking(move || manager.write(&prepared))
                .await
                .map_err(|e| ToolFailure::Execution(e.to_string()))?
                .map_err(ToolFailure::Execution)?;
            Ok(ToolResult {
                model_output: result.model,
                outcome: ToolOutcome::TerminalWrite(result.result),
            })
        })
    }
}

pub fn spec() -> Result<ToolSpec, InvalidToolName> {
    Ok(ToolSpec::new(WRITE_NAME)?
        .description(DESCRIPTION)
        .lark(GRAMMAR)
        .capabilities(ToolCapabilities {
            read_only: false,
            mutates_workspace: true,
            idempotent: false,
        }))
}
