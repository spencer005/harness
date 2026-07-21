//! Fuzzy path correction for user-supplied file paths.
//!
//! When a tool receives a path that does not exist (e.g. `ira/crates/ira_ast/lexer.rs`
//! while the real path is `ira/crates/ira_ast/src/lexer.rs`), this module suggests
//! corrections by:
//!
//! 1. Finding the longest existing prefix of the path.
//! 2. Walking that directory to find files matching the missing suffix.
//! 3. Ranking candidates by edit distance.
//! 4. Listing directory contents for additional context.

use std::path::Path;

/// Result of a path correction attempt.
pub struct Correction {
    /// The corrected full path suggestion (relative to workspace).
    pub suggested: String,
    /// The deepest existing directory prefix (relative to workspace).
    pub deepest_prefix: String,
    /// Listing of the deepest existing directory for context.
    pub listing: Vec<String>,
}

/// If `requested` does not exist under `workspace_root`, attempt to suggest a
/// correction by walking the filesystem near the longest existing prefix.
///
/// Returns `None` when:
/// - The path already exists (no correction needed).
/// - No part of the path exists at all.
/// - No sufficiently close match is found.
pub fn suggest_correction(workspace_root: &Path, requested: &str) -> Option<Correction> {
    let abs = workspace_root.join(requested);
    if abs.exists() {
        return None;
    }

    // Split into components for prefix walking.
    let parts: Vec<&str> = requested.split('/').collect();

    // Find the deepest existing directory prefix.
    let prefix_len = deepest_existing_prefix(workspace_root, &parts)?;

    let existing_parts = &parts[..prefix_len];
    let missing_parts = &parts[prefix_len..];
    let existing_prefix = existing_parts.join("/");
    let missing_suffix = missing_parts.join("/");

    let abs_prefix = if existing_prefix.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(&existing_prefix)
    };

    // Collect candidate files from the existing prefix directory (limited walk).
    let mut candidates: Vec<(usize, String)> = Vec::new();
    let mut budget = 1_000;
    walk_files(
        &abs_prefix,
        &missing_suffix,
        "",
        &mut candidates,
        3,         // max walk depth
        &mut budget,
    );

    // Sort by edit distance, remove duplicates.
    candidates.sort();
    candidates.dedup();
    candidates.sort_by_key(|(dist, _)| *dist);

    // Collect a simple directory listing of the prefix for context.
    let listing = list_directory(&abs_prefix, &existing_prefix);

    // Prepend the existing prefix to get the full relative path from workspace.
    candidates.into_iter().next().map(|(_, rel)| {
        let suggested = if existing_prefix.is_empty() {
            rel
        } else {
            format!("{existing_prefix}/{rel}")
        };
        Correction {
            suggested,
            deepest_prefix: existing_prefix,
            listing,
        }
    })
}

/// Returns the number of leading path components that form an existing
/// directory under `workspace_root`. Returns `None` if even the workspace
/// root isn't a directory (shouldn't happen in practice).
fn deepest_existing_prefix(workspace_root: &Path, parts: &[&str]) -> Option<usize> {
    let mut len = parts.len();
    loop {
        let prefix = parts[..len].join("/");
        let check = if prefix.is_empty() {
            workspace_root.to_path_buf()
        } else {
            workspace_root.join(&prefix)
        };
        if check.is_dir() {
            return Some(len);
        }
        if len == 0 {
            return None;
        }
        len -= 1;
    }
}

/// Recursively walk `current` (under `root`) collecting file paths whose
/// relative path from `root` is a close edit-distance match to `target`.
///
/// `relative` is the path from `root` to `current`.
fn walk_files(
    root: &Path,
    target: &str,
    relative: &str,
    candidates: &mut Vec<(usize, String)>,
    depth: usize,
    budget: &mut usize,
) {
    if depth == 0 || *budget == 0 {
        return;
    }

    let Ok(entries) = std::fs::read_dir(root) else { return };

    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;

        let name = entry.file_name().to_string_lossy().to_string();
        // Skip hidden files/directories.
        if name.starts_with('.') {
            continue;
        }

        // The path of this entry relative to the walk root.
        let rel = if relative.is_empty() {
            name.clone()
        } else {
            format!("{relative}/{name}")
        };

        let Ok(file_type) = entry.file_type() else { continue };

        if file_type.is_dir() {
            // Recurse into subdirectories.
            walk_files(
                &entry.path(),
                target,
                &rel,
                candidates,
                depth - 1,
                budget,
            );
        } else if file_type.is_file() {
            let score = levenshtein(&rel, target);
            candidates.push((score, rel));
        }
    }
}

