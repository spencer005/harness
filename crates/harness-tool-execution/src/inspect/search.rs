use std::{fmt::Write, path::Path};

use fff_search::{
    FFFMode, FilePicker, FilePickerOptions, GrepMode, GrepSearchOptions, QueryParser,
};

use super::{ShellWord, resolve};

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    args: &[ShellWord],
) -> Result<String, String> {
    if args.is_empty() {
        return Err("failed to parse `inspect` search input: pattern is required".into());
    }
    let mut pattern = None;
    let mut root_arg = None;
    let mut max = 100usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].value.as_str() {
            "--max" => {
                index += 1;
                let value = args.get(index).ok_or(
                    "failed to parse `inspect` search input: `--max` needs a value",
                )?;
                max = super::positive(&value.value, "search --max")?;
                index += 1;
            }
            value if value.starts_with('-') => {
                index += 1;
            }
            value => {
                if pattern.is_none() {
                    pattern = Some(value.to_owned());
                } else if root_arg.is_none() {
                    root_arg = Some(value.to_owned());
                }
                index += 1;
            }
        }
    }
    let pattern = pattern.ok_or("failed to parse `inspect` search input: pattern is required")?;
    let base = if let Some(root_arg) = root_arg {
        let (_, path) = resolve(workspace, &root_arg)?;
        path
    } else {
        workspace.path().to_owned()
    };

    let base_str = base.to_string_lossy().to_string();
    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: base_str,
        enable_content_indexing: true,
        mode: FFFMode::Ai,
        watch: false,
        ..Default::default()
    })
    .map_err(|e| format!("failed to initialize fff: {e}"))?;
    picker
        .collect_files()
        .map_err(|e| format!("failed to index files for fff: {e}"))?;

    let parser = QueryParser::new(fff_search::GrepConfig);
    let query = parser.parse(&pattern);
    let options = GrepSearchOptions {
        max_matches_per_file: 50,
        smart_case: true,
        page_limit: 10_000,
        mode: GrepMode::PlainText,
        classify_definitions: false,
        ..Default::default()
    };
    let result = picker.grep(&query, &options);
    let total_matches = result.matches.len();

    let mut output = String::new();
    let mut current_path = String::new();
    let mut displayed = 0usize;
    for grep_match in &result.matches {
        if displayed >= max {
            continue;
        }
        let Some(file) = result.files.get(grep_match.file_index) else {
            continue;
        };
        let path = file.relative_path(&picker);
        if path != current_path {
            if !current_path.is_empty() {
                output.push('\n');
            }
            let _ = writeln!(output, "{path}");
            current_path = path;
        }
        let _ = writeln!(output, "{} {}", grep_match.line_number, grep_match.line_content);
        displayed += 1;
    }
    if total_matches > displayed {
        let _ = writeln!(
            output,
            "\n[fff output truncated: showing first {displayed} of {total_matches} matches; refine the query or path constraint]"
        );
    }
    if total_matches == 0 {
        output.push_str("no results\n");
    }
    Ok(output)
}
