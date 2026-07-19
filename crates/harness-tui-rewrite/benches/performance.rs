#![allow(dead_code)]
#![allow(
    unused_imports,
    reason = "private source modules include cfg(test) imports when Cargo checks a benchmark target"
)]

use std::{cmp::Ordering, time::Instant};

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use ratatui::{Terminal, backend::TestBackend, layout::Rect};

#[path = "../src/app.rs"]
mod app;
#[path = "../src/control.rs"]
mod control;
#[path = "../src/display/mod.rs"]
mod display;
#[path = "../src/domain.rs"]
mod domain;
#[path = "../src/input/mod.rs"]
mod input;
#[path = "../src/transcript/mod.rs"]
mod transcript;
#[path = "../src/view.rs"]
mod view;

use app::Application;
use domain::{
    AgentId, AgentState, AgentStatus, ExternalText, InitialState, MessageRole, ModelState,
    PersistedTranscriptEntry, ProviderKind, ProviderState, ProviderTransport, TranscriptPayload,
    TranscriptSnapshotEntry,
};
use input::{BoundedInput, HorizontalUnit, InputFragment, PromptEditor, RawInput};
use transcript::{Transcript, TranscriptScrollDirection};

const PROMPT_BYTES: usize = 1024 * 1024;
const TRANSCRIPT_ENTRIES: usize = 1_000;
const TRANSCRIPT_WIDTH: u16 = 100;
const TRANSCRIPT_HEIGHT: usize = 40;

fn prompt_benchmarks(criterion: &mut Criterion) {
    let source = multiline_prompt(PROMPT_BYTES);

    criterion.bench_function("prompt/layout_initial/1_mib_multiline", |bencher| {
        bencher.iter_batched(
            || PromptEditor::import(source.clone(), source.len()).unwrap(),
            |mut editor| black_box(editor.layout_metrics(80)),
            BatchSize::LargeInput,
        );
    });

    let mut cached = PromptEditor::import(source.clone(), source.len()).unwrap();
    let metrics = cached.layout_metrics(80);
    let first_visible = metrics.cursor_row.saturating_add(1).saturating_sub(16);
    criterion.bench_function("prompt/layout_cached_metrics/1_mib_multiline", |bencher| {
        bencher.iter(|| black_box(cached.layout_metrics(black_box(80))));
    });
    criterion.bench_function("prompt/visible_viewport/1_mib_multiline", |bencher| {
        bencher.iter(|| {
            black_box(cached.viewport(black_box(80), black_box(first_visible), black_box(16)))
        });
    });

    let midpoint = source[..source.len() / 2].rfind('\n').unwrap() + 1;
    criterion.bench_function("prompt/edit_middle/1_mib", |bencher| {
        bencher.iter_batched(
            || {
                (
                    PromptEditor::import(source.clone(), midpoint).unwrap(),
                    bounded("inserted"),
                )
            },
            |(mut editor, fragment)| {
                editor.insert(fragment).unwrap();
                black_box(editor);
            },
            BatchSize::LargeInput,
        );
    });

    let unicode = "a好e\u{301}👨‍👩‍👧‍👦 ".repeat(20_000);
    criterion.bench_function("prompt/unicode_cursor/128_graphemes", |bencher| {
        bencher.iter_batched(
            || PromptEditor::import(unicode.clone(), unicode.len()).unwrap(),
            |mut editor| {
                for _ in 0..128 {
                    editor.move_horizontal(Ordering::Less, HorizontalUnit::Grapheme, false);
                }
                black_box(editor);
            },
            BatchSize::LargeInput,
        );
    });

    let replacement = "replacement ".repeat(8 * 1024);
    criterion.bench_function("prompt/replace_full_selection/1_mib", |bencher| {
        bencher.iter_batched(
            || {
                (
                    fully_selected_prompt(&source, 80),
                    InputFragment::<RawInput>::new(replacement.clone())
                        .bound()
                        .unwrap(),
                )
            },
            |(mut editor, fragment)| {
                editor.insert(fragment).unwrap();
                black_box(editor);
            },
            BatchSize::LargeInput,
        );
    });
}

