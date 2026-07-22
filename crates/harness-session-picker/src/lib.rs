use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, StatefulWidget, Widget},
    buffer::Buffer,
    layout::Rect,
};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use std::time::SystemTime;
use chrono::{DateTime, Local};

#[derive(Clone)]
pub struct SessionMeta {
    pub id: String,
    pub modified: SystemTime,
    pub all_text: String,
    pub model: String,
    pub title: String,
}

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
        let matcher = SkimMatcherV2::default();
        let mut filtered: Vec<(&SessionMeta, i64)> = self.sessions.iter().map(|s| {
            if self.query.is_empty() {
                (s, 0)
            } else {
                let text_to_match = format!("{} {} {} {}", s.title, s.id, s.model, s.all_text);
                let score = matcher.fuzzy_match(&text_to_match, &self.query).unwrap_or(0);
                (s, score)
            }
        }).filter(|(_, score)| self.query.is_empty() || *score > 0).collect();

        if !self.query.is_empty() {
            filtered.sort_by(|a, b| b.1.cmp(&a.1));
        }
        filtered
    }
}

pub struct SessionPickerWidget<'a> {
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> SessionPickerWidget<'a> {
    pub fn new() -> Self {
        Self { _marker: std::marker::PhantomData }
    }
}

impl<'a> StatefulWidget for SessionPickerWidget<'a> {
    type State = SessionPickerState;

    fn render(self, area: Rect, buf: &mut Buffer, state: &mut Self::State) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Length(2), Constraint::Min(0)].as_ref())
            .split(area);

        let prompt_text = if state.exit_armed {
            format!("> {} (Press Ctrl-C again to exit)", state.query)
        } else {
            format!("> {}", state.query)
        };

        let search_block = Paragraph::new(prompt_text)
            .block(Block::default().title(Span::styled(" RESUME SESSION ", Style::default().add_modifier(Modifier::BOLD))).borders(Borders::NONE));
        Widget::render(search_block, chunks[0], buf);

        let filtered = state.filtered_sessions();
        let items: Vec<ListItem> = filtered
            .iter()
            .map(|(s, _)| {
                let dt: DateTime<Local> = s.modified.into();
                let date_str = dt.format("%Y-%m-%d %H:%M").to_string();
                let display_title = if s.title.is_empty() { &s.id } else { &s.title };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:16} ", date_str), Style::default().fg(Color::DarkGray)),
                    Span::raw(display_title.to_string()),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::NONE))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED).add_modifier(Modifier::BOLD))
            .highlight_symbol(" ");
        StatefulWidget::render(list, chunks[1], buf, &mut state.list_state);
    }
}
