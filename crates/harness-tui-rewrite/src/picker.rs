use ratatui::widgets::ListState;
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub struct SessionMeta {
    pub id: String,
    pub modified: SystemTime,
    pub all_text: String,
    pub model: String,
    pub title: String,
    pub initial_entries: Vec<harness_runtime_api::TranscriptSnapshotEntry>,
}

#[derive(Clone, Debug)]
pub struct SessionPickerState {
    pub sessions: Vec<SessionMeta>,
    pub query: String,
    pub list_state: ListState,
    pub exit_armed: bool,
}

impl SessionPickerState {
    pub fn new(sessions: Vec<SessionMeta>) -> Self {
        let mut state = ListState::default();
        if !sessions.is_empty() {
            state.select(Some(0));
        }
        Self {
            sessions,
            query: String::new(),
            list_state: state,
            exit_armed: false,
        }
    }

    pub fn filtered_sessions(&self) -> Vec<(&SessionMeta, i64)> {
        let query_lower = self.query.to_lowercase();
        let mut filtered: Vec<(&SessionMeta, i64)> = self
            .sessions
            .iter()
            .map(|s| {
                if query_lower.is_empty() {
                    (s, 0)
                } else {
                    let mut score = 0;
                    if s.title.to_lowercase().contains(&query_lower) {
                        score += 10;
                    }
                    if s.id.to_lowercase().contains(&query_lower) {
                        score += 5;
                    }
                    if s.model.to_lowercase().contains(&query_lower) {
                        score += 3;
                    }
                    if s.all_text.to_lowercase().contains(&query_lower) {
                        score += 1;
                    }
                    (s, score)
                }
            })
            .filter(|(_, score)| query_lower.is_empty() || *score > 0)
            .collect();

        if !query_lower.is_empty() {
            filtered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.modified.cmp(&a.0.modified)));
        }
        filtered
    }
}

#[derive(Clone, Debug)]
pub struct RewindOptionMeta {
    pub sequence: u64,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct RewindPickerState {
    pub options: Vec<RewindOptionMeta>,
    pub list_state: ListState,
}

impl RewindPickerState {
    pub fn new(options: Vec<RewindOptionMeta>) -> Self {
        let mut state = ListState::default();
        if !options.is_empty() {
            state.select(Some(options.len() - 1));
        }
        Self {
            options,
            list_state: state,
        }
    }
}
