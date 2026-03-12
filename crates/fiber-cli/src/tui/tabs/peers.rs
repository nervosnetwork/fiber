//! Peers tab: list connected peers, connect/disconnect actions.

use crossterm::event::{KeyCode, KeyEvent};
use fiber_json_types::{DisconnectPeerParams, ListPeersResult, PeerInfo, Pubkey};
use ratatui::widgets::TableState;

use super::TabKind;
use crate::rpc_client::RpcClient;

/// View mode for the peers tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerView {
    List,
    ConnectForm,
}

/// Peers tab state.
pub struct PeersTab {
    pub all_peers: Vec<PeerInfo>,
    pub peers: Vec<PeerInfo>,
    pub error: Option<String>,
    pub table_state: TableState,
    pub view: PeerView,
    pub status_message: Option<String>,

    // Search filter (client-side substring match on pubkey/address)
    pub search_query: String,
    pub searching: bool,

    // Pending disconnect action awaiting confirmation
    pub pending_disconnect: Option<Pubkey>,

    // Connect peer form
    pub form_fields: Vec<(String, String)>,
    pub form_selected: usize,
    pub form_editing: bool,
}

impl PeersTab {
    pub fn new() -> Self {
        Self {
            all_peers: Vec::new(),
            peers: Vec::new(),
            error: None,
            table_state: TableState::default(),
            view: PeerView::List,
            status_message: None,
            search_query: String::new(),
            searching: false,
            pending_disconnect: None,
            form_fields: Vec::new(),
            form_selected: 0,
            form_editing: false,
        }
    }

    fn apply_filter(&mut self) {
        if self.search_query.is_empty() {
            self.peers = self.all_peers.clone();
        } else {
            let query = self.search_query.to_lowercase();
            self.peers = self
                .all_peers
                .iter()
                .filter(|p| {
                    let pubkey = format!("{}", p.pubkey).to_lowercase();
                    let addr = p.address.to_lowercase();
                    pubkey.contains(&query) || addr.contains(&query)
                })
                .cloned()
                .collect();
        }
        // Reset selection
        if !self.peers.is_empty() {
            self.table_state.select(Some(0));
        } else {
            self.table_state.select(None);
        }
    }

    pub async fn fetch_data(&mut self, client: &RpcClient) {
        match client
            .call_typed_no_params::<ListPeersResult>("list_peers")
            .await
        {
            Ok(result) => {
                self.all_peers = result.peers;
                self.error = None;
                self.apply_filter();
            }
            Err(e) => {
                self.error = Some(e.to_string());
            }
        }
    }

    pub async fn handle_key(&mut self, key: KeyEvent, client: &RpcClient) -> Option<TabKind> {
        match self.view {
            PeerView::List => {
                if self.searching {
                    self.handle_search_key(key);
                    None
                } else {
                    self.handle_list_key(key, client).await
                }
            }
            PeerView::ConnectForm => Some(TabKind::EnterEditing),
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.searching = false;
            }
            KeyCode::Char(c) => {
                self.search_query.push(c);
                self.apply_filter();
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.apply_filter();
            }
            _ => {}
        }
    }

    async fn handle_list_key(&mut self, key: KeyEvent, _client: &RpcClient) -> Option<TabKind> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.table_state.select_previous();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.peers.is_empty() {
                    self.table_state.select_next();
                    if let Some(sel) = self.table_state.selected() {
                        if sel >= self.peers.len() {
                            self.table_state.select(Some(self.peers.len() - 1));
                        }
                    }
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.table_state.select_first();
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.peers.is_empty() {
                    self.table_state.select_last();
                }
            }
            KeyCode::Char('/') => {
                // Enter search mode
                self.search_query.clear();
                self.searching = true;
            }
            KeyCode::Char('c') => {
                // Connect peer form
                self.init_connect_form();
                self.view = PeerView::ConnectForm;
                return Some(TabKind::EnterEditing);
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                // Disconnect selected peer — request confirmation
                if let Some(sel) = self.table_state.selected() {
                    if let Some(peer) = self.peers.get(sel) {
                        self.pending_disconnect = Some(peer.pubkey);
                        return Some(TabKind::RequestConfirm);
                    }
                }
            }
            _ => {}
        }
        None
    }

    /// Execute a previously confirmed disconnect action.
    pub async fn execute_confirmed(&mut self, client: &RpcClient) {
        let pubkey = match self.pending_disconnect.take() {
            Some(p) => p,
            None => return,
        };
        let params = DisconnectPeerParams { pubkey };
        match client
            .call_typed::<_, serde_json::Value>("disconnect_peer", &params)
            .await
        {
            Ok(_) => {
                self.status_message = Some("Peer disconnected".to_string());
                self.fetch_data(client).await;
            }
            Err(e) => {
                self.status_message = Some(format!("Disconnect failed: {}", e));
            }
        }
    }

    fn init_connect_form(&mut self) {
        self.form_fields = vec![
            ("Address (multiaddr)".to_string(), String::new()),
            (
                "Pubkey (optional, alternative to address)".to_string(),
                String::new(),
            ),
        ];
        self.form_selected = 0;
        self.form_editing = true;
    }

    pub async fn handle_editing_key(&mut self, key: KeyEvent, client: &RpcClient) {
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                if self.form_selected > 0 {
                    self.form_selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Tab => {
                if self.form_selected < self.form_fields.len() - 1 {
                    self.form_selected += 1;
                }
            }
            KeyCode::Char(c) => {
                if let Some(field) = self.form_fields.get_mut(self.form_selected) {
                    field.1.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(field) = self.form_fields.get_mut(self.form_selected) {
                    field.1.pop();
                }
            }
            KeyCode::Enter => {
                self.submit_connect_peer(client).await;
            }
            KeyCode::Esc => {
                self.view = PeerView::List;
                self.form_editing = false;
            }
            _ => {}
        }
    }

    pub fn should_exit_editing(&self) -> bool {
        !self.form_editing
    }

    async fn submit_connect_peer(&mut self, client: &RpcClient) {
        let address = self
            .form_fields
            .first()
            .map(|f| f.1.clone())
            .unwrap_or_default();
        let pubkey = self
            .form_fields
            .get(1)
            .map(|f| f.1.clone())
            .unwrap_or_default();

        if address.is_empty() && pubkey.is_empty() {
            self.status_message = Some("Address or pubkey is required".to_string());
            return;
        }

        let mut params = serde_json::json!({});
        if !address.is_empty() {
            params["address"] = serde_json::Value::String(address);
        }
        if !pubkey.is_empty() {
            params["pubkey"] = serde_json::Value::String(pubkey);
        }
        params["save"] = serde_json::Value::Bool(true);

        match client.call("connect_peer", vec![params]).await {
            Ok(_) => {
                self.status_message = Some("Peer connected successfully".to_string());
                self.view = PeerView::List;
                self.form_editing = false;
                self.fetch_data(client).await;
            }
            Err(e) => {
                self.status_message = Some(format!("Connect failed: {}", e));
            }
        }
    }
}
