//! Application state and main event loop for the TUI.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::Backend;
use ratatui::Terminal;

use super::event::{Event, EventHandler};
use super::tabs::channels::ChannelView;
use super::tabs::invoices::InvoiceView;
use super::tabs::payments::PaymentView;
use super::tabs::peers::PeerView;
use super::tabs::{
    ChannelsTab, DashboardTab, GraphTab, InvoicesTab, LogsTab, PaymentsTab, PeersTab, TabKind,
};
use super::ui;
use crate::rpc_client::RpcClient;

use copypasta::{ClipboardContext, ClipboardProvider};

use fiber_json_types::{Hash256, NodeInfoResult, Pubkey};

/// How often to poll for keyboard events (milliseconds).
const EVENT_POLL_MS: u64 = 100;

/// How often to refresh data from the RPC (seconds).
const DATA_REFRESH_SECS: u64 = 5;

/// A detail popup overlay showing key-value rows for a selected item.
#[derive(Debug, Clone)]
pub struct DetailPopup {
    pub title: String,
    pub rows: Vec<(String, String)>,
    /// Index of the highlighted row (for selective copy).
    pub selected: usize,
}

impl DetailPopup {
    pub fn new(title: impl Into<String>, rows: Vec<(String, String)>) -> Self {
        Self {
            title: title.into(),
            rows,
            selected: 0,
        }
    }
}

/// Actions that require user confirmation before execution.
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    /// Shutdown a channel (cooperative close).
    ShutdownChannel { channel_id: Hash256 },
    /// Abandon a channel (force drop).
    AbandonChannel { channel_id: Hash256 },
    /// Disconnect a peer.
    DisconnectPeer { pubkey: Pubkey },
}

impl ConfirmAction {
    /// Human-readable description for the confirmation popup.
    pub fn description(&self) -> String {
        match self {
            ConfirmAction::ShutdownChannel { channel_id } => {
                format!("Shutdown channel {}?", channel_id)
            }
            ConfirmAction::AbandonChannel { channel_id } => {
                format!("Abandon channel {}?", channel_id)
            }
            ConfirmAction::DisconnectPeer { pubkey } => {
                format!("Disconnect peer {}?", pubkey)
            }
        }
    }

    /// Title for the confirmation popup.
    pub fn title(&self) -> &'static str {
        match self {
            ConfirmAction::ShutdownChannel { .. } => "Confirm Shutdown",
            ConfirmAction::AbandonChannel { .. } => "Confirm Abandon",
            ConfirmAction::DisconnectPeer { .. } => "Confirm Disconnect",
        }
    }
}

/// A pending confirmation dialog.
#[derive(Debug, Clone)]
pub struct ConfirmDialog {
    pub action: ConfirmAction,
}

/// Which tab is currently active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTab {
    Dashboard,
    Channels,
    Payments,
    Peers,
    Invoices,
    Graph,
    Logs,
}

impl ActiveTab {
    pub fn all() -> &'static [ActiveTab] {
        &[
            ActiveTab::Dashboard,
            ActiveTab::Channels,
            ActiveTab::Payments,
            ActiveTab::Peers,
            ActiveTab::Invoices,
            ActiveTab::Graph,
            ActiveTab::Logs,
        ]
    }

    pub fn label(&self) -> &'static str {
        match self {
            ActiveTab::Dashboard => "Dashboard",
            ActiveTab::Channels => "Channels",
            ActiveTab::Payments => "Payments",
            ActiveTab::Peers => "Peers",
            ActiveTab::Invoices => "Invoices",
            ActiveTab::Graph => "Graph",
            ActiveTab::Logs => "Logs",
        }
    }

    pub fn index(&self) -> usize {
        match self {
            ActiveTab::Dashboard => 0,
            ActiveTab::Channels => 1,
            ActiveTab::Payments => 2,
            ActiveTab::Peers => 3,
            ActiveTab::Invoices => 4,
            ActiveTab::Graph => 5,
            ActiveTab::Logs => 6,
        }
    }

    pub fn from_index(idx: usize) -> Self {
        match idx {
            0 => ActiveTab::Dashboard,
            1 => ActiveTab::Channels,
            2 => ActiveTab::Payments,
            3 => ActiveTab::Peers,
            4 => ActiveTab::Invoices,
            5 => ActiveTab::Graph,
            6 => ActiveTab::Logs,
            _ => ActiveTab::Dashboard,
        }
    }
}

/// The focus mode of the app -- normal browsing, or editing a form field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Normal mode: keyboard shortcuts navigate tabs, select items, etc.
    Normal,
    /// Editing mode: keystrokes go to the active input field.
    Editing,
}

/// How long a flash message stays visible in the footer (seconds).
const FLASH_DURATION_SECS: u64 = 5;

