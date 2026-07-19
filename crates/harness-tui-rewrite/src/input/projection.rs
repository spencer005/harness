//! Safe visual projection of user-owned prompt text.

use std::borrow::Cow;

use unicode_width::UnicodeWidthStr;

use crate::control::{
    C0_CONTROL_END, C0_CONTROL_PICTURE_OFFSET, C0_CONTROL_START, DELETE, DELETE_CONTROL_PICTURE,
    LINE_FEED, is_directional_formatting,
};

/// Visible substitute for directional formatting and non-C0 controls.
const VISIBLE_SUBSTITUTE: char = '\u{2426}';
/// Visible base added when a standalone grapheme otherwise occupies no cells.
const DOTTED_CIRCLE: char = '\u{25cc}';
/// Presentation marker for a pathologically large source grapheme.
const OMITTED_GRAPHEME: &str = "…";
/// Maximum source bytes projected for one grapheme.
///
/// Exact prompt storage remains unchanged. This bound prevents one combining
/// sequence from forcing a prompt frame to copy the complete prompt.
const MAX_PROJECTED_GRAPHEME_SOURCE_BYTES: usize = 64 * 1024;

/// Returns a terminal-safe visual representation for one source grapheme.
///
/// The returned text is presentation only. Prompt storage and runtime
/// submission retain the original source grapheme unchanged.
pub(super) fn display_grapheme(grapheme: &str) -> Cow<'_, str> {
    if grapheme.len() > MAX_PROJECTED_GRAPHEME_SOURCE_BYTES {
        return Cow::Borrowed(OMITTED_GRAPHEME);
    }

    let needs_projection = grapheme.chars().any(requires_projection);
    if !needs_projection && grapheme.width() > 0 {
        return Cow::Borrowed(grapheme);
    }

    let mut projected = String::with_capacity(grapheme.len().max(3));
    for character in grapheme.chars() {
        if character == LINE_FEED {
            continue;
        }
        projected.push(project_character(character));
    }
    if projected.width() == 0 && !projected.is_empty() {
        projected.insert(0, DOTTED_CIRCLE);
    }
    Cow::Owned(projected)
}

/// Returns the cell width of the safe projection.
pub(super) fn display_width(grapheme: &str) -> usize {
    display_grapheme(grapheme).width()
}

fn requires_projection(character: char) -> bool {
    character.is_control() || is_directional_formatting(character)
}

fn project_character(character: char) -> char {
    if (C0_CONTROL_START..=C0_CONTROL_END).contains(&character) {
        return char::from_u32(C0_CONTROL_PICTURE_OFFSET + u32::from(character))
            .expect("C0 control picture is a valid Unicode scalar");
    }
    if character == DELETE {
        return DELETE_CONTROL_PICTURE;
    }
    if requires_projection(character) {
        return VISIBLE_SUBSTITUTE;
    }
    character
}

#[cfg(test)]
mod tests {
    use unicode_segmentation::UnicodeSegmentation;

    use super::{
        MAX_PROJECTED_GRAPHEME_SOURCE_BYTES, OMITTED_GRAPHEME, display_grapheme, display_width,
    };

    #[test]
    fn controls_are_visible_without_changing_source_text() {
        let source = "\u{1b}[31m\u{202e}\t";
        let projected = source
            .graphemes(true)
            .map(display_grapheme)
            .collect::<String>();

        assert_eq!(projected, "␛[31m␦␉");
        assert_eq!(source, "\u{1b}[31m\u{202e}\t");
        assert!(projected.chars().all(|character| !character.is_control()));
    }

    #[test]
    fn standalone_zero_width_graphemes_receive_a_visible_base() {
        assert_eq!(display_grapheme("\u{301}"), "◌\u{301}");
    }
    #[test]
    fn oversized_grapheme_has_a_bounded_projection() {
        let source = format!("a{}", "\u{301}".repeat(MAX_PROJECTED_GRAPHEME_SOURCE_BYTES));
        let grapheme = source.graphemes(true).next().unwrap();

        assert_eq!(display_grapheme(grapheme), OMITTED_GRAPHEME);
        assert_eq!(display_width(grapheme), 1);
        assert_eq!(source.len(), 1 + MAX_PROJECTED_GRAPHEME_SOURCE_BYTES * 2);
    }
}
