use std::fmt::Write as _;
use std::process::{Command, ExitStatus};

use serde::Deserialize;

use super::{ShellWord, WorkspaceRoot};

pub(crate) fn check(workspace: &WorkspaceRoot, args: &[ShellWord]) -> Result<String, String> {
    let mut parsed_args = parse_cargo_check_command(args)?;
    if !parsed_args.iter().any(|arg| arg.starts_with("--message-format")) {
        parsed_args.push("--message-format=json".to_string());
    }
    let output = Command::new("cargo")
        .args(&parsed_args)
        .current_dir(workspace.path())
        .output()
        .map_err(|e| format!("failed to execute `cargo check`: {e}"))?;

    let formatted = format_cargo_check_output(
        output.status,
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    );

    Ok(formatted)
}

pub(crate) fn test(workspace: &WorkspaceRoot, args: &[ShellWord]) -> Result<String, String> {
    let parsed = parse_cargo_test_command(args)?;
    let filters = if parsed.filters.is_empty() {
        vec![None]
    } else {
        parsed.filters.iter().map(Some).collect::<Vec<_>>()
    };
    let label_filters = filters.len() > 1;
    let mut formatted = String::new();

    for filter in filters {
        let mut cmd_args = parsed.cargo_args.clone();
        if let Some(filter) = filter {
            cmd_args.push(filter.clone());
        }
        if !parsed.libtest_args.is_empty() {
            cmd_args.push("--".to_string());
            cmd_args.extend(parsed.libtest_args.iter().cloned());
        }

        let output = Command::new("cargo")
            .args(&cmd_args)
            .current_dir(workspace.path())
            .output()
            .map_err(|error| {
                format!(
                    "{formatted}failed to execute `cargo test`: {error}"
                )
            })?;

        if label_filters {
            let _ = writeln!(
                formatted,
                "filter {}",
                filter.expect("multiple test invocations always have filters")
            );
        }
        formatted.push_str(&format_cargo_test_output(
            output.status,
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        ));
        if !formatted.ends_with('\n') {
            formatted.push('\n');
        }

        if !output.status.success()
            && !rust_error_locations(&String::from_utf8_lossy(&output.stderr)).is_empty()
        {
            break;
        }
    }

    Ok(formatted)
}

fn parse_cargo_check_command(args: &[ShellWord]) -> Result<Vec<String>, String> {
    let mut command_args = vec!["check".to_string(), "--locked".to_string()];
    for word in args {
        match word.value.as_str() {
            "--lib" | "--all-targets" => command_args.push(word.value.clone()),
            package if !package.starts_with('-') && !package.contains('=') => {
                command_args.push("-p".to_string());
                command_args.push(package.to_string());
            }
            _ => {
                return Err(
                    "failed to parse `inspect` check input: expected package names, --lib, or --all-targets"
                        .to_string(),
                );
            }
        }
    }
    Ok(command_args)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoTestCommand {
    cargo_args: Vec<String>,
    filters: Vec<String>,
    libtest_args: Vec<String>,
}

fn parse_cargo_test_command(args: &[ShellWord]) -> Result<CargoTestCommand, String> {
    let mut parsed = CargoTestCommand {
        cargo_args: vec!["test".to_string(), "--locked".to_string()],
        filters: Vec::new(),
        libtest_args: Vec::new(),
    };
    let mut index = 0;
    let mut parsing_libtest_args = false;
    while index < args.len() {
        let value = args[index].value.as_str();
        if parsing_libtest_args {
            match value {
                "--exact" | "--ignored" | "--include-ignored" | "--show-output" | "--nocapture" => {
                    parsed.libtest_args.push(value.to_string());
                }
                "--skip" | "--test-threads" => {
                    parsed.libtest_args.push(value.to_string());
                    index += 1;
                    let argument = args.get(index).ok_or_else(|| {
                        format!(
                            "failed to parse `inspect` test input: {value} requires an argument"
                        )
                    })?;
                    parsed.libtest_args.push(argument.value.clone());
                }
                value if value.starts_with("--skip=") || value.starts_with("--test-threads=") => {
                    parsed.libtest_args.push(value.to_string());
                }
                _ => {
                    return Err(format!(
                        "failed to parse `inspect` test input: unsupported libtest option `{value}`"
                    ));
                }
            }
            index += 1;
            continue;
        }

        match value {
            "--" => parsing_libtest_args = true,
            "--lib"
            | "--bins"
            | "--examples"
            | "--tests"
            | "--benches"
            | "--all-targets"
            | "--doc"
            | "--workspace"
            | "--all"
            | "--all-features"
            | "--no-default-features"
            | "--release"
            | "--no-fail-fast" => parsed.cargo_args.push(value.to_string()),
            "-p" | "--package" | "--bin" | "--example" | "--test" | "--bench" | "--exclude"
            | "--features" => {
                parsed.cargo_args.push(value.to_string());
                index += 1;
                let argument = args.get(index).ok_or_else(|| {
                    format!("failed to parse `inspect` test input: {value} requires an argument")
                })?;
                parsed.cargo_args.push(argument.value.clone());
            }
            value
                if [
                    "--package=",
                    "--bin=",
                    "--example=",
                    "--test=",
                    "--bench=",
                    "--exclude=",
                    "--features=",
                ]
                .iter()
                .any(|prefix| value.starts_with(prefix) && value.len() > prefix.len()) =>
            {
                parsed.cargo_args.push(value.to_string());
            }
            value if value.starts_with('-') => {
                return Err(format!(
                    "failed to parse `inspect` test input: unsupported cargo test option `{value}`"
                ));
            }
            filter => parsed.filters.push(filter.to_string()),
        }
        index += 1;
    }

    if parsed.filters.is_empty()
        && parsed
            .libtest_args
            .iter()
            .any(|argument| argument == "--exact")
    {
        return Err(
            "failed to parse `inspect` test input: --exact requires at least one test filter"
                .to_string(),
        );
    }

    Ok(parsed)
}

fn output_status_text(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_string())
}

