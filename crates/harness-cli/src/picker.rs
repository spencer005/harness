use std::{fs, io, path::Path, time::SystemTime};
use harness_tui_rewrite::picker::SessionMeta;
use crate::CliError;

pub fn list_sessions(root: &Path) -> Result<Vec<SessionMeta>, CliError> {
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
        let Some(_raw_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let mut all_text = String::new();
        let mut model = String::new();
        let mut title = String::new();
        let mut initial_entries = Vec::new();
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(records) = serde_json::from_str::<Vec<crate::SerializableRecord>>(&content) {
                for record in &records {
                    if let Some(entry) = crate::transcript_snapshot_entry(&crate::from_serializable_record(record.clone(), &harness_session_store::SessionId::new("preview").unwrap())) {
                        initial_entries.push(entry);
                    }
                    match &record.payload {
                        crate::SerializablePayload::InputMessage { text, .. } => {
                            all_text.push_str(text);
                            all_text.push(' ');
                        }
                        crate::SerializablePayload::AssistantMessage { text, .. } => {
                            all_text.push_str(text);
                            all_text.push(' ');
                        }
                        crate::SerializablePayload::ProviderBinding { model: m, .. } if model.is_empty() => {
                            model = m.clone();
                        }
                        crate::SerializablePayload::Metadata { title: t } if title.is_empty() => {
                            title = t.clone();
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
            initial_entries,
        });
    }

    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    Ok(sessions)
}

