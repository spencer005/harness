//! Pure tool-output summarization helpers.

use super::{
    PendingToolOutputSummary, TOOL_OUTPUT_SUMMARY_CONTEXT_RECORDS,
    TOOL_OUTPUT_SUMMARY_PROMPT_MAX_TOKENS, TOOL_OUTPUT_SUMMARY_TARGET_TOKENS,
    TOOL_OUTPUT_SUMMARY_THRESHOLD_TOKENS, text::diagnostic_summary,
};
use crate::{
    compact::estimate_text_tokens,
    sessions::{FreeformToolCallRecord, HistoryRecord},
    tools::{
        NativeToolExecutionOutput, TERMINAL_OPEN_TOOL_NAME, TERMINAL_READ_TOOL_NAME,
        TERMINAL_WRITE_TOOL_NAME,
    },
};

/// Return whether a freeform tool output needs summarization.
pub(super) fn tool_output_needs_summary(
    call: &FreeformToolCallRecord,
    output: &NativeToolExecutionOutput,
) -> bool {
    let output_tokens = estimate_text_tokens(&output.model_output);
    is_terminal_tool_name(&call.name)
        && !is_explicit_sed_range_terminal_input(&call.input)
        && output_tokens > TOOL_OUTPUT_SUMMARY_THRESHOLD_TOKENS
}
/// Build instructions for the tool-output summarizer request.
pub(super) fn tool_output_summary_instructions() -> String {
    format!(
        "Summarize terminal output faithfully. Do not add analysis, speculation, code review, bug findings, or conclusions not explicitly present in the output. For source or search output: treat each displayed line as evidence only for itself; do not combine separate lines into new snippets; do not infer duplicates, syntax errors, or likely bugs. Only mention code issues if they are explicit compiler/test/linter diagnostics in the output. If output is truncated or summarized, say what was omitted by category, but do not infer its contents. Keep response under approximately {TOOL_OUTPUT_SUMMARY_TARGET_TOKENS} tokens."
    )
}

/// Return the cwd, call record, and output for a pending summary.
pub(super) fn pending_tool_output_summary_parts<'a>(
    pending: &'a PendingToolOutputSummary,
    default_cwd: &'a str,
) -> (
    &'a str,
    &'a FreeformToolCallRecord,
    &'a NativeToolExecutionOutput,
) {
    match pending {
        PendingToolOutputSummary::RootFreeform {
            call_record,
            output,
        } => (default_cwd, call_record, output),
    }
}

/// Build the user prompt for a tool-output summary request.
pub(super) fn tool_output_summary_prompt(
    context: &str,
    cwd: &str,
    call: &FreeformToolCallRecord,
    output: &str,
) -> String {
    let output = summary_prompt_output(output);
    format!(
        "Recent conversation context:\n{context}\n\nTerminal tool metadata:\n- cwd: {cwd}\n- tool: {}\n- call_id: {}\n- input:\n{}\n\nRaw model-facing terminal output follows. Summarize faithfully. Prioritize task-relevant output and preserve exact strings when useful.\n\n```terminal-output\n{}\n```",
        call.name, call.call_id, call.input, output
    )
}
fn summary_prompt_output(output: &str) -> String {
    let original_tokens = estimate_text_tokens(output);
    if original_tokens <= TOOL_OUTPUT_SUMMARY_PROMPT_MAX_TOKENS {
        return output.to_string();
    }

    let notice = format!(
        "\n\n[tool output truncated for summarizer prompt: original approximately {original_tokens} tokens; retained first {TOOL_OUTPUT_SUMMARY_PROMPT_MAX_TOKENS} tokens]\n"
    );
    let notice_tokens = estimate_text_tokens(&notice);
    let content_budget = TOOL_OUTPUT_SUMMARY_PROMPT_MAX_TOKENS.saturating_sub(notice_tokens);
    let prefix = estimated_token_prefix(output, content_budget);
    format!("{prefix}{notice}")
}

