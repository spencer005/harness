//! Persisted-goal completion control.

use std::{future::Future, pin::Pin};

use harness_tool_api::{
    GoalRequest, InvalidToolName, Prepared, ToolCapabilities, ToolExecutionFailure,
    ToolExecutionRequest, ToolExecutor, ToolFailure, ToolFailureCategory, ToolInvocation,
    ToolOutcome, ToolPreparationRequest, ToolResult, ToolSpec,
};

/// Name of the persisted-goal control tool.
pub const NAME: &str = "goal";
/// Model-facing description of the goal completion control.
pub const DESCRIPTION: &str = "Mark the active persisted goal complete after verifying that its completion criteria are satisfied. Send exactly `complete`. This control has no workspace side effects.";
/// Lark grammar for the goal completion control.
pub const LARK_GRAMMAR: &str = "start: complete\ncomplete: \"complete\"";

/// Builds the dynamically registered goal tool specification.
pub fn spec() -> Result<ToolSpec, InvalidToolName> {
    Ok(ToolSpec::new(NAME)?
        .description(DESCRIPTION)
        .lark(LARK_GRAMMAR)
        .capabilities(ToolCapabilities {
            read_only: true,
            mutates_workspace: false,
            idempotent: true,
        }))
}

/// Fallback executor used only if the runtime does not intercept the control call.
pub struct Executor;

impl ToolExecutor for Executor {
    fn prepare(&self, request: ToolPreparationRequest) -> Result<ToolInvocation, ToolFailure> {
        if request.tool.as_str() != NAME || request.route.identifier != NAME {
            return Err(ToolFailure::Execution(format!(
                "executor route does not match `{NAME}` for tool {}",
                request.tool.as_str()
            )));
        }
        let input = match request.input.decode_freeform() {
            Ok(input) => input,
            Err(error) => {
                return Ok(ToolInvocation::Goal(Prepared::Rejected(
                    harness_tool_api::ToolInputRejection {
                        message: error.to_string(),
                    },
                )));
            }
        };
        let prepared = if input.trim() == "complete" {
            Prepared::Ready(GoalRequest)
        } else {
            Prepared::Rejected(harness_tool_api::ToolInputRejection {
                message: "goal expects exactly `complete`".into(),
            })
        };
        Ok(ToolInvocation::Goal(prepared))
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>> {
        let message = match request.invocation {
            ToolInvocation::Goal(Prepared::Ready(_)) => "No persisted goal is active. Do not call the `goal` tool again for this task. Provide the completed response directly as your final answer.".to_owned(),
            ToolInvocation::Goal(Prepared::Rejected(rejection)) => rejection.message,
            invocation => {
                return Box::pin(std::future::ready(Err(ToolFailure::Execution(format!(
                    "`{NAME}` received prepared invocation for `{}`",
                    invocation.tool().name()
                )))));
            }
        };
        Box::pin(std::future::ready(Ok(ToolResult {
            model_output: message.clone(),
            outcome: ToolOutcome::Failed(ToolExecutionFailure {
                category: ToolFailureCategory::InvalidInput,
                message,
            }),
        })))
    }
}
