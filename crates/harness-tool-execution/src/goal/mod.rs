//! Persisted-goal completion control.

use std::{future::Future, pin::Pin, sync::Arc};

use harness_tool_api::{
    InvalidToolName, ToolCapabilities, ToolExecutionRequest, ToolExecutor, ToolFailure, ToolResult,
    ToolSpec,
};

use crate::WorkspaceRoot;

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
    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>> {
        let result = if request.input.as_str().trim() == "complete" {
            Ok(ToolResult {
                model_output: "No persisted goal is active. Do not call the `goal` tool again for this task. Provide the completed response directly as your final answer."
                    .to_owned(),
                presentation: None,
                artifacts: Vec::new(),
            })
        } else {
            Err(ToolFailure::InvalidInput(
                "goal expects exactly `complete`".into(),
            ))
        };
        Box::pin(std::future::ready(result))
    }
}

::inventory::submit! {
    crate::inventory::ToolRegistration {
        spec,
        executor: |_workspace: WorkspaceRoot| Arc::new(Executor),
    }
}
