use crate::db::SecondaryStore;
use chrono::{Local, TimeZone};
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use fiber::fiber::{channel::ChannelActorState, graph::PaymentSession, types::Pubkey};
use fiber::{
    fiber::{network::PeerInfo, types::NodeAnnouncement},
    Config,
};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::{
    backend::CrosstermBackend,
    widgets::{Block, Borders, List, ListItem},
    Terminal,
};
use ratatui::{layout::Alignment, style::Color};
use ratatui::{
    style::Style,
    text::{Line, Span, Text},
};
use std::{
    collections::HashMap,
    io::{self},
};
use tentacle::secio::PeerId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiView {
    Peers,
    Nodes,
    Channels,
    Payments,
}

struct TuiState {
    peers: Vec<PeerInfo>,
    nodes: Vec<NodeAnnouncement>,
    channels: Vec<ChannelActorState>,
    payment_sessions: Vec<PaymentSession>,
    peer_channels_map: HashMap<PeerId, Vec<ChannelActorState>>,
    view: TuiView,
    selected_idx: usize,
    show_detail: bool,
    list_state: ratatui::widgets::ListState,
}

impl TuiState {
    fn new(config: &Config) -> Result<Self, String> {
        let store_path = config
            .fiber
            .as_ref()
            .ok_or_else(|| "fiber config is required but absent".to_string())?
            .store_path();
        let store = SecondaryStore::new_secondary(store_path);
        let peers: Vec<PeerInfo> = {
            let local_peer_id = config
                .fiber
                .as_ref()
                .map(|f| Pubkey::from(f.public_key()).tentacle_peer_id());
            if let Some(peer_id) = local_peer_id {
                store.list_peers(&peer_id)
            } else {
                Vec::new()
            }
        };
        let nodes: Vec<NodeAnnouncement> = store.list_network_nodes();
        let channels = store.list_direct_channels();
        let payment_sessions = store.list_payment_sessions();
        let mut peer_channels_map = HashMap::new();
        for peer in &peers {
            let peer_pubkey = peer.pubkey;
            let channels: Vec<_> = channels
                .iter()
                .filter(|channel| channel.remote_pubkey == peer_pubkey)
                .cloned()
                .collect();
            peer_channels_map.insert(peer.peer_id.clone(), channels);
        }
        Ok(Self {
            peers,
            nodes,
            channels,
            payment_sessions,
            peer_channels_map,
            view: TuiView::Peers,
            selected_idx: 0,
            show_detail: false,
            list_state: ratatui::widgets::ListState::default(),
        })
    }

    fn get_node_name(&self, peer_id: &PeerId) -> String {
        self.nodes
            .iter()
            .find(|node| node.peer_id() == *peer_id)
            .map(|node| node.node_name.to_string())
            .unwrap_or_else(|| "Unknown".to_string())
    }

    fn get_node_name_by_pubkey(&self, pubkey: &Pubkey) -> String {
        self.nodes
            .iter()
            .find(|node| node.node_id == *pubkey)
            .map(|node| node.node_name.to_string())
            .unwrap_or_else(|| "Unknown".to_string())
    }

    fn session_route(&self, session: &PaymentSession) -> String {
        session
            .route
            .nodes
            .iter()
            .map(|n| self.get_node_name_by_pubkey(&n.pubkey))
            .collect::<Vec<_>>()
            .join(" -> ")
    }

    fn human_time(&self, timestamp: u64) -> String {
        let dt = Local.timestamp_millis_opt(timestamp as i64).unwrap();
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
    }

    fn render_peers(&self) -> Vec<ListItem> {
        self.peers
            .iter()
            .map(|peer| {
                let addr_str = peer
                    .addresses
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let lines = vec![
                    Line::from(vec![Span::styled(
                        self.get_node_name(&peer.peer_id),
                        Style::default().fg(Color::Green),
                    )]),
                    Line::raw(format!("  PeerId: {}", peer.peer_id)),
                    Line::raw(format!("  Addrs: {}", addr_str)),
                    Line::raw(format!(
                        "  Channels: {}",
                        self.peer_channels_map
                            .get(&peer.peer_id)
                            .map(|channels| channels.len())
                            .unwrap_or(0)
                    )),
                ];
                ListItem::new(lines)
            })
            .collect()
    }

