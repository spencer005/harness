use std::{io, io::Read};

use anyhow::{Context, Result};
use tiktoken_rs::{CoreBPE, Rank, o200k_base_singleton};

const TOKEN_BOUNDARY: &str = "│";
const DEFAULT_FUZZY_LIMIT: usize = 20;
const O200K_BASE_ORDINARY_TOKEN_COUNT: Rank = 199_998;

enum Mode {
    Mark,
    FilterMulti,
    FilterSingle,
    Fuzzy { query: FuzzyQuery, limit: usize },
}

struct FuzzyQuery {
    chars: Vec<char>,
    lowercase_chars: Vec<char>,
}

impl FuzzyQuery {
    fn new(text: String) -> Result<Self> {
        anyhow::ensure!(!text.is_empty(), "fuzzy query must not be empty");

        let chars = text.chars().collect();
        let lowercase_chars = text.chars().flat_map(char::to_lowercase).collect();

        Ok(Self {
            chars,
            lowercase_chars,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FuzzyScore {
    quality: u8,
    omitted_leading_space: bool,
    internal_gaps: usize,
    start: usize,
    extra_chars: usize,
}

fn main() -> Result<()> {
    match parse_mode()? {
        Mode::Mark => mark_tokens(),
        Mode::FilterMulti => filter_lines(false),
        Mode::FilterSingle => filter_lines(true),
        Mode::Fuzzy { query, limit } => fuzzy_query(&query, limit),
    }
}

fn parse_mode() -> Result<Mode> {
    let mut args = std::env::args().skip(1);
    let mode = match args.next().as_deref() {
        None | Some("mark") => Mode::Mark,
        Some("--filter-multi") => Mode::FilterMulti,
        Some("--filter-single") => Mode::FilterSingle,
        Some("--fuzzy") => {
            let query = args
                .next()
                .context("--fuzzy requires a non-empty query argument")?;
            let query = FuzzyQuery::new(query)?;
            let limit = match args.next() {
                Some(raw_limit) => {
                    let limit = raw_limit
                        .parse::<usize>()
                        .with_context(|| format!("invalid result limit {raw_limit:?}"))?;
                    anyhow::ensure!(limit > 0, "result limit must be greater than zero");
                    limit
                }
                None => DEFAULT_FUZZY_LIMIT,
            };
            Mode::Fuzzy { query, limit }
        }
        Some(_) => return usage_error(),
    };

    anyhow::ensure!(
        args.next().is_none(),
        "usage:\n  token_inspector [mark|--filter-multi|--filter-single]\n  \
         token_inspector --fuzzy <query> [limit]"
    );

    Ok(mode)
}

fn usage_error<T>() -> Result<T> {
    anyhow::bail!(
        "usage:\n  token_inspector [mark|--filter-multi|--filter-single]\n  \
         token_inspector --fuzzy <query> [limit]"
    )
}

fn read_stdin() -> Result<String> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .context("failed to read stdin")?;
    Ok(input)
}

fn mark_tokens() -> Result<()> {
    let input = read_stdin()?;
    let tokenizer = o200k_base_singleton();

    for token in tokenizer.encode_ordinary(&input) {
        let token_text = tokenizer
            .decode(&[token])
            .with_context(|| format!("failed to decode token {token}"))?;
        print!("{TOKEN_BOUNDARY}{token_text}");
    }

    Ok(())
}

fn filter_lines(single: bool) -> Result<()> {
    let input = read_stdin()?;
    let tokenizer = o200k_base_singleton();

    for line in input.lines() {
        let token_count = tokenizer.encode_ordinary(line).len();
        if (single && token_count == 1) || (!single && token_count > 1) {
            if single {
                println!("{line}");
            } else {
                println!("{token_count}\t{line}");
            }
        }
    }

    Ok(())
}

fn fuzzy_query(query: &FuzzyQuery, limit: usize) -> Result<()> {
    let tokenizer = o200k_base_singleton();
    let mut matches = Vec::new();

    for rank in 0..O200K_BASE_ORDINARY_TOKEN_COUNT {
        let bytes = ordinary_token_bytes(tokenizer, rank)?;
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        if let Some(score) = fuzzy_score(&text, query) {
            matches.push((score, rank, text));
        }
    }

    matches.sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    for (_, rank, text) in matches.into_iter().take(limit) {
        println!("{rank}\t{text:?}");
    }

    Ok(())
}

fn ordinary_token_bytes(tokenizer: &CoreBPE, rank: Rank) -> Result<Vec<u8>> {
    tokenizer
        .decode_bytes(&[rank])
        .with_context(|| format!("ordinary o200k_base token rank {rank} is missing"))
}

fn fuzzy_score(candidate: &str, query: &FuzzyQuery) -> Option<FuzzyScore> {
    let mut best = fuzzy_score_view(candidate, query, false);

    if let Some(without_space) = candidate.strip_prefix(' ') {
        best = minimum_score(best, fuzzy_score_view(without_space, query, true));
    }

    best
}

fn minimum_score(left: Option<FuzzyScore>, right: Option<FuzzyScore>) -> Option<FuzzyScore> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(score), None) | (None, Some(score)) => Some(score),
        (None, None) => None,
    }
}

