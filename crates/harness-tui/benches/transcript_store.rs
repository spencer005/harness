use criterion::{Criterion, black_box, criterion_group, criterion_main};
use harness_tui::transcript::TranscriptStore;

fn append_streaming_delta(c: &mut Criterion) {
    c.bench_function("transcript_store_append_streaming_delta", |b| {
        b.iter(|| {
            let mut store = TranscriptStore::from_lines(Vec::new());
            let id = store.push_line("assistant: ".to_string());
            for _ in 0..128 {
                store.append_to_entry(id, black_box("token ")).unwrap();
            }
            black_box(store.total_bytes())
        })
    });
}

fn prepend_and_trim_retention(c: &mut Criterion) {
    c.bench_function("transcript_store_prepend_and_trim_retention", |b| {
        b.iter(|| {
            let current = (0..512)
                .map(|index| format!("assistant: current {index}"))
                .collect::<Vec<_>>();
            let mut store = TranscriptStore::from_lines(current);
            let older = (0..96)
                .map(|index| format!("assistant: older {index}"))
                .collect::<Vec<_>>();
            store.prepend_lines(black_box(older));
            while store.len() > 512 {
                store.drop_newest();
            }
            black_box(store.revision())
        })
    });
}

criterion_group!(benches, append_streaming_delta, prepend_and_trim_retention);
criterion_main!(benches);
