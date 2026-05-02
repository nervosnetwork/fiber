//! Graph tab: browse network graph nodes and channels.

use ckb_jsonrpc_types::JsonBytes;
use crossterm::event::{KeyCode, KeyEvent};
use fiber_json_types::{
    ChannelInfo, GraphChannelsParams, GraphChannelsResult, GraphNodesParams, GraphNodesResult,
    NodeInfo,
};
use ratatui::widgets::TableState;

use crate::rpc_client::RpcClient;

/// Whether showing nodes or channels in the graph tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphView {
    Nodes,
    Channels,
}

/// Graph tab state.
pub struct GraphTab {
    pub view: GraphView,

    // Nodes
    pub nodes: Vec<NodeInfo>,
    pub nodes_error: Option<String>,
    pub nodes_table_state: TableState,
    pub nodes_last_cursor: Option<JsonBytes>,
    pub nodes_current_after: Option<JsonBytes>,
    pub nodes_cursor_stack: Vec<JsonBytes>,
    pub nodes_page: usize,

    // Channels
    pub channels: Vec<ChannelInfo>,
    pub channels_error: Option<String>,
    pub channels_table_state: TableState,
    pub channels_last_cursor: Option<JsonBytes>,
    pub channels_current_after: Option<JsonBytes>,
    pub channels_cursor_stack: Vec<JsonBytes>,
    pub channels_page: usize,
}

impl GraphTab {
    pub fn new() -> Self {
        Self {
            view: GraphView::Nodes,
            nodes: Vec::new(),
            nodes_error: None,
            nodes_table_state: TableState::default(),
            nodes_last_cursor: None,
            nodes_current_after: None,
            nodes_cursor_stack: Vec::new(),
            nodes_page: 1,
            channels: Vec::new(),
            channels_error: None,
            channels_table_state: TableState::default(),
            channels_last_cursor: None,
            channels_current_after: None,
            channels_cursor_stack: Vec::new(),
            channels_page: 1,
        }
    }

    async fn fetch_nodes_page(&mut self, client: &RpcClient, after: Option<JsonBytes>) {
        let params = GraphNodesParams {
            limit: Some(100),
            after: after.clone(),
        };
        match client
            .call_typed::<_, GraphNodesResult>("graph_nodes", &params)
            .await
        {
            Ok(result) => {
                self.nodes_current_after = after;
                self.nodes = result.nodes;
                // last_cursor is always present; treat empty bytes as "no more pages"
                if result.last_cursor.is_empty() {
                    self.nodes_last_cursor = None;
                } else {
                    self.nodes_last_cursor = Some(result.last_cursor);
                }
                self.nodes_error = None;
                if !self.nodes.is_empty() {
                    self.nodes_table_state.select(Some(0));
                } else {
                    self.nodes_table_state.select(None);
                }
            }
            Err(e) => {
                self.nodes_error = Some(e.to_string());
            }
        }
    }

    async fn fetch_channels_page(&mut self, client: &RpcClient, after: Option<JsonBytes>) {
        let params = GraphChannelsParams {
            limit: Some(100),
            after: after.clone(),
        };
        match client
            .call_typed::<_, GraphChannelsResult>("graph_channels", &params)
            .await
        {
            Ok(result) => {
                self.channels_current_after = after;
                self.channels = result.channels;
                if result.last_cursor.is_empty() {
                    self.channels_last_cursor = None;
                } else {
                    self.channels_last_cursor = Some(result.last_cursor);
                }
                self.channels_error = None;
                if !self.channels.is_empty() {
                    self.channels_table_state.select(Some(0));
                } else {
                    self.channels_table_state.select(None);
                }
            }
            Err(e) => {
                self.channels_error = Some(e.to_string());
            }
        }
    }

    async fn next_nodes_page(&mut self, client: &RpcClient) {
        if let Some(cursor) = self.nodes_last_cursor.clone() {
            self.nodes_cursor_stack.push(cursor.clone());
            self.nodes_page += 1;
            self.fetch_nodes_page(client, Some(cursor)).await;
        }
    }