fn fuzzy_score_view(
    candidate: &str,
    query: &FuzzyQuery,
    omitted_leading_space: bool,
) -> Option<FuzzyScore> {
    let chars: Vec<char> = candidate.chars().collect();
    let lowercase_chars: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();

    if chars == query.chars {
        return Some(score(0, omitted_leading_space, 0, 0, 0));
    }
    if lowercase_chars == query.lowercase_chars {
        return Some(score(1, omitted_leading_space, 0, 0, 0));
    }
    if chars.starts_with(&query.chars) {
        return Some(score(
            2,
            omitted_leading_space,
            0,
            0,
            chars.len() - query.chars.len(),
        ));
    }
    if lowercase_chars.starts_with(&query.lowercase_chars) {
        return Some(score(
            3,
            omitted_leading_space,
            0,
            0,
            lowercase_chars.len() - query.lowercase_chars.len(),
        ));
    }
    if let Some(start) = contiguous_start(&chars, &query.chars) {
        return Some(score(
            4,
            omitted_leading_space,
            0,
            start,
            chars.len() - query.chars.len(),
        ));
    }
    if let Some(start) = contiguous_start(&lowercase_chars, &query.lowercase_chars) {
        return Some(score(
            5,
            omitted_leading_space,
            0,
            start,
            lowercase_chars.len() - query.lowercase_chars.len(),
        ));
    }
    if let Some((internal_gaps, start)) = subsequence_metrics(&chars, &query.chars) {
        return Some(score(
            6,
            omitted_leading_space,
            internal_gaps,
            start,
            chars.len() - query.chars.len(),
        ));
    }
    if let Some((internal_gaps, start)) =
        subsequence_metrics(&lowercase_chars, &query.lowercase_chars)
    {
        return Some(score(
            7,
            omitted_leading_space,
            internal_gaps,
            start,
            lowercase_chars.len() - query.lowercase_chars.len(),
        ));
    }

    None
}

fn score(
    quality: u8,
    omitted_leading_space: bool,
    internal_gaps: usize,
    start: usize,
    extra_chars: usize,
) -> FuzzyScore {
    FuzzyScore {
        quality,
        omitted_leading_space,
        internal_gaps,
        start,
        extra_chars,
    }
}

fn contiguous_start(candidate: &[char], query: &[char]) -> Option<usize> {
    candidate
        .windows(query.len())
        .position(|window| window == query)
}

fn subsequence_metrics(candidate: &[char], query: &[char]) -> Option<(usize, usize)> {
    if query.len() == 1 {
        return candidate
            .iter()
            .position(|candidate_char| candidate_char == &query[0])
            .map(|start| (0, start));
    }

    candidate
        .iter()
        .enumerate()
        .filter(|(_, candidate_char)| candidate_char == &&query[0])
        .filter_map(|(start, _)| {
            let mut query_index = 1;

            for (candidate_index, candidate_char) in candidate.iter().enumerate().skip(start + 1) {
                if *candidate_char == query[query_index] {
                    query_index += 1;
                    if query_index == query.len() {
                        let internal_gaps = candidate_index - start + 1 - query.len();
                        return Some((internal_gaps, start));
                    }
                }
            }

            None
        })
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str) -> FuzzyQuery {
        FuzzyQuery::new(text.to_owned()).unwrap()
    }
    fn matched_score(candidate: &str, query: &FuzzyQuery) -> FuzzyScore {
        fuzzy_score(candidate, query).expect("candidate should match query")
    }

    #[test]
    fn fuzzy_ranking_prioritizes_complete_token_matches() {
        let query = query("directory");

        assert!(matched_score("directory", &query) < matched_score(" directory", &query));
        assert!(matched_score(" directory", &query) < matched_score("Directory", &query));
        assert!(matched_score("Directory", &query) < matched_score("directoryPath", &query));
        assert!(matched_score("directoryPath", &query) < matched_score("subdirectory", &query));
    }

    #[test]
    fn fuzzy_ranking_prefers_compact_ordered_matches() {
        let query = query("abc");

        assert!(matched_score("xxa-b-c", &query) < matched_score("a----b----c", &query));
        assert_eq!(fuzzy_score("acb", &query), None);
    }

    #[test]
    fn explicit_leading_space_remains_part_of_the_query() {
        let query = query(" directory");

        assert!(fuzzy_score(" directory", &query).is_some());
        assert_eq!(fuzzy_score("directory", &query), None);
    }
}
