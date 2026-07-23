use std::{fmt::Write, path::Path};

use fff_search::{FFFMode, FilePicker, FilePickerOptions};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::bytes::{Regex, RegexBuilder};

use super::{ShellWord, resolve};

#[derive(Debug)]
struct SearchOptions {
    pattern: String,
    roots: Vec<String>,
    max: usize,
    literal: bool,
    ignore_case: bool,
    files_only: bool,
    includes: Vec<String>,
    excludes: Vec<String>,
}

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    args: &[ShellWord],
) -> Result<String, String> {
    let options = parse_options(args)?;
    let matcher = build_matcher(&options)?;
    let includes = build_globs(&options.includes, "--glob")?;
    let excludes = build_globs(&options.excludes, "--exclude")?;
    let roots = if options.roots.is_empty() {
        vec![None]
    } else {
        options.roots.iter().map(Some).collect()
    };
    let mut paths = Vec::new();

    for root in roots {
        let (_, base) = match root {
            Some(root) => resolve(workspace, root)?,
            None => (String::new(), workspace.path().to_owned()),
        };
        if base.is_file() {
            paths.push((base.clone(), base.display().to_string()));
            continue;
        }

        let base_str = base.to_string_lossy().to_string();
        let mut picker = FilePicker::new(FilePickerOptions {
            base_path: base_str,
            enable_content_indexing: false,
            mode: FFFMode::Ai,
            watch: false,
            ..Default::default()
        })
        .map_err(|error| format!("failed to initialize search file index: {error}"))?;
        picker
            .collect_files()
            .map_err(|error| format!("failed to collect search files: {error}"))?;
        paths.extend(
            picker
                .get_files()
                .iter()
                .filter(|file| !file.is_deleted() && !file.is_binary() && file.size > 0)
                .map(|file| {
                    (
                        file.absolute_path(&picker, &base),
                        file.relative_path(&picker),
                    )
                }),
        );
    }

    search_paths(
        paths
            .iter()
            .map(|(absolute, relative)| (absolute.as_path(), relative.clone())),
        &matcher,
        &includes,
        &excludes,
        &options,
    )
}

fn parse_options(args: &[ShellWord]) -> Result<SearchOptions, String> {
    let mut pattern = None;
    let mut roots = Vec::new();
    let mut max = 100usize;
    let mut literal = false;
    let mut ignore_case = false;
    let mut files_only = false;
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut index = 0;

    while index < args.len() {
        let value = args[index].value.as_str();
        match value {
            "-F" => literal = true,
            "-i" => ignore_case = true,
            "--files" => files_only = true,
            "--max" | "-g" | "--glob" | "--exclude" => {
                index += 1;
                let argument = args.get(index).ok_or_else(|| {
                    format!("failed to parse `inspect` search input: `{value}` requires a value")
                })?;
                match value {
                    "--max" => max = super::positive(&argument.value, "search --max")?,
                    "-g" | "--glob" => includes.push(argument.value.clone()),
                    "--exclude" => excludes.push(argument.value.clone()),
                    _ => unreachable!(),
                }
            }
            value if value.starts_with('-') => {
                return Err(format!(
                    "failed to parse `inspect` search input: unsupported option `{value}`"
                ));
            }
            value if pattern.is_none() => pattern = Some(value.to_owned()),
            value => roots.push(value.to_owned()),
        }
        index += 1;
    }

    Ok(SearchOptions {
        pattern: pattern
            .ok_or("failed to parse `inspect` search input: pattern is required")?,
        roots,
        max,
        literal,
        ignore_case,
        files_only,
        includes,
        excludes,
    })
}

fn build_matcher(options: &SearchOptions) -> Result<Regex, String> {
    let pattern = if options.literal {
        regex::escape(&options.pattern)
    } else {
        options.pattern.clone()
    };
    let case_insensitive =
        options.ignore_case || !options.pattern.bytes().any(|byte| byte.is_ascii_uppercase());
    RegexBuilder::new(&pattern)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|error| {
            format!(
                "failed to parse `inspect` search input: invalid regular expression `{}`: {error}",
                options.pattern
            )
        })
}

