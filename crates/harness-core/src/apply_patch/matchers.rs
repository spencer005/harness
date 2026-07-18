use std::{ops::Range, path::Path, str};

use super::{ast::UpdateChunk, error::ApplyPatchError};

#[derive(Debug, Clone, PartialEq, Eq)]
struct LineSpan {
    content: Range<usize>,
    full: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Replacement {
    start_line: usize,
    old_len: usize,
    new_lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchMode {
    Exact,
    TrimEnd,
    Trim,
    Normalized,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SequenceSearch {
    None,
    Unique(usize),
    Ambiguous,
}

pub(crate) fn derive_updated_file(
    path: &Path,
    original: &[u8],
    chunks: &[UpdateChunk],
) -> Result<Vec<u8>, ApplyPatchError> {
    let lines = split_lines(original);
    let replacements = compute_replacements(original, &lines, path, chunks)?;
    apply_replacements(path, original, &lines, &replacements)
}

fn compute_replacements(
    original: &[u8],
    lines: &[LineSpan],
    path: &Path,
    chunks: &[UpdateChunk],
) -> Result<Vec<Replacement>, ApplyPatchError> {
    let mut replacements = Vec::new();
    let mut line_index = 0;

    for chunk in chunks {
        if let Some(ctx_line) = &chunk.change_context {
            match find_sequence(
                original,
                lines,
                std::slice::from_ref(ctx_line),
                line_index,
                false,
            ) {
                SequenceSearch::Unique(index) => {
                    line_index = index + 1;
                }
                SequenceSearch::None => {
                    return Err(ApplyPatchError::Apply(format!(
                        "Failed to find context '{}' in {}",
                        ctx_line,
                        path.display()
                    )));
                }
                SequenceSearch::Ambiguous => {
                    return Err(ApplyPatchError::Apply(format!(
                        "Ambiguous context '{}' in {}",
                        ctx_line,
                        path.display()
                    )));
                }
            }
        }

        if chunk.old_lines.is_empty() {
            let insertion_index = if chunk.is_end_of_file {
                lines.len()
            } else if chunk.change_context.is_some() {
                line_index
            } else {
                insertion_point_before_final_newline(lines)
            };
            replacements.push(Replacement {
                start_line: insertion_index,
                old_len: 0,
                new_lines: chunk.new_lines.clone(),
            });
            line_index = insertion_index;
            continue;
        }

        match find_sequence(
            original,
            lines,
            &chunk.old_lines,
            line_index,
            chunk.is_end_of_file,
        ) {
            SequenceSearch::Unique(start_index) => {
                replacements.push(Replacement {
                    start_line: start_index,
                    old_len: chunk.old_lines.len(),
                    new_lines: chunk.new_lines.clone(),
                });
                line_index = start_index + chunk.old_lines.len();
            }
            SequenceSearch::None => {
                return Err(ApplyPatchError::Apply(format!(
                    "Failed to find expected lines in {}:\n{}",
                    path.display(),
                    chunk.old_lines.join("\n")
                )));
            }
            SequenceSearch::Ambiguous => {
                return Err(ApplyPatchError::Apply(format!(
                    "Ambiguous match for expected lines in {}:\n{}",
                    path.display(),
                    chunk.old_lines.join("\n")
                )));
            }
        }
    }

    Ok(replacements)
}

fn insertion_point_before_final_newline(lines: &[LineSpan]) -> usize {
    if lines
        .last()
        .is_some_and(|line| line.content.start == line.content.end)
    {
        lines.len() - 1
    } else {
        lines.len()
    }
}

fn split_lines(bytes: &[u8]) -> Vec<LineSpan> {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'\n' {
            end += 1;
        }
        let full_end = if end < bytes.len() && bytes[end] == b'\n' {
            end + 1
        } else {
            end
        };
        lines.push(LineSpan {
            content: start..end,
            full: start..full_end,
        });
        start = full_end;
    }
    lines
}

fn find_sequence(
    original: &[u8],
    lines: &[LineSpan],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> SequenceSearch {
    if pattern.is_empty() {
        return SequenceSearch::Unique(start);
    }
    if pattern.len() > lines.len() {
        return SequenceSearch::None;
    }
    if eof {
        let index = lines.len() - pattern.len();
        if sequence_matches(original, lines, pattern, index, MatchMode::Exact) {
            return SequenceSearch::Unique(index);
        }
        for mode in [MatchMode::TrimEnd, MatchMode::Trim, MatchMode::Normalized] {
            if sequence_matches(original, lines, pattern, index, mode) {
                return SequenceSearch::Unique(index);
            }
        }
        return SequenceSearch::None;
    }

    for mode in [
        MatchMode::Exact,
        MatchMode::TrimEnd,
        MatchMode::Trim,
        MatchMode::Normalized,
    ] {
        match sequence_match_for_mode(original, lines, pattern, start, mode) {
            SequenceSearch::None => {}
            result => return result,
        }
    }

    SequenceSearch::None
}

fn sequence_match_for_mode(
    original: &[u8],
    lines: &[LineSpan],
    pattern: &[String],
    start: usize,
    mode: MatchMode,
) -> SequenceSearch {
    if start > lines.len().saturating_sub(pattern.len()) {
        return SequenceSearch::None;
    }

    let mut matches = (start..=lines.len() - pattern.len())
        .filter(|index| sequence_matches(original, lines, pattern, *index, mode));
    let Some(first) = matches.next() else {
        return SequenceSearch::None;
    };
    if matches.next().is_some() {
        SequenceSearch::Ambiguous
    } else {
        SequenceSearch::Unique(first)
    }
}

fn sequence_matches(
    original: &[u8],
    lines: &[LineSpan],
    pattern: &[String],
    index: usize,
    mode: MatchMode,
) -> bool {
    pattern.iter().enumerate().all(|(offset, pattern_line)| {
        line_matches(
            &original[lines[index + offset].content.clone()],
            pattern_line,
            mode,
        )
    })
}

fn line_matches(original_line: &[u8], pattern_line: &str, mode: MatchMode) -> bool {
    match mode {
        MatchMode::Exact => original_line == pattern_line.as_bytes(),
        MatchMode::TrimEnd => str::from_utf8(original_line)
            .is_ok_and(|line| line.trim_end() == pattern_line.trim_end()),
        MatchMode::Trim => {
            str::from_utf8(original_line).is_ok_and(|line| line.trim() == pattern_line.trim())
        }
        MatchMode::Normalized => str::from_utf8(original_line)
            .is_ok_and(|line| normalize_for_matching(line) == normalize_for_matching(pattern_line)),
    }
}

fn apply_replacements(
    path: &Path,
    original: &[u8],
    lines: &[LineSpan],
    replacements: &[Replacement],
) -> Result<Vec<u8>, ApplyPatchError> {
    let mut output = Vec::with_capacity(original.len());
    let mut byte_cursor = 0;
    for replacement in replacements {
        let byte_start = match lines.get(replacement.start_line) {
            Some(line) => line.full.start,
            None if replacement.start_line == lines.len() => original.len(),
            None => {
                return Err(ApplyPatchError::Apply(format!(
                    "Invalid replacement position in {}",
                    path.display()
                )));
            }
        };
        let byte_end = if replacement.old_len == 0 {
            byte_start
        } else {
            let end_line = replacement
                .start_line
                .checked_add(replacement.old_len - 1)
                .and_then(|index| lines.get(index))
                .ok_or_else(|| {
                    ApplyPatchError::Apply(format!(
                        "Invalid replacement range in {}",
                        path.display()
                    ))
                })?;
            end_line.full.end
        };

        if byte_start < byte_cursor {
            return Err(ApplyPatchError::Apply(format!(
                "Overlapping or out-of-order update hunks in {}",
                path.display()
            )));
        }

        output.extend_from_slice(&original[byte_cursor..byte_start]);
        output.extend_from_slice(&render_replacement(original, lines, replacement));
        byte_cursor = byte_end;
    }
    output.extend_from_slice(&original[byte_cursor..]);
    Ok(output)
}

fn render_replacement(original: &[u8], lines: &[LineSpan], replacement: &Replacement) -> Vec<u8> {
    let mut bytes = Vec::new();
    if replacement.old_len == 0 {
        if replacement.start_line == lines.len()
            && !original.is_empty()
            && !original.ends_with(b"\n")
        {
            bytes.push(b'\n');
        }
        for line in &replacement.new_lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        return bytes;
    }

    let last_replaced_line = &lines[replacement.start_line + replacement.old_len - 1];
    let replaced_region_ended_with_newline = last_replaced_line.full.end
        > last_replaced_line.content.end
        && original[last_replaced_line.full.end - 1] == b'\n';
    for (index, line) in replacement.new_lines.iter().enumerate() {
        bytes.extend_from_slice(line.as_bytes());
        if index + 1 < replacement.new_lines.len() || replaced_region_ended_with_newline {
            bytes.push(b'\n');
        }
    }
    bytes
}

fn normalize_for_matching(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| match character {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_file_without_requiring_unrelated_bytes_to_be_utf8() {
        let original = b"\xff\nmatch\n";
        let chunks = vec![UpdateChunk {
            change_context: None,
            old_lines: vec!["match".to_string()],
            new_lines: vec!["changed".to_string()],
            is_end_of_file: false,
        }];

        let updated = derive_updated_file(Path::new("bytes.txt"), original, &chunks).unwrap();

        assert_eq!(updated, b"\xff\nchanged\n");
    }

    #[test]
    fn replacement_preserves_missing_final_newline() {
        let chunks = vec![UpdateChunk {
            change_context: None,
            old_lines: vec!["last".to_string()],
            new_lines: vec!["changed".to_string()],
            is_end_of_file: false,
        }];

        let updated =
            derive_updated_file(Path::new("newline.txt"), b"first\nlast", &chunks).unwrap();

        assert_eq!(updated, b"first\nchanged");
    }

    #[test]
    fn duplicate_matches_are_ambiguous() {
        let chunks = vec![UpdateChunk {
            change_context: None,
            old_lines: vec!["same".to_string()],
            new_lines: vec!["changed".to_string()],
            is_end_of_file: false,
        }];

        let error =
            derive_updated_file(Path::new("ambiguous.txt"), b"same\nsame\n", &chunks).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Ambiguous match for expected lines in ambiguous.txt:\nsame"
        );
    }

    #[test]
    fn addition_before_an_earlier_replacement_is_rejected_without_panicking() {
        let chunks = vec![
            UpdateChunk {
                change_context: None,
                old_lines: Vec::new(),
                new_lines: vec!["appended".to_string()],
                is_end_of_file: false,
            },
            UpdateChunk {
                change_context: None,
                old_lines: vec!["first".to_string()],
                new_lines: vec!["changed".to_string()],
                is_end_of_file: false,
            },
        ];

        let error =
            derive_updated_file(Path::new("ordered.txt"), b"first\nlast\n", &chunks).unwrap_err();

        assert_eq!(
            error.to_string(),
            "Failed to find expected lines in ordered.txt:\nfirst"
        );
    }

    #[test]
    fn apply_replacements_defensively_rejects_out_of_order_ranges() {
        let original = b"first\nlast\n";
        let lines = split_lines(original);
        let replacements = vec![
            Replacement {
                start_line: lines.len(),
                old_len: 0,
                new_lines: vec!["appended".to_string()],
            },
            Replacement {
                start_line: 0,
                old_len: 1,
                new_lines: vec!["changed".to_string()],
            },
        ];

        let error = apply_replacements(Path::new("ordered.txt"), original, &lines, &replacements)
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "Overlapping or out-of-order update hunks in ordered.txt"
        );
    }
}
