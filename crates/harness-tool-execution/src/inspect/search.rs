use std::{
    collections::HashSet,
    fmt::Write,
    path::{Path, PathBuf},
};

use fff_search::{FFFMode, FilePicker, FilePickerOptions};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::bytes::{Regex, RegexBuilder};

use harness_tool_api::{
    InspectJobSuccess, InspectSearchFile, InspectSearchMatch, InspectSearchMode,
    InspectSearchRequest, InspectSearchResult, SearchCase,
};
use super::{InspectCommandOutput, ShellWord, resolve};

const DEFAULT_RESULT_LIMIT: usize = 100;


pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    request: &InspectSearchRequest,
) -> Result<InspectCommandOutput, String> {
    let includes = build_globs(&request.includes, "--glob")?;
    let excludes = build_globs(&request.excludes, "--exclude")?;
    let paths = collect_paths(
        workspace,
        &request.roots,
        matches!(request.mode, InspectSearchMode::Files),
    )?;

    let (model, result) = match &request.mode {
        InspectSearchMode::Content {
            patterns,
            literal,
            case,
            files_with_matches,
        } => {
            let matcher = build_matcher(patterns, *literal, *case)?;
            search_paths(
                paths
                    .iter()
                    .map(|(absolute, relative)| (absolute.as_path(), relative.as_str())),
                &matcher,
                &includes,
                &excludes,
                request.maximum_results,
                *files_with_matches,
            )?
        }
        InspectSearchMode::Files => list_paths(
            paths
                .iter()
                .map(|(absolute, relative)| (absolute.as_path(), relative.as_str())),
            &includes,
            &excludes,
            request.maximum_results,
        ),
    };
    Ok(InspectCommandOutput {
        model,
        result: InspectJobSuccess::Search(result),
    })
}

pub(crate) fn prepare(args: &[ShellWord]) -> Result<InspectSearchRequest, String> {
    let files_count = args
        .iter()
        .take_while(|word| word.quoted || word.value != "--")
        .filter(|word| !word.quoted && word.value == "--files")
        .count();
    if files_count > 1 {
        return Err(
            "failed to parse `inspect` search input: `--files` may be specified only once".into(),
        );
    }
    let files_mode = files_count == 1;

    let mut patterns = Vec::new();
    let mut roots = Vec::new();
    let mut max = DEFAULT_RESULT_LIMIT;
    let mut literal = false;
    let mut case = SearchCase::Smart;
    let mut files_with_matches = false;
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut positional_only = false;
    let mut index = 0;

    while index < args.len() {
        let word = &args[index];
        let value = word.value.as_str();
        if positional_only || word.quoted {
            push_positional(value, files_mode, &mut patterns, &mut roots, &mut includes);
            index += 1;
            continue;
        }

        match value {
            "--" => positional_only = true,
            "--files" => {}
            "-F" | "--fixed-strings" | "--fixed-string" => literal = true,
            "-i" | "--ignore-case" => case = SearchCase::Insensitive,
            "-s" | "--case-sensitive" => case = SearchCase::Sensitive,
            "-S" | "--smart-case" => case = SearchCase::Smart,
            "-l" | "--files-with-matches" => files_with_matches = true,
            "-n" | "--line-number" | "-H" | "--with-filename" | "--heading" => {}
            "--max" | "-e" | "--regexp" | "-g" | "--glob" | "--exclude" | "--color" => {
                index += 1;
                let argument = args.get(index).ok_or_else(|| {
                    let expected = match value {
                        "-e" | "--regexp" => "a pattern",
                        "-g" | "--glob" | "--exclude" => "a glob",
                        "--color" => "a value such as `never`",
                        _ => "a positive integer",
                    };
                    format!("failed to parse `inspect` search input: `{value}` requires {expected}")
                })?;
                match value {
                    "--max" => max = super::positive(&argument.value, "search --max")?,
                    "-e" | "--regexp" => patterns.push(argument.value.clone()),
                    "-g" | "--glob" => push_glob(&argument.value, &mut includes, &mut excludes),
                    "--exclude" => excludes.push(argument.value.clone()),
                    "--color" => {}
                    _ => unreachable!(),
                }
            }
            _ if value.starts_with("--regexp=") => {
                push_nonempty_option_value(value, "--regexp=", "a pattern", &mut patterns)?;
            }
            _ if value.starts_with("--glob=") => {
                let mut globs = Vec::new();
                push_nonempty_option_value(value, "--glob=", "a glob", &mut globs)?;
                push_glob(&globs[0], &mut includes, &mut excludes);
            }
            _ if value.starts_with("--exclude=") => {
                push_nonempty_option_value(value, "--exclude=", "a glob", &mut excludes)?;
            }
            _ if value.starts_with("--max=") => {
                let raw = option_value(value, "--max=", "a positive integer")?;
                max = super::positive(raw, "search --max")?;
            }
            _ if value.starts_with("--color=") => {
                let _ = option_value(value, "--color=", "a value such as `never`")?;
            }
            _ if value.starts_with("-e") && value.len() > 2 => {
                patterns.push(value[2..].to_owned());
            }
            _ if value.starts_with("-g") && value.len() > 2 => {
                push_glob(&value[2..], &mut includes, &mut excludes);
            }
            _ if value.starts_with('-') => {
                return Err(format!(
                    "failed to parse `inspect` search input: unsupported option `{value}`. \
                     Supported `rg` forms include `-e/--regexp`, `-F/--fixed-strings`, \
                     `-i/--ignore-case`, `-s/--case-sensitive`, `-S/--smart-case`, \
                     `-g/--glob`, `-l/--files-with-matches`, and `--files`. \
                     Use `--` before a pattern or path that starts with `-`"
                ));
            }
            _ => push_positional(value, files_mode, &mut patterns, &mut roots, &mut includes),
        }
        index += 1;
    }

    let mode = if files_mode {
        if files_with_matches {
            return Err(
                "failed to parse `inspect` search input: `--files` lists searchable paths and \
                 cannot be combined with `-l/--files-with-matches`; remove one of these options"
                    .into(),
            );
        }
        InspectSearchMode::Files
    } else {
        if patterns.is_empty() {
            return Err(
                "failed to parse `inspect` search input: a pattern is required. \
                 Supply it positionally or with `-e/--regexp`; use `--files` to list paths"
                    .into(),
            );
        }
        InspectSearchMode::Content {
            patterns,
            literal,
            case,
            files_with_matches,
        }
    };

    Ok(InspectSearchRequest {
        mode,
        roots,
        maximum_results: max,
        includes,
        excludes,
    })
}

