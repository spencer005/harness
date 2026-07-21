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
    let (root_is_file, base) = if let Some(root_arg) = root_arg {
        let (_, path) = resolve(workspace, &root_arg)?;
        (path.is_file(), path)
    } else {
        (false, workspace.path().to_owned())
    };

    // When a specific file is given, read and search it directly — fff's
    // FilePicker can't handle a file as base_path (it walks directories).
    if root_is_file {
        return file_search(&pattern, &base, max);
    }

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

    // Single-byte patterns bypass fff's bigram index (it returns None for
    // patterns shorter than 2 bytes), which can cause the grep pipeline to
    // produce zero matches.  Search those directly with memchr.
    if pattern.len() == 1 {
        return single_byte_search(&picker, pattern.as_bytes()[0], max, &base);
    }

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

/// Search a single file directly — bypasses fff entirely.
/// Uses memchr's SIMD `Finder` for O(n) substring search.
fn file_search(pattern: &str, path: &Path, max: usize) -> Result<String, String> {
    let content = std::fs::read(path)
        .map_err(|e| format!("failed to read `{}`: {e}", path.display()))?;

    let case_insensitive = !pattern.bytes().any(|b| b.is_ascii_uppercase());

    // Owned needle storage that lives long enough for the Finder to borrow.
    // memchr's Finder does not require 'static, but the borrowed data must
    // outlive the if-else block — hence the outer binding.
    let owned_needle;
    let (finder, haystack): (memchr::memmem::Finder<'_>, Vec<u8>) = if case_insensitive {
        owned_needle = pattern.to_ascii_lowercase().into_bytes();
        let haystack = content.to_ascii_lowercase();
        (memchr::memmem::Finder::new(&owned_needle), haystack)
    } else {
        (memchr::memmem::Finder::new(pattern.as_bytes()), content.clone())
    };

    // Line-start offsets (from original content, for correct display).
    let mut line_starts: Vec<usize> = vec![0];
    for (offset, &b) in content.iter().enumerate() {
        if b == b'\n' {
            line_starts.push(offset + 1);
        }
    }

    let path_str = path.to_string_lossy();
    let mut output = String::new();
    let mut total_matches: usize = 0;
    let mut displayed: usize = 0;
    let mut file_matched = false;

    let mut pos = 0;
    while let Some(abs_pos) = finder.find(&haystack[pos..]).map(|p| pos + p) {
        total_matches += 1;

        if displayed < max {
            if !file_matched {
                if !output.is_empty() {
                    output.push('\n');
                }
                let _ = writeln!(output, "{path_str}");
                file_matched = true;
            }

            // Find line number via the line-starts index.
            let line_idx = match line_starts.binary_search(&abs_pos) {
                Ok(i) => i + 1,
                Err(i) => i,
            };

            // Extract the full line from ORIGINAL content for display.
            let line_start = line_starts[line_idx - 1];
            let line_end = content[line_start..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|p| line_start + p)
                .unwrap_or(content.len());
            let line_bytes = &content[line_start..line_end];
            let line_text = String::from_utf8_lossy(line_bytes);

            let _ = writeln!(output, "{line_idx} {line_text}");
            displayed += 1;
        }

        pos = abs_pos + 1;
    }

    if total_matches > displayed {
        let _ = writeln!(
            output,
            "\n[output truncated: showing first {displayed} of {total_matches} matches; refine the query or path constraint]"
        );
    }
    if total_matches == 0 {
        output.push_str("no results\n");
    }
    Ok(output)
}

/// Search for a single-byte pattern across all collected files using memchr.
/// Bypasses fff's bigram index which breaks for 1-byte patterns.
fn single_byte_search(
    picker: &FilePicker,
    byte: u8,
    max: usize,
    base: &Path,
) -> Result<String, String> {
    let files = picker.get_files();
    let mut output = String::new();
    let mut total_matches: usize = 0;
    let mut displayed: usize = 0;

    for file in files {
        if file.is_deleted() || file.is_binary() || file.size == 0 {
            continue;
        }

        let abs_path = file.absolute_path(picker, base);
        let content = match std::fs::read(&abs_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Find every occurrence of the byte, computing line numbers as we go.
        // Emit matches grouped by file path, matching the fff grep output format.
        let path = file.relative_path(picker);
        let mut line: usize = 1;
        let mut line_start: usize = 0;
        let mut file_matched = false;

        for (offset, &ch) in content.iter().enumerate() {
            if ch == b'\n' {
                line += 1;
                line_start = offset + 1;
                continue;
            }

            if ch != byte {
                continue;
            }

            total_matches += 1;

            // Once display budget is exhausted just count.
            if displayed >= max {
                continue;
            }

            if !file_matched {
                if !output.is_empty() {
                    output.push('\n');
                }
                let _ = writeln!(output, "{path}");
                file_matched = true;
            }

            // Extract the line content (everything up to the next \n).
            let line_end = content[offset..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|pos| offset + pos)
                .unwrap_or(content.len());
            let line_bytes = &content[line_start..line_end];
            let line_text = String::from_utf8_lossy(line_bytes);

            let _ = writeln!(output, "{line} {line_text}");
            displayed += 1;
        }
    }

    if total_matches > displayed {
        let _ = writeln!(
            output,
            "\n[output truncated: showing first {displayed} of {total_matches} matches; refine the query or path constraint]"
        );
    }
    if total_matches == 0 {
        output.push_str("no results\n");
    }
    Ok(output)
}