fn estimated_token_prefix(text: &str, max_tokens: u64) -> &str {
    if max_tokens == 0 {
        return "";
    }
    if estimate_text_tokens(text) <= max_tokens {
        return text;
    }

    let boundaries = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let mut low = 0usize;
    let mut high = boundaries.len() - 1;
    while low < high {
        let middle = (low + high).div_ceil(2);
        let candidate = &text[..boundaries[middle]];
        if estimate_text_tokens(candidate) <= max_tokens {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    &text[..boundaries[low]]
}

/// Build recent conversation context for a tool-output summary request.
pub(super) fn recent_tool_summary_context(history: &[HistoryRecord]) -> String {
    let mut records = history
        .iter()
        .rev()
        .take(TOOL_OUTPUT_SUMMARY_CONTEXT_RECORDS)
        .map(history_record_summary_text)
        .collect::<Vec<_>>();
    records.reverse();
    if records.is_empty() {
        "No prior conversation context is available.".to_string()
    } else {
        records.join("\n\n")
    }
}

/// Normalize the summarizer text returned by the model.
pub(super) fn normalized_tool_output_summary(summary: &str) -> String {
    let summary = summary.trim();
    if summary.is_empty() {
        "terminal output summarized by gpt-5.5-low\n\nresult:\nThe summarizer returned an empty summary."
            .to_string()
    } else {
        summary.to_string()
    }
}

/// Replace model-facing raw terminal output with a summary.
pub(super) fn summarized_tool_output(
    output: NativeToolExecutionOutput,
    summary: &str,
) -> NativeToolExecutionOutput {
    NativeToolExecutionOutput::split(
        format!(
            "terminal output summarized by gpt-5.5-low\nraw model-facing output: approximately {} tokens, {} bytes\nsummary:\n{}",
            estimate_text_tokens(&output.model_output),
            output.model_output.len(),
            normalized_tool_output_summary(summary)
        ),
        output.display_output,
    )
    .with_structured_display(output.display)
}

/// Replace model-facing raw terminal output with a summary-failure message.
pub(super) fn tool_output_summary_failure_output(
    output: NativeToolExecutionOutput,
    message: &str,
) -> NativeToolExecutionOutput {
    NativeToolExecutionOutput::split(
        format!(
            "terminal output summary failed: {message}\nraw model-facing output omitted because it exceeded {} tokens",
            TOOL_OUTPUT_SUMMARY_THRESHOLD_TOKENS
        ),
        output.display_output,
    )
    .with_structured_display(output.display)
}

fn is_terminal_tool_name(name: &str) -> bool {
    matches!(
        name,
        TERMINAL_OPEN_TOOL_NAME | TERMINAL_WRITE_TOOL_NAME | TERMINAL_READ_TOOL_NAME
    )
}

fn is_explicit_sed_range_terminal_input(input: &str) -> bool {
    let Some(submitted_input) = terminal_tool_submitted_command(input) else {
        return false;
    };
    let commands = submitted_input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    !commands.is_empty()
        && commands
            .iter()
            .all(|command| is_sed_print_range_command(command))
}

fn is_sed_print_range_command(command: &str) -> bool {
    let command = command.trim_start();
    let command = command
        .strip_suffix("; exit")
        .or_else(|| command.strip_suffix(";exit"))
        .unwrap_or(command)
        .trim_end();
    if command.contains("&&")
        || command.contains("||")
        || command.contains('|')
        || command.contains(';')
    {
        return false;
    }
    if !command.starts_with("sed") {
        return false;
    }
    let after_name = &command["sed".len()..];
    if !after_name.starts_with(char::is_whitespace) {
        return false;
    }
    let mut arguments = after_name.split_whitespace();
    let Some(first_argument) = arguments.next() else {
        return false;
    };
    if first_argument != "-n" {
        return false;
    }
    let Some(script) = arguments.next() else {
        return false;
    };
    let script = script.trim_matches(|character| character == '\'' || character == '"');
    script.ends_with('p')
        && script.contains(',')
        && script
            .chars()
            .take_while(|character| *character != ',')
            .any(|character| character.is_ascii_digit())
        && script
            .chars()
            .skip_while(|character| *character != ',')
            .any(|character| character.is_ascii_digit())
}

fn terminal_tool_submitted_command(input: &str) -> Option<&str> {
    let mut offset = 0usize;
    for segment in input.split_inclusive('\n') {
        let next_offset = offset + segment.len();
        let line = line_without_ending(segment);
        if let Some((key, value)) = line.split_once(':')
            && key.trim() == "command"
        {
            let value = value.trim_start();
            return if value.is_empty() {
                Some(&input[next_offset..])
            } else {
                Some(value)
            };
        }
        offset = next_offset;
    }
    let line = line_without_ending(&input[offset..]);
    if let Some((key, value)) = line.split_once(':')
        && key.trim() == "command"
    {
        let value = value.trim_start();
        return if value.is_empty() {
            Some("")
        } else {
            Some(value)
        };
    }
    None
}

fn line_without_ending(line: &str) -> &str {
    line.strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(line)
}

fn history_record_summary_text(record: &HistoryRecord) -> String {
    match record {
        HistoryRecord::UserMessage(message) => format!("user: {}", message.text),
        HistoryRecord::DeveloperMessage(message) => format!("developer: {}", message.text),
        HistoryRecord::AssistantMessage(message) => format!("assistant: {}", message.text),
        HistoryRecord::FreeformToolCall(call) => {
            format!(
                "freeform tool call: {} {}\n{}",
                call.name, call.call_id, call.input
            )
        }
        HistoryRecord::FreeformToolOutput(output) => format!(
            "freeform tool output: {}\n{}",
            output.call_id,
            diagnostic_summary(output.transcript_output())
        ),
        HistoryRecord::FunctionToolCall(call) => {
            format!(
                "function tool call: {} {}\n{}",
                call.name, call.call_id, call.arguments
            )
        }
        HistoryRecord::FunctionToolOutput(output) => format!(
            "function tool output: {}\n{}",
            output.call_id,
            diagnostic_summary(output.transcript_output())
        ),
    }
}