fn push_positional(
    value: &str,
    files_mode: bool,
    patterns: &mut Vec<String>,
    roots: &mut Vec<String>,
    includes: &mut Vec<String>,
) {
    if !files_mode && patterns.is_empty() {
        patterns.push(value.to_owned());
    } else if contains_glob_syntax(value) {
        includes.push(normalize_glob(value));
        let root = glob_root(value);
        if !roots.iter().any(|existing| existing == &root) {
            roots.push(root);
        }
    } else if value != "." || roots.is_empty() {
        roots.push(value.to_owned());
    }
}

fn push_glob(pattern: &str, includes: &mut Vec<String>, excludes: &mut Vec<String>) {
    if let Some(exclude) = pattern.strip_prefix('!').filter(|value| !value.is_empty()) {
        excludes.push(exclude.to_owned());
    } else {
        includes.push(pattern.to_owned());
    }
}

fn push_nonempty_option_value(
    argument: &str,
    prefix: &str,
    expected: &str,
    destination: &mut Vec<String>,
) -> Result<(), String> {
    destination.push(option_value(argument, prefix, expected)?.to_owned());
    Ok(())
}

fn option_value<'a>(argument: &'a str, prefix: &str, expected: &str) -> Result<&'a str, String> {
    let value = argument.strip_prefix(prefix).unwrap_or_default();
    if value.is_empty() {
        return Err(format!(
            "failed to parse `inspect` search input: `{}` requires {expected}",
            prefix.trim_end_matches('=')
        ));
    }
    Ok(value)
}

fn contains_glob_syntax(value: &str) -> bool {
    value
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'{'))
}

fn normalize_glob(value: &str) -> String {
    value.strip_prefix("./").unwrap_or(value).to_owned()
}

fn glob_root(pattern: &str) -> String {
    let wildcard = pattern
        .bytes()
        .position(|byte| matches!(byte, b'*' | b'?' | b'[' | b'{'))
        .unwrap_or(pattern.len());
    let prefix = &pattern[..wildcard];
    match prefix.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(index) => pattern[..index].to_owned(),
        None => ".".to_owned(),
    }
}

