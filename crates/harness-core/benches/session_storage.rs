use std::{
    env, fs,
    hint::black_box,
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use criterion::{Criterion, criterion_group, criterion_main};
use harness_core::sessions::{
    CompactionCheckpointRecord, HistoryRecord, MessageRecord, SessionId, SessionMeta,
    SessionRecordKind, SessionStore,
};

const MESSAGE_PAIRS: usize = 512;
const CHECKPOINT_PREFIX_PAIRS: usize = 384;
const CHECKPOINT_BETWEEN_PAIRS: usize = 128;
const CHECKPOINT_AFTER_PAIRS: usize = 64;
const CHECKPOINT_HISTORY_ITEMS: usize = 128;
const BASE_CREATED_AT_MS: u64 = 1_717_171_717_000;

static TEMP_ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

struct SessionFixture {
    root: PathBuf,
    store: SessionStore,
    path: PathBuf,
}

impl SessionFixture {
    fn many_messages(label: &str) -> Self {
        let ordinal = next_ordinal();
        let root = temp_root(label, ordinal);
        cleanup_root(&root);

        let store = SessionStore::new(&root);
        let mut writer = store.create_session(session_meta(label, ordinal)).unwrap();
        writer.append_many(message_records(MESSAGE_PAIRS));
        writer.flush().unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);

        Self { root, store, path }
    }

    fn with_latest_checkpoint(label: &str) -> Self {
        let ordinal = next_ordinal();
        let root = temp_root(label, ordinal);
        cleanup_root(&root);

        let store = SessionStore::new(&root);
        let mut writer = store.create_session(session_meta(label, ordinal)).unwrap();
        writer.append_many(compaction_records());
        writer.flush().unwrap();
        let path = writer.path().to_path_buf();
        drop(writer);

        Self { root, store, path }
    }
}

impl Drop for SessionFixture {
    fn drop(&mut self) {
        cleanup_root(&self.root);
    }
}

fn session_storage(c: &mut Criterion) {
    let mut group = c.benchmark_group("session_storage");

    group.bench_function("create_write_flush_many_messages", |b| {
        b.iter_custom(|iterations| {
            let mut elapsed = Duration::ZERO;

            for _ in 0..iterations {
                let ordinal = next_ordinal();
                let root = temp_root("create-write-flush", ordinal);
                cleanup_root(&root);
                let records = message_records(MESSAGE_PAIRS);

                let start = Instant::now();
                let store = SessionStore::new(&root);
                let mut writer = store
                    .create_session(session_meta("create-write-flush", ordinal))
                    .unwrap();
                let seqs = writer.append_many(records);
                writer.flush().unwrap();
                black_box(seqs.len());
                elapsed += start.elapsed();

                drop(writer);
                cleanup_root(&root);
            }

            elapsed
        });
    });

    let read_fixture = SessionFixture::many_messages("read-all-records");
    let records = read_fixture.store.read_records(&read_fixture.path).unwrap();
    assert_eq!(records.len(), MESSAGE_PAIRS * 2 + 1);

    group.bench_function("read_all_records", |b| {
        b.iter(|| {
            let records = read_fixture
                .store
                .read_records(black_box(&read_fixture.path))
                .unwrap();
            black_box(records.len())
        });
    });
    drop(read_fixture);

    let checkpoint_fixture = SessionFixture::with_latest_checkpoint("latest-checkpoint");
    let latest = checkpoint_fixture
        .store
        .load_latest_history(&checkpoint_fixture.path)
        .unwrap();
    assert_eq!(latest.checkpoint_history.len(), CHECKPOINT_HISTORY_ITEMS);
    assert_eq!(
        latest.records_after_checkpoint.len(),
        CHECKPOINT_AFTER_PAIRS * 2
    );

    group.bench_function("load_latest_compaction_checkpoint", |b| {
        b.iter(|| {
            let latest = checkpoint_fixture
                .store
                .load_latest_history(black_box(&checkpoint_fixture.path))
                .unwrap();
            black_box((
                latest.checkpoint_history.len(),
                latest.records_after_checkpoint.len(),
            ))
        });
    });

    group.finish();
}

fn next_ordinal() -> u64 {
    TEMP_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn temp_root(label: &str, ordinal: u64) -> PathBuf {
    env::temp_dir().join(format!(
        "harness-core-session-storage-bench-{label}-{}-{ordinal}",
        std::process::id()
    ))
}

fn cleanup_root(root: &Path) {
    match fs::remove_dir_all(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => panic!(
            "failed to remove benchmark temp root {}: {error}",
            root.display()
        ),
    }
}

fn session_meta(label: &str, ordinal: u64) -> SessionMeta {
    SessionMeta {
        id: SessionId::new(format!("{label}-{}-{ordinal}", std::process::id())),
        parent_session_id: None,
        forked_from_session_id: None,
        created_at_ms: BASE_CREATED_AT_MS + ordinal,
        updated_at_ms: BASE_CREATED_AT_MS + ordinal,
        cwd: "/tmp/session-storage-bench".to_string(),
        model: "gpt-bench".to_string(),
        originator: "criterion".to_string(),
        client_version: "0.0.0-bench".to_string(),
        source: "bench".to_string(),
        title: Some(format!("{label} {ordinal}")),
        preview: Some("deterministic session storage benchmark".to_string()),
    }
}

fn message_records(pairs: usize) -> Vec<SessionRecordKind> {
    (0..pairs)
        .flat_map(|index| {
            [
                SessionRecordKind::UserMessage(MessageRecord {
                    text: message_text("user", index),
                }),
                SessionRecordKind::AssistantMessage(MessageRecord {
                    text: message_text("assistant", index),
                }),
            ]
        })
        .collect()
}

fn compaction_records() -> Vec<SessionRecordKind> {
    let mut records = Vec::with_capacity(
        (CHECKPOINT_PREFIX_PAIRS + CHECKPOINT_BETWEEN_PAIRS + CHECKPOINT_AFTER_PAIRS) * 2 + 2,
    );
    records.extend(message_records(CHECKPOINT_PREFIX_PAIRS));
    records.push(SessionRecordKind::CompactionCheckpoint(
        CompactionCheckpointRecord {
            replacement_history: checkpoint_history("initial"),
        },
    ));
    records.extend(message_records(CHECKPOINT_BETWEEN_PAIRS));
    records.push(SessionRecordKind::CompactionCheckpoint(
        CompactionCheckpointRecord {
            replacement_history: checkpoint_history("latest"),
        },
    ));
    records.extend(message_records(CHECKPOINT_AFTER_PAIRS));
    records
}

fn checkpoint_history(label: &str) -> Vec<HistoryRecord> {
    (0..CHECKPOINT_HISTORY_ITEMS)
        .map(|index| {
            if index % 2 == 0 {
                HistoryRecord::UserMessage(MessageRecord {
                    text: format!("{label} checkpoint user history item {index:04}"),
                })
            } else {
                HistoryRecord::AssistantMessage(MessageRecord {
                    text: format!("{label} checkpoint assistant history item {index:04}"),
                })
            }
        })
        .collect()
}

fn message_text(role: &str, index: usize) -> String {
    format!(
        "{role} message {index:04}: deterministic payload for postcard and zstd session storage. \
         alpha beta gamma delta epsilon alpha beta gamma delta epsilon."
    )
}

criterion_group!(benches, session_storage);
criterion_main!(benches);