    fn render_nodes(&self) -> Vec<ListItem> {
        self.nodes
            .iter()
            .map(|node| {
                let addr_str = node
                    .addresses
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let lines = vec![
                    Line::from(vec![Span::styled(
                        format!("{}", node.node_name),
                        Style::default().fg(Color::Green),
                    )]),
                    Line::raw(format!("  PeerId: {}", node.peer_id())),
                    Line::raw(format!("  Addrs: {}", addr_str)),
                ];
                ListItem::new(lines)
            })
            .collect()
    }

    fn render_channels(&self) -> Vec<ListItem> {
        self.channels
            .iter()
            .map(|ch| {
                let capacity_label = if ch.funding_udt_type_script.is_some() {
                    "UDT Capacity"
                } else {
                    "CKB Capacity"
                };
                let lines = vec![
                    Line::from(vec![Span::styled(
                        format!("ChannelId: {}", ch.id),
                        Style::default().fg(Color::Green),
                    )]),
                    Line::raw(format!(
                        "  Remote Name: {}",
                        self.get_node_name_by_pubkey(&ch.remote_pubkey)
                    )),
                    Line::raw(format!("  Remote Pubkey: {}", ch.remote_pubkey,)),
                    Line::raw(format!("  State: {:?}", ch.state)),
                    Line::raw(format!(
                        "  {}: {}",
                        capacity_label,
                        ch.get_liquid_capacity()
                    )),
                ];
                ListItem::new(lines)
            })
            .collect()
    }

    fn render_payments(&self) -> Vec<ListItem> {
        self.payment_sessions
            .iter()
            .map(|sess| {
                let lines = vec![
                    Line::from(vec![Span::styled(
                        format!("Payment Hash: {}", sess.payment_hash()),
                        Style::default().fg(Color::Green),
                    )]),
                    Line::raw(format!("  Status: {:?}", sess.status)),
                    Line::raw(format!("  CreatedAt: {}", self.human_time(sess.created_at))),
                    Line::raw(format!("  Route: {}", self.session_route(sess))),
                    Line::raw(format!("  Last Error: {:?}", sess.last_error)),
                ];
                ListItem::new(lines)
            })
            .collect()
    }

