use std::{fs, io, path::Path, time::SystemTime};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    Terminal,
};
use harness_session_picker::{SessionMeta, SessionPickerState, SessionPickerWidget};
use crate::CliError;
use harness_tui_rewrite::render_preview;
use harness_tui_rewrite::domain::InitialState;
use harness_tui_rewrite::domain::ExternalText;

pub fn pick_session_tui(root: &Path) -> Result<harness_session_store::SessionId, CliError> {
    let directory = root.join("sessions");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CliError::NoSessionsAvailable);
        }
        Err(error) => return Err(CliError::Io { source: error }),
    };

    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| CliError::Io { source })?;
        if !entry.file_type().map_err(|source| CliError::Io { source })?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(raw_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let mut all_text = String::new();
        let mut model = String::new();
        let mut title = String::new();
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(records) = serde_json::from_str::<Vec<crate::SerializableRecord>>(&content) {
                for record in records {
                    match record.payload {
                        crate::SerializablePayload::InputMessage { text, .. } => {
                            all_text.push_str(&text);
                            all_text.push(' ');
                        }
                        crate::SerializablePayload::AssistantMessage { text, .. } => {
                            all_text.push_str(&text);
                            all_text.push(' ');
                        }
                        crate::SerializablePayload::ProviderBinding { model: m, .. } if model.is_empty() => {
                            model = m;
                        }
                        crate::SerializablePayload::Metadata { title: t } if title.is_empty() => {
                            title = t;
                        }
                        _ => {}
                    }
                }
            }
        }

        sessions.push(SessionMeta {
            id: entry.file_name().to_string_lossy().replace(".json", ""),
            modified,
            all_text,
            model,
            title,
        });
    }

    if sessions.is_empty() {
        return Err(CliError::NoSessionsAvailable);
    }
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));

    enable_raw_mode().map_err(|source| CliError::Io { source })?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|source| CliError::Io { source })?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|source| CliError::Io { source })?;

    let res = run_picker_loop(&mut terminal, sessions, root);

    disable_raw_mode().map_err(|source| CliError::Io { source })?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(|source| CliError::Io { source })?;
    terminal.show_cursor().map_err(|source| CliError::Io { source })?;

    let selected_session_id = res?;
    harness_session_store::SessionId::new(selected_session_id.clone())
        .map_err(|_| CliError::SessionNotFound { id: selected_session_id })
}

fn build_initial_state_for_preview(session_id: &str, root: &Path) -> Option<InitialState> {
    let sid = harness_session_store::SessionId::new(session_id).ok()?;
    let records = crate::read_session_records(root, &sid).ok()?;
    let startup = crate::SessionStartup {
        session_id: sid,
        initial_transcript_entries: records
            .iter()
            .filter_map(crate::transcript_snapshot_entry)
            .collect(),
    };
    
    let transcript = startup
        .initial_transcript_entries
        .into_iter()
        .map(harness_tui_rewrite::runtime::adapter::convert_snapshot_entry)
        .collect();
    
    Some(InitialState {
        session_id: ExternalText::new(session_id.to_string()),
        thread_title: ExternalText::new(format!("Preview")),
        provider: None,
        model: harness_tui_rewrite::domain::ModelState {
            model: ExternalText::new("preview".to_string()),
            reasoning_effort: None,
            service_tier: None,
        },
        developer_mode: false,
        response_streaming: false,
        last_ttft_ms: None,
        transcript,
        prompt: String::new(),
        prompt_cursor: 0,
        queued_steering: None,
        agents: Vec::new(),
        active_activity_ids: Vec::new(),
    })
}

fn run_picker_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    sessions: Vec<SessionMeta>,
    root: &Path,
) -> Result<String, CliError> {
    let mut state = SessionPickerState::new(sessions);

    loop {
        let selected_id = {
            let filtered = state.filtered_sessions();
            state.list_state.selected().and_then(|i| {
                if i < filtered.len() {
                    Some(filtered[i].0.id.clone())
                } else {
                    None
                }
            })
        };

        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(30), Constraint::Percentage(70)].as_ref())
                .split(f.area());

            f.render_stateful_widget(SessionPickerWidget::new(), chunks[0], &mut state);

            if let Some(ref id) = selected_id {
                if let Some(initial) = build_initial_state_for_preview(id, root) {
                    render_preview(initial, f, chunks[1]);
                }
            }
        }).map_err(|source| CliError::Io { source })?;

        if let Event::Key(key) = event::read().map_err(|source| CliError::Io { source })? {
            if key.kind == KeyEventKind::Press {
                let is_ctrl = key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL);
                if is_ctrl && key.code == KeyCode::Char('c') {
                    if !state.query.is_empty() {
                        state.query.clear();
                        state.exit_armed = false;
                    } else if state.exit_armed {
                        return Err(CliError::NoSessionSelected);
                    } else {
                        state.exit_armed = true;
                    }
                    continue;
                } else {
                    state.exit_armed = false;
                }

                let filtered_len = state.filtered_sessions().len();
                match key.code {
                    KeyCode::Esc => {
                        return Err(CliError::NoSessionSelected);
                    }
                    KeyCode::Enter => {
                        if let Some(id) = selected_id {
                            return Ok(id);
                        }
                    }
                    KeyCode::Down => {
                        let i = match state.list_state.selected() {
                            Some(i) => {
                                if filtered_len > 0 && i >= filtered_len - 1 { 0 } else { i + 1 }
                            }
                            None => 0,
                        };
                        state.list_state.select(Some(i));
                    }
                    KeyCode::Up => {
                        let i = match state.list_state.selected() {
                            Some(i) => {
                                if i == 0 { if filtered_len == 0 { 0 } else { filtered_len - 1 } } else { i - 1 }
                            }
                            None => 0,
                        };
                        state.list_state.select(Some(i));
                    }
                    KeyCode::Backspace => {
                        state.query.pop();
                    }
                    KeyCode::Char(c) => {
                        state.query.push(c);
                    }
                    _ => {}
                }
            }
        }
    }
}
