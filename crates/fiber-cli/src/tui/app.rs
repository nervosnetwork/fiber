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

    // Last data refresh time
    last_refresh: Instant,
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
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
            }
            _ => {
                // Pass to active tab's form handler
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
        }
    }

    /// Pass key events to the active tab.
    async fn handle_tab_key(&mut self, key: KeyEvent) {
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

    /// Handle key events when a confirmation dialog is active.
    async fn handle_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
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

    /// Get the copyable identifier string from the currently focused item.
    fn get_copyable_value(&self) -> Option<String> {
        match self.active_tab {
            ActiveTab::Channels => {
                let sel = self.channels_tab.table_state.selected()?;
                let ch = self.channels_tab.channels.get(sel)?;
                match self.channels_tab.view {
                    ChannelView::List | ChannelView::Detail => Some(format!("{}", ch.channel_id)),
                    _ => None,
                }
            }
            ActiveTab::Payments => {
                let sel = self.payments_tab.table_state.selected()?;
                let p = self.payments_tab.payments.get(sel)?;
                match self.payments_tab.view {
                    PaymentView::List | PaymentView::Detail => Some(format!("{}", p.payment_hash)),
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
    }
}
