//! UI rendering for the TUI.
//!
//! This module handles all drawing logic using ratatui widgets.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, LineGauge, List, ListItem, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table, Tabs, Wrap,
};
use ratatui::Frame;

use super::app::{ActiveTab, App, ConfirmDialog, DetailPopup, InputMode};
use super::tabs::channels::{ChannelView, ChannelsTab};
use super::tabs::dashboard::DashboardTab;
use super::tabs::graph::{GraphTab, GraphView};
use super::tabs::invoices::{InvoiceView, InvoicesTab};
use super::tabs::logs::{LogLevel, LogsTab};
use super::tabs::payments::{PaymentView, PaymentsTab};
use super::tabs::peers::{PeerView, PeersTab};

/// The highlight style used for selected rows in tables.
const fn highlight_style() -> Style {
    Style::new()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}

/// Map channel state name to a distinct color.
fn channel_state_color(state: &str) -> Color {
    match state {
        "Ready" => Color::Green,
        "Closed" => Color::Red,
        "ShuttingDown" => Color::Yellow,
        "Negotiating" => Color::LightBlue,
        "Collaborating" => Color::LightCyan,
        "Signing" => Color::Magenta,
        "AwaitTxSig" => Color::LightMagenta,
        "AwaitReady" => Color::Cyan,
        _ => Color::Gray,
    }
}

/// Format CKB shannons into a human-readable string.
fn format_ckb(shannons: u128) -> String {
    let ckb = shannons as f64 / 1e8;
    if ckb >= 1.0 {
        format!("{:.4} CKB", ckb)
    } else {
        format!("{} shannons", shannons)
    }
}

/// Public wrapper for format_ckb, used by popup row builders in app.rs.
pub fn format_ckb_pub(shannons: u128) -> String {
    format_ckb(shannons)
}

/// Truncate a hex string for display.
fn truncate_hex(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...{}", &s[..max_len / 2], &s[s.len() - max_len / 2..])
    }
}

/// Format a UNIX timestamp (milliseconds) into a human-readable string.
fn format_timestamp(ms: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms as i64) {
        chrono::LocalResult::Single(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        _ => format!("{}ms", ms),
    }
}

/// Public wrapper for format_timestamp, used by popup row builders in app.rs.
pub fn format_timestamp_pub(ms: u64) -> String {
    format_timestamp(ms)
}

/// Render a vertical scrollbar next to an area.
fn render_scrollbar(f: &mut Frame, area: Rect, total_items: usize, position: usize) {
    if total_items == 0 {
        return;
    }
    let mut scrollbar_state = ScrollbarState::new(total_items).position(position);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("^"))
        .end_symbol(Some("v"));
    f.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
}

/// Main drawing entry point.
pub fn draw(f: &mut Frame, app: &mut App) {
    let size = f.area();

    // Layout: header (5 lines) + tabs bar (3 lines) + main content + footer (1 line)
    let chunks = Layout::vertical([
        Constraint::Length(5), // Node info header
        Constraint::Length(3), // Tab bar
        Constraint::Min(10),   // Main content area
        Constraint::Length(1), // Footer / status bar
    ])
    .split(size);

    draw_header(f, app, chunks[0]);
    draw_tab_bar(f, app, chunks[1]);
    draw_tab_content(f, app, chunks[2]);
    draw_footer(f, app, chunks[3]);

    // Help overlay
    if app.show_help {
        draw_help_overlay(f, size);
    }

    // Detail popup overlay
    if let Some(ref popup) = app.detail_popup {
        draw_detail_popup(f, popup, size);
    }

    // Confirmation dialog overlay (shown on top of everything)
    if let Some(ref dialog) = app.confirm_dialog {
        draw_confirm_dialog(f, dialog, size);
    }
}

/// Draw the node info header.
fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Fiber Network Node ")
        .title_alignment(Alignment::Center)
        .style(Style::default().fg(Color::Cyan));

    if let Some(ref info) = app.node_info {
        let name = info.node_name.as_deref().unwrap_or("(unnamed)");
        let pubkey = truncate_hex(&format!("{}", info.pubkey), 20);

        let line1 = Line::from(vec![
            Span::styled("Node: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                name,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled("Pubkey: ", Style::default().fg(Color::Yellow)),
            Span::styled(pubkey, Style::default().fg(Color::Green)),
            Span::raw("  "),
            Span::styled("Version: ", Style::default().fg(Color::Yellow)),
            Span::styled(&info.version, Style::default().fg(Color::White)),
        ]);

        let line2 = Line::from(vec![
            Span::styled("Channels: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}", info.channel_count),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("  "),
            Span::styled("Pending: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}", info.pending_channel_count),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw("  "),
            Span::styled("Peers: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}", info.peers_count),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("  "),
            Span::styled("Addresses: ", Style::default().fg(Color::Yellow)),
            Span::styled(info.addresses.join(", "), Style::default().fg(Color::White)),
        ]);

        let text = vec![line1, line2];
        let paragraph = Paragraph::new(text).block(block);
        f.render_widget(paragraph, area);
    } else if let Some(ref err) = app.node_info_error {
        let text = Line::from(vec![
            Span::styled("Error: ", Style::default().fg(Color::Red)),
            Span::raw(err.as_str()),
        ]);
        let paragraph = Paragraph::new(text).block(block);
        f.render_widget(paragraph, area);
    } else {
        let paragraph = Paragraph::new("Connecting...").block(block);
        f.render_widget(paragraph, area);
    }
}

/// Draw the tab bar.
fn draw_tab_bar(f: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = ActiveTab::all()
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let label = format!(" {} {} ", i + 1, tab.label());
            if *tab == app.active_tab {
                Line::from(Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(Span::styled(label, Style::default().fg(Color::DarkGray)))
            }
        })
        .collect();

    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Tabs "),
        )
        .select(app.active_tab.index())
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::raw(" | "));

    f.render_widget(tabs, area);
}

