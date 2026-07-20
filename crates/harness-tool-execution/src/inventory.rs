//! Dynamic registration and execution inventory for workspace tools.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use harness_tool_api::{
    DuplicateToolName, InvalidToolName, ToolExecutionRequest, ToolExecutor,
    ToolFailure, ToolRegistry, ToolResult, ToolSpec,
};

use crate::WorkspaceRoot;

/// Static descriptor published by one tool module.
pub struct ToolRegistration {
    pub spec: fn() -> Result<ToolSpec, InvalidToolName>,
    pub executor: fn(WorkspaceRoot) -> Arc<dyn ToolExecutor>,
}

::inventory::collect!(ToolRegistration);

/// Provider-facing tool registry and runtime executor dispatch table.
pub struct ToolInventory {
    executors: HashMap<String, Arc<dyn ToolExecutor>>,
}

impl ToolInventory {
    /// Registers every published tool into the existing provider registry.
    pub fn register_into(
        registry: &mut ToolRegistry,
        workspace: WorkspaceRoot,
    ) -> Result<Self, DuplicateToolName> {
        let mut executors = HashMap::new();
        for registration in ::inventory::iter::<ToolRegistration> {
            let advertised = (registration.spec)()
                .map_err(|error| {
                    DuplicateToolName(
                        harness_tool_api::ToolName::new(error.to_string())
                            .expect("registration error is nonempty"),
                    )
                })?
                .build();
            let route = advertised.executor.identifier.clone();
            if executors.contains_key(&route) {
                return Err(DuplicateToolName(
                    harness_tool_api::ToolName::new(route).expect("route is nonempty"),
                ));
            }
            registry.register(advertised)?;
            executors.insert(route, (registration.executor)(workspace.clone()));
        }
        Ok(Self { executors })
    }
}

impl ToolExecutor for ToolInventory {
    fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult, ToolFailure>> + Send + '_>> {
        match self.executors.get(&request.route.identifier) {
            Some(executor) => executor.execute(request),
            None => Box::pin(std::future::ready(Err(ToolFailure::Execution(format!(
                "no executor registered for route `{}`",
                request.route.identifier
            ))))),
        }
    }
}
