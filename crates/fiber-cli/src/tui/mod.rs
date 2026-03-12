//! Terminal User Interface (TUI) for Fiber Network Node.
//!
//! Provides a full-featured dashboard with real-time monitoring and interactive
//! controls for managing channels, payments, peers, invoices, and graph data.

pub mod app;
pub mod event;
pub mod tabs;
pub mod theme;
pub mod ui;

use anyhow::Result;

use crate::rpc_client::RpcClient;
use theme::Theme;

/// Launch the TUI application.
pub async fn run(client: RpcClient, theme: Theme) -> Result<()> {
    // Resolve the theme palette before entering raw mode (env var access).
    let palette = theme::resolve_palette(theme);

    // Set up terminal
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // Create and run the app (first frame renders immediately; data fetches on first tick)
    let mut app = app::App::new(client, palette);
    app.logs_tab.maintain_log_file();
    let result = app.run(&mut terminal).await;

    // Restore terminal
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}
