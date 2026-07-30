//! Closed registration and execution inventory for built-in workspace tools.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use harness_tool_api::{
    BuiltInTool, DuplicateToolName, ToolExecutionRequest, ToolExecutor, ToolFailure,
    ToolInvocation, ToolPreparationRequest, ToolRegistry, ToolResult,
};

use crate::{WorkspaceRoot, edit_file, goal, inspect, terminal};

/// Provider-facing registry and runtime executor dispatch table.
pub struct ToolInventory {
    executors: HashMap<String, Arc<dyn ToolExecutor>>,
}

impl ToolInventory {
    /// Registers every built-in tool into the provider registry.
    pub fn register_into(
        registry: &mut ToolRegistry,
        workspace: WorkspaceRoot,
    ) -> Result<Self, DuplicateToolName> {
        let mut executors = HashMap::new();
        for built_in in BuiltInTool::ALL {
            let (advertised, executor): (_, Arc<dyn ToolExecutor>) = match built_in {
                BuiltInTool::Inspect => (
                    inspect::spec().expect("inspect has a valid built-in name").build(),
                    Arc::new(inspect::Executor::new(workspace.clone())),
                ),
                BuiltInTool::EditFile => (
                    edit_file::spec()
                        .expect("edit_file has a valid built-in name")
                        .build(),
                    Arc::new(edit_file::Executor::new(workspace.clone())),
                ),
                BuiltInTool::TerminalOpen => (
                    terminal::open_spec()
                        .expect("terminal_open has a valid built-in name")
                        .build(),
                    Arc::new(terminal::OpenExecutor::new(workspace.clone())),
                ),
                BuiltInTool::TerminalRead => (
                    terminal::read_spec()
                        .expect("terminal_read has a valid built-in name")
                        .build(),
                    Arc::new(terminal::ReadExecutor::new(workspace.clone())),
                ),
                BuiltInTool::TerminalWrite => (
                    terminal::write_spec()
                        .expect("terminal_write has a valid built-in name")
                        .build(),
                    Arc::new(terminal::WriteExecutor::new(workspace.clone())),
                ),
                BuiltInTool::Goal => (
                    goal::spec().expect("goal has a valid built-in name").build(),
                    Arc::new(goal::Executor),
                ),
            };
            assert_eq!(advertised.definition.name.as_str(), built_in.name());
            let route = advertised.executor.identifier.clone();
            if executors.contains_key(&route) {
                return Err(DuplicateToolName(
                    harness_tool_api::ToolName::new(route).expect("route is nonempty"),
                ));
            }
            registry.register(advertised)?;
            executors.insert(route, executor);
        }
        Ok(Self { executors })
    }

    fn executor(
        &self,
        route: &harness_tool_api::ToolExecutorRoute,
    ) -> Result<&Arc<dyn ToolExecutor>, ToolFailure> {
        self.executors.get(&route.identifier).ok_or_else(|| {
            ToolFailure::Execution(format!(
                "no executor registered for route `{}`",
                route.identifier
            ))
        })
    }
}

impl ToolExecutor for ToolInventory {
    fn prepare(&self, request: ToolPreparationRequest) -> Result<ToolInvocation, ToolFailure> {
        self.executor(&request.route)?.prepare(request)
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>> {
        match self.executor(&request.route) {
            Ok(executor) => executor.execute(request),
            Err(failure) => Box::pin(std::future::ready(Err(failure))),
        }
    }
}
