//! Logs/Events tab: displays a scrollable event log.
//!
//! Logs are persisted to a file (`~/.fnn-cli/tui.log`) so that they survive
//! across TUI sessions.  Previous entries are loaded on startup and new ones
//! are appended in real time.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::ListState;

/// Maximum number of historical log lines to retain in the file.
const MAX_LOG_LINES: usize = 2000;

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

impl LogLevel {
    fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        match s {
            "INFO" => Some(LogLevel::Info),
            "WARN" => Some(LogLevel::Warn),
            "ERROR" => Some(LogLevel::Error),
            _ => None,
        }
    }
}

/// Logs tab state.
pub struct LogsTab {
    pub entries: Vec<LogEntry>,
    pub list_state: ListState,
    pub auto_scroll: bool,
    log_file_path: Option<PathBuf>,
}

impl LogsTab {
    pub fn new() -> Self {
        let log_file_path = Self::resolve_log_path();
        let mut tab = Self {
            entries: Vec::new(),
            list_state: ListState::default(),
            auto_scroll: true,
            log_file_path,
        };
        tab.load_history();
        tab.add_info("TUI session started");
        tab
    }

    /// Resolve the log file path: `~/.fnn-cli/tui.log`.
    fn resolve_log_path() -> Option<PathBuf> {
        let home = std::env::var("HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| std::env::var("USERPROFILE").ok().map(PathBuf::from))?;
        let dir = home.join(".fnn-cli");
        if !dir.exists() {
            fs::create_dir_all(&dir).ok()?;
        }
        Some(dir.join("tui.log"))
    }

    /// Load previous log entries from the persistent file.
    fn load_history(&mut self) {
        let path = match self.log_file_path.as_ref() {
            Some(p) if p.exists() => p,
            _ => return,
        };
        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            // Format: "HH:MM:SS LEVEL message"
            if let Some(entry) = Self::parse_log_line(&line) {
                self.entries.push(entry);
            }
        }
        // Trim to MAX_LOG_LINES
        if self.entries.len() > MAX_LOG_LINES {
            self.entries.drain(..self.entries.len() - MAX_LOG_LINES);
        }
        if !self.entries.is_empty() {
            self.list_state.select(Some(self.entries.len() - 1));
        }
    }

    /// Parse a single persisted log line back into a `LogEntry`.
    fn parse_log_line(line: &str) -> Option<LogEntry> {
        // Expected: "HH:MM:SS LEVEL rest of message"
        let timestamp = line.get(..8)?;
        // Validate timestamp format (HH:MM:SS)
        if timestamp.len() != 8
            || timestamp.as_bytes()[2] != b':'
            || timestamp.as_bytes()[5] != b':'
        {
            return None;
        }
        let rest = line.get(9..)?;
        let (level_str, message) = rest.split_once(' ')?;
        let level = LogLevel::from_str(level_str)?;
        Some(LogEntry {
            timestamp: timestamp.to_string(),
            level,
            message: message.to_string(),
        })
    }

    /// Append a single entry to the persistent log file.
    fn append_to_file(&self, entry: &LogEntry) {
        let path = match self.log_file_path.as_ref() {
            Some(p) => p,
            None => return,
        };
        let mut file = match OpenOptions::new().create(true).append(true).open(path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let _ = writeln!(
            file,
            "{} {} {}",
            entry.timestamp,
            entry.level.as_str(),
            entry.message
        );
    }

    /// Truncate the log file if it has grown too large.
    /// Called periodically (e.g. on session start) to keep the file bounded.
    fn truncate_if_needed(&self) {
        let path = match self.log_file_path.as_ref() {
            Some(p) if p.exists() => p,
            _ => return,
        };
        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
        if lines.len() > MAX_LOG_LINES {
            let trimmed = &lines[lines.len() - MAX_LOG_LINES..];
            if let Ok(mut f) = fs::File::create(path) {
                for line in trimmed {
                    let _ = writeln!(f, "{}", line);
                }
            }
        }
    }

    pub fn add_entry(&mut self, level: LogLevel, message: &str) {
        let now = chrono::Local::now();
        let entry = LogEntry {
            timestamp: now.format("%H:%M:%S").to_string(),
            level,
            message: message.to_string(),
        };
        self.append_to_file(&entry);
        self.entries.push(entry);
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

    /// Truncate the persistent log file on startup to keep it bounded.
    pub fn maintain_log_file(&self) {
        self.truncate_if_needed();
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
