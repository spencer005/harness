//! Pure text formatting helpers shared by harness submodules.

/// Return a compact single-line diagnostic summary.
pub(super) fn diagnostic_summary(message: &str) -> String {
    const MAX_DIAGNOSTIC_CHARS: usize = 240;

    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX_DIAGNOSTIC_CHARS {
        return normalized;
    }
    let mut summary = normalized
        .chars()
        .take(MAX_DIAGNOSTIC_CHARS.saturating_sub(1))
        .collect::<String>();
    summary.push('…');
    summary
}
