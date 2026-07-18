//! Pure tool execution output record builders.

use crate::{
    sessions::{FreeformToolOutputRecord, FunctionToolOutputRecord},
    tools::NativeToolExecutionOutput,
};

/// Build a freeform tool output record from native execution output.
pub(super) fn freeform_tool_output_record(
    call_id: String,
    output: NativeToolExecutionOutput,
) -> FreeformToolOutputRecord {
    let NativeToolExecutionOutput {
        model_output,
        display_output,
        display,
    } = output;
    let display_output = display_output_override(display_output, &model_output);
    FreeformToolOutputRecord {
        call_id,
        output: model_output,
        display_output,
        display,
    }
}

/// Build a function tool output record from native execution output.
pub(super) fn function_tool_output_record(
    call_id: String,
    output: NativeToolExecutionOutput,
) -> FunctionToolOutputRecord {
    let (output, display_output) = split_tool_execution_output(output);
    FunctionToolOutputRecord {
        call_id,
        output,
        display_output,
    }
}

fn split_tool_execution_output(output: NativeToolExecutionOutput) -> (String, Option<String>) {
    let NativeToolExecutionOutput {
        model_output,
        display_output,
        display: _,
    } = output;
    let display_output = display_output_override(display_output, &model_output);
    (model_output, display_output)
}

/// Return transcript-only display output when it differs from model output.
pub(super) fn display_output_override(
    display_output: String,
    model_output: &str,
) -> Option<String> {
    (display_output != model_output).then_some(display_output)
}
