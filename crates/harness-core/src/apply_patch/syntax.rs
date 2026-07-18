use std::path::Path;

use super::{
    ast::{PatchDocument, PatchHunk, PatchPath, UpdateChunk},
    error::ApplyPatchError,
};

const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
const END_PATCH_MARKER: &str = "*** End Patch";
const ADD_FILE_MARKER: &str = "*** Add File: ";
const DELETE_FILE_MARKER: &str = "*** Delete File: ";
const UPDATE_FILE_MARKER: &str = "*** Update File: ";
const MOVE_TO_MARKER: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";
const CHANGE_CONTEXT_MARKER: &str = "@@ ";
const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";

pub(crate) fn parse_patch(patch: &str) -> Result<PatchDocument, ApplyPatchError> {
    let trimmed_patch = patch.trim_matches(['\n', '\r']);
    let lines: Vec<&str> = trimmed_patch.lines().collect();
    check_patch_boundaries(&lines)?;

    let mut remaining_lines = &lines[1..lines.len() - 1];
    let mut line_number = 2;
    let mut hunks = Vec::new();
    while !remaining_lines.is_empty() {
        let (hunk, parsed_lines) = parse_one_hunk(remaining_lines, line_number)?;
        hunks.push(hunk);
        line_number += parsed_lines;
        remaining_lines = &remaining_lines[parsed_lines..];
    }
    Ok(PatchDocument::new(hunks))
}

fn check_patch_boundaries(lines: &[&str]) -> Result<(), ApplyPatchError> {
    if lines.first().copied() != Some(BEGIN_PATCH_MARKER) {
        return Err(ApplyPatchError::InvalidPatch(
            "The first line of the patch must be '*** Begin Patch'".to_string(),
        ));
    }
    if lines.last().copied() != Some(END_PATCH_MARKER) {
        return Err(ApplyPatchError::InvalidPatch(
            "The last line of the patch must be '*** End Patch'".to_string(),
        ));
    }
    Ok(())
}

fn parse_one_hunk(
    lines: &[&str],
    line_number: usize,
) -> Result<(PatchHunk, usize), ApplyPatchError> {
    let first_line = lines[0];
    if let Some(path) = first_line.strip_prefix(ADD_FILE_MARKER) {
        let mut contents = Vec::new();
        let mut parsed_lines = 1;
        for add_line in &lines[1..] {
            if let Some(line_to_add) = add_line.strip_prefix('+') {
                contents.extend_from_slice(line_to_add.as_bytes());
                contents.push(b'\n');
                parsed_lines += 1;
            } else {
                break;
            }
        }
        if parsed_lines == 1 {
            return Err(ApplyPatchError::InvalidHunk {
                message: format!(
                    "Add file hunk for path '{}' is empty",
                    Path::new(path).display()
                ),
                line_number,
            });
        }
        return Ok((
            PatchHunk::AddFile {
                path: PatchPath::new(path),
                contents,
            },
            parsed_lines,
        ));
    }

    if let Some(path) = first_line.strip_prefix(DELETE_FILE_MARKER) {
        return Ok((
            PatchHunk::DeleteFile {
                path: PatchPath::new(path),
            },
            1,
        ));
    }

    if let Some(path) = first_line.strip_prefix(UPDATE_FILE_MARKER) {
        let mut remaining_lines = &lines[1..];
        let mut parsed_lines = 1;
        let move_path = remaining_lines
            .first()
            .and_then(|line| line.strip_prefix(MOVE_TO_MARKER));

        if move_path.is_some() {
            remaining_lines = &remaining_lines[1..];
            parsed_lines += 1;
        }

        let mut chunks = Vec::new();
        while !remaining_lines.is_empty() {
            if remaining_lines[0].is_empty() {
                parsed_lines += 1;
                remaining_lines = &remaining_lines[1..];
                continue;
            }
            if remaining_lines[0].starts_with("*** ") {
                break;
            }

            let (chunk, chunk_lines) = parse_update_file_chunk(
                remaining_lines,
                line_number + parsed_lines,
                chunks.is_empty(),
            )?;
            chunks.push(chunk);
            parsed_lines += chunk_lines;
            remaining_lines = &remaining_lines[chunk_lines..];
        }

        if chunks.is_empty() && move_path.is_none() {
            return Err(ApplyPatchError::InvalidHunk {
                message: format!(
                    "Update file hunk for path '{}' is empty",
                    Path::new(path).display()
                ),
                line_number,
            });
        }

        return Ok((
            PatchHunk::UpdateFile {
                path: PatchPath::new(path),
                move_path: move_path.map(PatchPath::new),
                chunks,
            },
            parsed_lines,
        ));
    }

    Err(ApplyPatchError::InvalidHunk {
        message: format!(
            "'{first_line}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
        ),
        line_number,
    })
}

