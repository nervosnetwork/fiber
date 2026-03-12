//! Terminal User Interface (TUI) for Fiber Network Node.
//!
//! Provides a full-featured dashboard with real-time monitoring and interactive
//! controls for managing channels, payments, peers, invoices, and graph data.

pub mod app;
pub mod event;
pub mod tabs;
pub mod ui;

use anyhow::Result;

use crate::rpc_client::RpcClient;

/// Launch the TUI application.
pub async fn run(client: RpcClient) -> Result<()> {
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

    // Create and run the app
    let mut app = app::App::new(client);
    app.fetch_all_data().await;
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
