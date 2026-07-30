//! Semantic transcript projection into control-free display documents.

use harness_tool_api as tool;

use crate::{
    display::{ControlFree, DisplayDocument, RawDocumentBuilder, StyleId},
    domain::{MessageRole, ToolActivity, ToolActivityPhase, TranscriptPayload},
};

pub(super) fn project(
    payload: &TranscriptPayload,
    tool_expanded: bool,
) -> DisplayDocument<ControlFree> {
    let mut builder = RawDocumentBuilder::new();
    match payload {
        TranscriptPayload::Message { role, text } => {
            let (marker, style) = match role {
                MessageRole::User => ("» ", StyleId::User),
                MessageRole::Developer => ("» ", StyleId::Developer),
                MessageRole::Assistant => ("• ", StyleId::Assistant),
                MessageRole::Tool => ("⚙ ", StyleId::Plain),
            };
            builder.plain(marker, style, false);
            project_markdown(&mut builder, text.as_str(), style);
        }
        TranscriptPayload::PlainText(text) => project_plain_text(&mut builder, text.as_str()),
        TranscriptPayload::Thinking(text) => {
            builder.plain("∴ ", StyleId::Thinking, false);
            project_markdown(&mut builder, text.as_str(), StyleId::Thinking);
        }
        TranscriptPayload::Error { message, .. } => {
            builder.plain("× ", StyleId::Error, false);
            project_markdown(&mut builder, message.as_str(), StyleId::Error);
        }
        TranscriptPayload::ToolActivity(activity) => {
            project_tool_activity(&mut builder, activity, tool_expanded);
        }
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

fn project_tool_activity(
    builder: &mut RawDocumentBuilder,
    activity: &ToolActivity,
    expanded: bool,
) {
    if let tool::ToolInvocation::Inspect(invocation) = &activity.invocation {
        project_inspect_activity(builder, invocation, &activity.phase);
        return;
    }
    if let tool::ToolInvocation::EditFile(invocation) = &activity.invocation {
        project_edit_file_activity(builder, invocation, &activity.phase);
        return;
    }

    let status = activity.status();
    builder.plain(if expanded { "▾ " } else { "▸ " }, StyleId::Muted, false);
    builder.plain("⚙ ", StyleId::Plain, false);
    builder.plain(tool_label(activity.invocation.tool()), StyleId::Bold, false);
    match status {
        tool::ToolActivityStatus::Running => {
            builder.plain("  running", StyleId::Active, false);
        }
        tool::ToolActivityStatus::Successful => {}
        tool::ToolActivityStatus::PartiallySuccessful => {
            builder.plain("  partial", StyleId::Muted, false);
        }
        tool::ToolActivityStatus::Failed => {
            builder.plain("  failed", StyleId::Muted, false);
        }
        tool::ToolActivityStatus::Interrupted => {
            builder.plain("  interrupted", StyleId::Muted, false);
        }
    }
    builder.plain("  ", StyleId::Muted, false);
    builder.plain(invocation_summary(&activity.invocation), StyleId::Plain, true);

    if let ToolActivityPhase::Finished { outcome, .. } = &activity.phase {
        let style = match outcome {
            tool::ToolOutcome::Failed(_) => StyleId::Error,
            tool::ToolOutcome::Interrupted(_) => StyleId::Muted,
            _ => StyleId::Plain,
        };
        builder.plain("  ·  ", StyleId::Muted, false);
        builder.plain(outcome_summary(outcome), style, true);
    }

    if expanded {
        builder.line_break();
        builder.plain("  Invocation", StyleId::Heading, false);
        render_invocation(builder, &activity.invocation);
        if let ToolActivityPhase::Finished { outcome, .. } = &activity.phase {
            builder.line_break();
            builder.plain("  Result", StyleId::Heading, false);
            render_outcome(builder, outcome);
        }
    }
}

fn project_inspect_activity(
    builder: &mut RawDocumentBuilder,
    invocation: &tool::Prepared<tool::InspectRequest>,
    phase: &ToolActivityPhase,
) {
    let request = match invocation {
        tool::Prepared::Ready(request) => request,
        tool::Prepared::Rejected(rejection) => {
            builder.plain(&rejection.message, StyleId::Error, true);
            return;
        }
    };

    match phase {
        ToolActivityPhase::Running => {
            for (index, job) in request.jobs.iter().enumerate() {
                render_inspect_job_heading(builder, job, index > 0);
            }
        }
        ToolActivityPhase::Finished { outcome, .. } => match outcome {
            tool::ToolOutcome::Inspect(result) => {
                for (index, (job, outcome)) in
                    request.jobs.iter().zip(&result.jobs).enumerate()
                {
                    render_inspect_job_heading(builder, job, index > 0);
                    match outcome {
                        tool::InspectJobOutcome::Succeeded(success) => {
                            render_inspect_success(builder, success, Some(job))
                        }
                        tool::InspectJobOutcome::Failed(failure) => {
                            render_execution_failure(builder, failure)
                        }
                    }
                }
            }
            tool::ToolOutcome::Failed(failure) => {
                for (index, job) in request.jobs.iter().enumerate() {
                    render_inspect_job_heading(builder, job, index > 0);
                }
                render_execution_failure(builder, failure);
            }
            tool::ToolOutcome::Interrupted(interruption) => {
                for (index, job) in request.jobs.iter().enumerate() {
                    render_inspect_job_heading(builder, job, index > 0);
                }
                detail_line(builder, "", &interruption.message, StyleId::Muted);
            }
            _ => {}
        },
    }
}
fn project_edit_file_activity(
    builder: &mut RawDocumentBuilder,
    invocation: &tool::Prepared<tool::EditFileRequest>,
    phase: &ToolActivityPhase,
) {
    let request = match invocation {
        tool::Prepared::Ready(request) => request,
        tool::Prepared::Rejected(rejection) => {
            builder.plain(&rejection.message, StyleId::Error, true);
            return;
        }
    };

    match phase {
        ToolActivityPhase::Running => {
            for (index, operation) in request.operations.iter().enumerate() {
                render_edit_operation_heading(
                    builder,
                    operation,
                    index > 0,
                    StyleId::Bold,
                    StyleId::Plain,
                );
            }
        }
        ToolActivityPhase::Finished { outcome, .. } => match outcome {
            tool::ToolOutcome::EditFile(result) => {
                for (index, (operation, outcome)) in request
                    .operations
                    .iter()
                    .zip(&result.operations)
                    .enumerate()
                {
                    match outcome {
                        tool::EditOperationOutcome::Succeeded(change) => {
                            render_file_change(builder, change, index > 0);
                        }
                        tool::EditOperationOutcome::PartiallySucceeded {
                            change, message, ..
                        } => {
                            render_file_change(builder, change, index > 0);
                            detail_line(builder, "", message, StyleId::Error);
                        }
                        tool::EditOperationOutcome::Failed { message, .. } => {
                            render_edit_operation_heading(
                                builder,
                                operation,
                                index > 0,
                                StyleId::Muted,
                                StyleId::Muted,
                            );
                            detail_line(builder, "", message, StyleId::Error);
                        }
                    }
                }
            }
            tool::ToolOutcome::Failed(failure) => {
                for (index, operation) in request.operations.iter().enumerate() {
                    render_edit_operation_heading(
                        builder,
                        operation,
                        index > 0,
                        StyleId::Muted,
                        StyleId::Muted,
                    );
                }
                render_execution_failure(builder, failure);
            }
            tool::ToolOutcome::Interrupted(interruption) => {
                for (index, operation) in request.operations.iter().enumerate() {
                    render_edit_operation_heading(
                        builder,
                        operation,
                        index > 0,
                        StyleId::Muted,
                        StyleId::Muted,
                    );
                }
                detail_line(builder, "", &interruption.message, StyleId::Muted);
            }
            _ => {}
        },
    }
}

fn render_edit_operation_heading(
    builder: &mut RawDocumentBuilder,
    operation: &tool::EditOperation,
    separate: bool,
    verb_style: StyleId,
    path_style: StyleId,
) {
    if separate {
        builder.line_break();
    }

    match operation {
        tool::EditOperation::Edit { path, .. } => {
            builder.plain("Edit", verb_style, false);
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(path, path_style, true);
        }
        tool::EditOperation::Add { path, .. } => {
            builder.plain("Add", verb_style, false);
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(path, path_style, true);
        }
        tool::EditOperation::Remove { path } => {
            builder.plain("Remove", verb_style, false);
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(path, path_style, true);
        }
        tool::EditOperation::Move { from, to } => {
            builder.plain("Move", verb_style, false);
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(from, path_style, true);
            builder.plain(" → ", StyleId::Muted, false);
            builder.plain(to, path_style, true);
        }
    }
}

fn render_inspect_job_heading(
    builder: &mut RawDocumentBuilder,
    job: &tool::InspectJobRequest,
    separate: bool,
) {
    if separate {
        builder.line_break();
    }

    let verb = match job {
        tool::InspectJobRequest::Read(_) => "Read",
        tool::InspectJobRequest::List(_) => "List",
        tool::InspectJobRequest::Stat(_) => "Stat",
        tool::InspectJobRequest::Bytes(_) => "Bytes",
        tool::InspectJobRequest::ByteSearch(_) => "Byte search",
        tool::InspectJobRequest::Strings(_) => "Strings",
        tool::InspectJobRequest::Elf(_) => "ELF",
        tool::InspectJobRequest::Search(request) => match &request.mode {
            tool::InspectSearchMode::Content { .. } => "Search",
            tool::InspectSearchMode::Files => "Files",
        },
        tool::InspectJobRequest::Which(_) => "Which",
        tool::InspectJobRequest::Check(_) => "Check",
        tool::InspectJobRequest::Test(_) => "Test",
        tool::InspectJobRequest::Ps(_) => "Processes",
        tool::InspectJobRequest::Pwd => "Workspace",
    };
    builder.plain(verb, StyleId::Bold, false);

    match job {
        tool::InspectJobRequest::Read(request) => {
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(&request.path, StyleId::Plain, true);
            let ranges = line_ranges(&request.ranges);
            if !ranges.is_empty() {
                builder.plain(format!(":{ranges}"), StyleId::Muted, true);
            }
        }
        tool::InspectJobRequest::List(request) => {
            if !request.paths.is_empty() {
                builder.plain(" ", StyleId::Muted, false);
                builder.plain(request.paths.join(", "), StyleId::Plain, true);
            }
            if request.depth != 1 {
                builder.plain(format!(" · depth {}", request.depth), StyleId::Muted, true);
            }
            if request.exact {
                builder.plain(" · exact", StyleId::Muted, true);
            }
        }
        tool::InspectJobRequest::Stat(request) => {
            if !request.paths.is_empty() {
                builder.plain(" ", StyleId::Muted, false);
                builder.plain(request.paths.join(", "), StyleId::Plain, true);
            }
            if request.metadata {
                builder.plain(" · metadata", StyleId::Muted, true);
            }
        }
        tool::InspectJobRequest::Bytes(request) => {
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(&request.path, StyleId::Plain, true);
            builder.plain(
                format!(" {}+{}", request.offset, request.length),
                StyleId::Muted,
                true,
            );
        }
        tool::InspectJobRequest::ByteSearch(request) => {
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(&request.path, StyleId::Plain, true);
            builder.plain(" · ", StyleId::Muted, false);
            builder.plain(hex_bytes(&request.pattern), StyleId::Code, true);
        }
        tool::InspectJobRequest::Strings(request) => {
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(&request.path, StyleId::Plain, true);
            if let Some(literal) = &request.literal {
                builder.plain(" · ", StyleId::Muted, false);
                builder.plain(format!("{literal:?}"), StyleId::Code, true);
            }
        }
        tool::InspectJobRequest::Elf(request) => {
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(&request.path, StyleId::Plain, true);
            builder.plain(" · ", StyleId::Muted, false);
            builder.plain(elf_query_label(&request.query), StyleId::Plain, true);
        }
        tool::InspectJobRequest::Search(request) => {
            if let tool::InspectSearchMode::Content {
                patterns,
                literal,
                case,
                files_with_matches,
            } = &request.mode
            {
                builder.plain(" ", StyleId::Muted, false);
                builder.plain(format!("{:?}", patterns.join(" | ")), StyleId::Plain, true);
                if *literal {
                    builder.plain(" · fixed", StyleId::Muted, true);
                }
                match case {
                    tool::SearchCase::Smart => {}
                    tool::SearchCase::Sensitive => {
                        builder.plain(" · case-sensitive", StyleId::Muted, true);
                    }
                    tool::SearchCase::Insensitive => {
                        builder.plain(" · ignore-case", StyleId::Muted, true);
                    }
                }
                if *files_with_matches {
                    builder.plain(" · files only", StyleId::Muted, true);
                }
            }
            let roots = if request.roots.is_empty() {
                ".".to_owned()
            } else {
                request.roots.join(", ")
            };
            builder.plain(" in ", StyleId::Muted, false);
            builder.plain(roots, StyleId::Plain, true);
            if !request.includes.is_empty() {
                builder.plain(" · include ", StyleId::Muted, false);
                builder.plain(request.includes.join(", "), StyleId::Plain, true);
            }
            if !request.excludes.is_empty() {
                builder.plain(" · exclude ", StyleId::Muted, false);
                builder.plain(request.excludes.join(", "), StyleId::Plain, true);
            }
        }
        tool::InspectJobRequest::Which(request) => {
            builder.plain(" ", StyleId::Muted, false);
            builder.plain(&request.query, StyleId::Plain, true);
        }
        tool::InspectJobRequest::Check(request) => {
            if !request.cargo_arguments.is_empty() {
                builder.plain(" ", StyleId::Muted, false);
                builder.plain(request.cargo_arguments.join(" "), StyleId::Plain, true);
            }
        }
        tool::InspectJobRequest::Test(request) => {
            if !request.cargo_arguments.is_empty() {
                builder.plain(" ", StyleId::Muted, false);
                builder.plain(request.cargo_arguments.join(" "), StyleId::Plain, true);
            }
            if !request.filters.is_empty() {
                builder.plain(" · ", StyleId::Muted, false);
                builder.plain(request.filters.join(", "), StyleId::Plain, true);
            }
            if !request.libtest_arguments.is_empty() {
                builder.plain(" · libtest ", StyleId::Muted, false);
                builder.plain(request.libtest_arguments.join(" "), StyleId::Plain, true);
            }
        }
        tool::InspectJobRequest::Ps(request) => {
            if let Some(filter) = &request.filter {
                builder.plain(" ", StyleId::Muted, false);
                builder.plain(filter, StyleId::Plain, true);
            }
        }
        tool::InspectJobRequest::Pwd => {}
    }
}

fn tool_label(tool: tool::BuiltInTool) -> &'static str {
    match tool {
        tool::BuiltInTool::Inspect => "Inspect",
        tool::BuiltInTool::EditFile => "Edit file",
        tool::BuiltInTool::TerminalOpen => "Terminal open",
        tool::BuiltInTool::TerminalRead => "Terminal read",
        tool::BuiltInTool::TerminalWrite => "Terminal write",
        tool::BuiltInTool::Goal => "Goal",
    }
}

fn invocation_summary(invocation: &tool::ToolInvocation) -> String {
    match invocation {
        tool::ToolInvocation::Inspect(tool::Prepared::Ready(request)) => {
            if let [job] = request.jobs.as_slice() {
                inspect_job_summary(job)
            } else {
                format!("{} inspect jobs", request.jobs.len())
            }
        }
        tool::ToolInvocation::EditFile(tool::Prepared::Ready(request)) => {
            if let [operation] = request.operations.as_slice() {
                edit_operation_summary(operation)
            } else {
                format!("{} filesystem operations", request.operations.len())
            }
        }
        tool::ToolInvocation::TerminalOpen(tool::Prepared::Ready(request)) => {
            format!("run {}", request.command.lines().next().unwrap_or_default())
        }
        tool::ToolInvocation::TerminalRead(tool::Prepared::Ready(request)) => {
            format!("read terminal {}", request.terminal_id)
        }
        tool::ToolInvocation::TerminalWrite(tool::Prepared::Ready(request)) => {
            format!("write to terminal {}", request.terminal_id)
        }
        tool::ToolInvocation::Goal(tool::Prepared::Ready(_)) => "complete persisted goal".to_owned(),
        tool::ToolInvocation::Inspect(tool::Prepared::Rejected(_))
        | tool::ToolInvocation::EditFile(tool::Prepared::Rejected(_))
        | tool::ToolInvocation::TerminalOpen(tool::Prepared::Rejected(_))
        | tool::ToolInvocation::TerminalRead(tool::Prepared::Rejected(_))
        | tool::ToolInvocation::TerminalWrite(tool::Prepared::Rejected(_))
        | tool::ToolInvocation::Goal(tool::Prepared::Rejected(_)) => "invalid input".to_owned(),
    }
}

fn inspect_job_summary(job: &tool::InspectJobRequest) -> String {
    match job {
        tool::InspectJobRequest::Read(request) => {
            format!("read {} {}", request.path, line_ranges(&request.ranges))
        }
        tool::InspectJobRequest::List(request) => {
            format!("list {}", request.paths.join(", "))
        }
        tool::InspectJobRequest::Stat(request) => {
            format!("stat {}", request.paths.join(", "))
        }
        tool::InspectJobRequest::Bytes(request) => {
            format!("bytes {} {}+{}", request.path, request.offset, request.length)
        }
        tool::InspectJobRequest::ByteSearch(request) => {
            format!("byte-search {} ({} bytes)", request.path, request.pattern.len())
        }
        tool::InspectJobRequest::Strings(request) => format!("strings {}", request.path),
        tool::InspectJobRequest::Elf(request) => {
            format!("elf {} {}", request.path, elf_query_label(&request.query))
        }
        tool::InspectJobRequest::Search(request) => match &request.mode {
            tool::InspectSearchMode::Content { patterns, .. } => {
                format!("search {}", patterns.join(" | "))
            }
            tool::InspectSearchMode::Files => "list searchable files".to_owned(),
        },
        tool::InspectJobRequest::Which(request) => format!("which {}", request.query),
        tool::InspectJobRequest::Check(request) => {
            format!("cargo check {}", request.cargo_arguments.join(" "))
        }
        tool::InspectJobRequest::Test(request) => {
            format!("cargo test {}", request.filters.join(" "))
        }
        tool::InspectJobRequest::Ps(request) => request
            .filter
            .as_ref()
            .map_or_else(|| "list processes".to_owned(), |filter| format!("ps {filter}")),
        tool::InspectJobRequest::Pwd => "print workspace directory".to_owned(),
    }
}

fn edit_operation_summary(operation: &tool::EditOperation) -> String {
    match operation {
        tool::EditOperation::Add { path, .. } => format!("add {path}"),
        tool::EditOperation::Remove { path } => format!("remove {path}"),
        tool::EditOperation::Move { from, to } => format!("move {from} to {to}"),
        tool::EditOperation::Edit { path, segments } => {
            format!("edit {path} ({} segments)", segments.len())
        }
    }
}

fn outcome_summary(outcome: &tool::ToolOutcome) -> String {
    match outcome {
        tool::ToolOutcome::Inspect(result) => {
            let succeeded = result
                .jobs
                .iter()
                .filter(|job| matches!(job, tool::InspectJobOutcome::Succeeded(_)))
                .count();
            let failed = result.jobs.len().saturating_sub(succeeded);
            format!("{succeeded} succeeded, {failed} failed")
        }
        tool::ToolOutcome::EditFile(result) => {
            let mut changes = 0usize;
            let mut additions = 0usize;
            let mut deletions = 0usize;
            for operation in &result.operations {
                let change = match operation {
                    tool::EditOperationOutcome::Succeeded(change)
                    | tool::EditOperationOutcome::PartiallySucceeded { change, .. } => Some(change),
                    tool::EditOperationOutcome::Failed { .. } => None,
                };
                if let Some(change) = change {
                    changes += 1;
                    additions += change.additions;
                    deletions += change.deletions;
                }
            }
            format!("{changes} files changed, +{additions} -{deletions}")
        }
        tool::ToolOutcome::TerminalOpen(result)
        | tool::ToolOutcome::TerminalRead(result)
        | tool::ToolOutcome::TerminalWrite(result) => {
            format!("terminal {} {}", result.terminal_id, terminal_state(result.state))
        }
        tool::ToolOutcome::Goal(_) => "goal completed".to_owned(),
        tool::ToolOutcome::Failed(failure) => failure.message.clone(),
        tool::ToolOutcome::Interrupted(interruption) => interruption.message.clone(),
    }
}

fn render_invocation(builder: &mut RawDocumentBuilder, invocation: &tool::ToolInvocation) {
    match invocation {
        tool::ToolInvocation::Inspect(tool::Prepared::Ready(request)) => {
            for job in &request.jobs {
                detail_line(builder, "• ", &inspect_job_summary(job), StyleId::Plain);
                render_inspect_job_options(builder, job);
            }
        }
        tool::ToolInvocation::EditFile(tool::Prepared::Ready(request)) => {
            for operation in &request.operations {
                detail_line(
                    builder,
                    "• ",
                    &edit_operation_summary(operation),
                    StyleId::Plain,
                );
                render_edit_operation(builder, operation);
            }
        }
        tool::ToolInvocation::TerminalOpen(tool::Prepared::Ready(request)) => {
            detail_line(builder, "command  ", &request.command, StyleId::Code);
            if let Some(workdir) = &request.workdir {
                detail_line(builder, "workdir  ", workdir, StyleId::Plain);
            }
            detail_line(
                builder,
                "size     ",
                &format!("{}x{}", request.cols, request.rows),
                StyleId::Muted,
            );
        }
        tool::ToolInvocation::TerminalRead(tool::Prepared::Ready(request)) => {
            detail_line(
                builder,
                "terminal ",
                &request.terminal_id.to_string(),
                StyleId::Plain,
            );
            detail_line(
                builder,
                "poll     ",
                &format!("{} ms", request.poll_after_ms),
                StyleId::Muted,
            );
        }
        tool::ToolInvocation::TerminalWrite(tool::Prepared::Ready(request)) => {
            detail_line(
                builder,
                "terminal ",
                &request.terminal_id.to_string(),
                StyleId::Plain,
            );
            detail_line(builder, "input    ", &request.input, StyleId::Code);
        }
        tool::ToolInvocation::Goal(tool::Prepared::Ready(_)) => {
            detail_line(builder, "action   ", "complete", StyleId::Plain);
        }
        tool::ToolInvocation::Inspect(tool::Prepared::Rejected(rejection))
        | tool::ToolInvocation::EditFile(tool::Prepared::Rejected(rejection))
        | tool::ToolInvocation::TerminalOpen(tool::Prepared::Rejected(rejection))
        | tool::ToolInvocation::TerminalRead(tool::Prepared::Rejected(rejection))
        | tool::ToolInvocation::TerminalWrite(tool::Prepared::Rejected(rejection))
        | tool::ToolInvocation::Goal(tool::Prepared::Rejected(rejection)) => {
            detail_line(builder, "error    ", &rejection.message, StyleId::Error);
        }
    }
}

fn render_inspect_job_options(
    builder: &mut RawDocumentBuilder,
    job: &tool::InspectJobRequest,
) {
    match job {
        tool::InspectJobRequest::Read(_) | tool::InspectJobRequest::Pwd => {}
        tool::InspectJobRequest::List(request) => {
            detail_line(
                builder,
                "    options ",
                &format!(
                    "depth {}, limit {}, exact {}",
                    request.depth, request.limit, request.exact
                ),
                StyleId::Muted,
            );
        }
        tool::InspectJobRequest::Stat(request) => {
            detail_line(
                builder,
                "    metadata ",
                if request.metadata { "yes" } else { "no" },
                StyleId::Muted,
            );
        }
        tool::InspectJobRequest::Bytes(_)
        | tool::InspectJobRequest::ByteSearch(_)
        | tool::InspectJobRequest::Which(_) => {}
        tool::InspectJobRequest::Strings(request) => {
            if let Some(literal) = &request.literal {
                detail_line(builder, "    filter  ", literal, StyleId::Muted);
            }
            detail_line(
                builder,
                "    limit   ",
                &request.maximum_results.to_string(),
                StyleId::Muted,
            );
        }
        tool::InspectJobRequest::Elf(_) => {}
        tool::InspectJobRequest::Search(request) => {
            detail_line(
                builder,
                "    roots   ",
                &request.roots.join(", "),
                StyleId::Muted,
            );
            if !request.includes.is_empty() {
                detail_line(
                    builder,
                    "    include ",
                    &request.includes.join(", "),
                    StyleId::Muted,
                );
            }
            if !request.excludes.is_empty() {
                detail_line(
                    builder,
                    "    exclude ",
                    &request.excludes.join(", "),
                    StyleId::Muted,
                );
            }
        }
        tool::InspectJobRequest::Check(request) => {
            detail_line(
                builder,
                "    cargo   ",
                &request.cargo_arguments.join(" "),
                StyleId::Muted,
            );
        }
        tool::InspectJobRequest::Test(request) => {
            detail_line(
                builder,
                "    cargo   ",
                &request.cargo_arguments.join(" "),
                StyleId::Muted,
            );
            detail_line(
                builder,
                "    libtest ",
                &request.libtest_arguments.join(" "),
                StyleId::Muted,
            );
        }
        tool::InspectJobRequest::Ps(request) => {
            if let Some(filter) = &request.filter {
                detail_line(builder, "    filter  ", filter, StyleId::Muted);
            }
        }
    }
}

fn render_edit_operation(builder: &mut RawDocumentBuilder, operation: &tool::EditOperation) {
    match operation {
        tool::EditOperation::Add { body, .. } => render_body(builder, body),
        tool::EditOperation::Remove { .. } | tool::EditOperation::Move { .. } => {}
        tool::EditOperation::Edit { segments, .. } => {
            for segment in segments {
                match segment {
                    tool::EditSegment::Replace { start, end, body } => {
                        detail_line(
                            builder,
                            "    replace ",
                            &format!("{}-{}", start.line_number, end.line_number),
                            StyleId::Muted,
                        );
                        render_body(builder, body);
                    }
                    tool::EditSegment::Delete { start, end } => detail_line(
                        builder,
                        "    delete  ",
                        &format!("{}-{}", start.line_number, end.line_number),
                        StyleId::Muted,
                    ),
                    tool::EditSegment::Insert {
                        position,
                        anchor,
                        body,
                    } => {
                        let position = match position {
                            tool::EditInsertPosition::Before => "before",
                            tool::EditInsertPosition::After => "after",
                            tool::EditInsertPosition::Append => "append",
                        };
                        detail_line(
                            builder,
                            "    insert  ",
                            &format!("{position} {}", anchor.line_number),
                            StyleId::Muted,
                        );
                        render_body(builder, body);
                    }
                }
            }
        }
    }
}

fn render_body(builder: &mut RawDocumentBuilder, body: &str) {
    for line in body.lines() {
        detail_line(builder, "      │ ", line, StyleId::Code);
    }
}

fn render_outcome(builder: &mut RawDocumentBuilder, outcome: &tool::ToolOutcome) {
    match outcome {
        tool::ToolOutcome::Inspect(result) => {
            for job in &result.jobs {
                match job {
                    tool::InspectJobOutcome::Succeeded(success) => {
                        render_inspect_success(builder, success, None)
                    }
                    tool::InspectJobOutcome::Failed(failure) => {
                        render_execution_failure(builder, failure)
                    }
                }
            }
        }
        tool::ToolOutcome::EditFile(result) => {
            for operation in &result.operations {
                match operation {
                    tool::EditOperationOutcome::Succeeded(change) => {
                        render_file_change(builder, change, true)
                    }
                    tool::EditOperationOutcome::PartiallySucceeded {
                        change, message, ..
                    } => {
                        render_file_change(builder, change, true);
                        detail_line(builder, "", message, StyleId::Error);
                    }
                    tool::EditOperationOutcome::Failed { message, .. } => {
                        detail_line(builder, "", message, StyleId::Error)
                    }
            }
        }
        }
        tool::ToolOutcome::TerminalOpen(result)
        | tool::ToolOutcome::TerminalRead(result)
        | tool::ToolOutcome::TerminalWrite(result) => render_terminal_result(builder, result),
        tool::ToolOutcome::Goal(_) => {
            detail_line(builder, "status   ", "completed", StyleId::DiffAdded);
        }
        tool::ToolOutcome::Failed(failure) => render_execution_failure(builder, failure),
        tool::ToolOutcome::Interrupted(interruption) => {
            detail_line(
                builder,
                "interrupted ",
                &interruption.message,
                StyleId::Muted,
            );
        }
    }
}

fn render_inspect_success(
    builder: &mut RawDocumentBuilder,
    success: &tool::InspectJobSuccess,
    job: Option<&tool::InspectJobRequest>,
) {
    match success {
        tool::InspectJobSuccess::Read(_) => {}
        tool::InspectJobSuccess::List(result) => {
            for entry in &result.entries {
                let kind = inspect_path_kind(entry.kind);
                let size = entry
                    .line_count
                    .map(|count| format!("{count} lines"))
                    .or_else(|| entry.byte_count.map(|count| format!("{count} bytes")))
                    .unwrap_or_else(|| kind.to_owned());
                builder.line_break();
                builder.plain("    ", StyleId::Muted, false);
                builder.plain(&entry.path, StyleId::Plain, true);
                if let Some(target) = &entry.symlink_target {
                    builder.plain(" -> ", StyleId::Muted, false);
                    builder.plain(target, StyleId::Plain, true);
                }
                builder.plain(format!(" ({size})"), StyleId::Muted, true);
            }
            if result.entries.is_empty() {
                detail_line(builder, "", "no entries", StyleId::Muted);
            }
            if result.truncated {
                detail_line(builder, "… ", "more entries not shown", StyleId::Muted);
            }
        }
        tool::InspectJobSuccess::Stat(result) => {
            for entry in &result.entries {
                builder.line_break();
                builder.plain("    ", StyleId::Muted, false);
                builder.plain(&entry.path, StyleId::Plain, true);
                builder.plain(
                    format!(
                        "  {}  {} bytes  mode {:o}  modified {}",
                        inspect_path_kind(entry.kind),
                        entry.byte_count,
                        entry.permissions,
                        entry.modified_unix_seconds
                    ),
                    StyleId::Muted,
                    true,
                );
                if let Some(metadata) = &entry.metadata {
                    detail_line(
                        builder,
                        "metadata ",
                        &format!(
                            "uid {} gid {} inode {} device {} links {} blocks {}",
                            metadata.uid,
                            metadata.gid,
                            metadata.inode,
                            metadata.device,
                            metadata.links,
                            metadata.blocks
                        ),
                        StyleId::Muted,
                    );
                }
            }
        }
        tool::InspectJobSuccess::Bytes(result) => {
            detail_line(
                builder,
                "",
                &format!(
                    "{} bytes at offset {} of {}",
                    result.bytes.len(),
                    result.offset,
                    result.file_size
                ),
                StyleId::Muted,
            );
            detail_line(builder, "", &hex_bytes(&result.bytes), StyleId::Code);
        }
        tool::InspectJobSuccess::ByteSearch(result) => {
            if result.offsets.is_empty() {
                detail_line(builder, "", "no matches", StyleId::Muted);
            } else {
                detail_line(
                    builder,
                    "",
                    &result
                        .offsets
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                    StyleId::Code,
                );
                let omitted = result.total_matches.saturating_sub(result.offsets.len());
                if omitted > 0 {
                    detail_line(builder, "… ", &format!("{omitted} more matches"), StyleId::Muted);
                }
            }
        }
        tool::InspectJobSuccess::Strings(result) => {
            for found in &result.matches {
                detail_line(
                    builder,
                    &format!("{:>8} ", found.offset),
                    &found.text,
                    StyleId::Code,
                );
            }
            if result.matches.is_empty() {
                detail_line(builder, "", "no strings", StyleId::Muted);
            }
            let omitted = result.total_matches.saturating_sub(result.matches.len());
            if omitted > 0 {
                detail_line(builder, "… ", &format!("{omitted} more strings"), StyleId::Muted);
            }
        }
        tool::InspectJobSuccess::Elf(result) => {
            if let Some(summary) = &result.summary {
                detail_line(
                    builder,
                    "",
                    &format!(
                        "{}-bit {} {} {} entry {:#x}",
                        summary.bits,
                        summary.architecture,
                        summary.endianness,
                        summary.kind,
                        summary.entry
                    ),
                    StyleId::Plain,
                );
            }
            for entry in &result.entries {
                render_elf_entry(builder, entry);
            }
            if result.summary.is_none() && result.entries.is_empty() {
                detail_line(builder, "", "no results", StyleId::Muted);
            }
        }
        tool::InspectJobSuccess::Search(result) => {
            let mode = job.and_then(|job| match job {
                tool::InspectJobRequest::Search(request) => Some(&request.mode),
                _ => None,
            });
            render_inspect_search(builder, result, mode);
        }
        tool::InspectJobSuccess::Which(result) => {
            for found in &result.matches {
                builder.line_break();
                builder.plain("    ", StyleId::Muted, false);
                builder.plain(&found.name, StyleId::Plain, true);
                builder.plain("  ", StyleId::Muted, false);
                builder.plain(&found.path, StyleId::Plain, true);
            }
            if result.matches.is_empty() {
                detail_line(builder, "", "not found", StyleId::Muted);
            }
        }
        tool::InspectJobSuccess::Check(result) => {
            if result.diagnostics.is_empty() && result.failure.is_none() {
                detail_line(builder, "", "no diagnostics", StyleId::Muted);
            }
            for diagnostic in &result.diagnostics {
                let level = match diagnostic.level {
                    tool::InspectDiagnosticLevel::Error => "error",
                    tool::InspectDiagnosticLevel::Warning => "warning",
                    tool::InspectDiagnosticLevel::Note => "note",
                    tool::InspectDiagnosticLevel::Help => "help",
                };
                let location = diagnostic.path.as_ref().map_or_else(String::new, |path| {
                    format!(
                        "{}:{}:{} ",
                        path,
                        diagnostic.line.unwrap_or(0),
                        diagnostic.column.unwrap_or(0)
                    )
                });
                detail_line(
                    builder,
                    level,
                    &format!("{location}{}", diagnostic.message),
                    if diagnostic.level == tool::InspectDiagnosticLevel::Error {
                        StyleId::Error
                    } else {
                        StyleId::Plain
                    },
                );
                if let Some(label) = &diagnostic.label {
                    detail_line(builder, "label    ", label, StyleId::Muted);
                }
            }
            if let Some(failure) = &result.failure {
                detail_line(builder, "failure  ", failure, StyleId::Error);
            }
        }
        tool::InspectJobSuccess::Test(result) => {
            detail_line(
                builder,
                "",
                &format!(
                    "{} passed, {} failed, {} ignored",
                    result.passed, result.failed, result.ignored
                ),
                if result.failed == 0 {
                    StyleId::Plain
                } else {
                    StyleId::Error
                },
            );
            for failure in &result.failures {
                detail_line(
                    builder,
                    "failure  ",
                    failure.name.as_deref().unwrap_or("unnamed test"),
                    StyleId::Error,
                );
                render_body(builder, &failure.output);
            }
            if let Some(failure) = &result.execution_failure {
                detail_line(builder, "execution ", failure, StyleId::Error);
            }
        }
        tool::InspectJobSuccess::Ps(result) => {
            for process in &result.processes {
                detail_line(
                    builder,
                    "",
                    &format!(
                        "{} pid {} cpu {}% mem {}% {}",
                        process.user,
                        process.pid,
                        process.cpu_percent,
                        process.memory_percent,
                        process.command
                    ),
                    StyleId::Plain,
                );
            }
            if result.processes.is_empty() {
                detail_line(builder, "", "no processes", StyleId::Muted);
            }
        }
        tool::InspectJobSuccess::Pwd { path } => {
            detail_line(builder, "", path, StyleId::Plain);
        }
    }
}


fn render_inspect_search(
    builder: &mut RawDocumentBuilder,
    result: &tool::InspectSearchResult,
    mode: Option<&tool::InspectSearchMode>,
) {
    let noun = match mode {
        Some(tool::InspectSearchMode::Files)
        | Some(tool::InspectSearchMode::Content {
            files_with_matches: true,
            ..
        }) => "files",
        Some(tool::InspectSearchMode::Content { .. }) | None => "matches",
    };
    if result.total_matches == 0 {
        detail_line(
            builder,
            "",
            if noun == "files" {
                "no results"
            } else {
                "no matches"
            },
            StyleId::Muted,
        );
    } else {
        detail_line(
            builder,
            "",
            &format!("{} {noun}", result.total_matches),
            StyleId::Muted,
        );
    }
}

fn render_elf_entry(builder: &mut RawDocumentBuilder, entry: &tool::InspectElfEntry) {
    match entry {
        tool::InspectElfEntry::Section {
            name,
            file_offset,
            file_size,
            virtual_address,
            size,
        } => detail_line(
            builder,
            "section  ",
            &format!(
                "{name} file {:?}+{:?} virtual {virtual_address:#x}+{size}",
                file_offset, file_size
            ),
            StyleId::Plain,
        ),
        tool::InspectElfEntry::Segment {
            name,
            file_offset,
            file_size,
            virtual_address,
            size,
        } => detail_line(
            builder,
            "segment  ",
            &format!(
                "{} file {file_offset:#x}+{file_size} virtual {virtual_address:#x}+{size}",
                name.as_deref().unwrap_or("-")
            ),
            StyleId::Plain,
        ),
        tool::InspectElfEntry::Symbol {
            name,
            virtual_address,
            size,
        } => detail_line(
            builder,
            "symbol   ",
            &format!("{name} {virtual_address:#x}+{size}"),
            StyleId::Plain,
        ),
        tool::InspectElfEntry::Relocation {
            section,
            offset,
            target,
            kind,
        } => detail_line(
            builder,
            "reloc    ",
            &format!("{section} {offset:#x} {kind} {target}"),
            StyleId::Plain,
        ),
        tool::InspectElfEntry::Mapping {
            file_offset,
            virtual_address,
        } => detail_line(
            builder,
            "mapping  ",
            &format!("{file_offset:#x} -> {virtual_address:#x}"),
            StyleId::Plain,
        ),
        tool::InspectElfEntry::Dynamic { name, value } => detail_line(
            builder,
            "dynamic  ",
            &format!("{name} {value}"),
            StyleId::Plain,
        ),
        tool::InspectElfEntry::Notice(message) => {
            detail_line(builder, "notice   ", message, StyleId::Muted)
        }
    }
}

fn render_file_change(
    builder: &mut RawDocumentBuilder,
    change: &tool::FileChange,
    separate: bool,
) {
    if separate {
        builder.line_break();
    }

    match change.kind {
        tool::FileChangeKind::Added => {
            builder.plain("Added", StyleId::Bold, false);
            builder.plain(" ", StyleId::Plain, false);
            builder.plain(
                change.new_path.as_deref().unwrap_or_default(),
                StyleId::Plain,
                true,
            );
        }
        tool::FileChangeKind::Removed => {
            builder.plain("Removed", StyleId::Bold, false);
            builder.plain(" ", StyleId::Plain, false);
            builder.plain(
                change.old_path.as_deref().unwrap_or_default(),
                StyleId::Plain,
                true,
            );
        }
        tool::FileChangeKind::Modified => {
            builder.plain("Modified", StyleId::Bold, false);
            builder.plain(" ", StyleId::Plain, false);
            builder.plain(
                change
                    .new_path
                    .as_deref()
                    .or(change.old_path.as_deref())
                    .unwrap_or_default(),
                StyleId::Plain,
                true,
            );
        }
        tool::FileChangeKind::Moved => {
            builder.plain("Moved", StyleId::Bold, false);
            builder.plain(" ", StyleId::Plain, false);
            builder.plain(
                change.old_path.as_deref().unwrap_or_default(),
                StyleId::Plain,
                true,
            );
            builder.plain(" → ", StyleId::Plain, false);
            builder.plain(
                change.new_path.as_deref().unwrap_or_default(),
                StyleId::Plain,
                true,
            );
        }
    }
    if change.additions > 0 {
        builder.plain("  ", StyleId::Muted, false);
        builder.plain(
            format!("+{}", change.additions),
            StyleId::DiffAdded,
            true,
        );
    }
    if change.deletions > 0 {
        builder.plain(
            if change.additions > 0 { " " } else { "  " },
            StyleId::Muted,
            false,
        );
        builder.plain(
            format!("-{}", change.deletions),
            StyleId::DiffRemoved,
            true,
        );
    }
    for hunk in &change.hunks {
        for line in &hunk.lines {
            builder.line_break();
            let (prefix, style) = match line.kind {
                tool::DiffLineKind::Context => (" ", StyleId::DiffContext),
                tool::DiffLineKind::Added => ("+", StyleId::DiffAdded),
                tool::DiffLineKind::Removed => ("-", StyleId::DiffRemoved),
            };
            builder.plain(format!("    {prefix} "), style, false);
            builder.plain(&line.text, style, true);
        }
    }
}

fn render_terminal_result(builder: &mut RawDocumentBuilder, result: &tool::TerminalResult) {
    detail_line(
        builder,
        "terminal ",
        &format!("{} {}", result.terminal_id, terminal_state(result.state)),
        StyleId::Plain,
    );
    if result.earlier_output_omitted > 0 {
        detail_line(
            builder,
            "omitted  ",
            &format!("{} earlier bytes", result.earlier_output_omitted),
            StyleId::Muted,
        );
    }
    if !result.output.is_empty() {
        builder.line_break();
        builder.plain("    ", StyleId::Muted, false);
        builder.terminal(&result.output, StyleId::Plain, true);
    }
}

fn render_execution_failure(
    builder: &mut RawDocumentBuilder,
    failure: &tool::ToolExecutionFailure,
) {
    let category = match failure.category {
        tool::ToolFailureCategory::InvalidInput => "invalid input",
        tool::ToolFailureCategory::TimedOut => "timed out",
        tool::ToolFailureCategory::Cancelled => "cancelled",
        tool::ToolFailureCategory::Execution => "execution",
    };
    detail_line(builder, category, &failure.message, StyleId::Error);
}

fn detail_line(
    builder: &mut RawDocumentBuilder,
    label: &str,
    value: &str,
    style: StyleId,
) {
    builder.line_break();
    builder.plain("    ", StyleId::Muted, false);
    builder.plain(label, StyleId::Muted, false);
    builder.plain(value, style, true);
}

fn line_ranges(ranges: &[tool::LineRange]) -> String {
    ranges
        .iter()
        .map(|range| {
            let end = range
                .start_line
                .saturating_add(range.line_count)
                .saturating_sub(1);
            format!("{}-{end}", range.start_line)
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn elf_query_label(query: &tool::InspectElfQuery) -> String {
    match query {
        tool::InspectElfQuery::Summary => "summary".to_owned(),
        tool::InspectElfQuery::Sections => "sections".to_owned(),
        tool::InspectElfQuery::Segments => "segments".to_owned(),
        tool::InspectElfQuery::Symbols(filter) => {
            format!("symbols {}", filter.as_deref().unwrap_or_default())
        }
        tool::InspectElfQuery::Relocations(filter) => {
            format!("relocations {}", filter.as_deref().unwrap_or_default())
        }
        tool::InspectElfQuery::Dynamic(filter) => {
            format!("dynamic {}", filter.as_deref().unwrap_or_default())
        }
        tool::InspectElfQuery::Address(address) => format!("address {address:#x}"),
        tool::InspectElfQuery::Offset(offset) => format!("offset {offset:#x}"),
    }
}

fn inspect_path_kind(kind: tool::InspectPathKind) -> &'static str {
    match kind {
        tool::InspectPathKind::File => "file",
        tool::InspectPathKind::Directory => "directory",
        tool::InspectPathKind::Symlink => "symlink",
        tool::InspectPathKind::Other => "other",
    }
}

fn terminal_state(state: tool::TerminalProcessState) -> String {
    match state {
        tool::TerminalProcessState::Running => "running".to_owned(),
        tool::TerminalProcessState::Exited { code } => format!("exited {code}"),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
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
            project_markdown(builder, body, style);
            return;
        }
    }
    let style = if text.starts_with("error:") || text.starts_with("responses actor error:") {
        StyleId::Error
    } else {
        StyleId::Muted
    };
    builder.plain("· ", style, false);
    project_markdown(builder, text, style);
}

fn project_markdown(builder: &mut RawDocumentBuilder, text: &str, base_style: StyleId) {
    for (line_index, line) in text.split_inclusive('\n').enumerate() {
        if line_index > 0 {
            builder.line_break();
        }

        let line = line.trim_end_matches(['\r', '\n']);
        let line_style = if line.starts_with('#') {
            StyleId::Heading
        } else {
            base_style
        };
        project_markdown_line(builder, line, line_style);
    }
}

fn project_markdown_line(builder: &mut RawDocumentBuilder, line: &str, base_style: StyleId) {
    let bytes = line.as_bytes();
    let mut cursor = 0usize;
    let mut plain_start = 0usize;

    while cursor < bytes.len() {
        let delimiter_len = match bytes[cursor] {
            b'`' => 1,
            b'*' if bytes.get(cursor + 1) == Some(&b'*') => 2,
            b'*' => 1,
            _ => {
                cursor += 1;
                continue;
            }
        };
        let delimiter = &line[cursor..cursor + delimiter_len];
        let content_start = cursor + delimiter_len;
        let Some(close_offset) = line[content_start..].find(delimiter) else {
            cursor = content_start;
            continue;
        };
        let close_start = content_start + close_offset;
        if close_start == content_start {
            cursor = close_start + delimiter_len;
            continue;
        }

        if plain_start < cursor {
            builder.plain(&line[plain_start..cursor], base_style, true);
        }
        builder.plain(delimiter, base_style, true);
        let nested_style = match delimiter {
            "`" if base_style == StyleId::Heading => StyleId::HeadingCode,
            "`" => StyleId::Code,
            "**" => bold_style(base_style),
            "*" => StyleId::Italic,
            _ => unreachable!("delimiter is selected above"),
        };
        builder.plain(&line[content_start..close_start], nested_style, true);
        builder.plain(delimiter, base_style, true);

        cursor = close_start + delimiter_len;
        plain_start = cursor;
    }

    if plain_start < line.len() {
        builder.plain(&line[plain_start..], base_style, true);
    }
}

fn bold_style(base_style: StyleId) -> StyleId {
    match base_style {
        StyleId::Assistant => StyleId::AssistantBold,
        StyleId::User => StyleId::UserBold,
        StyleId::Developer => StyleId::DeveloperBold,
        StyleId::Tool => StyleId::ToolBold,
        StyleId::Thinking | StyleId::Muted => StyleId::ThinkingBold,
        StyleId::Error => StyleId::ErrorBold,
        StyleId::Heading => StyleId::Heading,
        _ => StyleId::Bold,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ExternalText;

    #[test]
    fn projection_never_selects_visual_markers() {
        let document = project(
            &TranscriptPayload::Message {
                role: MessageRole::Assistant,
                text: ExternalText::new("hello"),
            },
            false,
        );
        assert_eq!(document.selectable_text(), "hello");
    }

    #[test]
    fn terminal_output_projection_strips_controls_before_selection() {
        let document = project(
            &TranscriptPayload::ToolActivity(ToolActivity {
                call_id: ExternalText::new("call"),
                invocation: tool::ToolInvocation::TerminalRead(tool::Prepared::Ready(
                    tool::TerminalReadRequest {
                        terminal_id: 7,
                        poll_after_ms: 8_000,
                    },
                )),
                raw_input: ExternalText::new("terminal: 7"),
                raw_input_encoding: crate::domain::ToolInputEncoding::Freeform,
                phase: ToolActivityPhase::Finished {
                    outcome: tool::ToolOutcome::TerminalRead(tool::TerminalResult {
                        terminal_id: 7,
                        output: "a\u{1b}[31mred\u{1b}[0m".to_owned(),
                        earlier_output_omitted: 0,
                        state: tool::TerminalProcessState::Exited { code: 0 },
                    }),
                    raw_output: ExternalText::new("ignored raw output"),
                },
            }),
            true,
        );
        let selectable = document.selectable_text();
        assert!(selectable.contains("ared"));
        assert!(!selectable.contains('\u{1b}'));
    }

    #[test]
    fn markdown_wysiwyg_formatting() {
        let document = project(
            &TranscriptPayload::Message {
                role: MessageRole::Assistant,
                text: ExternalText::new("# Heading `code`\n**bold text** *italic* `code`"),
            },
            false,
        );
        assert_eq!(
            document.selectable_text(),
            "# Heading `code`\n**bold text** *italic* `code`"
        );
    }
}