/// Main application state.
pub struct App {
    pub client: RpcClient,
    pub should_quit: bool,
    pub active_tab: ActiveTab,
    pub input_mode: InputMode,

    // Node info (always shown in header)
    pub node_info: Option<NodeInfoResult>,
    pub node_info_error: Option<String>,

    // Per-tab state
    pub dashboard_tab: DashboardTab,
    pub channels_tab: ChannelsTab,
    pub payments_tab: PaymentsTab,
    pub peers_tab: PeersTab,
    pub invoices_tab: InvoicesTab,
    pub graph_tab: GraphTab,
    pub logs_tab: LogsTab,

    // Show help overlay
    pub show_help: bool,

    // Confirmation dialog (shown as popup)
    pub confirm_dialog: Option<ConfirmDialog>,

    // Detail popup overlay (shown on top of list content)
    pub detail_popup: Option<DetailPopup>,

    // Flash message shown in footer (message, is_error, timestamp)
    pub flash_message: Option<(String, bool, Instant)>,

    // Last data refresh time
    pub last_refresh: Instant,
}

impl App {
    pub fn new(client: RpcClient) -> Self {
        Self {
            client,
            should_quit: false,
            active_tab: ActiveTab::Dashboard,
            input_mode: InputMode::Normal,
            node_info: None,
            node_info_error: None,
            dashboard_tab: DashboardTab::new(),
            channels_tab: ChannelsTab::new(),
            payments_tab: PaymentsTab::new(),
            peers_tab: PeersTab::new(),
            invoices_tab: InvoicesTab::new(),
            graph_tab: GraphTab::new(),
            logs_tab: LogsTab::new(),
            show_help: false,
            confirm_dialog: None,
            detail_popup: None,
            flash_message: None,
            last_refresh: Instant::now() - Duration::from_secs(DATA_REFRESH_SECS + 1),
        }
    }

    /// Main event loop.
    pub async fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        let event_handler = EventHandler::new(Duration::from_millis(EVENT_POLL_MS));