fn transcript_benchmarks(criterion: &mut Criterion) {
    let entries = transcript_entries(TRANSCRIPT_ENTRIES, 1_000);

    criterion.bench_function("transcript/viewport_initial/1000_entries", |bencher| {
        bencher.iter_batched(
            || Transcript::import(entries.clone(), false).unwrap(),
            |mut transcript| {
                black_box(transcript.viewport(TRANSCRIPT_WIDTH, TRANSCRIPT_HEIGHT));
            },
            BatchSize::LargeInput,
        );
    });

    let mut cached = Transcript::import(entries.clone(), false).unwrap();
    black_box(cached.viewport(TRANSCRIPT_WIDTH, TRANSCRIPT_HEIGHT));
    criterion.bench_function("transcript/viewport_cached/1000_entries", |bencher| {
        bencher.iter(|| {
            black_box(cached.viewport(black_box(TRANSCRIPT_WIDTH), black_box(TRANSCRIPT_HEIGHT)))
        });
    });

    let mut reflowing = Transcript::import(entries.clone(), false).unwrap();
    black_box(reflowing.viewport(80, TRANSCRIPT_HEIGHT));
    let mut narrow = false;
    criterion.bench_function("transcript/width_reflow/1000_entries", |bencher| {
        bencher.iter(|| {
            narrow = !narrow;
            let width = if narrow { 80 } else { 120 };
            black_box(reflowing.viewport(black_box(width), TRANSCRIPT_HEIGHT));
        });
    });

    criterion.bench_function("transcript/assistant_delta_batch/64_events", |bencher| {
        bencher.iter_batched(
            || {
                let mut transcript =
                    Transcript::import(transcript_entries(256, 1_000), false).unwrap();
                transcript.begin_response_stream().unwrap();
                transcript
            },
            |mut transcript| {
                transcript
                    .append_assistant_delta(ExternalText::new("streaming delta ".repeat(64)))
                    .unwrap();
                black_box(transcript.viewport(TRANSCRIPT_WIDTH, TRANSCRIPT_HEIGHT));
            },
            BatchSize::LargeInput,
        );
    });

    criterion.bench_function("transcript/page_prepend/96_entries", |bencher| {
        bencher.iter_batched(
            transcript_ready_for_page,
            |(mut transcript, page)| {
                transcript.apply_page(page, Some(904), false).unwrap();
                black_box(transcript.viewport(TRANSCRIPT_WIDTH, TRANSCRIPT_HEIGHT));
            },
            BatchSize::LargeInput,
        );
    });

    criterion.bench_function("transcript/selection_extract/256_entries", |bencher| {
        bencher.iter_batched(
            selected_transcript,
            |mut transcript| black_box(transcript.selected_text()),
            BatchSize::LargeInput,
        );
    });

    criterion.bench_function(
        "transcript/selection_edge_scroll_with_stream_delta",
        |bencher| {
            bencher.iter_batched(
                transcript_with_streaming_selection,
                |mut transcript| {
                    transcript
                        .append_assistant_delta(ExternalText::new("next delta "))
                        .unwrap();
                    transcript.scroll_selection(
                        TRANSCRIPT_WIDTH,
                        TRANSCRIPT_HEIGHT,
                        TranscriptScrollDirection::Older,
                        12,
                    );
                    black_box(transcript.viewport(TRANSCRIPT_WIDTH, TRANSCRIPT_HEIGHT));
                },
                BatchSize::LargeInput,
            );
        },
    );
}

fn frame_benchmarks(criterion: &mut Criterion) {
    let area = Rect::new(0, 0, 120, 48);
    let mut application = application_fixture(512, 128 * 1024);
    black_box(view::prepare(&mut application, area, Instant::now()));
    criterion.bench_function("frame/prepare_cached/512_entries", |bencher| {
        bencher.iter(|| {
            black_box(view::prepare(
                &mut application,
                black_box(area),
                Instant::now(),
            ))
        });
    });

    let mut reflowing = application_fixture(512, 128 * 1024);
    black_box(view::prepare(
        &mut reflowing,
        Rect::new(0, 0, 100, 48),
        Instant::now(),
    ));
    let mut narrow = false;
    criterion.bench_function("frame/prepare_width_change/512_entries", |bencher| {
        bencher.iter(|| {
            narrow = !narrow;
            let width = if narrow { 100 } else { 140 };
            black_box(view::prepare(
                &mut reflowing,
                Rect::new(0, 0, black_box(width), 48),
                Instant::now(),
            ))
        });
    });

    let mut render_application = application_fixture(512, 128 * 1024);
    let prepared = view::prepare(&mut render_application, area, Instant::now());
    let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
    criterion.bench_function("frame/render/512_entries", |bencher| {
        bencher.iter(|| {
            terminal
                .draw(|frame| view::render(frame, black_box(&prepared)))
                .unwrap();
        });
    });
}

fn multiline_prompt(minimum_bytes: usize) -> String {
    let mut prompt = String::with_capacity(minimum_bytes + 32);
    let mut line = 0usize;
    while prompt.len() < minimum_bytes {
        prompt.push_str(&format!("line {line:06} α好 e\u{301}\n"));
        line = line.saturating_add(1);
    }
    prompt
}

fn bounded(text: impl Into<String>) -> InputFragment<BoundedInput> {
    InputFragment::<RawInput>::new(text).bound().unwrap()
}