/// Draw the content of the active tab.
fn draw_tab_content(f: &mut Frame, app: &mut App, area: Rect) {
    match app.active_tab {
        ActiveTab::Dashboard => {
            draw_dashboard_tab(f, &app.dashboard_tab, app.node_info.as_ref(), area)
        }
        ActiveTab::Channels => draw_channels_tab(f, &mut app.channels_tab, area),
        ActiveTab::Payments => draw_payments_tab(f, &mut app.payments_tab, area),
        ActiveTab::Peers => draw_peers_tab(f, &mut app.peers_tab, area),
        ActiveTab::Invoices => draw_invoices_tab(f, &mut app.invoices_tab, area),
        ActiveTab::Graph => draw_graph_tab(f, &mut app.graph_tab, area),
        ActiveTab::Logs => draw_logs_tab(f, &mut app.logs_tab, area),
    }
}

/// Draw the footer / status bar.
fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let mode_str = match app.input_mode {
        InputMode::Normal => "NORMAL",
        InputMode::Editing => "EDITING",
    };
    let mode_color = match app.input_mode {
        InputMode::Normal => Color::Green,
        InputMode::Editing => Color::Yellow,
    };

    // If there is an active flash message, show it prominently
    if let Some((ref msg, is_error, _)) = app.flash_message {
        let flash_color = if is_error { Color::Red } else { Color::Green };
        let footer = Line::from(vec![
            Span::styled(
                format!(" {} ", mode_str),
                Style::default()
                    .fg(Color::Black)
                    .bg(mode_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                msg.as_str(),
                Style::default()
                    .fg(flash_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(Paragraph::new(footer), area);
        return;
    }

    let footer = Line::from(vec![
        Span::styled(
            format!(" {} ", mode_str),
            Style::default()
                .fg(Color::Black)
                .bg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            "q:Quit  Tab:Switch  r:Refresh  ?:Help",
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        Span::styled(
            format!("RPC: {}", app.client.url()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    f.render_widget(Paragraph::new(footer), area);
}

// ── Dashboard Tab ──────────────────────────────────────────────────────

fn draw_dashboard_tab(
    f: &mut Frame,
    tab: &DashboardTab,
    node_info: Option<&fiber_json_types::NodeInfoResult>,
    area: Rect,
) {
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Dashboard ")
        .title_alignment(Alignment::Left);

    let inner = outer_block.inner(area);
    f.render_widget(outer_block, area);

    // Layout: top stats row + capacity gauge + channel state breakdown + TLC info + network overview
    let chunks = Layout::vertical([
        Constraint::Length(3), // Summary stats row
        Constraint::Length(3), // Capacity utilization gauge
        Constraint::Length(9), // Channel state breakdown
        Constraint::Length(5), // TLC stats
        Constraint::Min(4),    // Network overview (from graph)
    ])
    .split(inner);

    // ── Row 1: Summary stats ────────────────────────────────────────
    draw_dashboard_summary(f, tab, node_info, chunks[0]);

    // ── Row 2: Capacity utilization gauge ───────────────────────────
    draw_dashboard_capacity_gauge(f, tab, chunks[1]);

    // ── Row 3: Channel state breakdown ──────────────────────────────
    draw_dashboard_channel_states(f, tab, chunks[2]);

    // ── Row 4: TLC stats ────────────────────────────────────────────
    draw_dashboard_tlc_stats(f, tab, chunks[3]);

    // ── Row 5: Network overview (graph nodes & channels) ────────────
    draw_dashboard_network(f, tab, chunks[4]);
}

fn draw_dashboard_summary(
    f: &mut Frame,
    tab: &DashboardTab,
    node_info: Option<&fiber_json_types::NodeInfoResult>,
    area: Rect,
) {
    let stats = &tab.stats;

    let peers_count = node_info.map_or(0, |n| n.peers_count);
    let channel_count = node_info.map_or(stats.total_channels as u32, |n| n.channel_count);
    let pending_count = node_info.map_or(stats.pending_count as u32, |n| n.pending_channel_count);

    let line = Line::from(vec![
        Span::styled("  Channels: ", Style::default().fg(Color::Yellow)),
        Span::styled(
            format!("{}", channel_count),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled("Pending: ", Style::default().fg(Color::Yellow)),
        Span::styled(
            format!("{}", pending_count),
            Style::default().fg(Color::Magenta),
        ),
        Span::raw("   "),
        Span::styled("Peers: ", Style::default().fg(Color::Yellow)),
        Span::styled(format!("{}", peers_count), Style::default().fg(Color::Cyan)),
        Span::raw("   "),
        Span::styled("Total Capacity: ", Style::default().fg(Color::Yellow)),
        Span::styled(
            format_ckb(stats.total_capacity),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Overview ");

    let paragraph = Paragraph::new(line).block(block);
    f.render_widget(paragraph, area);
}

fn draw_dashboard_capacity_gauge(f: &mut Frame, tab: &DashboardTab, area: Rect) {
    let stats = &tab.stats;

    let ratio = if stats.total_capacity > 0 {
        stats.total_local_balance as f64 / stats.total_capacity as f64
    } else {
        0.0
    };

    let label = format!(
        "Local: {}  |  Remote: {}",
        format_ckb(stats.total_local_balance),
        format_ckb(stats.total_remote_balance),
    );

    let gauge = LineGauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Capacity (Local / Remote) "),
        )
        .filled_style(Style::default().fg(Color::Green))
        .unfilled_style(Style::default().fg(Color::Red))
        .ratio(ratio)
        .label(label)
        .line_set(ratatui::symbols::line::THICK);

    f.render_widget(gauge, area);
}

fn draw_dashboard_channel_states(f: &mut Frame, tab: &DashboardTab, area: Rect) {
    let stats = &tab.stats;

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Ready:        ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}", stats.ready_count),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  ("),
            Span::styled(
                format!("{} enabled", stats.enabled_count),
                Style::default().fg(Color::Green),
            ),
            Span::raw(", "),
            Span::styled(
                format!("{} disabled", stats.disabled_count),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(")"),
        ]),
        Line::from(vec![
            Span::styled("  Pending:      ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}", stats.pending_count),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Shutting Down: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}", stats.shutting_down_count),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Closed:       ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}", stats.closed_count),
                Style::default().fg(Color::Red),
            ),
        ]),
        Line::from(""),
        draw_state_bar(stats),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Channel States ");

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

/// Draw a simple text-based bar chart showing channel state distribution.
fn draw_state_bar(stats: &super::tabs::dashboard::DashboardStats) -> Line<'static> {
    let total = stats.total_channels;
    if total == 0 {
        return Line::from(Span::styled(
            "  No channels",
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Each character represents a fraction of channels; use ~40 chars wide
    let bar_width: usize = 40;
    let ready_chars =
        (stats.ready_count * bar_width / total).max(if stats.ready_count > 0 { 1 } else { 0 });
    let pending_chars =
        (stats.pending_count * bar_width / total).max(if stats.pending_count > 0 { 1 } else { 0 });
    let shutting_chars = (stats.shutting_down_count * bar_width / total)
        .max(if stats.shutting_down_count > 0 { 1 } else { 0 });
    let closed_chars = bar_width.saturating_sub(ready_chars + pending_chars + shutting_chars);

    let mut spans = vec![Span::raw("  ")];
    if ready_chars > 0 {
        spans.push(Span::styled(
            "\u{2588}".repeat(ready_chars),
            Style::default().fg(Color::Green),
        ));
    }
    if pending_chars > 0 {
        spans.push(Span::styled(
            "\u{2588}".repeat(pending_chars),
            Style::default().fg(Color::Cyan),
        ));
    }
    if shutting_chars > 0 {
        spans.push(Span::styled(
            "\u{2588}".repeat(shutting_chars),
            Style::default().fg(Color::Yellow),
        ));
    }
    if closed_chars > 0 && stats.closed_count > 0 {
        spans.push(Span::styled(
            "\u{2588}".repeat(closed_chars),
            Style::default().fg(Color::Red),
        ));
    }

    Line::from(spans)
}

fn draw_dashboard_tlc_stats(f: &mut Frame, tab: &DashboardTab, area: Rect) {
    let stats = &tab.stats;

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Pending TLCs: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}", stats.total_pending_tlcs),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("   "),
            Span::styled("Offered: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format_ckb(stats.total_offered_tlc),
                Style::default().fg(Color::Green),
            ),
            Span::raw("   "),
            Span::styled("Received: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format_ckb(stats.total_received_tlc),
                Style::default().fg(Color::Magenta),
            ),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" TLC Activity ");

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn draw_dashboard_network(f: &mut Frame, tab: &DashboardTab, area: Rect) {
    let net = &tab.network_stats;

    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Network Topology ");

    if net.total_nodes == 0 && net.total_channels == 0 {
        let text = Paragraph::new("  No graph data available")
            .style(Style::default().fg(Color::DarkGray))
            .block(outer_block);
        f.render_widget(text, area);
        return;
    }

    let inner = outer_block.inner(area);
    f.render_widget(outer_block, area);

    // Split: left = adjacency tree, right = stats text
    let chunks = Layout::horizontal([
        Constraint::Min(20),    // Adjacency list (takes remaining space)
        Constraint::Length(36), // Stats panel
    ])
    .split(inner);

    // ── Right: stats text ───────────────────────────────────────────
    let inactive = net.total_channels.saturating_sub(net.active_channels);
    let stats_lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(" Nodes: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}", net.total_nodes),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Channels: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}", net.total_channels),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("   Active: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", net.active_channels),
                Style::default().fg(Color::Green),
            ),
        ]),
        Line::from(vec![
            Span::styled("   Inactive: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}", inactive),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Capacity: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format_ckb(net.total_capacity),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];
    let stats_block = Block::default()
        .borders(Borders::LEFT)
        .border_type(BorderType::Plain);
    let stats_paragraph = Paragraph::new(stats_lines).block(stats_block);
    f.render_widget(stats_paragraph, chunks[1]);

    // ── Left: adjacency tree list ───────────────────────────────────
    let max_lines = chunks[0].height as usize;
    let mut lines: Vec<Line<'_>> = Vec::new();

    // Self node header.
    if !tab.self_label.is_empty() {
        let peer_count = tab.self_peers.len();
        lines.push(Line::from(vec![
            Span::styled(
                " ◆ ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                tab.self_label.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ({} peers, {} ch)", peer_count, tab.self_degree),
                Style::default().fg(Color::DarkGray),
            ),
        ]));

        // List direct peers.
        let peer_limit = max_lines.saturating_sub(4); // reserve room for "Other" section
        let show_count = tab.self_peers.len().min(peer_limit);
        for (i, peer) in tab.self_peers.iter().take(show_count).enumerate() {
            let is_last = i == show_count - 1 && tab.self_peers.len() <= show_count;
            let branch = if is_last { " └─" } else { " ├─" };
            let status_dot = if peer.active_count > 0 {
                Span::styled(" ●", Style::default().fg(Color::Green))
            } else {
                Span::styled(" ○", Style::default().fg(Color::DarkGray))
            };
            let ch_label = if peer.channel_count > 1 {
                format!(" {}ch", peer.channel_count)
            } else {
                " 1ch".to_string()
            };
            lines.push(Line::from(vec![
                Span::styled(branch, Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(" [{}]", peer.label),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(ch_label, Style::default().fg(Color::White)),
                Span::styled(
                    format!("  {}", format_ckb(peer.capacity)),
                    Style::default().fg(Color::Green),
                ),
                status_dot,
            ]));
        }
        if tab.self_peers.len() > show_count {
            lines.push(Line::from(Span::styled(
                format!(" └─ … +{} more", tab.self_peers.len() - show_count),
                Style::default().fg(Color::DarkGray),
            )));
        }
    } else if net.total_nodes > 0 {
        // No self node in graph — just show summary.
        lines.push(Line::from(Span::styled(
            " (own node not in graph)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Other connections section.
    if !tab.other_connections.is_empty() {
        let remaining = max_lines.saturating_sub(lines.len() + 1);
        if remaining > 1 {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " Other connections:",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )));
            let show_other = tab.other_connections.len().min(remaining.saturating_sub(2));
            for conn in tab.other_connections.iter().take(show_other) {
                let status_dot = if conn.active_count > 0 {
                    Span::styled(" ●", Style::default().fg(Color::Green))
                } else {
                    Span::styled(" ○", Style::default().fg(Color::DarkGray))
                };
                let ch_label = if conn.channel_count > 1 {
                    format!(" {}ch", conn.channel_count)
                } else {
                    String::new()
                };
                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled(conn.label_a.clone(), Style::default().fg(Color::White)),
                    Span::styled(" ── ", Style::default().fg(Color::DarkGray)),
                    Span::styled(conn.label_b.clone(), Style::default().fg(Color::White)),
                    Span::styled(ch_label, Style::default().fg(Color::DarkGray)),
                    status_dot,
                ]));
            }
            if tab.other_connections.len() > show_other {
                lines.push(Line::from(Span::styled(
                    format!("   … +{} more", tab.other_connections.len() - show_other),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " No topology data",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, chunks[0]);
}

// ── Channels Tab ────────────────────────────────────────────────────────

fn draw_channels_tab(f: &mut Frame, tab: &mut ChannelsTab, area: Rect) {
    match tab.view {
        ChannelView::List => draw_channels_list(f, tab, area),
        ChannelView::OpenForm => draw_form(
            f,
            "Open Channel",
            &tab.form_fields,
            tab.form_selected,
            tab.status_message.as_deref(),
            area,
        ),
        ChannelView::UpdateForm => draw_form(
            f,
            "Update Channel",
            &tab.form_fields,
            tab.form_selected,
            tab.status_message.as_deref(),
            area,
        ),
    }
}

fn draw_channels_list(f: &mut Frame, tab: &mut ChannelsTab, area: Rect) {
    if let Some(ref err) = tab.error {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Channels ");
        let text = Paragraph::new(format!("Error: {}", err))
            .style(Style::default().fg(Color::Red))
            .block(block);
        f.render_widget(text, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("Channel ID"),
        Cell::from("State"),
        Cell::from("Peer"),
        Cell::from("Local Balance"),
        Cell::from("Remote Balance"),
        Cell::from("Public"),
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = tab
        .channels
        .iter()
        .map(|ch| {
            let state = ChannelsTab::state_name(ch);
            let state_color = channel_state_color(state);

            Row::new(vec![
                Cell::from(truncate_hex(&format!("{}", ch.channel_id), 16)),
                Cell::from(Span::styled(state, Style::default().fg(state_color))),
                Cell::from(truncate_hex(&format!("{}", ch.pubkey), 16)),
                Cell::from(format_ckb(ch.local_balance)),
                Cell::from(format_ckb(ch.remote_balance)),
                Cell::from(if ch.is_public { "yes" } else { "no" }),
            ])
        })
        .collect();

    let filter_label = if tab.include_closed {
        " (showing closed)"
    } else if tab.only_pending {
        " (pending only)"
    } else {
        ""
    };

    let title = format!(
        " Channels ({}){}  [o:Open  s:Shutdown  a:Abandon  c:Closed  p:Pending  Enter:Detail] ",
        tab.channels.len(),
        filter_label,
    );

    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Length(14),
            Constraint::Length(18),
            Constraint::Length(18),
            Constraint::Length(18),
            Constraint::Length(6),
        ],
    )
    .header(header)
    .row_highlight_style(highlight_style())
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title),
    );

    f.render_stateful_widget(table, area, &mut tab.table_state);

    // Scrollbar
    let position = tab.table_state.selected().unwrap_or(0);
    render_scrollbar(f, area, tab.channels.len(), position);
}

// ── Payments Tab ────────────────────────────────────────────────────────

fn draw_payments_tab(f: &mut Frame, tab: &mut PaymentsTab, area: Rect) {
    match tab.view {
        PaymentView::List => draw_payments_list(f, tab, area),
        PaymentView::SendForm => draw_form(
            f,
            "Send Payment",
            &tab.form_fields,
            tab.form_selected,
            tab.status_message.as_deref(),
            area,
        ),
    }
}

fn draw_payments_list(f: &mut Frame, tab: &mut PaymentsTab, area: Rect) {
    if let Some(ref err) = tab.error {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Payments ");
        let text = Paragraph::new(format!("Error: {}", err))
            .style(Style::default().fg(Color::Red))
            .block(block);
        f.render_widget(text, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("Payment Hash"),
        Cell::from("Status"),
        Cell::from("Fee"),
        Cell::from("Created"),
        Cell::from("Updated"),
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = tab
        .payments
        .iter()
        .map(|p| {
            let status = PaymentsTab::status_name(p);
            let status_color = match p.status {
                fiber_json_types::PaymentStatus::Success => Color::Green,
                fiber_json_types::PaymentStatus::Failed => Color::Red,
                fiber_json_types::PaymentStatus::Inflight => Color::Yellow,
                fiber_json_types::PaymentStatus::Created => Color::Cyan,
            };

            Row::new(vec![
                Cell::from(truncate_hex(&format!("{}", p.payment_hash), 20)),
                Cell::from(Span::styled(status, Style::default().fg(status_color))),
                Cell::from(format_ckb(p.fee)),
                Cell::from(format_timestamp(p.created_at)),
                Cell::from(format_timestamp(p.last_updated_at)),
            ])
        })
        .collect();

    let filter_label = if tab.status_filter.is_some() {
        format!("  filter:{}", tab.filter_label())
    } else {
        String::new()
    };

    let page_indicator = if tab.last_cursor.is_some() || tab.current_page > 1 {
        format!("  pg {}  []:Prev/Next", tab.current_page)
    } else {
        String::new()
    };

    let title = format!(
        " Payments ({}){}{}  [n:Send  f:Filter  Enter:Detail] ",
        tab.payments.len(),
        filter_label,
        page_indicator,
    );

    let table = Table::new(
        rows,
        [
            Constraint::Length(22),
            Constraint::Length(10),
            Constraint::Length(18),
            Constraint::Length(20),
            Constraint::Length(20),
        ],
    )
    .header(header)
    .row_highlight_style(highlight_style())
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title),
    );

    f.render_stateful_widget(table, area, &mut tab.table_state);

    let position = tab.table_state.selected().unwrap_or(0);
    render_scrollbar(f, area, tab.payments.len(), position);
}

// ── Peers Tab ───────────────────────────────────────────────────────────

fn draw_peers_tab(f: &mut Frame, tab: &mut PeersTab, area: Rect) {
    match tab.view {
        PeerView::List => draw_peers_list(f, tab, area),
        PeerView::ConnectForm => draw_form(
            f,
            "Connect Peer",
            &tab.form_fields,
            tab.form_selected,
            tab.status_message.as_deref(),
            area,
        ),
    }
}

fn draw_peers_list(f: &mut Frame, tab: &mut PeersTab, area: Rect) {
    if let Some(ref err) = tab.error {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Peers ");
        let text = Paragraph::new(format!("Error: {}", err))
            .style(Style::default().fg(Color::Red))
            .block(block);
        f.render_widget(text, area);
        return;
    }

    // When searching, split area to reserve a search input bar at the bottom
    let (table_area, search_area) = if tab.searching {
        let chunks = Layout::vertical([
            Constraint::Min(5),    // Table
            Constraint::Length(3), // Search bar
        ])
        .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    let header = Row::new(vec![Cell::from("Pubkey"), Cell::from("Address")]).style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = tab
        .peers
        .iter()
        .map(|peer| {
            Row::new(vec![
                Cell::from(truncate_hex(&format!("{}", peer.pubkey), 24)),
                Cell::from(peer.address.clone()),
            ])
        })
        .collect();

    let search_label = if !tab.search_query.is_empty() && !tab.searching {
        format!("  search:\"{}\"", tab.search_query)
    } else {
        String::new()
    };

    let title = format!(
        " Peers ({}/{}){}  [c:Connect  d:Disconnect  /:Search] ",
        tab.peers.len(),
        tab.all_peers.len(),
        search_label,
    );

    let table = Table::new(rows, [Constraint::Length(26), Constraint::Min(30)])
        .header(header)
        .row_highlight_style(highlight_style())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(title),
        );

    f.render_stateful_widget(table, table_area, &mut tab.table_state);

    let position = tab.table_state.selected().unwrap_or(0);
    render_scrollbar(f, table_area, tab.peers.len(), position);

    // Draw search input bar when in search mode
    if let Some(search_area) = search_area {
        let search_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Search (Esc/Enter:Done) ")
            .style(Style::default().fg(Color::Yellow));

        let search_text = Line::from(vec![
            Span::styled("/ ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{}_", tab.search_query),
                Style::default().fg(Color::White),
            ),
        ]);

        let search_paragraph = Paragraph::new(search_text).block(search_block);
        f.render_widget(search_paragraph, search_area);
    }
}

// ── Invoices Tab ────────────────────────────────────────────────────────

fn draw_invoices_tab(f: &mut Frame, tab: &mut InvoicesTab, area: Rect) {
    match tab.view {
        InvoiceView::Main => draw_invoices_main(f, tab, area),
        InvoiceView::CreateForm => draw_form(
            f,
            "Create Invoice",
            &tab.form_fields,
            tab.form_selected,
            tab.status_message.as_deref(),
            area,
        ),
        InvoiceView::LookupResult => {
            if tab.form_editing {
                draw_form(
                    f,
                    "Lookup Invoice",
                    &tab.form_fields,
                    tab.form_selected,
                    tab.status_message.as_deref(),
                    area,
                );
            } else {
                draw_invoice_lookup_result(f, tab, area);
            }
        }
        InvoiceView::CancelForm => draw_form(
            f,
            "Cancel Invoice",
            &tab.form_fields,
            tab.form_selected,
            tab.status_message.as_deref(),
            area,
        ),
        InvoiceView::ParseForm => draw_form(
            f,
            "Parse Invoice",
            &tab.form_fields,
            tab.form_selected,
            tab.status_message.as_deref(),
            area,
        ),
        InvoiceView::ParseResult => draw_parse_result(f, tab, area),
    }
}

fn draw_invoices_main(f: &mut Frame, tab: &mut InvoicesTab, area: Rect) {
    let filter_info = format!(
        " Invoices  [n:New  l:Lookup  c:Cancel  p:Parse  f:Filter({})  ]/[:Page]  Pg {} ",
        tab.filter_label(),
        tab.current_page
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(filter_info);

    if tab.invoices.is_empty() {
        let msg = if tab.status_filter.is_some() {
            "No invoices matching current filter.\n\nPress 'f' to cycle filter, or 'n' to create a new invoice."
        } else {
            "No invoices found.\n\nPress 'n' to create a new invoice, or 'l' to lookup by payment hash."
        };
        let text = Paragraph::new(msg)
            .block(block)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(text, area);
    } else {
        let items: Vec<ListItem> = tab
            .invoices
            .iter()
            .map(|inv| {
                let amount = inv
                    .invoice
                    .amount
                    .map(format_ckb)
                    .unwrap_or_else(|| "N/A".to_string());
                let status_str = InvoicesTab::status_name(&inv.status);
                let status_color = match inv.status {
                    fiber_json_types::CkbInvoiceStatus::Open => Color::Green,
                    fiber_json_types::CkbInvoiceStatus::Paid => Color::Cyan,
                    fiber_json_types::CkbInvoiceStatus::Cancelled => Color::Red,
                    fiber_json_types::CkbInvoiceStatus::Expired => Color::DarkGray,
                    fiber_json_types::CkbInvoiceStatus::Received => Color::Yellow,
                };
                let addr_short = truncate_hex(&inv.invoice_address, 30);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("[{}] ", status_str),
                        Style::default().fg(status_color),
                    ),
                    Span::styled(format!("{}: ", amount), Style::default().fg(Color::Cyan)),
                    Span::raw(addr_short),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(block)
            .highlight_style(highlight_style());

        f.render_stateful_widget(list, area, &mut tab.list_state);

        let position = tab.list_state.selected().unwrap_or(0);
        render_scrollbar(f, area, tab.invoices.len(), position);
    }
}

fn draw_invoice_lookup_result(f: &mut Frame, tab: &InvoicesTab, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Invoice Lookup  [Esc:Back] ");

    if let Some(ref err) = tab.lookup_error {
        let text = Paragraph::new(format!("Error: {}", err))
            .style(Style::default().fg(Color::Red))
            .block(block);
        f.render_widget(text, area);
        return;
    }

    if let Some(ref result) = tab.lookup_result {
        let status_str = format!("{:?}", result.status);
        let amount = result
            .invoice
            .amount
            .map(format_ckb)
            .unwrap_or_else(|| "N/A".to_string());

        let lines = vec![
            Line::from(vec![
                Span::styled("Address:  ", Style::default().fg(Color::Yellow)),
                Span::raw(truncate_hex(&result.invoice_address, 60)),
            ]),
            Line::from(vec![
                Span::styled("Status:   ", Style::default().fg(Color::Yellow)),
                Span::raw(status_str),
            ]),
            Line::from(vec![
                Span::styled("Amount:   ", Style::default().fg(Color::Yellow)),
                Span::raw(amount),
            ]),
            Line::from(vec![
                Span::styled("Currency: ", Style::default().fg(Color::Yellow)),
                Span::raw(format!("{:?}", result.invoice.currency)),
            ]),
        ];

        let paragraph = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        f.render_widget(paragraph, area);
    } else {
        let text = Paragraph::new("Looking up...").block(block);
        f.render_widget(text, area);
    }
}

fn draw_parse_result(f: &mut Frame, tab: &InvoicesTab, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Parse Invoice Result  [Esc:Back] ");

    if let Some(ref err) = tab.parse_error {
        let text = Paragraph::new(format!("Error: {}", err))
            .style(Style::default().fg(Color::Red))
            .block(block);
        f.render_widget(text, area);
        return;
    }

    if let Some(ref result) = tab.parse_result {
        // Pretty-print the JSON result
        let formatted =
            serde_json::to_string_pretty(result).unwrap_or_else(|_| format!("{}", result));
        let lines: Vec<Line> = formatted
            .lines()
            .map(|l| Line::from(Span::raw(l.to_string())))
            .collect();

        let paragraph = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false });
        f.render_widget(paragraph, area);
    } else {
        let text = Paragraph::new("Parsing...").block(block);
        f.render_widget(text, area);
    }
}

// ── Graph Tab ───────────────────────────────────────────────────────────

fn draw_graph_tab(f: &mut Frame, tab: &mut GraphTab, area: Rect) {
    // Split area for a small selector and the content
    let chunks = Layout::vertical([
        Constraint::Length(3), // Sub-tab selector
        Constraint::Min(5),    // Content
    ])
    .split(area);

    // Sub-tab selector: Nodes | Channels
    let sub_tabs = vec![
        Line::from(Span::styled(
            " Nodes ",
            if tab.view == GraphView::Nodes {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        )),
        Line::from(Span::styled(
            " Channels ",
            if tab.view == GraphView::Channels {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        )),
    ];

    let sub_tab_widget = Tabs::new(sub_tabs)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(" Graph  [h/l:Switch] "),
        )
        .select(match tab.view {
            GraphView::Nodes => 0,
            GraphView::Channels => 1,
        })
        .divider(" | ");

    f.render_widget(sub_tab_widget, chunks[0]);

    match tab.view {
        GraphView::Nodes => draw_graph_nodes(f, tab, chunks[1]),
        GraphView::Channels => draw_graph_channels(f, tab, chunks[1]),
    }
}

fn draw_graph_nodes(f: &mut Frame, tab: &mut GraphTab, area: Rect) {
    if let Some(ref err) = tab.nodes_error {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Graph Nodes ");
        let text = Paragraph::new(format!("Error: {}", err))
            .style(Style::default().fg(Color::Red))
            .block(block);
        f.render_widget(text, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("Name"),
        Cell::from("Pubkey"),
        Cell::from("Addresses"),
        Cell::from("Version"),
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = tab
        .nodes
        .iter()
        .map(|node| {
            Row::new(vec![
                Cell::from(if node.node_name.is_empty() {
                    "(unnamed)".to_string()
                } else {
                    node.node_name.clone()
                }),
                Cell::from(truncate_hex(&format!("{}", node.pubkey), 20)),
                Cell::from(node.addresses.join(", ")),
                Cell::from(node.version.clone()),
            ])
        })
        .collect();

    let page_indicator = if tab.nodes_last_cursor.is_some() || tab.nodes_page > 1 {
        format!("  pg {}  []:Prev/Next", tab.nodes_page)
    } else {
        String::new()
    };

    let title = format!(" Graph Nodes ({}){} ", tab.nodes.len(), page_indicator);

    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(22),
            Constraint::Min(30),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .row_highlight_style(highlight_style())
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title),
    );

    f.render_stateful_widget(table, area, &mut tab.nodes_table_state);

    let position = tab.nodes_table_state.selected().unwrap_or(0);
    render_scrollbar(f, area, tab.nodes.len(), position);
}

fn draw_graph_channels(f: &mut Frame, tab: &mut GraphTab, area: Rect) {
    if let Some(ref err) = tab.channels_error {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Graph Channels ");
        let text = Paragraph::new(format!("Error: {}", err))
            .style(Style::default().fg(Color::Red))
            .block(block);
        f.render_widget(text, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from("Node 1"),
        Cell::from("Node 2"),
        Cell::from("Capacity"),
        Cell::from("Created"),
    ])
    .style(
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = tab
        .channels
        .iter()
        .map(|ch| {
            Row::new(vec![
                Cell::from(truncate_hex(&format!("{}", ch.node1), 16)),
                Cell::from(truncate_hex(&format!("{}", ch.node2), 16)),
                Cell::from(format_ckb(ch.capacity)),
                Cell::from(format_timestamp(ch.created_timestamp)),
            ])
        })
        .collect();

    let page_indicator = if tab.channels_last_cursor.is_some() || tab.channels_page > 1 {
        format!("  pg {}  []:Prev/Next", tab.channels_page)
    } else {
        String::new()
    };

    let title = format!(
        " Graph Channels ({}){} ",
        tab.channels.len(),
        page_indicator
    );

    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Length(18),
            Constraint::Length(18),
            Constraint::Length(20),
        ],
    )
    .header(header)
    .row_highlight_style(highlight_style())
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(title),
    );

    f.render_stateful_widget(table, area, &mut tab.channels_table_state);

    let position = tab.channels_table_state.selected().unwrap_or(0);
    render_scrollbar(f, area, tab.channels.len(), position);
}

// ── Logs Tab ────────────────────────────────────────────────────────────

fn draw_logs_tab(f: &mut Frame, tab: &mut LogsTab, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(
            " Logs ({})  [j/k:Scroll  G:Bottom] ",
            tab.entries.len()
        ));

    let items: Vec<ListItem> = tab
        .entries
        .iter()
        .map(|entry| {
            let level_style = match entry.level {
                LogLevel::Info => Style::default().fg(Color::Green),
                LogLevel::Warn => Style::default().fg(Color::Yellow),
                LogLevel::Error => Style::default().fg(Color::Red),
            };
            let level_str = match entry.level {
                LogLevel::Info => "INFO ",
                LogLevel::Warn => "WARN ",
                LogLevel::Error => "ERROR",
            };

            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", entry.timestamp),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(level_str, level_style),
                Span::raw(" "),
                Span::raw(entry.message.as_str()),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style());

    f.render_stateful_widget(list, area, &mut tab.list_state);

    let position = tab.list_state.selected().unwrap_or(0);
    render_scrollbar(f, area, tab.entries.len(), position);
}

// ── Shared Form Renderer ────────────────────────────────────────────────

fn draw_form(
    f: &mut Frame,
    title: &str,
    fields: &[(String, String)],
    selected: usize,
    status: Option<&str>,
    area: Rect,
) {
    let field_count_label = format!(" {}/{} ", selected + 1, fields.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(
            " {}  [Enter:Submit  Tab/Up/Down:Fields  Esc:Cancel] ",
            title
        ))
        .title_bottom(Line::from(Span::styled(
            field_count_label,
            Style::default().fg(Color::DarkGray),
        )));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    for (i, (label, value)) in fields.iter().enumerate() {
        let is_selected = i == selected;
        let label_style = if is_selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let cursor = if is_selected { "> " } else { "  " };

        lines.push(Line::from(vec![
            Span::styled(cursor, Style::default().fg(Color::Yellow)),
            Span::styled(format!("{}: ", label), label_style),
        ]));

        // Show input value with cursor indicator for active field
        let value_display = if is_selected {
            format!("{}_", value)
        } else if value.is_empty() {
            "(empty)".to_string()
        } else {
            value.clone()
        };

        let value_style = if is_selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::UNDERLINED)
        } else if value.is_empty() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Gray)
        };

        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(value_display, value_style),
        ]));

        // Separator between fields
        if i < fields.len() - 1 {
            lines.push(Line::from(Span::styled(
                "    ────────────────────────────",
                Style::default().fg(Color::Indexed(236)), // very dark gray
            )));
        } else {
            lines.push(Line::from(""));
        }
    }

    if let Some(status_msg) = status {
        lines.push(Line::from(""));
        let status_color = if status_msg.contains("failed") || status_msg.contains("Failed") {
            Color::Red
        } else if status_msg.contains("Invalid") || status_msg.contains("required") {
            Color::Yellow
        } else {
            Color::Magenta
        };
        lines.push(Line::from(Span::styled(
            status_msg,
            Style::default().fg(status_color),
        )));
    }

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(paragraph, inner);
}