        loop {
            // Draw the UI (pass &mut self so stateful widgets can update)
            terminal.draw(|f| ui::draw(f, self))?;

            // Handle events
            match event_handler.poll_event()? {
                Event::Key(key) => self.handle_key(key).await,
                Event::Tick => {
                    // Auto-clear expired flash message
                    if let Some((_, _, when)) = &self.flash_message {
                        if when.elapsed() >= Duration::from_secs(FLASH_DURATION_SECS) {
                            self.flash_message = None;
                        }
                    }
                    // Auto-refresh data periodically
                    if self.last_refresh.elapsed() >= Duration::from_secs(DATA_REFRESH_SECS) {
                        self.fetch_all_data().await;
                        self.last_refresh = Instant::now();
                    }
                }
                Event::Resize => {
                    // Terminal will re-draw on next iteration
                }
            }

            if self.should_quit {
                return Ok(());
            }
        }
    }

    /// Handle a key event.
    async fn handle_key(&mut self, key: KeyEvent) {
        // Global shortcuts (work in all modes)
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        // If a confirmation dialog is active, handle y/n/Esc
        if self.confirm_dialog.is_some() {
            self.handle_confirm_key(key).await;
            return;
        }

        // If a detail popup is active, handle navigation/copy/close
        if self.detail_popup.is_some() {
            self.handle_detail_popup_key(key);
            return;
        }

        if self.input_mode == InputMode::Editing {
            self.handle_editing_key(key).await;
            return;
        }

        // Normal mode shortcuts
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.show_help = !self.show_help;
            }
            KeyCode::Esc => {
                if self.show_help {
                    self.show_help = false;
                } else {
                    // Pass to active tab
                    self.handle_tab_key(key).await;
                }
            }
            // Tab switching: number keys 1-7
            KeyCode::Char('1') => self.active_tab = ActiveTab::Dashboard,
            KeyCode::Char('2') => self.active_tab = ActiveTab::Channels,
            KeyCode::Char('3') => self.active_tab = ActiveTab::Payments,
            KeyCode::Char('4') => self.active_tab = ActiveTab::Peers,
            KeyCode::Char('5') => self.active_tab = ActiveTab::Invoices,
            KeyCode::Char('6') => self.active_tab = ActiveTab::Graph,
            KeyCode::Char('7') => self.active_tab = ActiveTab::Logs,
            // Tab switching: Tab/BackTab
            KeyCode::Tab => {
                let idx = self.active_tab.index();
                let next = (idx + 1) % ActiveTab::all().len();
                self.active_tab = ActiveTab::from_index(next);
            }
            KeyCode::BackTab => {
                let idx = self.active_tab.index();
                let prev = if idx == 0 {
                    ActiveTab::all().len() - 1
                } else {
                    idx - 1
                };
                self.active_tab = ActiveTab::from_index(prev);
            }
            // Refresh data
            KeyCode::Char('r') => {
                self.fetch_all_data().await;
                self.last_refresh = Instant::now();
            }
            // Copy primary identifier of selected item to clipboard
            KeyCode::Char('y') => {
                self.copy_to_clipboard();
            }
            // Pass to active tab
            _ => {
                self.handle_tab_key(key).await;
            }
        }
    }

    /// Handle key events in editing mode.
    async fn handle_editing_key(&mut self, key: KeyEvent) {
        // Pass to active tab's form handler (including Esc).
        match self.active_tab {
            ActiveTab::Peers => {
                self.peers_tab.handle_editing_key(key, &self.client).await;
                self.propagate_status(&ActiveTab::Peers);
                if self.peers_tab.should_exit_editing() {
                    self.input_mode = InputMode::Normal;
                }
            }
            ActiveTab::Invoices => {
                self.invoices_tab
                    .handle_editing_key(key, &self.client)
                    .await;
                self.propagate_status(&ActiveTab::Invoices);
                if self.invoices_tab.should_exit_editing() {
                    self.input_mode = InputMode::Normal;
                }
            }
            ActiveTab::Payments => {
                self.payments_tab
                    .handle_editing_key(key, &self.client)
                    .await;
                self.propagate_status(&ActiveTab::Payments);
                if self.payments_tab.should_exit_editing() {
                    self.input_mode = InputMode::Normal;
                }
            }
            ActiveTab::Channels => {
                self.channels_tab
                    .handle_editing_key(key, &self.client)
                    .await;
                self.propagate_status(&ActiveTab::Channels);
                if self.channels_tab.should_exit_editing() {
                    self.input_mode = InputMode::Normal;
                }
            }
            _ => {
                // Other tabs don't have editing mode
                self.input_mode = InputMode::Normal;
            }
        }
    }

    /// Pass key events to the active tab.
    async fn handle_tab_key(&mut self, key: KeyEvent) {
        // Intercept Enter to open detail popup when in list view
        if key.code == KeyCode::Enter && self.try_open_detail_popup() {
            return;
        }

        match self.active_tab {
            ActiveTab::Dashboard => {
                // Dashboard is read-only, no key handling needed
            }
            ActiveTab::Channels => {
                if let Some(kind) = self.channels_tab.handle_key(key, &self.client).await {
                    match kind {
                        TabKind::EnterEditing => {
                            self.input_mode = InputMode::Editing;
                        }
                        TabKind::RequestConfirm => {
                            // Build confirm dialog from channel pending action
                            if let Some(ref action) = self.channels_tab.pending_action {
                                let confirm_action = match action {
                                    super::tabs::channels::ChannelPendingAction::Shutdown {
                                        channel_id,
                                    } => ConfirmAction::ShutdownChannel {
                                        channel_id: *channel_id,
                                    },
                                    super::tabs::channels::ChannelPendingAction::Abandon {
                                        channel_id,
                                    } => ConfirmAction::AbandonChannel {
                                        channel_id: *channel_id,
                                    },
                                };
                                self.confirm_dialog = Some(ConfirmDialog {
                                    action: confirm_action,
                                });
                            }
                        }
                    }
                }
                self.propagate_status(&ActiveTab::Channels);
            }
            ActiveTab::Payments => {
                if let Some(kind) = self.payments_tab.handle_key(key, &self.client).await {
                    if kind == TabKind::EnterEditing {
                        self.input_mode = InputMode::Editing;
                    }
                }
                self.propagate_status(&ActiveTab::Payments);
            }
            ActiveTab::Peers => {
                if let Some(kind) = self.peers_tab.handle_key(key, &self.client).await {
                    match kind {
                        TabKind::EnterEditing => {
                            self.input_mode = InputMode::Editing;
                        }
                        TabKind::RequestConfirm => {
                            // Build confirm dialog from peer pending action
                            if let Some(pubkey) = self.peers_tab.pending_disconnect {
                                self.confirm_dialog = Some(ConfirmDialog {
                                    action: ConfirmAction::DisconnectPeer { pubkey },
                                });
                            }
                        }
                    }
                }
                self.propagate_status(&ActiveTab::Peers);
            }
            ActiveTab::Invoices => {
                if let Some(kind) = self.invoices_tab.handle_key(key, &self.client).await {
                    if kind == TabKind::EnterEditing {
                        self.input_mode = InputMode::Editing;
                    }
                }
                self.propagate_status(&ActiveTab::Invoices);
            }
            ActiveTab::Graph => {
                self.graph_tab.handle_key(key, &self.client).await;
            }
            ActiveTab::Logs => {
                self.logs_tab.handle_key(key);
            }
        }
    }

    /// Try to open a detail popup for the currently selected item.
    /// Returns true if a popup was opened (Enter should be consumed).
    fn try_open_detail_popup(&mut self) -> bool {
        match self.active_tab {
            ActiveTab::Channels => {
                if self.channels_tab.view != ChannelView::List {
                    return false;
                }
                let sel = match self.channels_tab.table_state.selected() {
                    Some(s) => s,
                    None => return false,
                };
                let ch = match self.channels_tab.channels.get(sel) {
                    Some(c) => c,
                    None => return false,
                };
                let state = ChannelsTab::state_name(ch);
                let rows = vec![
                    ("Channel ID".to_string(), format!("{}", ch.channel_id)),
                    ("State".to_string(), state.to_string()),
                    ("Peer Pubkey".to_string(), format!("{}", ch.pubkey)),
                    (
                        "Public".to_string(),
                        if ch.is_public { "Yes" } else { "No" }.to_string(),
                    ),
                    (
                        "One-Way".to_string(),
                        if ch.is_one_way { "Yes" } else { "No" }.to_string(),
                    ),
                    (
                        "Acceptor".to_string(),
                        if ch.is_acceptor { "Yes" } else { "No" }.to_string(),
                    ),
                    (
                        "Enabled".to_string(),
                        if ch.enabled { "Yes" } else { "No" }.to_string(),
                    ),
                    (
                        "Local Balance".to_string(),
                        ui::format_ckb_pub(ch.local_balance),
                    ),
                    (
                        "Remote Balance".to_string(),
                        ui::format_ckb_pub(ch.remote_balance),
                    ),
                    (
                        "Offered TLC Balance".to_string(),
                        ui::format_ckb_pub(ch.offered_tlc_balance),
                    ),
                    (
                        "Received TLC Balance".to_string(),
                        ui::format_ckb_pub(ch.received_tlc_balance),
                    ),
                    (
                        "Pending TLCs".to_string(),
                        format!("{}", ch.pending_tlcs.len()),
                    ),
                    (
                        "TLC Expiry Delta".to_string(),
                        format!("{}", ch.tlc_expiry_delta),
                    ),
                    (
                        "TLC Fee Rate".to_string(),
                        format!(
                            "{}ppm ({}%)",
                            ch.tlc_fee_proportional_millionths,
                            ch.tlc_fee_proportional_millionths as f64 / 10_000.0
                        ),
                    ),
                    (
                        "Created".to_string(),
                        ui::format_timestamp_pub(ch.created_at),
                    ),
                    (
                        "Channel Outpoint".to_string(),
                        ch.channel_outpoint
                            .as_ref()
                            .map(|op| format!("{}", op))
                            .unwrap_or_else(|| "N/A".to_string()),
                    ),
                    (
                        "Funding UDT Script".to_string(),
                        ch.funding_udt_type_script
                            .as_ref()
                            .map(|s| format!("{:?}", s))
                            .unwrap_or_else(|| "N/A".to_string()),
                    ),
                    (
                        "Latest Commitment Tx".to_string(),
                        ch.latest_commitment_transaction_hash
                            .as_ref()
                            .map(|h| format!("{:#x}", h))
                            .unwrap_or_else(|| "N/A".to_string()),
                    ),
                    (
                        "Shutdown Tx".to_string(),
                        ch.shutdown_transaction_hash
                            .as_ref()
                            .map(|h| format!("{:#x}", h))
                            .unwrap_or_else(|| "N/A".to_string()),
                    ),
                    (
                        "Failure Detail".to_string(),
                        ch.failure_detail
                            .clone()
                            .unwrap_or_else(|| "N/A".to_string()),
                    ),
                ];
                self.detail_popup = Some(DetailPopup::new("Channel Detail", rows));
                true
            }
            ActiveTab::Payments => {
                if self.payments_tab.view != PaymentView::List {
                    return false;
                }
                let sel = match self.payments_tab.table_state.selected() {
                    Some(s) => s,
                    None => return false,
                };
                let p = match self.payments_tab.payments.get(sel) {
                    Some(p) => p,
                    None => return false,
                };
                let status = PaymentsTab::status_name(p);
                let mut rows = vec![
                    ("Payment Hash".to_string(), format!("{}", p.payment_hash)),
                    ("Status".to_string(), status.to_string()),
                    ("Fee".to_string(), ui::format_ckb_pub(p.fee)),
                    (
                        "Created".to_string(),
                        ui::format_timestamp_pub(p.created_at),
                    ),
                    (
                        "Updated".to_string(),
                        ui::format_timestamp_pub(p.last_updated_at),
                    ),
                ];
                if let Some(ref err) = p.failed_error {
                    rows.push(("Error".to_string(), err.clone()));
                }
                // Custom records
                if let Some(ref records) = p.custom_records {
                    if records.data.is_empty() {
                        rows.push(("Custom Records".to_string(), "(none)".to_string()));
                    } else {
                        rows.push((
                            "Custom Records".to_string(),
                            format!("{} entries", records.data.len()),
                        ));
                        for (k, v) in &records.data {
                            rows.push((
                                format!("  Record 0x{:x}", k),
                                format!(
                                    "0x{}",
                                    v.iter().map(|b| format!("{:02x}", b)).collect::<String>()
                                ),
                            ));
                        }
                    }
                }
                // Routers (debug builds only)
                #[cfg(debug_assertions)]
                {
                    if p.routers.is_empty() {
                        rows.push(("Routes".to_string(), "(none)".to_string()));
                    } else {
                        rows.push((
                            "Routes".to_string(),
                            format!("{} route(s)", p.routers.len()),
                        ));
                        for (ri, route) in p.routers.iter().enumerate() {
                            for (ni, node) in route.nodes.iter().enumerate() {
                                let label = format!("  Route {} Hop {}", ri + 1, ni + 1);
                                rows.push((
                                    label,
                                    format!(
                                        "{} amt={} ch={}",
                                        node.pubkey, node.amount, node.channel_outpoint
                                    ),
                                ));
                            }
                        }
                    }
                }
                self.detail_popup = Some(DetailPopup::new("Payment Detail", rows));
                true
            }
            ActiveTab::Invoices => {
                if self.invoices_tab.view != InvoiceView::Main {
                    return false;
                }
                let sel = match self.invoices_tab.list_state.selected() {
                    Some(s) => s,
                    None => return false,
                };
                let inv = match self.invoices_tab.invoices.get(sel) {
                    Some(i) => i,
                    None => return false,
                };
                let status = InvoicesTab::status_name(&inv.status);
                let amount = inv
                    .invoice
                    .amount
                    .map(ui::format_ckb_pub)
                    .unwrap_or_else(|| "N/A".to_string());
                let mut rows = vec![
                    ("Address".to_string(), inv.invoice_address.clone()),
                    ("Status".to_string(), status.to_string()),
                    (
                        "Currency".to_string(),
                        format!("{:?}", inv.invoice.currency),
                    ),
                    ("Amount".to_string(), amount),
                    (
                        "Payment Hash".to_string(),
                        format!("{}", inv.invoice.data.payment_hash),
                    ),
                    (
                        "Timestamp".to_string(),
                        ui::format_timestamp_pub(inv.invoice.data.timestamp as u64),
                    ),
                ];
                // Add attributes
                for attr in &inv.invoice.data.attrs {
                    let (key, value) = format_invoice_attribute(attr);
                    rows.push((key, value));
                }
                self.detail_popup = Some(DetailPopup::new("Invoice Detail", rows));
                true
            }
            ActiveTab::Peers => {
                if self.peers_tab.view != PeerView::List {
                    return false;
                }
                let sel = match self.peers_tab.table_state.selected() {
                    Some(s) => s,
                    None => return false,
                };
                let peer = match self.peers_tab.peers.get(sel) {
                    Some(p) => p,
                    None => return false,
                };
                let mut rows = vec![
                    ("Pubkey".to_string(), format!("{}", peer.pubkey)),
                    ("Address".to_string(), peer.address.clone()),
                ];

                // Enrich with graph node info (name, version, all addresses, etc.)
                let peer_pk = format!("{}", peer.pubkey);
                if let Some(node) = self
                    .graph_tab
                    .nodes
                    .iter()
                    .find(|n| format!("{}", n.pubkey) == peer_pk)
                {
                    rows.push(("--- Graph Node Info ---".to_string(), String::new()));
                    rows.push((
                        "Node Name".to_string(),
                        if node.node_name.is_empty() {
                            "(unnamed)".to_string()
                        } else {
                            node.node_name.clone()
                        },
                    ));
                    rows.push(("Version".to_string(), node.version.clone()));
                    rows.push(("All Addresses".to_string(), node.addresses.join(", ")));
                    rows.push(("Features".to_string(), node.features.join(", ")));
                    rows.push((
                        "Timestamp".to_string(),
                        ui::format_timestamp_pub(node.timestamp),
                    ));
                    rows.push(("Chain Hash".to_string(), format!("{}", node.chain_hash)));
                    rows.push((
                        "Auto-Accept Min CKB".to_string(),
                        ui::format_ckb_pub(node.auto_accept_min_ckb_funding_amount as u128),
                    ));
                }

                // Enrich with channel statistics for this peer
                let peer_channels: Vec<_> = self
                    .channels_tab
                    .channels
                    .iter()
                    .filter(|ch| format!("{}", ch.pubkey) == peer_pk)
                    .collect();
                if !peer_channels.is_empty() {
                    let total_local: u128 = peer_channels.iter().map(|ch| ch.local_balance).sum();
                    let total_remote: u128 = peer_channels.iter().map(|ch| ch.remote_balance).sum();
                    let total_capacity = total_local + total_remote;
                    let ready_count = peer_channels
                        .iter()
                        .filter(|ch| ChannelsTab::state_name(ch) == "Ready")
                        .count();

                    rows.push(("--- Channels ---".to_string(), String::new()));
                    rows.push((
                        "Total Channels".to_string(),
                        format!("{}", peer_channels.len()),
                    ));
                    rows.push(("Ready Channels".to_string(), format!("{}", ready_count)));
                    rows.push((
                        "Total Capacity".to_string(),
                        ui::format_ckb_pub(total_capacity),
                    ));
                    rows.push(("Local Balance".to_string(), ui::format_ckb_pub(total_local)));
                    rows.push((
                        "Remote Balance".to_string(),
                        ui::format_ckb_pub(total_remote),
                    ));
                } else {
                    rows.push(("--- Channels ---".to_string(), String::new()));
                    rows.push(("Total Channels".to_string(), "0".to_string()));
                }

                self.detail_popup = Some(DetailPopup::new("Peer Detail", rows));
                true
            }
            ActiveTab::Graph => {
                use super::tabs::graph::GraphView;
                match self.graph_tab.view {
                    GraphView::Nodes => {
                        let sel = match self.graph_tab.nodes_table_state.selected() {
                            Some(s) => s,
                            None => return false,
                        };
                        let node = match self.graph_tab.nodes.get(sel) {
                            Some(n) => n,
                            None => return false,
                        };
                        let rows = vec![
                            (
                                "Name".to_string(),
                                if node.node_name.is_empty() {
                                    "(unnamed)".to_string()
                                } else {
                                    node.node_name.clone()
                                },
                            ),
                            ("Pubkey".to_string(), format!("{}", node.pubkey)),
                            ("Version".to_string(), node.version.clone()),
                            ("Addresses".to_string(), node.addresses.join(", ")),
                            ("Features".to_string(), node.features.join(", ")),
                            (
                                "Timestamp".to_string(),
                                ui::format_timestamp_pub(node.timestamp),
                            ),
                            ("Chain Hash".to_string(), format!("{}", node.chain_hash)),
                            (
                                "Auto-Accept Min CKB".to_string(),
                                ui::format_ckb_pub(node.auto_accept_min_ckb_funding_amount as u128),
                            ),
                            (
                                "UDT Configs".to_string(),
                                format!("{:?}", node.udt_cfg_infos),
                            ),
                        ];
                        self.detail_popup = Some(DetailPopup::new("Graph Node Detail", rows));
                        true
                    }
                    GraphView::Channels => {
                        let sel = match self.graph_tab.channels_table_state.selected() {
                            Some(s) => s,
                            None => return false,
                        };
                        let ch = match self.graph_tab.channels.get(sel) {
                            Some(c) => c,
                            None => return false,
                        };
                        let mut rows = vec![
                            (
                                "Channel Outpoint".to_string(),
                                format!("{}", ch.channel_outpoint),
                            ),
                            ("Node 1".to_string(), format!("{}", ch.node1)),
                            ("Node 2".to_string(), format!("{}", ch.node2)),
                            ("Capacity".to_string(), ui::format_ckb_pub(ch.capacity)),
                            (
                                "Created".to_string(),
                                ui::format_timestamp_pub(ch.created_timestamp),
                            ),
                            ("Chain Hash".to_string(), format!("{}", ch.chain_hash)),
                            (
                                "UDT Script".to_string(),
                                ch.udt_type_script
                                    .as_ref()
                                    .map(|s| format!("{:?}", s))
                                    .unwrap_or_else(|| "N/A".to_string()),
                            ),
                        ];
                        // Add update info for node1
                        if let Some(ref info) = ch.update_info_of_node1 {
                            rows.push(("--- Node 1 Update ---".to_string(), String::new()));
                            append_channel_update_rows(&mut rows, info);
                        }
                        // Add update info for node2
                        if let Some(ref info) = ch.update_info_of_node2 {
                            rows.push(("--- Node 2 Update ---".to_string(), String::new()));
                            append_channel_update_rows(&mut rows, info);
                        }
                        self.detail_popup = Some(DetailPopup::new("Graph Channel Detail", rows));
                        true
                    }
                }
            }
            _ => false,
        }
    }

    /// Handle key events when a confirmation dialog is active.
    async fn handle_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                // Execute the confirmed action
                if let Some(dialog) = self.confirm_dialog.take() {
                    match dialog.action {
                        ConfirmAction::ShutdownChannel { .. }
                        | ConfirmAction::AbandonChannel { .. } => {
                            self.channels_tab.execute_confirmed(&self.client).await;
                            self.propagate_status(&ActiveTab::Channels);
                        }
                        ConfirmAction::DisconnectPeer { .. } => {
                            self.peers_tab.execute_confirmed(&self.client).await;
                            self.propagate_status(&ActiveTab::Peers);
                        }
                    }
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                // Cancel — clear the dialog and the pending action
                self.confirm_dialog = None;
                self.channels_tab.pending_action = None;
                self.peers_tab.pending_disconnect = None;
            }
            _ => {
                // Ignore other keys while dialog is shown
            }
        }
    }

    /// Handle key events when a detail popup is active.
    fn handle_detail_popup_key(&mut self, key: KeyEvent) {
        let popup = match self.detail_popup.as_mut() {
            Some(p) => p,
            None => return,
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.detail_popup = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !popup.rows.is_empty() && popup.selected < popup.rows.len() - 1 {
                    popup.selected += 1;
                    // Auto-scroll: keep selected row visible
                    // We approximate visible height later; just track selected here.
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if popup.selected > 0 {
                    popup.selected -= 1;
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                popup.selected = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !popup.rows.is_empty() {
                    popup.selected = popup.rows.len() - 1;
                }
            }
            KeyCode::Char('y') => {
                // Copy the value of the selected row to clipboard
                if let Some((_key, value)) = popup.rows.get(popup.selected) {
                    let value = value.clone();
                    match ClipboardContext::new() {
                        Ok(mut ctx) => match ctx.set_contents(value.clone()) {
                            Ok(()) => {
                                let short = if value.len() > 24 {
                                    format!("{}...", &value[..24])
                                } else {
                                    value
                                };
                                self.logs_tab
                                    .add_info(&format!("[Clipboard] Copied: {}", short));
                            }
                            Err(e) => {
                                self.logs_tab
                                    .add_error(&format!("[Clipboard] Failed to copy: {}", e));
                            }
                        },
                        Err(e) => {
                            self.logs_tab
                                .add_error(&format!("[Clipboard] Clipboard unavailable: {}", e));
                        }
                    }
                }
            }
            KeyCode::Char('u') => {
                // If we are on the Channels tab, allow 'u' to open the update form
                if self.active_tab == ActiveTab::Channels {
                    self.detail_popup = None;
                    self.channels_tab.init_update_form();
                    if self.channels_tab.view == ChannelView::UpdateForm {
                        self.input_mode = InputMode::Editing;
                    }
                }
            }
            _ => {}
        }
    }

    /// Get the copyable identifier string from the currently focused item.
    fn get_copyable_value(&self) -> Option<String> {
        match self.active_tab {
            ActiveTab::Channels => {
                let sel = self.channels_tab.table_state.selected()?;
                let ch = self.channels_tab.channels.get(sel)?;
                match self.channels_tab.view {
                    ChannelView::List => Some(format!("{}", ch.channel_id)),
                    _ => None,
                }
            }
            ActiveTab::Payments => {
                let sel = self.payments_tab.table_state.selected()?;
                let p = self.payments_tab.payments.get(sel)?;
                match self.payments_tab.view {
                    PaymentView::List => Some(format!("{}", p.payment_hash)),
                    _ => None,
                }
            }
            ActiveTab::Peers => {
                if self.peers_tab.view != PeerView::List {
                    return None;
                }
                let sel = self.peers_tab.table_state.selected()?;
                let peer = self.peers_tab.peers.get(sel)?;
                Some(format!("{}", peer.pubkey))
            }
            ActiveTab::Invoices => match self.invoices_tab.view {
                InvoiceView::Main => {
                    let sel = self.invoices_tab.list_state.selected()?;
                    let inv = self.invoices_tab.invoices.get(sel)?;
                    Some(inv.invoice_address.clone())
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Copy the current item's primary identifier to the system clipboard.
    fn copy_to_clipboard(&mut self) {
        let value = match self.get_copyable_value() {
            Some(v) => v,
            None => {
                self.logs_tab
                    .add_info("[Clipboard] Nothing to copy from current view");
                return;
            }
        };

        match ClipboardContext::new() {
            Ok(mut ctx) => match ctx.set_contents(value.clone()) {
                Ok(()) => {
                    let short = if value.len() > 24 {
                        format!("{}...", &value[..24])
                    } else {
                        value
                    };
                    self.logs_tab
                        .add_info(&format!("[Clipboard] Copied: {}", short));
                }
                Err(e) => {
                    self.logs_tab
                        .add_error(&format!("[Clipboard] Failed to copy: {}", e));
                }
            },
            Err(e) => {
                self.logs_tab
                    .add_error(&format!("[Clipboard] Clipboard unavailable: {}", e));
            }
        }
    }

    /// Check if a tab has a new status message and propagate it to the Logs tab.
    fn propagate_status(&mut self, tab: &ActiveTab) {
        let status = match tab {
            ActiveTab::Channels => self.channels_tab.status_message.take(),
            ActiveTab::Payments => self.payments_tab.status_message.take(),
            ActiveTab::Peers => self.peers_tab.status_message.take(),
            ActiveTab::Invoices => self.invoices_tab.status_message.take(),
            ActiveTab::Dashboard | ActiveTab::Graph | ActiveTab::Logs => None,
        };
        if let Some(msg) = status {
            let prefix = tab.label();
            let formatted = format!("[{}] {}", prefix, msg);
            let is_error =
                msg.contains("failed") || msg.contains("Failed") || msg.contains("Error");
            let is_warn =
                msg.contains("Invalid") || msg.contains("required") || msg.contains("couldn't");
            if is_error {
                self.logs_tab.add_error(&formatted);
            } else if is_warn {
                self.logs_tab.add_warn(&formatted);
            } else {
                self.logs_tab.add_info(&formatted);
            }
            // Show flash message in footer so user sees feedback on any tab view
            self.flash_message = Some((formatted, is_error || is_warn, Instant::now()));
        }
    }

    /// Fetch all data from the RPC endpoint.
    pub async fn fetch_all_data(&mut self) {
        // Fetch node info
        match self
            .client
            .call_typed_no_params::<NodeInfoResult>("node_info")
            .await
        {
            Ok(info) => {
                self.node_info = Some(info);
                self.node_info_error = None;
            }
            Err(e) => {
                let msg = e.to_string();
                if self.node_info_error.as_deref() != Some(&msg) {
                    self.logs_tab
                        .add_error(&format!("[Node] Connection error: {}", msg));
                }
                self.node_info_error = Some(msg);
            }
        }

        // Fetch per-tab data
        self.channels_tab.fetch_data(&self.client).await;
        self.payments_tab.fetch_data(&self.client).await;
        self.invoices_tab.fetch_data(&self.client).await;
        self.peers_tab.fetch_data(&self.client).await;
        self.graph_tab.fetch_data(&self.client).await;

        // Update dashboard aggregate stats from fetched channel data
        self.dashboard_tab
            .update_stats(&self.channels_tab.channels, self.node_info.as_ref());
        let own_pubkey = self
            .node_info
            .as_ref()
            .map(|info| format!("{}", info.pubkey));
        self.dashboard_tab.update_network_stats(
            &self.graph_tab.nodes,
            &self.graph_tab.channels,
            own_pubkey.as_deref(),
        );
    }
}

/// Format an invoice attribute into a (key, value) pair for the detail popup.
fn format_invoice_attribute(attr: &fiber_json_types::Attribute) -> (String, String) {
    match attr {
        fiber_json_types::Attribute::Description(d) => ("Description".to_string(), d.clone()),
        fiber_json_types::Attribute::PayeePublicKey(pk) => {
            ("Payee Public Key".to_string(), format!("{}", pk))
        }
        fiber_json_types::Attribute::ExpiryTime(d) => {
            ("Expiry Time".to_string(), format!("{}s", d.as_secs()))
        }
        fiber_json_types::Attribute::FallbackAddr(addr) => {
            ("Fallback Address".to_string(), addr.clone())
        }
        fiber_json_types::Attribute::UdtScript(s) => ("UDT Script".to_string(), s.clone()),
        fiber_json_types::Attribute::HashAlgorithm(alg) => {
            ("Hash Algorithm".to_string(), format!("{:?}", alg))
        }
        fiber_json_types::Attribute::Feature(features) => {
            ("Features".to_string(), features.join(", "))
        }
        fiber_json_types::Attribute::FinalHtlcTimeout(t) => {
            ("Final HTLC Timeout".to_string(), format!("{}", t))
        }
        fiber_json_types::Attribute::FinalHtlcMinimumExpiryDelta(d) => {
            ("Final HTLC Min Expiry Delta".to_string(), format!("{}", d))
        }
        fiber_json_types::Attribute::PaymentSecret(s) => ("Payment Secret".to_string(), s.clone()),
    }
}

/// Append channel update info rows to the given vector.
fn append_channel_update_rows(
    rows: &mut Vec<(String, String)>,
    info: &fiber_json_types::ChannelUpdateInfo,
) {
    rows.push((
        "  Timestamp".to_string(),
        ui::format_timestamp_pub(info.timestamp),
    ));
    rows.push((
        "  Enabled".to_string(),
        if info.enabled { "Yes" } else { "No" }.to_string(),
    ));
    rows.push((
        "  Outbound Liquidity".to_string(),
        info.outbound_liquidity
            .map(ui::format_ckb_pub)
            .unwrap_or_else(|| "N/A".to_string()),
    ));
    rows.push((
        "  TLC Expiry Delta".to_string(),
        format!("{}", info.tlc_expiry_delta),
    ));
    rows.push((
        "  TLC Minimum Value".to_string(),
        ui::format_ckb_pub(info.tlc_minimum_value),
    ));
    rows.push(("  Fee Rate".to_string(), format!("{}", info.fee_rate)));
}