#[derive(Deserialize, Debug)]
#[serde(tag = "reason", rename_all = "kebab-case")]
enum CargoMessage {
    CompilerMessage {
        message: CompilerMessageDetail,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Debug)]
struct CompilerMessageDetail {
    code: Option<CompilerMessageCode>,
    level: String,
    message: String,
    spans: Vec<CompilerMessageSpan>,
    #[serde(default)]
    children: Vec<CompilerMessageChild>,
}

#[derive(Deserialize, Debug)]
struct CompilerMessageCode {
    code: String,
}

#[derive(Deserialize, Debug)]
struct CompilerMessageSpan {
    file_name: String,
    line_start: usize,
    column_start: usize,
    is_primary: bool,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Deserialize, Debug)]
struct CompilerMessageChild {
    level: String,
    message: String,
    spans: Vec<CompilerMessageSpan>,
}

fn parse_json_diagnostics(stdout: &str) -> Vec<RustErrorLocation> {
    let mut locations = Vec::new();
    for line in stdout.lines() {
        if let Ok(msg) = sonic_rs::from_str::<CargoMessage>(line) {
            if let CargoMessage::CompilerMessage { message } = msg {
                if message.level == "error" {
                    let code = message.code
                        .map(|c| {
                            let c_str = c.code.as_str();
                            if let Some(rest) = c_str.strip_prefix("E0") {
                                rest.to_string()
                            } else if let Some(rest) = c_str.strip_prefix("E") {
                                rest.to_string()
                            } else {
                                c_str.to_string()
                            }
                        })
                        .unwrap_or_else(|| "0".to_string());
                    
                    if let Some(span) = message.spans.iter().find(|s| s.is_primary) {
                        // Extract the label from the primary span if present.
                        let label = span.label.clone();

                        // Look for delimiter-related notes in children so we can
                        // show both sides of unbalanced braces/parens.
                        let related = message.children.iter().find_map(|child| {
                            let msg = child.message.as_str();
                            if child.level != "note" {
                                return None;
                            }
                            // Rustc reports the nearest opener like:
                            //   "the nearest open delimiter"
                            //   "missing open `(` for this delimiter"
                            if !msg.contains("open delimiter")
                                && !msg.contains("missing open")
                            {
                                return None;
                            }
                            child.spans.first().map(|s| RustRelatedSpan {
                                path: s.file_name.clone(),
                                line: s.line_start,
                                column: s.column_start,
                                label: s.label.clone().unwrap_or_else(|| msg.to_string()),
                            })
                        });

                        locations.push(RustErrorLocation {
                            code,
                            summary: message.message.clone(),
                            path: span.file_name.clone(),
                            line: span.line_start,
                            column: span.column_start,
                            label,
                            related,
                        });
                    }
                }
            }
        }
    }
    locations
}