// ── Help Overlay ────────────────────────────────────────────────────────

fn draw_help_overlay(f: &mut Frame, area: Rect) {
    // Center the help popup
    let popup_width = 60u16.min(area.width.saturating_sub(4));
    let popup_height = 30u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .title(" Help ")
        .title_alignment(Alignment::Center)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    let help_text = vec![
        Line::from(Span::styled(
            "Global Shortcuts",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  q         Quit"),
        Line::from("  ?/F1      Toggle this help"),
        Line::from("  1-7       Switch to tab"),
        Line::from("  Tab       Next tab"),
        Line::from("  Shift+Tab Previous tab"),
        Line::from("  r         Refresh data"),
        Line::from("  y         Copy selected item to clipboard"),
        Line::from("  Ctrl+C    Force quit"),
        Line::from(""),
        Line::from(Span::styled(
            "Navigation",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  j/Down    Move down"),
        Line::from("  k/Up      Move up"),
        Line::from("  g/Home    Go to top"),
        Line::from("  G/End     Go to bottom"),
        Line::from("  Enter     Select/Detail"),
        Line::from("  Esc       Back/Cancel"),
        Line::from("  ]         Next page (Payments/Graph)"),
        Line::from("  [         Previous page (Payments/Graph)"),
        Line::from("  /         Search (Peers)"),
        Line::from(""),
        Line::from(Span::styled(
            "Tab Actions",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  See each tab's title bar for actions"),
        Line::from(""),
        Line::from(Span::styled(
            "Form Editing",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  Tab/Down  Next field"),
        Line::from("  S-Tab/Up  Previous field"),
        Line::from("  Enter     Submit form"),
        Line::from("  Esc       Cancel / back"),
    ];

    let paragraph = Paragraph::new(help_text)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, popup_area);
}

// ── Detail Popup Overlay ────────────────────────────────────────────────

fn draw_detail_popup(f: &mut Frame, popup: &DetailPopup, area: Rect) {
    // Padding inside the border: 2 chars left+right, 1 line top+bottom.
    let pad_x: u16 = 2;
    let pad_y: u16 = 1;

    let popup_width = 80u16.min(area.width.saturating_sub(4));
    // inner_width accounts for borders (2) and horizontal padding (2 * pad_x).
    let inner_width = popup_width.saturating_sub(2 + pad_x * 2) as usize;

    // First pass: compute total visual lines so we can size the popup.
    let visual_line_count = count_visual_lines(&popup.rows, inner_width);
    // +2 for borders, +2*pad_y for vertical padding.
    let popup_height = ((visual_line_count as u16).saturating_add(2 + pad_y * 2))
        .min(area.height.saturating_sub(4));

    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .title(format!(" {} ", popup.title))
        .title_alignment(Alignment::Center)
        .title_bottom(Line::from(Span::styled(
            " j/k:Navigate  y:Copy  u:Update(Ch)  Esc:Close ",
            Style::default().fg(Color::DarkGray),
        )))
        .style(Style::default().bg(Color::Black).fg(Color::White));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    // Apply padding inside the border.
    let padded = Rect::new(
        inner.x + pad_x,
        inner.y + pad_y,
        inner.width.saturating_sub(pad_x * 2),
        inner.height.saturating_sub(pad_y * 2),
    );

    if popup.rows.is_empty() {
        let text = Paragraph::new("No data").style(Style::default().fg(Color::DarkGray));
        f.render_widget(text, padded);
        return;
    }

    let visible_height = padded.height as usize;

    // Build all visual lines, tracking which belong to the selected row.
    let mut visual_lines: Vec<Line> = Vec::new();
    let mut selected_visual_start: usize = 0;
    let mut selected_visual_count: usize = 0;

    for (i, (key, value)) in popup.rows.iter().enumerate() {
        let is_selected = i == popup.selected;
        let marker = if is_selected { "> " } else { "  " };
        let key_prefix = format!("{}{}: ", marker, key);
        let prefix_len = key_prefix.len();

        let key_style = if is_selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        };
        let value_style = if is_selected {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        let first_line_capacity = inner_width.saturating_sub(prefix_len);
        let cont_indent = " ".repeat(prefix_len);
        let cont_capacity = inner_width.saturating_sub(prefix_len);

        if i == popup.selected {
            selected_visual_start = visual_lines.len();
        }

        if value.len() <= first_line_capacity || cont_capacity == 0 {
            visual_lines.push(Line::from(vec![
                Span::styled(marker.to_string(), Style::default().fg(Color::Cyan)),
                Span::styled(format!("{}: ", key), key_style),
                Span::styled(value.clone(), value_style),
            ]));
        } else {
            let mut remaining = value.as_str();
            let mut is_first = true;
            while !remaining.is_empty() {
                let cap = if is_first {
                    first_line_capacity
                } else {
                    cont_capacity
                };
                let split_at = remaining
                    .char_indices()
                    .nth(cap)
                    .map(|(idx, _)| idx)
                    .unwrap_or(remaining.len());
                let chunk = &remaining[..split_at];
                remaining = &remaining[split_at..];

                if is_first {
                    visual_lines.push(Line::from(vec![
                        Span::styled(marker.to_string(), Style::default().fg(Color::Cyan)),
                        Span::styled(format!("{}: ", key), key_style),
                        Span::styled(chunk.to_string(), value_style),
                    ]));
                    is_first = false;
                } else {
                    visual_lines.push(Line::from(vec![
                        Span::styled(cont_indent.clone(), Style::default()),
                        Span::styled(chunk.to_string(), value_style),
                    ]));
                }
            }
        }

        if i == popup.selected {
            selected_visual_count = visual_lines.len() - selected_visual_start;
        }
    }

    // Scroll: keep the selected row visible.
    let total_visual = visual_lines.len();
    let scroll_offset = {
        let selected_visual_end = selected_visual_start + selected_visual_count;
        if selected_visual_end > visible_height {
            let ideal = selected_visual_start;
            let max_scroll = total_visual.saturating_sub(visible_height);
            ideal.min(max_scroll)
        } else {
            0
        }
    };

    let lines: Vec<Line> = visual_lines
        .into_iter()
        .skip(scroll_offset)
        .take(visible_height)
        .collect();

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, padded);
}

/// Count how many visual lines the popup rows will occupy at the given width.
fn count_visual_lines(rows: &[(String, String)], inner_width: usize) -> usize {
    let mut count = 0usize;
    for (key, value) in rows {
        let prefix_len = key.len() + 4; // "  " marker + key + ": "
        let first_cap = inner_width.saturating_sub(prefix_len);
        let cont_cap = inner_width.saturating_sub(prefix_len);
        if value.len() <= first_cap || cont_cap == 0 {
            count += 1;
        } else {
            // first line
            count += 1;
            let mut remaining = value.len().saturating_sub(first_cap);
            while remaining > 0 {
                let used = remaining.min(cont_cap);
                count += 1;
                remaining -= used;
            }
        }
    }
    count
}

// ── Confirm Dialog Overlay ──────────────────────────────────────────────

fn draw_confirm_dialog(f: &mut Frame, dialog: &ConfirmDialog, area: Rect) {
    let popup_width = 50u16.min(area.width.saturating_sub(4));
    let popup_height = 7u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_width)) / 2;
    let y = (area.height.saturating_sub(popup_height)) / 2;
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .title(format!(" {} ", dialog.action.title()))
        .title_alignment(Alignment::Center)
        .style(Style::default().bg(Color::Black).fg(Color::Yellow));

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            dialog.action.description(),
            Style::default().fg(Color::White),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  y",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Confirm    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "n/Esc",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Cancel", Style::default().fg(Color::DarkGray)),
        ]),
    ];

    let paragraph = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, popup_area);
}