    fn detail_peers(&self) -> (&'static str, Vec<Line>) {
        let peer = self.peers.get(self.selected_idx).unwrap();
        let peer_channels = self.peer_channels_map.get(&peer.peer_id).unwrap();
        let detail_lines = vec![
            Line::from(vec![
                Span::styled("Pubkey: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", peer.pubkey)),
            ]),
            Line::from(vec![
                Span::styled("PeerId: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", peer.peer_id)),
            ]),
            Line::from(vec![
                Span::styled("Addresses: ", Style::default().fg(Color::Green)),
                Span::raw(
                    peer.addresses
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            ]),
            Line::from(vec![
                Span::styled("Channels: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", peer_channels.len())),
            ]),
        ];
        ("Peer Detail", detail_lines)
    }

    fn detail_nodes(&self) -> (&'static str, Vec<Line>) {
        let node = self.nodes.get(self.selected_idx).unwrap();
        let mut detail_lines = vec![
            Line::from(vec![
                Span::styled("Node Name: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", node.node_name)),
            ]),
            Line::from(vec![
                Span::styled("PeerId: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", node.peer_id())),
            ]),
            Line::from(vec![
                Span::styled("Addresses: ", Style::default().fg(Color::Green)),
                Span::raw(
                    node.addresses
                        .iter()
                        .map(|a| a.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            ]),
            Line::from(vec![
                Span::styled("Chain Hash: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{:?}", node.chain_hash)),
            ]),
            Line::from(vec![
                Span::styled("Timestamp: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", self.human_time(node.timestamp))),
            ]),
            Line::from(vec![
                Span::styled("Auto Accept Min CKB: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", node.auto_accept_min_ckb_funding_amount)),
            ]),
        ];
        let mut udt_lines = vec![];
        let udt_cfg_infos = &node.udt_cfg_infos;
        for (i, udt) in udt_cfg_infos.0.iter().enumerate() {
            if i == 0 {
                udt_lines.push(Line::from(vec![Span::styled(
                    "UDT Cfg Infos:",
                    Style::default().fg(Color::Green),
                )]));
            }
            udt_lines.push(Line::from(vec![
                Span::raw("    - "),
                Span::styled("name: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{:<7}", udt.name)),
                Span::raw("  "),
                Span::styled("hash_type: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{:<7}", format!("{:?}", udt.script.hash_type))),
                Span::raw("  "),
                Span::styled("auto_accept_amount: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{:<15}", format!("{:?}", udt.auto_accept_amount))),
            ]));
        }
        detail_lines.extend(udt_lines);
        ("Node Detail", detail_lines)
    }

    fn detail_channels(&self) -> (&'static str, Vec<Line>) {
        let ch = self.channels.get(self.selected_idx).unwrap();
        let detail_lines = vec![
            Line::from(vec![
                Span::styled("ChannelId: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", ch.id)),
            ]),
            Line::from(vec![
                Span::styled("Remote Pubkey: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", ch.remote_pubkey)),
            ]),
            Line::from(vec![
                Span::styled("State: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{:?}", ch.state)),
            ]),
            Line::from(vec![
                Span::styled("Capacity: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", ch.get_liquid_capacity())),
            ]),
            Line::from(vec![
                Span::styled("Local Balance: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", ch.to_local_amount)),
            ]),
            Line::from(vec![
                Span::styled("Remote Balance: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", ch.to_remote_amount)),
            ]),
            Line::from(vec![
                Span::styled("UDT type: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{:?}", ch.funding_udt_type_script)),
            ]),
        ];
        ("Channel Detail", detail_lines)
    }

    fn detail_sessions(&self) -> (&'static str, Vec<Line>) {
        let sess = self.payment_sessions.get(self.selected_idx).unwrap();
        let detail_lines = vec![
            Line::from(vec![
                Span::styled("Payment Hash: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{}", sess.payment_hash())),
            ]),
            Line::from(vec![
                Span::styled("Status: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{:?}", sess.status)),
            ]),
            Line::from(vec![
                Span::styled("CreatedAt: ", Style::default().fg(Color::Green)),
                Span::raw(self.human_time(sess.created_at)),
            ]),
            Line::from(vec![
                Span::styled("Route: ", Style::default().fg(Color::Green)),
                Span::raw(format!("{:?}", self.session_route(sess))),
            ]),
        ];
        ("Payment Session Detail", detail_lines)
    }

    fn render(&mut self, f: &mut ratatui::Frame) {
        let size = f.size();
        let block = Block::default()
            .title(match self.view {
                TuiView::Payments => format!(
                    "Payment Sessions ({} total) - Press 'p' for peers, 'n' for nodes, 'c' for channels",
                    self.payment_sessions.len()
                ),
                TuiView::Channels => format!(
                    "Channels ({} total) - Press 'p' for peers, 'n' for nodes, 's' for payments",
                    self.channels.len()
                ),
                TuiView::Nodes => format!(
                    "Known Nodes ({} nodes) - Press 'p' for peers, 'c' for channels, 's' for payments",
                    self.nodes.len()
                ),
                TuiView::Peers => format!(
                    "Connected Peers ({} peers) - Press 'n' for nodes, 'c' for channels, 's' for payments",
                    self.peers.len()
                ),
            })
            .borders(Borders::ALL);
        let items: Vec<ListItem> = match self.view {
            TuiView::Payments => self.render_payments(),
            TuiView::Channels => self.render_channels(),
            TuiView::Nodes => self.render_nodes(),
            TuiView::Peers => self.render_peers(),
        };
        let mut list_state = self.list_state.clone();
        list_state.select(Some(self.selected_idx));
        f.render_stateful_widget(
            List::new(items).block(block).highlight_symbol("▶ "),
            size,
            &mut list_state,
        );
        self.list_state = list_state;
        if self.show_detail {
            let (title, detail_lines): (&str, Vec<Line>) = match self.view {
                TuiView::Payments => self.detail_sessions(),
                TuiView::Channels => self.detail_channels(),
                TuiView::Nodes => self.detail_nodes(),
                TuiView::Peers => self.detail_peers(),
            };
            let popup_area = centered_rect(60, 40, size);
            f.render_widget(Clear, popup_area);
            let para = Paragraph::new(Text::from(detail_lines))
                .block(
                    Block::default()
                        .title(title)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Yellow)),
                )
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: true });
            f.render_widget(para, popup_area);
        }
    }

    fn handle_event(&mut self, key: KeyCode) -> bool {
        if self.show_detail {
            match key {
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                    self.show_detail = false;
                }
                _ => {}
            }
        } else {
            match key {
                KeyCode::Char('q') | KeyCode::Esc => return false,
                KeyCode::Char('n') => {
                    self.view = TuiView::Nodes;
                    self.selected_idx = 0;
                }
                KeyCode::Char('p') => {
                    self.view = TuiView::Peers;
                    self.selected_idx = 0;
                }
                KeyCode::Char('c') => {
                    self.view = TuiView::Channels;
                    self.selected_idx = 0;
                }
                KeyCode::Char('s') => {
                    self.view = TuiView::Payments;
                    self.selected_idx = 0;
                }
                KeyCode::Down => {
                    let len = match self.view {
                        TuiView::Payments => self.payment_sessions.len(),
                        TuiView::Channels => self.channels.len(),
                        TuiView::Nodes => self.nodes.len(),
                        TuiView::Peers => self.peers.len(),
                    };
                    if self.selected_idx + 1 < len {
                        self.selected_idx += 1;
                    }
                }
                KeyCode::Up => {
                    self.selected_idx = self.selected_idx.saturating_sub(1);
                }
                KeyCode::Enter => {
                    let len = match self.view {
                        TuiView::Payments => self.payment_sessions.len(),
                        TuiView::Channels => self.channels.len(),
                        TuiView::Nodes => self.nodes.len(),
                        TuiView::Peers => self.peers.len(),
                    };
                    if len > 0 {
                        self.show_detail = true;
                    }
                }
                _ => {}
            }
        }
        true
    }
}