/// Generate a simple sorted directory listing.
fn list_directory(abs_path: &Path, prefix: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(abs_path) else { return Vec::new() };

    let mut listing: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                return None;
            }
            let path_str = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            let is_dir = entry.file_type().ok().map(|t| t.is_dir()).unwrap_or(false);
            Some(if is_dir {
                format!("{path_str}/")
            } else {
                path_str
            })
        })
        .collect();
    listing.sort();
    listing
}

/// Compute Levenshtein distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_len = a.len();
    let b_len = b.len();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let n = a_chars.len();
    let m = b_chars.len();

    // Single-row DP for efficiency.
    let mut prev = vec![0usize; m + 1];
    let mut curr = vec![0usize; m + 1];

    for j in 0..=m {
        prev[j] = j;
    }

    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (curr[j - 1] + 1)
                .min(prev[j] + 1)
                .min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[m]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_levenshtein_identical() {
        assert_eq!(levenshtein("hello", "hello"), 0);
    }

    #[test]
    fn test_levenshtein_empty() {
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
    }

    #[test]
    fn test_levenshtein_substitution() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }

    #[test]
    fn test_levenshtein_insert_delete() {
        assert_eq!(levenshtein("abc", "ac"), 1);
        assert_eq!(levenshtein("ac", "abc"), 1);
    }

    #[test]
    fn test_list_directory() {
        // Create a temp dir structure.
        let tmp = std::env::temp_dir().join("harness-path-test-list");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("a.txt"), "").unwrap();
        std::fs::write(tmp.join("b.rs"), "").unwrap();

        let listing = list_directory(&tmp, "");
        assert!(listing.contains(&"a.txt".to_string()));
        assert!(listing.contains(&"b.rs".to_string()));
        assert!(listing.contains(&"sub/".to_string()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_list_directory_with_prefix() {
        let tmp = std::env::temp_dir().join("harness-path-test-prefix");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("lib.rs"), "").unwrap();

        let listing = list_directory(&tmp, "project/src");
        assert!(listing.contains(&"project/src/lib.rs".to_string()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_suggest_correction_when_path_exists() {
        let tmp = std::env::temp_dir().join("harness-path-exists");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("main.rs"), "fn main() {}").unwrap();

        assert!(suggest_correction(&tmp, "main.rs").is_none());
        assert!(suggest_correction(&tmp, "src").is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_suggest_correction_missing_subdir() {
        let tmp = std::env::temp_dir().join("harness-path-subdir");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("ira/crates/ira_ast/src")).unwrap();
        // File named lexer.rs inside src/.
        std::fs::write(tmp.join("ira/crates/ira_ast/src/lexer.rs"), "").unwrap();

        // User writes path missing "src/" subdirectory.
        let result = suggest_correction(&tmp, "ira/crates/ira_ast/lexer.rs");
        assert!(result.is_some(), "should find a correction");
        let correction = result.unwrap();
        assert_eq!(
            correction.suggested, "ira/crates/ira_ast/src/lexer.rs",
            "should insert missing src/ subdirectory"
        );
        // Listing should show contents of ira_ast/.
        assert!(correction.listing.contains(&"ira/crates/ira_ast/src/".to_string()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_suggest_correction_typo_in_filename() {
        let tmp = std::env::temp_dir().join("harness-path-typo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("src/lexer.rs"), "").unwrap();

        let result = suggest_correction(&tmp, "src/lexerr.rs");
        assert!(result.is_some(), "should correct typo");
        assert_eq!(result.unwrap().suggested, "src/lexer.rs");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_suggest_correction_typo_in_dirname() {
        let tmp = std::env::temp_dir().join("harness-path-dir-typo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("ira_ast/src")).unwrap();
        std::fs::write(tmp.join("ira_ast/src/lexer.rs"), "").unwrap();

        let result = suggest_correction(&tmp, "ira_astt/lexer.rs");
        assert!(result.is_some(), "should correct directory typo");
        let correction = result.unwrap();
        assert_eq!(correction.suggested, "ira_ast/src/lexer.rs");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_deep_nonexistent_path_returns_none() {
        let tmp = std::env::temp_dir().join("harness-path-deep-none");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // Nothing exists under tmp.
        let result = suggest_correction(&tmp, "nonexistent/deep/file.rs");
        assert!(result.is_none(), "nothing exists, no correction");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
