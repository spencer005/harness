use criterion::{Criterion, black_box, criterion_group, criterion_main};
use harness_core::{
    responses::{ResponsesCreateRequest, ResponsesModelCapabilities},
    tools::{
        NativeTool, parse_freeform_tool_call_from_frame, parse_freeform_tool_input_delta_from_frame,
    },
};
use sonic_rs::{Value, json};

fn responses_request_to_body(c: &mut Criterion) {
    let request = representative_request();

    c.bench_function(
        "responses_create_request_to_body_apply_patch_history",
        |b| {
            b.iter(|| {
                let body = black_box(&request)
                    .to_body()
                    .expect("representative response.create request serializes");
                black_box(body)
            });
        },
    );
}

fn parse_freeform_tool_input_delta(c: &mut Criterion) {
    let frame = freeform_tool_input_delta_frame();

    c.bench_function("parse_freeform_tool_input_delta_from_frame", |b| {
        b.iter(|| {
            let delta = parse_freeform_tool_input_delta_from_frame(black_box(&frame))
                .expect("freeform input delta frame parses")
                .expect("freeform input delta frame yields a delta");
            black_box(delta)
        });
    });
}

fn parse_freeform_tool_call(c: &mut Criterion) {
    let frame = freeform_tool_call_done_frame();

    c.bench_function("parse_freeform_tool_call_from_frame", |b| {
        b.iter(|| {
            let call = parse_freeform_tool_call_from_frame(black_box(&frame))
                .expect("freeform tool call frame parses")
                .expect("freeform tool call frame yields a call");
            black_box(call)
        });
    });
}

fn representative_request() -> ResponsesCreateRequest {
    let mut request =
        ResponsesCreateRequest::new("gpt-5.4-codex", ResponsesModelCapabilities::new(true))
            .with_instructions(
                "You are a coding agent. Keep edits scoped, use apply_patch for file changes, \
         and summarize verification results.",
            )
            .with_input(representative_history())
            .with_tools(vec![NativeTool::apply_patch()]);
    request.include = vec!["reasoning.encrypted_content".to_string()];
    request
}

fn representative_history() -> Vec<Value> {
    vec![
        json!({
            "type": "message",
            "role": "user",
            "content": "Implement the request builder path and keep the native apply_patch tool enabled."
        }),
        json!({
            "type": "message",
            "role": "assistant",
            "content": "I will inspect the request shape, update the harness code, and run targeted checks."
        }),
        json!({
            "type": "custom_tool_call",
            "call_id": "call_apply_patch_1",
            "name": "apply_patch",
            "input": "*** Begin Patch\n*** Update File: crates/harness-core/src/responses.rs\n@@\n-        tools: Vec::new(),\n+        tools: vec![FreeformTool::apply_patch()],\n*** End Patch\n"
        }),
        json!({
            "type": "custom_tool_call_output",
            "call_id": "call_apply_patch_1",
            "output": "Success. Updated crates/harness-core/src/responses.rs."
        }),
        json!({
            "type": "message",
            "role": "assistant",
            "content": "The request now carries the native custom apply_patch tool and serializes the Responses body."
        }),
        json!({
            "type": "message",
            "role": "user",
            "content": "Add a parsing check for streamed custom tool input deltas and final custom tool calls."
        }),
        json!({
            "type": "custom_tool_call",
            "call_id": "call_apply_patch_2",
            "name": "apply_patch",
            "input": "*** Begin Patch\n*** Update File: crates/harness-core/src/tools.rs\n@@\n-    Ok(None)\n+    parse_freeform_tool_call_from_frame(frame)\n*** End Patch\n"
        }),
        json!({
            "type": "custom_tool_call_output",
            "call_id": "call_apply_patch_2",
            "output": "Success. Updated crates/harness-core/src/tools.rs."
        }),
    ]
}

fn freeform_tool_input_delta_frame() -> Value {
    json!({
        "type": "response.custom_tool_call_input.delta",
        "item_id": "item_apply_patch_3",
        "call_id": "call_apply_patch_3",
        "delta": "*** Begin Patch\n*** Update File: crates/harness-core/src/lib.rs\n@@\n pub mod responses;\n+pub mod tools;\n*** End Patch\n"
    })
}

fn freeform_tool_call_done_frame() -> Value {
    json!({
        "type": "response.output_item.done",
        "output_index": 0,
        "item": {
            "type": "custom_tool_call",
            "id": "item_apply_patch_4",
            "call_id": "call_apply_patch_4",
            "name": "apply_patch",
            "input": "*** Begin Patch\n*** Update File: crates/harness-core/src/harness.rs\n@@\n-        self.submit_responses_request(request).await;\n+        self.submit_responses_request(request).await;\n*** End Patch\n"
        }
    })
}

criterion_group!(
    benches,
    responses_request_to_body,
    parse_freeform_tool_input_delta,
    parse_freeform_tool_call
);
criterion_main!(benches);
