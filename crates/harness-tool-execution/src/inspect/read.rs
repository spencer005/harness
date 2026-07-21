//! `inspect read` job formatting and structured transcript display.

use std::fmt::Write as _;

use crate::inspect::{
    InspectReadDisplayRecord, InspectReadNextRecord, InspectReadOutputRequest, line_anchor_word,
    edit_line_hash,
};

/// Format one read range as the model-facing anchor-prefixed text.
pub fn format_read_output(request: &InspectReadOutputRequest, text: &str) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    if request.start_line > total_lines {
        return format!("no lines; file has {total_lines} lines\n");
    }

    let start_index = request.start_line - 1;
    let end_index = if request.line_count == usize::MAX {
        total_lines
    } else {
        total_lines.min(start_index.saturating_add(request.line_count))
    };
    let first_line = request.start_line;
    let mut output = String::new();
    for (index, line) in lines[start_index..end_index].iter().enumerate() {
        let line_number = first_line + index;
        let anchor = format_line_anchor(line_number, edit_line_hash(line));
        let _ = writeln!(output, "{anchor}{line}");
    }
    if end_index < total_lines {
        let _ = writeln!(output, "next: {}+{}", end_index + 1, request.line_count);
    }
    output
}

/// Build the structured transcript display for one read range.
pub fn format_read_display(
    request: &InspectReadOutputRequest,
    text: &str,
) -> InspectReadDisplayRecord {
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    if request.start_line > total_lines {
        return InspectReadDisplayRecord {
            path: request.path.clone(),
            start_line: request.start_line,
            lines: Vec::new(),
            next: None,
        };
    }

    let start_index = request.start_line - 1;
    let end_index = if request.line_count == usize::MAX {
        total_lines
    } else {
        total_lines.min(start_index.saturating_add(request.line_count))
    };
    let next = (end_index < total_lines).then_some(InspectReadNextRecord {
        start_line: end_index + 1,
        line_count: request.line_count,
    });
    InspectReadDisplayRecord {
        path: request.path.clone(),
        start_line: request.start_line,
        lines: lines[start_index..end_index]
            .iter()
            .map(|line| (*line).to_string())
            .collect(),
        next,
    }
}

pub(crate) fn format_line_anchor(line_number: usize, hash: u8) -> String {
    format!("{line_number} {}", line_anchor_word(hash))
}