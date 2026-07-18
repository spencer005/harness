use criterion::{Criterion, black_box, criterion_group, criterion_main};
use harness_core::UiSnapshot;
use harness_tui::{TuiApp, render_app};
use ratatui::{Terminal, backend::TestBackend, layout::Rect};

const ENTRY_COUNT: usize = 512;
const VIEWPORT_HEIGHT: usize = 48;
const VIEWPORT_WIDTH: u16 = 120;

fn transcript_view_stateful_viewport(c: &mut Criterion) {
    let snapshot = UiSnapshot {
        transcript_lines: transcript_entries(),
        ..Default::default()
    };
    let mut app = TuiApp::new(snapshot);
    black_box(app.transcript_viewport_line_count(VIEWPORT_HEIGHT, VIEWPORT_WIDTH));

    c.bench_function("transcript_view_stateful_viewport_512_entries", |b| {
        b.iter(|| {
            black_box(app.transcript_viewport_line_count(
                black_box(VIEWPORT_HEIGHT),
                black_box(VIEWPORT_WIDTH),
            ));
        })
    });
}

fn transcript_view_render_app(c: &mut Criterion) {
    let snapshot = UiSnapshot {
        transcript_lines: transcript_entries(),
        thread_title: "new_harness · bench".to_string(),
        session_id: "bench-session".to_string(),
        ..Default::default()
    };
    let mut terminal =
        Terminal::new(TestBackend::new(VIEWPORT_WIDTH, 64)).expect("test backend initializes");
    let mut app = TuiApp::new(snapshot);
    terminal
        .draw(|frame| render_app(frame, &mut app))
        .expect("initial render succeeds");

    c.bench_function("transcript_view_render_app_512_entries", |b| {
        b.iter(|| {
            terminal
                .draw(|frame| render_app(frame, black_box(&mut app)))
                .expect("render succeeds");
        })
    });
}

fn transcript_view_select_visible_text(c: &mut Criterion) {
    let snapshot = UiSnapshot {
        transcript_lines: transcript_entries(),
        ..Default::default()
    };
    let mut app = TuiApp::new(snapshot);
    app.update_transcript_render_metrics(
        VIEWPORT_HEIGHT,
        0,
        Rect {
            x: 0,
            y: 0,
            width: VIEWPORT_WIDTH,
            height: VIEWPORT_HEIGHT as u16,
        },
    );
    black_box(app.transcript_viewport_line_count(VIEWPORT_HEIGHT, VIEWPORT_WIDTH));

    c.bench_function("transcript_view_select_visible_text", |b| {
        b.iter(|| {
            let selected = app
                .select_transcript_text(black_box(0), black_box(0), black_box(47), black_box(24))
                .expect("non-empty selection returns text");
            black_box(selected.len());
        })
    });
}

fn transcript_view_request_page_at_top(c: &mut Criterion) {
    let snapshot = UiSnapshot {
        transcript_lines: transcript_entries(),
        ..Default::default()
    };
    let mut app = TuiApp::new(snapshot);
    app.update_transcript_render_metrics(
        VIEWPORT_HEIGHT,
        usize::MAX / 2,
        Rect {
            x: 0,
            y: 0,
            width: VIEWPORT_WIDTH,
            height: VIEWPORT_HEIGHT as u16,
        },
    );

    c.bench_function("transcript_view_request_page_at_top", |b| {
        b.iter(|| {
            black_box(app.request_older_transcript_page_at_top());
            app.clear_transcript_page_loading();
        })
    });
}

fn transcript_entries() -> Vec<String> {
    (0..ENTRY_COUNT)
        .map(|index| match index % 8 {
            0 => format!("user> request {index}: explain a transcript rendering edge case"),
            1 => format!(
                "assistant: response {index}: this is a moderately long line that wraps across \
                 multiple columns and forces ratatui reflow plus unicode width accounting."
            ),
            2 => format!(
                "tool call: apply_patch call-{index}\n*** Begin Patch\n*** Update File: \
                 crates/harness-tui/src/lib.rs\n@@\n-old line {index}\n+new line {index}\n*** End Patch"
            ),
            3 => format!(
                "tool output: call-{index}\nChunk ID: chunk-{index}\nWall time: 0.123s\nOutput:\n\
                 line one\nline two\nline three\nline four\nline five"
            ),
            4 => format!(
                "assistant: unicode {index}: 好 café naïve emoji 🚀 words words words words words"
            ),
            5 => format!("developer> note {index}: keep render cache entries stable"),
            6 => format!(
                "assistant: ```rust\nfn rendered_{index}() {{\n    println!(\"bench\");\n}}\n```"
            ),
            _ => format!("assistant: short {index}"),
        })
        .collect()
}

criterion_group!(
    benches,
    transcript_view_stateful_viewport,
    transcript_view_render_app,
    transcript_view_select_visible_text,
    transcript_view_request_page_at_top
);
criterion_main!(benches);
