/// Append escaped current-working-directory context to request instructions.
pub(super) fn instructions_with_cwd(base_instructions: &str, cwd: &str) -> String {
    let context = format!(
        "<environment_context>\n  <cwd>{}</cwd>\n</environment_context>",
        xml_text_escape(cwd)
    );
    if base_instructions.is_empty() {
        context
    } else {
        format!("{base_instructions}\n\n{context}")
    }
}

/// Build a queued steering message for the root response.
pub(super) fn root_queued_steering_message(text: &str) -> String {
    format!(
        "User steering for the current response:\n- applied: {}\nAdjust your current task accordingly and continue.",
        text.trim()
    )
}

/// Return the root persist continuation developer message.
pub(super) fn persist_continuation_message(task: &str) -> String {
    format!(
        "Persist mode is active.\n\nPersisted task:\n{task}\n\nContinue working on the persisted task after this response completion. Do not stop just because a response is done. Before calling the `mark_task_complete` custom tool, verify the persisted task's completion criteria against the conversation and observed results. Only call `mark_task_complete` with exactly `complete` after those criteria are satisfied; that tool has no output and ends persist mode."
    )
}

fn xml_text_escape(text: &str) -> String {
    let mut escaped = String::new();
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            character => escaped.push(character),
        }
    }
    escaped
}
