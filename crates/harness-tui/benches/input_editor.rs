use criterion::{Criterion, black_box, criterion_group, criterion_main};
use harness_core::UiSnapshot;
use harness_tui::input::{InputEditor, VerticalDirection};

fn insert_and_submit(c: &mut Criterion) {
    c.bench_function("input_editor_insert_and_submit", |b| {
        b.iter(|| {
            let mut snapshot = UiSnapshot::default();
            let mut editor = InputEditor::default();
            for ch in black_box("implement cached transcript viewport rendering\n").chars() {
                editor.insert_char(&mut snapshot, ch);
            }
            black_box(editor.submit(&mut snapshot))
        })
    });
}

fn multiline_cursor_motion(c: &mut Criterion) {
    c.bench_function("input_editor_multiline_cursor_motion", |b| {
        b.iter(|| {
            let mut snapshot = UiSnapshot {
                input: black_box(
                    "alpha beta gamma\nshort\nunicode λambda value\nlast line of input",
                )
                .to_string(),
                input_cursor: "alpha beta gamma\nshort\nunicode λambda".len(),
                ..UiSnapshot::default()
            };
            let mut editor = InputEditor::default();
            for _ in 0..64 {
                editor.move_vertically(&mut snapshot, VerticalDirection::Up);
                editor.move_vertically(&mut snapshot, VerticalDirection::Down);
            }
            black_box(snapshot.input_cursor)
        })
    });
}

criterion_group!(benches, insert_and_submit, multiline_cursor_motion);
criterion_main!(benches);
