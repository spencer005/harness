//! Semantic transcript projection into control-free display documents.

use crate::{
    display::{ControlFree, DisplayDocument, RawDocumentBuilder, StyleId},
    domain::{
        InspectReadDisplay, MessageRole, ToolInvocationKind, ToolOutputDisplay, TranscriptPayload,
    },
};

pub(super) fn project(payload: &TranscriptPayload) -> DisplayDocument<ControlFree> {
    let mut builder = RawDocumentBuilder::new();
    match payload {
        TranscriptPayload::Message { role, text } => {
            let (marker, style) = match role {
                MessageRole::User => ("» ", StyleId::User),
                MessageRole::Developer => ("» ", StyleId::Developer),
                MessageRole::Assistant => ("• ", StyleId::Assistant),
            };
            builder.plain(marker, style, false);
            builder.plain(text.as_str(), style, true);
        }
        TranscriptPayload::PlainText(text) => project_plain_text(&mut builder, text.as_str()),
        TranscriptPayload::ToolCall {
            name, input, kind, ..
        } => {
            builder.plain("⚙ ", StyleId::Tool, false);
            builder.plain(name.as_str(), StyleId::Tool, true);
            if !input.as_str().is_empty() {
                builder.line_break();
                let label = match kind {
                    ToolInvocationKind::Freeform => "",
                    ToolInvocationKind::Function => "arguments: ",
                };
                if !label.is_empty() {
                    builder.plain(label, StyleId::Muted, false);
                }
                builder.plain(input.as_str(), StyleId::Tool, true);
            }
        }
        TranscriptPayload::ToolOutput {
            output,
            display_output,
            kind,
            ..
        } => match kind {
            crate::domain::ToolOutputKind::Freeform {
                display: Some(ToolOutputDisplay::InspectRead(reads)),
            } => {
                project_inspect_reads(&mut builder, reads);
            }
            crate::domain::ToolOutputKind::Freeform { display: None }
            | crate::domain::ToolOutputKind::Function => {
                builder.plain("⚙ ", StyleId::Tool, false);
                builder.terminal(
                    terminal_output_body(display_output.as_ref().unwrap_or(output).as_str()),
                    StyleId::Tool,
                    true,
                );
            }
        },
        TranscriptPayload::SessionClosed { closed_at_ms } => {
            builder.plain("· session closed: ", StyleId::Muted, false);
            builder.plain(closed_at_ms.to_string(), StyleId::Muted, true);
        }
        TranscriptPayload::Event(text) => {
            builder.plain("· ", StyleId::Muted, false);
            builder.plain(text.as_str(), StyleId::Muted, true);
        }
    }
    builder.build().parse().sanitize()
}

fn project_plain_text(builder: &mut RawDocumentBuilder, text: &str) {
    for (prefix, marker, style) in [
        ("assistant: ", "• ", StyleId::Assistant),
        ("assistant> ", "• ", StyleId::Assistant),
        ("developer> ", "» ", StyleId::Developer),
        ("user> ", "» ", StyleId::User),
        ("> ", "» ", StyleId::User),
    ] {
        if let Some(body) = text.strip_prefix(prefix) {
            builder.plain(marker, style, false);
            builder.plain(body, style, true);
            return;
        }
    }
    let style = if text.starts_with("error:") || text.starts_with("responses actor error:") {
        StyleId::Error
    } else {
        StyleId::Muted
    };
    builder.plain("· ", style, false);
    builder.plain(text, style, true);
}

fn project_inspect_reads(builder: &mut RawDocumentBuilder, reads: &[InspectReadDisplay]) {
    for (read_index, read) in reads.iter().enumerate() {
        if read_index > 0 {
            builder.line_break();
        }
        let end = read
            .start_line
            .saturating_add(read.lines.len())
            .saturating_sub(1);
        builder.plain("⚙ Read ", StyleId::Tool, false);
        builder.plain(read.path.as_str(), StyleId::Tool, true);
        builder.plain(
            if read.lines.is_empty() {
                format!(":{} no lines", read.start_line)
            } else {
                format!(":{}-{end}", read.start_line)
            },
            StyleId::Muted,
            false,
        );
        for (offset, line) in read.lines.iter().enumerate() {
            builder.line_break();
            builder.plain(
                format!("{} │ ", read.start_line.saturating_add(offset)),
                StyleId::Muted,
                false,
            );
            builder.plain(line.as_str(), StyleId::Plain, true);
        }
        if let Some(next) = read.next {
            builder.line_break();
            builder.plain("next ", StyleId::Muted, false);
            builder.plain(
                format!("{}+{}", next.start_line, next.line_count),
                StyleId::Muted,
                false,
            );
        }
    }
}

fn terminal_output_body(output: &str) -> &str {
    let mut saw_envelope = false;
    let mut offset = 0usize;
    for segment in output.split_inclusive('\n') {
        let line = segment.trim_end_matches(['\r', '\n']);
        let next = offset + segment.len();
        if line == "Output:" && saw_envelope {
            return &output[next..];
        }
        if line.starts_with("Chunk ID:")
            || line.starts_with("Wall time:")
            || line.starts_with("Process exited with code ")
            || line.starts_with("Terminal running with ID ")
            || line.starts_with("Original token count:")
        {
            saw_envelope = true;
        } else if !line.is_empty() {
            return output;
        }
        offset = next;
    }
    if saw_envelope { "" } else { output }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ExternalText;

    #[test]
    fn projection_never_selects_visual_markers() {
        let document = project(&TranscriptPayload::Message {
            role: MessageRole::Assistant,
            text: ExternalText::new("hello"),
        });
        assert_eq!(document.selectable_text(), "hello");
    }

    #[test]
    fn terminal_output_projection_strips_controls_before_selection() {
        let document = project(&TranscriptPayload::ToolOutput {
            call_id: ExternalText::new("call"),
            output: ExternalText::new(
                "Chunk ID: x\nProcess exited with code 0\nOutput:\na\u{1b}[31mred\u{1b}[0m",
            ),
            display_output: None,
            kind: crate::domain::ToolOutputKind::Freeform { display: None },
        });
        assert_eq!(document.selectable_text(), "ared");
    }
}