fn build_globs(patterns: &[String], option: &str) -> Result<Option<GlobSet>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|error| {
            format!(
                "failed to parse `inspect` search input: invalid {option} pattern `{pattern}`: {error}"
            )
        })?;
        builder.add(glob);
    }
    builder.build().map(Some).map_err(|error| {
        format!("failed to parse `inspect` search input: invalid {option} patterns: {error}")
    })
}

fn search_paths<'a>(
    paths: impl Iterator<Item = (&'a Path, String)>,
    matcher: &Regex,
    includes: &Option<GlobSet>,
    excludes: &Option<GlobSet>,
    options: &SearchOptions,
) -> Result<String, String> {
    let mut output = String::new();
    let mut total_matches = 0usize;
    let mut displayed = 0usize;

    for (absolute, relative) in paths {
        if includes
            .as_ref()
            .is_some_and(|patterns| !patterns.is_match(&relative))
            || excludes
                .as_ref()
                .is_some_and(|patterns| patterns.is_match(&relative))
        {
            continue;
        }
        let content = std::fs::read(absolute)
            .map_err(|error| format!("failed to read `{}`: {error}", absolute.display()))?;
        let mut file_heading_written = false;

        for (line_index, line) in content.split(|byte| *byte == b'\n').enumerate() {
            if !matcher.is_match(line) {
                continue;
            }
            total_matches += 1;
            if displayed >= options.max {
                continue;
            }
            if options.files_only {
                let _ = writeln!(output, "{relative}");
                displayed += 1;
                break;
            }
            if !file_heading_written {
                if !output.is_empty() {
                    output.push('\n');
                }
                let _ = writeln!(output, "{relative}");
                file_heading_written = true;
            }
            let text = String::from_utf8_lossy(line);
            let _ = writeln!(output, "{} {text}", line_index + 1);
            displayed += 1;
        }
    }

    if total_matches > displayed {
        let _ = writeln!(
            output,
            "\n[search output truncated: showing first {displayed} of {total_matches} matches; refine the query or path constraint]"
        );
    }
    if total_matches == 0 {
        output.push_str("no results\n");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(value: &str) -> ShellWord {
        ShellWord {
            value: value.to_string(),
            quoted: false,
        }
    }

    #[test]
    fn options_reject_unknown_flags_instead_of_silently_ignoring_them() {
        let error = parse_options(&[word("needle"), word("--unknown")]).unwrap_err();
        assert!(error.contains("unsupported option `--unknown`"));
    }

    #[test]
    fn regex_and_literal_modes_have_distinct_matching_contracts() {
        let regex = build_matcher(&parse_options(&[word("take|pin")]).unwrap()).unwrap();
        assert!(regex.is_match(b"take_body"));
        let literal =
            build_matcher(&parse_options(&[word("take|pin"), word("-F")]).unwrap()).unwrap();
        assert!(!literal.is_match(b"take_body"));
        assert!(literal.is_match(b"take|pin"));
    }
    #[test]
    fn execute_applies_regex_and_path_filters_through_workspace_boundary() {
        let root_path = std::env::temp_dir().join(format!(
            "inspect-search-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root_path.join("src")).unwrap();
        std::fs::write(
            root_path.join("src/main.rs"),
            "fn take_body() {}\nfn parse_anchor() {}\n",
        )
        .unwrap();
        std::fs::write(root_path.join("src/generated.rs"), "fn take_generated() {}\n").unwrap();
        std::fs::create_dir_all(root_path.join("tests")).unwrap();
        std::fs::write(root_path.join("tests/search.rs"), "fn parse_test() {}\n").unwrap();
        let workspace = super::super::WorkspaceRoot::open(&root_path).unwrap();

        let output = execute(
            &workspace,
            &[
                word("fn (take|parse)"),
                word("src"),
                word("tests"),
                word("--glob"),
                word("*.rs"),
                word("--exclude"),
                word("*generated.rs"),
            ],
        )
        .unwrap();

        assert!(output.contains("main.rs"));
        assert!(output.contains("take_body"));
        assert!(output.contains("parse_anchor"));
        assert!(output.contains("search.rs"));
        assert!(output.contains("parse_test"));
        assert!(!output.contains("generated.rs"));
        std::fs::remove_dir_all(root_path).unwrap();
    }
}