fn fully_selected_prompt(source: &str, width: u16) -> PromptEditor {
    let mut editor = PromptEditor::import(source.to_string(), 0).unwrap();
    let metrics = editor.layout_metrics(width);
    let start = editor.viewport(width, 0, 1).position_at(0, 0);
    let end = editor
        .viewport(width, metrics.line_count.saturating_sub(1), 1)
        .position_at(0, usize::MAX);
    editor.begin_selection_at(start);
    editor.extend_selection_to(end);
    editor
}

fn transcript_entries(count: usize, first_sequence: u64) -> Vec<TranscriptSnapshotEntry> {
    (0..count)
        .map(|index| TranscriptSnapshotEntry {
            sequence: Some(first_sequence.saturating_add(index as u64)),
            payload: TranscriptPayload::Message {
                role: MessageRole::Assistant,
                text: ExternalText::new(format!(
                    "entry {index:04}: {}",
                    "α好 wrapped transcript content ".repeat(6)
                )),
            },
        })
        .collect()
}

fn transcript_ready_for_page() -> (Transcript, Vec<PersistedTranscriptEntry>) {
    let mut transcript =
        Transcript::import(transcript_entries(TRANSCRIPT_ENTRIES, 1_000), false).unwrap();
    transcript.scroll_to_top(TRANSCRIPT_WIDTH, TRANSCRIPT_HEIGHT);
    assert!(
        transcript
            .request_older_page(TRANSCRIPT_WIDTH, TRANSCRIPT_HEIGHT)
            .is_some()
    );
    let page = (904..1_000)
        .map(|sequence| PersistedTranscriptEntry {
            sequence,
            payload: TranscriptPayload::Message {
                role: MessageRole::Assistant,
                text: ExternalText::new(format!("historical entry {sequence}")),
            },
        })
        .collect();
    (transcript, page)
}

fn selected_transcript() -> Transcript {
    let mut transcript = Transcript::import(transcript_entries(256, 1_000), false).unwrap();
    transcript.scroll_to_top(TRANSCRIPT_WIDTH, TRANSCRIPT_HEIGHT);
    let first = transcript
        .viewport(TRANSCRIPT_WIDTH, TRANSCRIPT_HEIGHT)
        .position_at(0, 0)
        .expect("first transcript row is selectable");
    transcript.scroll_to_bottom();
    let viewport = transcript.viewport(TRANSCRIPT_WIDTH, TRANSCRIPT_HEIGHT);
    let last = (0..viewport.lines.len())
        .rev()
        .find_map(|row| viewport.position_at(row, usize::MAX))
        .expect("tail transcript row is selectable");
    transcript.begin_selection(first);
    transcript.extend_selection(last);
    transcript
}

fn transcript_with_streaming_selection() -> Transcript {
    let mut transcript = Transcript::import(transcript_entries(256, 1_000), false).unwrap();
    transcript.begin_response_stream().unwrap();
    transcript
        .append_assistant_delta(ExternalText::new("stream begins "))
        .unwrap();
    let viewport = transcript.viewport(TRANSCRIPT_WIDTH, TRANSCRIPT_HEIGHT);
    let start = (0..viewport.lines.len())
        .rev()
        .find_map(|row| viewport.position_at(row, 0))
        .expect("stream tail is selectable");
    transcript.begin_selection(start);
    transcript
}

fn application_fixture(entry_count: usize, prompt_bytes: usize) -> Application {
    let prompt = multiline_prompt(prompt_bytes);
    let prompt_cursor = prompt.len();
    Application::import(InitialState {
        session_id: ExternalText::new("benchmark-session"),
        thread_title: ExternalText::new("benchmark thread"),
        provider: Some(ProviderState {
            display_name: ExternalText::new("benchmark provider"),
            kind: ProviderKind::HttpsApi,
            transport: ProviderTransport::Https,
        }),
        model: ModelState {
            model: ExternalText::new("benchmark-model"),
            reasoning_effort: Some(ExternalText::new("high")),
            service_tier: Some(ExternalText::new("default")),
        },
        developer_mode: true,
        response_streaming: false,
        last_ttft_ms: Some(42),
        transcript: transcript_entries(entry_count, 1_000),
        prompt,
        prompt_cursor,
        queued_steering: Some(ExternalText::new("queued steering")),
        agents: vec![AgentState {
            id: AgentId(1),
            path: ExternalText::new("benchmark-agent"),
            status: AgentStatus::Running,
            last_task_message: Some(ExternalText::new("task")),
            last_activity_message: Some(ExternalText::new("working")),
        }],
        active_activity_ids: vec![ExternalText::new("benchmark-activity")],
    })
    .unwrap()
}

criterion_group! {
    name = performance;
    config = Criterion::default().sample_size(20);
    targets = prompt_benchmarks, transcript_benchmarks, frame_benchmarks
}
criterion_main!(performance);