fn format_cargo_check_output(status: ExitStatus, stdout: &str, _stderr: &str) -> String {
    let diagnostics = parse_json_diagnostics(stdout);
    if diagnostics.is_empty() {
        if status.success() {
            return "ok\n".to_string();
        }
        return format!("cargo check failed {}\n", output_status_text(status));
    }

    format_rust_diagnostics(&diagnostics)
}

fn format_cargo_test_output(status: ExitStatus, stdout: &str, stderr: &str) -> String {
    let diagnostics = rust_error_locations(stderr);
    if !diagnostics.is_empty() {
        return format_rust_diagnostics(&diagnostics);
    }

    let summary = cargo_test_summary(stdout);
    if status.success() {
        return summary
            .map(|summary| format_cargo_test_summary("ok", summary))
            .unwrap_or_else(|| "ok\n".to_string());
    }

    let failures = rust_test_failure_sections(stdout);
    let runtime_failures = rust_test_runtime_failure_sections(stderr);
    if failures.is_empty() && runtime_failures.is_empty() {
        return format!("cargo test failed {}\n", output_status_text(status));
    }

    let mut output = String::from("Test failures\n");
    output.push_str(&failures);
    if !failures.is_empty() && !runtime_failures.is_empty() {
        output.push('\n');
    }
    output.push_str(&runtime_failures);
    if let Some(summary) = summary {
        output.push_str(&format_cargo_test_summary("FAILED", summary));
    } else {
        let _ = writeln!(output, "cargo test failed {}", output_status_text(status));
    }
    output
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CargoTestSummary {
    passed: usize,
    failed: usize,
    ignored: usize,
    measured: usize,
    filtered_out: usize,
}

fn cargo_test_summary(stdout: &str) -> Option<CargoTestSummary> {
    let mut total = CargoTestSummary::default();
    let mut found = false;
    for line in stdout.lines() {
        let Some(result) = line.trim().strip_prefix("test result: ") else {
            continue;
        };
        let Some((_, counts)) = result.split_once(". ") else {
            continue;
        };
        let mut parsed = CargoTestSummary::default();
        for count in counts.split(';') {
            let count = count.trim();
            if let Some(value) = test_summary_count(count, " passed") {
                parsed.passed = value;
            } else if let Some(value) = test_summary_count(count, " failed") {
                parsed.failed = value;
            } else if let Some(value) = test_summary_count(count, " ignored") {
                parsed.ignored = value;
            } else if let Some(value) = test_summary_count(count, " measured") {
                parsed.measured = value;
            } else if let Some(value) = test_summary_count(count, " filtered out") {
                parsed.filtered_out = value;
            }
        }
        total.passed += parsed.passed;
        total.failed += parsed.failed;
        total.ignored += parsed.ignored;
        total.measured += parsed.measured;
        total.filtered_out += parsed.filtered_out;
        found = true;
    }
    found.then_some(total)
}

fn test_summary_count(value: &str, suffix: &str) -> Option<usize> {
    value.strip_suffix(suffix)?.parse().ok()
}

fn format_cargo_test_summary(status: &str, summary: CargoTestSummary) -> String {
    format!(
        "{status}: {} passed; {} failed; {} ignored; {} measured; {} filtered out\n",
        summary.passed, summary.failed, summary.ignored, summary.measured, summary.filtered_out
    )
}

fn rust_test_failure_sections(stdout: &str) -> String {
    let lines = stdout.lines().collect::<Vec<_>>();
    let mut output = String::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim();
        let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|line| line.strip_suffix(" stdout ----"))
        else {
            index += 1;
            continue;
        };

        if !output.is_empty() {
            output.push('\n');
        }
        let _ = writeln!(output, "{name}");
        index += 1;
        while index < lines.len() {
            let body_line = lines[index];
            let trimmed = body_line.trim();
            if (trimmed.starts_with("---- ") && trimmed.ends_with(" stdout ----"))
                || trimmed == "failures:"
            {
                break;
            }
            if !trimmed.starts_with("note: run with `RUST_BACKTRACE=") {
                let _ = writeln!(output, "{body_line}");
            }
            index += 1;
        }
        while output.ends_with("\n\n") {
            output.pop();
        }
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

fn rust_test_runtime_failure_sections(stderr: &str) -> String {
    let mut output = String::new();
    let mut in_failure = false;

    for line in stderr.lines() {
        let trimmed = line.trim();
        if !in_failure {
            if trimmed.starts_with("thread '")
                && (trimmed.contains("' panicked at ")
                    || trimmed.contains("has overflowed its stack"))
            {
                let _ = writeln!(output, "{trimmed}");
                in_failure = true;
            }
            continue;
        }

        if trimmed.starts_with("error: test failed") || trimmed == "Caused by:" {
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with("note: run with `RUST_BACKTRACE=") {
            continue;
        }
        let _ = writeln!(output, "{line}");
    }

    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RustRelatedSpan {
    path: String,
    line: usize,
    column: usize,
    label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RustErrorLocation {
    code: String,
    summary: String,
    path: String,
    line: usize,
    column: usize,
    /// Optional label from the primary span (e.g. the `^` label).
    label: Option<String>,
    /// For brace/delimiter errors, the matching opener location.
    related: Option<RustRelatedSpan>,
}

fn rust_error_locations(stderr: &str) -> Vec<RustErrorLocation> {
    let mut locations = Vec::new();
    let mut current_error: Option<(String, String)> = None;

    for line in stderr.lines() {
        let trimmed = line.trim_start();
        if let Some(parsed) = rust_error_header(trimmed) {
            current_error = Some(parsed);
            continue;
        }

        let Some((code, summary)) = current_error.clone() else {
            continue;
        };
        let Some((path, line_number, column)) = rust_location_line(trimmed) else {
            continue;
        };
        locations.push(RustErrorLocation {
            code,
            summary,
            path,
            line: line_number,
            column,
            label: None,
            related: None,
        });
        current_error = None;
    }

    locations
}

fn format_rust_diagnostics(diagnostics: &[RustErrorLocation]) -> String {
    let mut output = String::from("E0 err lineposition\n");
    let mut paths = Vec::<&str>::new();
    for diagnostic in diagnostics {
        if !paths.contains(&diagnostic.path.as_str()) {
            paths.push(&diagnostic.path);
        }
    }

    for path in paths {
        let _ = writeln!(output, "{path}");
        for diagnostic in diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.path == path)
        {
            let _ = write!(
                output,
                "{} {} {}:{}",
                diagnostic.code, diagnostic.summary, diagnostic.line, diagnostic.column
            );
            // Show the matching side of brace/delimiter errors.
            if let Some(ref related) = diagnostic.related {
                let _ = write!(
                    output,
                    " {} {}:{}",
                    related.label, related.line, related.column,
                );
            }
            output.push('\n');
        }
    }
    output
}

fn rust_error_header(line: &str) -> Option<(String, String)> {
    if let Some(rest) = line.strip_prefix("error[E0") {
        let digits = rest
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() || !rest[digits.len()..].starts_with("]:") {
            return None;
        }
        let summary = rest[digits.len() + 2..].trim();
        if summary.is_empty() {
            return None;
        }
        return Some((digits, summary.to_string()));
    }

    let summary = line.strip_prefix("error:")?.trim();
    if summary.is_empty() {
        return None;
    }
    Some(("0".to_string(), summary.to_string()))
}

fn rust_location_line(line: &str) -> Option<(String, usize, usize)> {
    let location = line.strip_prefix("--> ")?;
    let mut parts = location.rsplitn(3, ':');
    let column = parts.next()?.parse::<usize>().ok()?;
    let line_number = parts.next()?.parse::<usize>().ok()?;
    let path = parts.next()?.to_string();
    Some((path, line_number, column))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(val: &str) -> ShellWord {
        ShellWord {
            value: val.to_string(),
            quoted: false,
        }
    }

    #[test]
    fn inspect_check_parser_accepts_packages_and_target_selectors() {
        assert_eq!(
            parse_cargo_check_command(&[word("harness-core"), word("--lib"), word("--all-targets")]).unwrap(),
            vec![
                "check".to_string(),
                "--locked".to_string(),
                "-p".to_string(),
                "harness-core".to_string(),
                "--lib".to_string(),
                "--all-targets".to_string(),
            ]
        );
        assert_eq!(
            parse_cargo_check_command(&[]).unwrap(),
            vec!["check".to_string(), "--locked".to_string()]
        );

        let error = parse_cargo_check_command(&[word("--message-format=short")]).unwrap_err();
        assert_eq!(
            error,
            "failed to parse `inspect` check input: expected package names, --lib, or --all-targets"
        );
    }

    #[test]
    fn inspect_check_non_rust_failure_is_compact() {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 101")
            .status()
            .unwrap();
        let output = format_cargo_check_output(
            status,
            "",
            "Updating crates.io index\n     Locking 8 packages to latest compatible versions\n",
        );

        assert_eq!(output, "cargo check failed 101\n");
    }

    #[test]
    fn inspect_check_rust_errors_are_grouped_by_path() {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 101")
            .status()
            .unwrap();
        let stdout = concat!(
            r#"{"reason":"compiler-message","message":{"code":{"code":"E0308"},"level":"error","message":"mismatched types","spans":[{"file_name":"src/lib.rs","line_start":10,"column_start":5,"is_primary":true}]}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"code":{"code":"E0425"},"level":"error","message":"cannot find value `missing` in this scope","spans":[{"file_name":"src/lib.rs","line_start":20,"column_start":9,"is_primary":true}]}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"code":null,"level":"error","message":"unexpected closing delimiter: `}`","spans":[{"file_name":"src/main.rs","line_start":55,"column_start":1,"is_primary":true}],"children":[{"level":"note","message":"the nearest open delimiter","spans":[{"file_name":"src/main.rs","line_start":42,"column_start":1,"is_primary":true,"label":"the nearest open delimiter"}]}]}}"#,
            "\n"
        );
        let output = format_cargo_check_output(
            status,
            stdout,
            "",
        );

        assert_eq!(
            output,
            "E0 err lineposition\nsrc/lib.rs\n308 mismatched types 10:5\n425 cannot find value `missing` in this scope 20:9\nsrc/main.rs\n0 unexpected closing delimiter: `}` 55:1 the nearest open delimiter 42:1\n"
        );
    }

    #[test]
    fn inspect_check_rust_unclosed_delimiter_error_is_simplified() {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 101")
            .status()
            .unwrap();
        let stdout = r#"{"reason":"compiler-message","message":{"code":null,"level":"error","message":"this file contains an unclosed delimiter","spans":[{"file_name":"src/globals.rs","line_start":740,"column_start":3,"is_primary":true}]}}"#;
        let output = format_cargo_check_output(
            status,
            stdout,
            "",
        );

        assert_eq!(
            output,
            "E0 err lineposition\nsrc/globals.rs\n0 this file contains an unclosed delimiter 740:3\n"
        );
    }

    #[test]
    fn inspect_test_output_summarizes_suites_and_retains_failures() {
        let success = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .status()
            .unwrap();
        let output = format_cargo_test_output(
            success,
            "test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 4 filtered out; finished in 0.01s\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n",
            "",
        );
        assert_eq!(
            output,
            "ok: 3 passed; 0 failed; 1 ignored; 0 measured; 4 filtered out\n"
        );

        let failure = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 101")
            .status()
            .unwrap();
        let output = format_cargo_test_output(
            failure,
            "running 1 test\ntest module::fails ... FAILED\n\nfailures:\n\n---- module::fails stdout ----\nthread 'module::fails' panicked at src/lib.rs:12:5:\nassertion failed: false\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\nfailures:\n    module::fails\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s\n",
            "",
        );
        assert_eq!(
            output,
            "Test failures\nmodule::fails\nthread 'module::fails' panicked at src/lib.rs:12:5:\nassertion failed: false\nFAILED: 0 passed; 1 failed; 0 ignored; 0 measured; 3 filtered out\n"
        );
    }

    #[test]
    fn inspect_test_output_retains_stack_overflow_panics_from_stderr() {
        let failure = std::process::Command::new("sh")
            .arg("-c")
            .arg("kill -ABRT $$")
            .status()
            .unwrap();
        let output = format_cargo_test_output(
            failure,
            "running 1 test\n",
            "thread 'executes_workload' (176968) has overflowed its stack\nfatal runtime error: stack overflow, aborting\nerror: test failed, to rerun pass `-p interp --test workload_tests`\n\nCaused by:\n  process didn't exit successfully (signal: 6, SIGABRT: process abort signal)\n",
        );

        assert_eq!(
            output,
            "Test failures\nthread 'executes_workload' (176968) has overflowed its stack\nfatal runtime error: stack overflow, aborting\ncargo test failed terminated by signal\n"
        );
    }
}