    async fn prev_nodes_page(&mut self, client: &RpcClient) {
        if self.nodes_page <= 1 {
            return;
        }
        if self.nodes_cursor_stack.len() <= 1 {
            // Go back to first page
            self.nodes_cursor_stack.clear();
            self.nodes_page = 1;
            self.fetch_nodes_page(client, None).await;
        } else {
            self.nodes_cursor_stack.pop();
            let prev_cursor = self.nodes_cursor_stack.pop();
            self.nodes_page -= 1;
            self.fetch_nodes_page(client, prev_cursor).await;
        }
    }

    async fn next_channels_page(&mut self, client: &RpcClient) {
        if let Some(cursor) = self.channels_last_cursor.clone() {
            self.channels_cursor_stack.push(cursor.clone());
            self.channels_page += 1;
            self.fetch_channels_page(client, Some(cursor)).await;
        }
    }

    async fn prev_channels_page(&mut self, client: &RpcClient) {
        if self.channels_page <= 1 {
            return;
        }
        if self.channels_cursor_stack.len() <= 1 {
            self.channels_cursor_stack.clear();
            self.channels_page = 1;
            self.fetch_channels_page(client, None).await;
        } else {
            self.channels_cursor_stack.pop();
            let prev_cursor = self.channels_cursor_stack.pop();
            self.channels_page -= 1;
            self.fetch_channels_page(client, prev_cursor).await;
        }
    }

    pub async fn handle_key(&mut self, key: KeyEvent, client: &RpcClient) {
        match key.code {
            // Toggle between nodes and channels views
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                self.view = match self.view {
                    GraphView::Nodes => GraphView::Channels,
                    GraphView::Channels => GraphView::Nodes,
                };
            }
            KeyCode::Up | KeyCode::Char('k') => match self.view {
                GraphView::Nodes => {
                    self.nodes_table_state.select_previous();
                }
                GraphView::Channels => {
                    self.channels_table_state.select_previous();
                }
            },
            KeyCode::Down | KeyCode::Char('j') => match self.view {
                GraphView::Nodes => {
                    if !self.nodes.is_empty() {
                        self.nodes_table_state.select_next();
                        if let Some(sel) = self.nodes_table_state.selected() {
                            if sel >= self.nodes.len() {
                                self.nodes_table_state.select(Some(self.nodes.len() - 1));
                            }
                        }
                    }
                }
                GraphView::Channels => {
                    if !self.channels.is_empty() {
                        self.channels_table_state.select_next();
                        if let Some(sel) = self.channels_table_state.selected() {
                            if sel >= self.channels.len() {
                                self.channels_table_state
                                    .select(Some(self.channels.len() - 1));
                            }
                        }
                    }
                }
            },
            KeyCode::Home | KeyCode::Char('g') => match self.view {
                GraphView::Nodes => self.nodes_table_state.select_first(),
                GraphView::Channels => self.channels_table_state.select_first(),
            },
            KeyCode::End | KeyCode::Char('G') => match self.view {
                GraphView::Nodes => {
                    if !self.nodes.is_empty() {
                        self.nodes_table_state.select_last();
                    }
                }
                GraphView::Channels => {
                    if !self.channels.is_empty() {
                        self.channels_table_state.select_last();
                    }
                }
            },
            KeyCode::Char(']') => {
                // Next page
                match self.view {
                    GraphView::Nodes => self.next_nodes_page(client).await,
                    GraphView::Channels => self.next_channels_page(client).await,
                }
            }
            KeyCode::Char('[') => {
                // Previous page
                match self.view {
                    GraphView::Nodes => self.prev_nodes_page(client).await,
                    GraphView::Channels => self.prev_channels_page(client).await,
                }
            }
            _ => {}
        }
    }
}
