//! `inspect read` execution and model-facing anchor formatting.

use std::fmt::Write as _;

use harness_tool_api::{
    InspectJobSuccess, InspectReadRequest, InspectReadResult, LineRange, SourceExcerpt,
};

use super::{InspectCommandOutput, edit_line_hash, line_anchor_word, resolve};

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    request: &InspectReadRequest,
) -> Result<InspectCommandOutput, String> {
    let (name, path) = resolve(workspace, &request.path)?;
    let bytes = std::fs::read(&path).map_err(|error| format!("inspect read {name}: {error}"))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("inspect read {name}: file is not UTF-8; use `bytes`"))?;
    let ranges = if request.ranges.is_empty() {
        vec![LineRange {
            start_line: 1,
            line_count: 1000,
        }]
    } else {
        request.ranges.clone()
    };

    let mut model = String::new();
    let mut excerpts = Vec::with_capacity(ranges.len());
    for range in ranges {
        let excerpt = excerpt(&name, range, &text);
        append_model_output(&mut model, &excerpt);
        excerpts.push(excerpt);
    }
    Ok(InspectCommandOutput {
        model,
        result: InspectJobSuccess::Read(InspectReadResult { excerpts }),
    })
}

fn excerpt(path: &str, range: LineRange, text: &str) -> SourceExcerpt {
    let lines = text.lines().collect::<Vec<_>>();
    let total_lines = lines.len();
    if range.start_line > total_lines {
        return SourceExcerpt {
            path: path.to_owned(),
            start_line: range.start_line,
            lines: Vec::new(),
            next: None,
        };
    }

    let start_index = range.start_line - 1;
    let end_index = if range.line_count == usize::MAX {
        total_lines
    } else {
        total_lines.min(start_index.saturating_add(range.line_count))
    };
    SourceExcerpt {
        path: path.to_owned(),
        start_line: range.start_line,
        lines: lines[start_index..end_index]
            .iter()
            .map(|line| (*line).to_owned())
            .collect(),
        next: (end_index < total_lines).then_some(LineRange {
            start_line: end_index + 1,
            line_count: range.line_count,
        }),
    }
}

fn append_model_output(output: &mut String, excerpt: &SourceExcerpt) {
    if excerpt.lines.is_empty() {
        let _ = writeln!(
            output,
            "no lines; requested range starts at {}",
            excerpt.start_line
        );
        return;
    }
    for (offset, line) in excerpt.lines.iter().enumerate() {
        let line_number = excerpt.start_line + offset;
        let anchor = format_line_anchor(line_number, edit_line_hash(line));
        let _ = writeln!(output, "{anchor}{line}");
    }
    if let Some(next) = excerpt.next {
        let _ = writeln!(
            output,
            "next: {}+{}",
            next.start_line, next.line_count
        );
    }
}

pub(crate) fn format_line_anchor(line_number: usize, hash: u8) -> String {
    format!("{}{} ", line_number, line_anchor_word(hash))
}