pub fn tui_render(config: &Config) -> Result<(), String> {
    enable_raw_mode().map_err(|e| format!("Failed to enable raw mode: {e}"))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)
        .map_err(|e| format!("Failed to enter alt screen: {e}"))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal =
        Terminal::new(backend).map_err(|e| format!("Failed to create terminal: {e}"))?;
    let mut state = TuiState::new(config)?;
    let res = (|| {
        loop {
            terminal.draw(|f| state.render(f))?;
            if event::poll(std::time::Duration::from_millis(200))? {
                if let Event::Key(key) = event::read()? {
                    if !state.handle_event(key.code) {
                        break;
                    }
                }
            }
        }
        Ok(())
    })();
    disable_raw_mode().ok();
    execute!(io::stdout(), LeaveAlternateScreen).ok();
    res.map_err(|e: std::io::Error| format!("TUI error: {e}"))
}

fn centered_rect(
    percent_x: u16,
    percent_y: u16,
    r: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let popup_layout = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Percentage((100 - percent_y) / 2),
            ratatui::layout::Constraint::Percentage(percent_y),
            ratatui::layout::Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    let vertical = popup_layout[1];
    let popup_layout = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Horizontal)
        .constraints([
            ratatui::layout::Constraint::Percentage((100 - percent_x) / 2),
            ratatui::layout::Constraint::Percentage(percent_x),
            ratatui::layout::Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical);
    popup_layout[1]
}
