//! MCP method dispatch against the harness tool registry and executors.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use harness_tool_api::{
    AdvertisedTool, ToolDefinition, ToolExecutionId, ToolExecutionPolicy, ToolFailure, ToolInput,
    ToolInputSchema, ToolName, ToolPreparationRequest, ToolRegistry,
};
use sonic_rs::{JsonValueTrait, Value, json};

use crate::{ExecutorMap, jsonrpc};


/// Builds the MCP `tools/list` result from the harness registry.
pub fn list(registry: &ToolRegistry) -> Value {
    let tools: Vec<Value> = registry
        .iter()
        .map(|(_, advertised)| {
            let ToolDefinition { name, description, input_schema, .. } = &advertised.definition;
            let (desc, schema) = match input_schema {
                // Freeform tools: expose as a function with one string `input`
                // parameter so ChatGPT knows how to call them. The description
                // includes the full native grammar via function_compatibility_description.
                ToolInputSchema::FreeformGrammar { .. } => {
                    let full_desc = advertised.definition.function_compatibility_description();
                    let schema = json!({
                        "type": "object",
                        "properties": {
                            "input": {
                                "type": "string",
                                "description": "Raw tool input as a single JSON string. See the tool description for the grammar."
                            }
                        },
                        "required": ["input"]
                    });
                    (full_desc, schema)
                }
                ToolInputSchema::JsonSchema(schema) => {
                    let schema = sonic_rs::from_str(schema.as_str()).unwrap_or(json!({}));
                    (description.clone(), schema)
                }
            };
            json!({
                "name": name.as_str(),
                "description": desc,
                "inputSchema": schema,
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// Runs an MCP `tools/call`. Arguments must agree with the schema advertised
/// by `tools/list`.
pub async fn call(
    registry: &ToolRegistry,
    executors: &ExecutorMap,
    params: Value,
) -> Result<Value, (i32, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((jsonrpc::code::INVALID_PARAMS, "tools/call requires `name`".to_owned()))?;
    let tool_name = ToolName::new(name).map_err(|_| (jsonrpc::code::INVALID_PARAMS, "empty tool name".to_owned()))?;
    let advertised = registry
        .get(&tool_name)
        .ok_or((jsonrpc::code::METHOD_NOT_FOUND, format!("unknown tool: {name}")))?;
    let executor = executors
        .get(&advertised.executor.identifier)
        .ok_or((
            jsonrpc::code::INTERNAL_ERROR,
            format!("executor not bound for tool: {name}"),
        ))?;

    let input = decode_input(advertised, params.get("arguments"))?;

    let input_str = match &input {
        ToolInput::Freeform(s) | ToolInput::FunctionJson(s) => s.clone(),
    };

    // Human-readable tool call banner.
    eprintln!();
    eprintln!("┌─ {} ────────────────────────────────────────", name);
    eprintln!("│ input:");
    for line in input_str.lines() {
        eprintln!("│   {line}");
    }
    if input_str.is_empty() {
        eprintln!("│   (empty)");
    }
    eprintln!("└─────────────────────────────────────────────");

    let preparation = ToolPreparationRequest {
        tool: tool_name.clone(),
        route: advertised.executor.clone(),
        input,
    };

    let invocation = match executor.prepare(preparation) {
        Ok(invocation) => invocation,
        Err(ToolFailure::InvalidInput(message)) => return Ok(render_rejection(message)),
        Err(ToolFailure::Execution(message)) => {
            return Err((jsonrpc::code::INTERNAL_ERROR, message));
        }
        Err(ToolFailure::TimedOut) => return Err((jsonrpc::code::INTERNAL_ERROR, "tool timed out".to_owned())),
        Err(ToolFailure::Cancelled) => return Err((jsonrpc::code::INTERNAL_ERROR, "tool cancelled".to_owned())),
    };

    // Rejected inputs surface as a successful MCP call with isError content.
    if let Some(rejection) = invocation.rejection() {
        eprintln!();
        eprintln!("┌─ {} ─ REJECTED ─────────────────────────────", name);
        eprintln!("│ {}", rejection.message);
        eprintln!("└─────────────────────────────────────────────");
        return Ok(render_rejection(rejection.message.clone()));
    }

    static NEXT_EXECUTION_ID: AtomicU64 = AtomicU64::new(1);
    const TOOL_DEADLINE: Duration = Duration::from_secs(120);

    let execution_id = ToolExecutionId(NEXT_EXECUTION_ID.fetch_add(1, Ordering::Relaxed));
    let request = harness_tool_api::ToolExecutionRequest {
        execution_id,
        tool: tool_name.clone(),
        route: advertised.executor.clone(),
        invocation: invocation.clone(),
        policy: ToolExecutionPolicy {
            deadline_ms: TOOL_DEADLINE.as_millis() as u64,
            cancellable: true,
        },
    };

    let result = match tokio::time::timeout(TOOL_DEADLINE, executor.execute(request)).await {
        Err(_) => return Ok(render_error("tool timed out after 120 seconds")),
        Ok(Ok(result)) => result,
        Ok(Err(ToolFailure::InvalidInput(message))) => return Ok(render_rejection(message)),
        Ok(Err(ToolFailure::Execution(message))) => {
            return Ok(render_error(&message));
        }
        Ok(Err(ToolFailure::TimedOut)) => return Ok(render_error("tool timed out")),
        Ok(Err(ToolFailure::Cancelled)) => return Ok(render_error("tool cancelled")),
    };

    let is_error = !matches!(
        result.outcome.status(),
        harness_tool_api::ToolActivityStatus::Running
            | harness_tool_api::ToolActivityStatus::Successful
            | harness_tool_api::ToolActivityStatus::PartiallySuccessful
    );

    let status_label = match result.outcome.status() {
        harness_tool_api::ToolActivityStatus::Successful => "OK",
        harness_tool_api::ToolActivityStatus::PartiallySuccessful => "PARTIAL",
        harness_tool_api::ToolActivityStatus::Failed => "FAILED",
        harness_tool_api::ToolActivityStatus::Interrupted => "INTERRUPTED",
        harness_tool_api::ToolActivityStatus::Running => "RUNNING",
    };

    eprintln!();
    eprintln!("┌─ {} ─ {} ───────────────────────────────", name, status_label);
    eprintln!("│ output:");
    for line in result.model_output.lines() {
        eprintln!("│   {line}");
    }
    if result.model_output.is_empty() {
        eprintln!("│   (empty)");
    }
    eprintln!("└─────────────────────────────────────────────");

    Ok(json!({
        "content": [{ "type": "text", "text": result.model_output }],
        "isError": is_error,
    }))
}

fn decode_input(
    advertised: &AdvertisedTool,
    arguments: Option<&Value>,
) -> Result<ToolInput, (i32, String)> {
    let arguments = arguments.ok_or((
        jsonrpc::code::INVALID_PARAMS,
        format!(
            "tools/call for `{}` requires `arguments` matching its advertised input schema",
            advertised.definition.name.as_str()
        ),
    ))?;
    let raw = sonic_rs::to_string(arguments).map_err(|error| {
        (
            jsonrpc::code::INVALID_PARAMS,
            format!("failed to serialize tool arguments: {error}"),
        )
    })?;

    match &advertised.definition.input_schema {
        ToolInputSchema::FreeformGrammar { .. } => {
            if !arguments.is_object() {
                return Err((
                    jsonrpc::code::INVALID_PARAMS,
                    "freeform compatibility arguments must be a JSON object containing exactly one string property named `input`".to_owned(),
                ));
            }
            Ok(ToolInput::FunctionJson(raw))
        }
        ToolInputSchema::JsonSchema(_) => Ok(ToolInput::FunctionJson(raw)),
    }
}

fn render_rejection(message: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": format!("tool input rejected: {message}") }],
        "isError": true,
    })
}

fn render_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}