fn parse_update_file_chunk(
    lines: &[&str],
    line_number: usize,
    allow_missing_context: bool,
) -> Result<(UpdateChunk, usize), ApplyPatchError> {
    if lines.is_empty() {
        return Err(ApplyPatchError::InvalidHunk {
            message: "Update hunk does not contain any lines".to_string(),
            line_number,
        });
    }

    let (change_context, start_index) = if lines[0] == EMPTY_CHANGE_CONTEXT_MARKER {
        (None, 1)
    } else if let Some(context) = lines[0].strip_prefix(CHANGE_CONTEXT_MARKER) {
        (Some(context.to_string()), 1)
    } else if allow_missing_context {
        (None, 0)
    } else {
        return Err(ApplyPatchError::InvalidHunk {
            message: format!(
                "Expected update hunk to start with a @@ context marker, got: '{}'",
                lines[0]
            ),
            line_number,
        });
    };

    if start_index >= lines.len() {
        return Err(ApplyPatchError::InvalidHunk {
            message: "Update hunk does not contain any lines".to_string(),
            line_number: line_number + start_index,
        });
    }

    let mut chunk = UpdateChunk {
        change_context,
        old_lines: Vec::new(),
        new_lines: Vec::new(),
        is_end_of_file: false,
    };
    let mut parsed_body_lines = 0;
    for line in &lines[start_index..] {
        match *line {
            EOF_MARKER => {
                if parsed_body_lines == 0 {
                    return Err(ApplyPatchError::InvalidHunk {
                        message: "Update hunk does not contain any lines".to_string(),
                        line_number: line_number + start_index,
                    });
                }
                chunk.is_end_of_file = true;
                parsed_body_lines += 1;
                break;
            }
            line_contents => match line_contents.chars().next() {
                None => {
                    chunk.old_lines.push(String::new());
                    chunk.new_lines.push(String::new());
                    parsed_body_lines += 1;
                }
                Some(' ') => {
                    chunk.old_lines.push(line_contents[1..].to_string());
                    chunk.new_lines.push(line_contents[1..].to_string());
                    parsed_body_lines += 1;
                }
                Some('+') => {
                    chunk.new_lines.push(line_contents[1..].to_string());
                    parsed_body_lines += 1;
                }
                Some('-') => {
                    chunk.old_lines.push(line_contents[1..].to_string());
                    parsed_body_lines += 1;
                }
                _ => {
                    if parsed_body_lines == 0 {
                        return Err(ApplyPatchError::InvalidHunk {
                            message: format!(
                                "Unexpected line found in update hunk: '{line_contents}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                            ),
                            line_number: line_number + start_index,
                        });
                    }
                    break;
                }
            },
        }
    }

    if parsed_body_lines == 0 || (chunk.old_lines.is_empty() && chunk.new_lines.is_empty()) {
        return Err(ApplyPatchError::InvalidHunk {
            message: "Update hunk does not contain any lines".to_string(),
            line_number: line_number + start_index,
        });
    }

    Ok((chunk, parsed_body_lines + start_index))
}
