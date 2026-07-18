use criterion::{Criterion, black_box, criterion_group, criterion_main};
use harness_tui::transcript::TranscriptLineIndex;

fn build_transcript_line_index(c: &mut Criterion) {
    c.bench_function("transcript_line_index_build_512_entries", |b| {
        let entries = (0..512)
            .map(|index| match index % 7 {
                0 => String::new(),
                1 => format!("assistant: entry {index}"),
                2 => format!("assistant: entry {index}\nsecond"),
                3 => format!("tool output: call-{index}\none\ntwo\nthree\nfour\nfive\nsix\nseven"),
                _ => format!("assistant: entry {index}\nsecond\nthird\nfourth"),
            })
            .collect::<Vec<_>>();
        b.iter(|| {
            let index = TranscriptLineIndex::build(black_box(&entries), |entry| {
                entry.lines().count().max(1)
            });
            black_box(index.total_lines())
        })
    });
}

fn resolve_visual_lines(c: &mut Criterion) {
    c.bench_function("transcript_line_index_resolve_visible_lines", |b| {
        let entries = (0..512)
            .map(|entry| {
                if entry % 11 == 0 {
                    String::new()
                } else {
                    (0..=(entry % 5))
                        .map(|line| format!("entry {entry} line {line}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            })
            .collect::<Vec<_>>();
        let index = TranscriptLineIndex::build(&entries, |entry| entry.lines().count().max(1));
        let total_lines = index.total_lines().max(1);
        b.iter(|| {
            for offset in 0..80 {
                black_box(index.line_address(black_box((total_lines - 1).saturating_sub(offset))));
            }
        })
    });
}

fn visible_ranges(c: &mut Criterion) {
    c.bench_function("transcript_line_index_visible_ranges", |b| {
        let entries = (0..512)
            .map(|entry| format!("assistant: entry {entry}\nsecond\nthird"))
            .collect::<Vec<_>>();
        let index = TranscriptLineIndex::build(&entries, |entry| entry.lines().count().max(1));
        b.iter(|| {
            black_box(index.visible_ranges(black_box(1200), black_box(48)));
        })
    });
}

criterion_group!(
    benches,
    build_transcript_line_index,
    resolve_visual_lines,
    visible_ranges
);
criterion_main!(benches);