fn build_matcher(
    patterns: &[String],
    literal: bool,
    case: SearchCase,
) -> Result<Regex, String> {
    let mut combined = String::new();
    for (index, pattern) in patterns.iter().enumerate() {
        if index > 0 {
            combined.push('|');
        }
        combined.push_str("(?:");
        if literal {
            combined.push_str(&regex::escape(pattern));
        } else {
            combined.push_str(pattern);
        }
        combined.push(')');
    }
    let case_insensitive = match case {
        SearchCase::Smart => !patterns
            .iter()
            .any(|pattern| pattern.bytes().any(|byte| byte.is_ascii_uppercase())),
        SearchCase::Sensitive => false,
        SearchCase::Insensitive => true,
    };
    RegexBuilder::new(&combined)
        .case_insensitive(case_insensitive)
        .build()
        .map_err(|error| {
            format!(
                "failed to parse `inspect` search input: invalid regular expression `{}`: {error}",
                patterns.join("|")
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

fn collect_paths(
    workspace: &super::WorkspaceRoot,
    roots: &[String],
    include_all_file_types: bool,
) -> Result<Vec<(PathBuf, String)>, String> {
    let default_root = ".".to_owned();
    let roots = if roots.is_empty() {
        std::slice::from_ref(&default_root)
    } else {
        roots
    };
    let mut seen = HashSet::new();
    let mut paths = Vec::new();

    for root in roots {
        let (_, base) = resolve(workspace, root)?;
        if base.is_file() {
            if seen.insert(base.clone()) {
                paths.push((base.clone(), display_path(workspace, &base)));
            }
            continue;
        }

        let mut picker = FilePicker::new(FilePickerOptions {
            base_path: base.to_string_lossy().to_string(),
            enable_content_indexing: false,
            mode: FFFMode::Ai,
            watch: false,
            ..Default::default()
        })
        .map_err(|error| format!("failed to initialize search file index: {error}"))?;
        picker
            .collect_files()
            .map_err(|error| format!("failed to collect search files: {error}"))?;
        for file in picker.get_files().iter().filter(|file| {
            !file.is_deleted() && (include_all_file_types || (!file.is_binary() && file.size > 0))
        }) {
            let absolute = file.absolute_path(&picker, &base);
            if seen.insert(absolute.clone()) {
                let relative = display_path(workspace, &absolute);
                paths.push((absolute, relative));
            }
        }
    }
    paths.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(paths)
}

fn display_path(workspace: &super::WorkspaceRoot, absolute: &Path) -> String {
    absolute
        .strip_prefix(workspace.path())
        .unwrap_or(absolute)
        .display()
        .to_string()
}

fn path_selected(
    absolute: &Path,
    relative: &str,
    includes: &Option<GlobSet>,
    excludes: &Option<GlobSet>,
) -> bool {
    let absolute = absolute.to_string_lossy();
    let included = includes
        .as_ref()
        .is_none_or(|patterns| patterns.is_match(relative) || patterns.is_match(absolute.as_ref()));
    let excluded = excludes.as_ref().is_some_and(|patterns| {
        patterns.is_match(relative) || patterns.is_match(absolute.as_ref())
    });
    included && !excluded
}

fn list_paths<'a>(
    paths: impl Iterator<Item = (&'a Path, &'a str)>,
    includes: &Option<GlobSet>,
    excludes: &Option<GlobSet>,
    max: usize,
) -> (String, InspectSearchResult) {
    let mut model = String::new();
    let mut files = Vec::new();
    let mut total = 0;
    for (absolute, relative) in paths {
        if !path_selected(absolute, relative, includes, excludes) {
            continue;
        }
        total += 1;
        if files.len() < max {
            let _ = writeln!(model, "{relative}");
            files.push(InspectSearchFile {
                path: relative.to_owned(),
                matches: Vec::new(),
            });
        }
    }
    let truncated = total > files.len();
    if truncated {
        let _ = writeln!(
            model,
            "\n[search output truncated: showing first {max} of {total} files; refine the path or glob constraint]"
        );
    }
    if total == 0 {
        model.push_str("no results\n");
    }
    (
        model,
        InspectSearchResult {
            files,
            total_matches: total,
            truncated,
        },
    )
}

fn search_paths<'a>(
    paths: impl Iterator<Item = (&'a Path, &'a str)>,
    matcher: &Regex,
    includes: &Option<GlobSet>,
    excludes: &Option<GlobSet>,
    max: usize,
    files_with_matches: bool,
) -> Result<(String, InspectSearchResult), String> {
    let mut model = String::new();
    let mut files = Vec::new();
    let mut total_matches = 0usize;
    let mut displayed = 0usize;

    for (absolute, relative) in paths {
        if !path_selected(absolute, relative, includes, excludes) {
            continue;
        }
        let content = std::fs::read(absolute)
            .map_err(|error| format!("failed to read `{}`: {error}", absolute.display()))?;

        if files_with_matches {
            if content
                .split(|byte| *byte == b'\n')
                .any(|line| matcher.is_match(line))
            {
                total_matches += 1;
                if displayed < max {
                    let _ = writeln!(model, "{relative}");
                    files.push(InspectSearchFile {
                        path: relative.to_owned(),
                        matches: Vec::new(),
                    });
                    displayed += 1;
                }
            }
            continue;
        }

        let mut file = InspectSearchFile {
            path: relative.to_owned(),
            matches: Vec::new(),
        };
        for (line_index, line) in content.split(|byte| *byte == b'\n').enumerate() {
            if !matcher.is_match(line) {
                continue;
            }
            total_matches += 1;
            if displayed >= max {
                continue;
            }
            if file.matches.is_empty() {
                if !model.is_empty() {
                    model.push('\n');
                }
                let _ = writeln!(model, "{relative}");
            }
            let text = String::from_utf8_lossy(line).into_owned();
            let _ = writeln!(model, "{} {text}", line_index + 1);
            file.matches.push(InspectSearchMatch {
                line_number: Some(line_index + 1),
                line: Some(text),
            });
            displayed += 1;
        }
        if !file.matches.is_empty() {
            files.push(file);
        }
    }

    let truncated = total_matches > displayed;
    if truncated {
        let noun = if files_with_matches {
            "files"
        } else {
            "matches"
        };
        let _ = writeln!(
            model,
            "\n[search output truncated: showing first {displayed} of {total_matches} {noun}; refine the query or path constraint]"
        );
    }
    if total_matches == 0 {
        model.push_str("no results\n");
    }
    Ok((
        model,
        InspectSearchResult {
            files,
            total_matches,
            truncated,
        },
    ))
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

    fn workspace(name: &str) -> (PathBuf, super::super::WorkspaceRoot) {
        let root = std::env::temp_dir().join(format!(
            "inspect-search-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = super::super::WorkspaceRoot::open(&root).unwrap();
        (root, workspace)
    }

    #[test]
    fn options_reject_unknown_flags_with_supported_forms() {
        let error = prepare(&[word("needle"), word("--unknown")]).unwrap_err();

        assert!(error.contains("unsupported option `--unknown`"));
        assert!(error.contains("Use `--` before a pattern or path that starts with `-`"));
    }

    #[test]
    fn execute_accepts_positional_wildcards_without_shell_expansion() {
        let (root, workspace) = workspace("wildcard");
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn needle() {}\n").unwrap();
        std::fs::write(root.join("src/main.txt"), "needle\n").unwrap();
        std::fs::write(root.join("src/nested/lib.rs"), "fn needle() {}\n").unwrap();

        let request =
            prepare(&[word("needle"), word("src/*.rs"), word("src/nested/*.rs")]).unwrap();
        let model_output = execute(&workspace, &request).unwrap().model;

        assert!(model_output.contains("src/main.rs\n1 fn needle() {}"));
        assert!(model_output.contains("src/nested/lib.rs\n1 fn needle() {}"));
        assert!(!model_output.contains("main.txt"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn execute_supports_rg_pattern_glob_and_files_with_matches_forms() {
        let (root, workspace) = workspace("rg-options");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/keep.rs"), "Needle\n").unwrap();
        std::fs::write(root.join("src/skip.generated.rs"), "Needle\n").unwrap();
        std::fs::write(root.join("src/keep.txt"), "Needle\n").unwrap();

        let request = prepare(&[
            word("--regexp=Needle"),
            word("-l"),
            word("-s"),
            word("--color=never"),
            word("src"),
            word("--glob=*.rs"),
            word("-g!*.generated.rs"),
        ])
        .unwrap();
        let model_output = execute(&workspace, &request).unwrap().model;

        assert_eq!(model_output, "src/keep.rs\n");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn files_mode_needs_no_pattern_includes_empty_files_and_is_bounded() {
        let (root, workspace) = workspace("files");
        std::fs::create_dir_all(root.join("src")).unwrap();
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(root.join("src").join(name), "").unwrap();
        }
        std::fs::write(root.join("src/readme.md"), "").unwrap();

        let request = prepare(&[
            word("src"),
            word("--glob"),
            word("*.rs"),
            word("--files"),
            word("--max=2"),
        ])
        .unwrap();
        let model_output = execute(&workspace, &request).unwrap().model;

        assert!(model_output.starts_with("src/a.rs\nsrc/b.rs\n"));
        assert!(!model_output.contains("src/c.rs\n"));
        assert!(model_output.contains("showing first 2 of 3 files"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn execute_applies_regex_and_path_filters_through_workspace_boundary() {
        let (root, workspace) = workspace("filters");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn take_body() {}\nfn parse_anchor() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/generated.rs"), "fn take_generated() {}\n").unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests/search.rs"), "fn parse_test() {}\n").unwrap();

        let request = prepare(&[
            word("fn (take|parse)"),
            word("src"),
            word("tests"),
            word("--glob"),
            word("*.rs"),
            word("--exclude"),
            word("*generated.rs"),
        ])
        .unwrap();
        let model_output = execute(&workspace, &request).unwrap().model;

        assert!(model_output.contains("src/main.rs"));
        assert!(model_output.contains("take_body"));
        assert!(model_output.contains("parse_anchor"));
        assert!(model_output.contains("tests/search.rs"));
        assert!(model_output.contains("parse_test"));
        assert!(!model_output.contains("generated.rs"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
