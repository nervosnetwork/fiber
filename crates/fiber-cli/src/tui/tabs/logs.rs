//! Logs/Events tab: displays a scrollable event log.
//!
//! Since there is no RPC for streaming events, this tab captures
//! TUI-level events (actions taken, RPC errors, status messages)
//! as a simple session log.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::ListState;

/// A single log entry.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: LogLevel,
    pub message: String,
}

/// Log severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// Logs tab state.
pub struct LogsTab {
    pub entries: Vec<LogEntry>,
    pub list_state: ListState,
    pub auto_scroll: bool,
}

impl LogsTab {
    pub fn new() -> Self {
        let mut tab = Self {
            entries: Vec::new(),
            list_state: ListState::default(),
            auto_scroll: true,
        };
        tab.add_info("TUI session started");
        tab
    }

    pub fn add_entry(&mut self, level: LogLevel, message: &str) {
        let now = chrono::Local::now();
        self.entries.push(LogEntry {
            timestamp: now.format("%H:%M:%S").to_string(),
            level,
            message: message.to_string(),
        });
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    pub fn add_info(&mut self, message: &str) {
        self.add_entry(LogLevel::Info, message);
    }

    pub fn add_warn(&mut self, message: &str) {
        self.add_entry(LogLevel::Warn, message);
    }

    pub fn add_error(&mut self, message: &str) {
        self.add_entry(LogLevel::Error, message);
    }

    fn scroll_to_bottom(&mut self) {
        if !self.entries.is_empty() {
            self.list_state.select(Some(self.entries.len() - 1));
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.list_state.select_previous();
                self.auto_scroll = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.entries.is_empty() {
                    self.list_state.select_next();
                    if let Some(sel) = self.list_state.selected() {
                        if sel >= self.entries.len() {
                            self.list_state.select(Some(self.entries.len() - 1));
                        }
                    }
                }
                // Re-enable auto-scroll when at the bottom
                if let Some(sel) = self.list_state.selected() {
                    if sel >= self.entries.len().saturating_sub(1) {
                        self.auto_scroll = true;
                    }
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.list_state.select_first();
                self.auto_scroll = false;
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll_to_bottom();
                self.auto_scroll = true;
            }
            _ => {}
        }
    }
}
