//! Channels tab: list channels with state, balances, and actions.

use crossterm::event::{KeyCode, KeyEvent};
use fiber_json_types::{
    AbandonChannelParams, Channel, Hash256, ListChannelsParams, ListChannelsResult,
    ShutdownChannelParams,
};
use ratatui::widgets::TableState;
use serde_json;

use super::TabKind;
use crate::rpc_client::RpcClient;

/// View mode for the channels tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelView {
    /// List of channels
    List,
    /// Open channel form
    OpenForm,
    /// Update channel form
    UpdateForm,
}

/// Pending destructive action awaiting confirmation.
#[derive(Debug, Clone)]
pub enum ChannelPendingAction {
    Shutdown { channel_id: Hash256 },
    Abandon { channel_id: Hash256 },
}

/// Channels tab state.
pub struct ChannelsTab {
    pub channels: Vec<Channel>,
    pub error: Option<String>,
    pub table_state: TableState,
    pub view: ChannelView,
    pub include_closed: bool,
    pub only_pending: bool,
    pub status_message: Option<String>,
    pub pending_action: Option<ChannelPendingAction>,

    // Open channel form fields
    pub form_fields: Vec<(String, String)>,
    pub form_selected: usize,
    pub form_editing: bool,
}

impl ChannelsTab {
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
            error: None,
            table_state: TableState::default(),
            view: ChannelView::List,
            include_closed: false,
            only_pending: false,
            status_message: None,
            pending_action: None,
            form_fields: Vec::new(),
            form_selected: 0,
            form_editing: false,
        }
    }

    pub async fn fetch_data(&mut self, client: &RpcClient) {
        let params = ListChannelsParams {
            pubkey: None,
            include_closed: if self.include_closed {
                Some(true)
            } else {
                None
            },
            only_pending: if self.only_pending { Some(true) } else { None },
        };
        match client
            .call_typed::<_, ListChannelsResult>("list_channels", &params)
            .await
        {
            Ok(result) => {
                self.channels = result.channels;
                self.error = None;
                // Clamp selection if out of bounds
                if let Some(sel) = self.table_state.selected() {
                    if sel >= self.channels.len() && !self.channels.is_empty() {
                        self.table_state.select(Some(self.channels.len() - 1));
                    }
                }
            }
            Err(e) => {
                self.error = Some(e.to_string());
            }
        }
    }

    pub async fn handle_key(&mut self, key: KeyEvent, client: &RpcClient) -> Option<TabKind> {
        match self.view {
            ChannelView::List => self.handle_list_key(key, client).await,
            ChannelView::OpenForm | ChannelView::UpdateForm => Some(TabKind::EnterEditing),
        }
    }

    async fn handle_list_key(&mut self, key: KeyEvent, client: &RpcClient) -> Option<TabKind> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.table_state.select_previous();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.channels.is_empty() {
                    self.table_state.select_next();
                    // Clamp to last item
                    if let Some(sel) = self.table_state.selected() {
                        if sel >= self.channels.len() {
                            self.table_state.select(Some(self.channels.len() - 1));
                        }
                    }
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.table_state.select_first();
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.channels.is_empty() {
                    self.table_state.select_last();
                }
            }
            KeyCode::Char('c') => {
                // Toggle include_closed (mutually exclusive with only_pending)
                self.include_closed = !self.include_closed;
                if self.include_closed {
                    self.only_pending = false;
                }
                self.fetch_data(client).await;
            }
            KeyCode::Char('p') => {
                // Toggle only_pending (mutually exclusive with include_closed)
                self.only_pending = !self.only_pending;
                if self.only_pending {
                    self.include_closed = false;
                }
                self.fetch_data(client).await;
            }
            KeyCode::Char('o') => {
                // Open channel form
                self.init_open_form();
                self.view = ChannelView::OpenForm;
                return Some(TabKind::EnterEditing);
            }
            KeyCode::Char('s') => {
                // Shutdown selected channel — request confirmation
                if let Some(sel) = self.table_state.selected() {
                    if let Some(ch) = self.channels.get(sel) {
                        self.pending_action = Some(ChannelPendingAction::Shutdown {
                            channel_id: ch.channel_id,
                        });
                        return Some(TabKind::RequestConfirm);
                    }
                }
            }
            KeyCode::Char('a') => {
                // Abandon selected channel — request confirmation (only for non-Ready/non-Closed)
                if let Some(sel) = self.table_state.selected() {
                    if let Some(ch) = self.channels.get(sel) {
                        let state = Self::state_name(ch);
                        if state == "Ready" || state == "Closed" {
                            self.status_message = Some(
                                "Cannot abandon a Ready or Closed channel (use shutdown instead)"
                                    .to_string(),
                            );
                        } else {
                            self.pending_action = Some(ChannelPendingAction::Abandon {
                                channel_id: ch.channel_id,
                            });
                            return Some(TabKind::RequestConfirm);
                        }
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn init_open_form(&mut self) {
        self.form_fields = vec![
            ("Peer Pubkey".to_string(), String::new()),
            ("Funding Amount (CKB shannons)".to_string(), String::new()),
            ("Public (true/false)".to_string(), "true".to_string()),
            ("One-Way (true/false, optional)".to_string(), String::new()),
            (
                "Funding UDT Type Script (JSON, optional)".to_string(),
                String::new(),
            ),
            (
                "Shutdown Script (JSON, optional)".to_string(),
                String::new(),
            ),
            (
                "Commitment Delay Epoch (u64, optional)".to_string(),
                String::new(),
            ),
            (
                "Commitment Fee Rate (u64, optional)".to_string(),
                String::new(),
            ),
            (
                "Funding Fee Rate (u64, optional)".to_string(),
                String::new(),
            ),
            ("TLC Expiry Delta (ms, optional)".to_string(), String::new()),
            (
                "TLC Min Value (shannons, optional)".to_string(),
                String::new(),
            ),
            (
                "TLC Fee Proportional Millionths (optional)".to_string(),
                String::new(),
            ),
            (
                "Max TLC Value In Flight (shannons, optional)".to_string(),
                String::new(),
            ),
            (
                "Max TLC Number In Flight (optional)".to_string(),
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
                // Submit the form based on current view
                match self.view {
                    ChannelView::OpenForm => self.submit_open_channel(client).await,
                    ChannelView::UpdateForm => self.submit_update_channel(client).await,
                    _ => {}
                }
            }
            KeyCode::Esc => {
                // Return to the appropriate view
                self.view = ChannelView::List;
                self.form_editing = false;
            }
            _ => {}
        }
    }

    pub fn should_exit_editing(&self) -> bool {
        !self.form_editing
    }

    async fn submit_open_channel(&mut self, client: &RpcClient) {
        let pubkey = self
            .form_fields
            .first()
            .map(|f| f.1.clone())
            .unwrap_or_default();
        let amount_str = self
            .form_fields
            .get(1)
            .map(|f| f.1.clone())
            .unwrap_or_default();
        let is_public = self
            .form_fields
            .get(2)
            .map(|f| f.1.clone())
            .unwrap_or_default();

        if pubkey.is_empty() || amount_str.is_empty() {
            self.status_message = Some("Pubkey and amount are required".to_string());
            return;
        }

        let amount: u128 = match amount_str.parse() {
            Ok(a) => a,
            Err(_) => {
                self.status_message = Some("Invalid amount".to_string());
                return;
            }
        };

        let mut params = serde_json::json!({
            "pubkey": pubkey,
            "funding_amount": format!("0x{:x}", amount),
            "public": is_public.to_lowercase() == "true",
        });

        // one_way (bool, index 3)
        let one_way = self.form_fields.get(3).map(|f| f.1.as_str()).unwrap_or("");
        if !one_way.is_empty() {
            params["one_way"] = serde_json::Value::Bool(one_way.to_lowercase() == "true");
        }

        // funding_udt_type_script (JSON, index 4)
        let udt_script = self.form_fields.get(4).map(|f| f.1.as_str()).unwrap_or("");
        if !udt_script.is_empty() {
            match serde_json::from_str::<serde_json::Value>(udt_script) {
                Ok(v) => params["funding_udt_type_script"] = v,
                Err(_) => {
                    self.status_message =
                        Some("Invalid JSON for funding_udt_type_script".to_string());
                    return;
                }
            }
        }

        // shutdown_script (JSON, index 5)
        let shutdown = self.form_fields.get(5).map(|f| f.1.as_str()).unwrap_or("");
        if !shutdown.is_empty() {
            match serde_json::from_str::<serde_json::Value>(shutdown) {
                Ok(v) => params["shutdown_script"] = v,
                Err(_) => {
                    self.status_message = Some("Invalid JSON for shutdown_script".to_string());
                    return;
                }
            }
        }

        // commitment_delay_epoch (u64 hex, index 6)
        let delay = self.form_fields.get(6).map(|f| f.1.as_str()).unwrap_or("");
        if !delay.is_empty() {
            if let Ok(v) = delay.parse::<u64>() {
                params["commitment_delay_epoch"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid commitment_delay_epoch".to_string());
                return;
            }
        }

        // commitment_fee_rate (u64 hex, index 7)
        let cfr = self.form_fields.get(7).map(|f| f.1.as_str()).unwrap_or("");
        if !cfr.is_empty() {
            if let Ok(v) = cfr.parse::<u64>() {
                params["commitment_fee_rate"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid commitment_fee_rate".to_string());
                return;
            }
        }

        // funding_fee_rate (u64 hex, index 8)
        let ffr = self.form_fields.get(8).map(|f| f.1.as_str()).unwrap_or("");
        if !ffr.is_empty() {
            if let Ok(v) = ffr.parse::<u64>() {
                params["funding_fee_rate"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid funding_fee_rate".to_string());
                return;
            }
        }

        // tlc_expiry_delta (u64 hex, index 9)
        let ted = self.form_fields.get(9).map(|f| f.1.as_str()).unwrap_or("");
        if !ted.is_empty() {
            if let Ok(v) = ted.parse::<u64>() {
                params["tlc_expiry_delta"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid tlc_expiry_delta".to_string());
                return;
            }
        }

        // tlc_min_value (u128 hex, index 10)
        let tmv = self.form_fields.get(10).map(|f| f.1.as_str()).unwrap_or("");
        if !tmv.is_empty() {
            if let Ok(v) = tmv.parse::<u128>() {
                params["tlc_min_value"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid tlc_min_value".to_string());
                return;
            }
        }

        // tlc_fee_proportional_millionths (u128 hex, index 11)
        let tfpm = self.form_fields.get(11).map(|f| f.1.as_str()).unwrap_or("");
        if !tfpm.is_empty() {
            if let Ok(v) = tfpm.parse::<u128>() {
                params["tlc_fee_proportional_millionths"] =
                    serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid tlc_fee_proportional_millionths".to_string());
                return;
            }
        }

        // max_tlc_value_in_flight (u128 hex, index 12)
        let mtvif = self.form_fields.get(12).map(|f| f.1.as_str()).unwrap_or("");
        if !mtvif.is_empty() {
            if let Ok(v) = mtvif.parse::<u128>() {
                params["max_tlc_value_in_flight"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid max_tlc_value_in_flight".to_string());
                return;
            }
        }

        // max_tlc_number_in_flight (u64 hex, index 13)
        let mtnif = self.form_fields.get(13).map(|f| f.1.as_str()).unwrap_or("");
        if !mtnif.is_empty() {
            if let Ok(v) = mtnif.parse::<u64>() {
                params["max_tlc_number_in_flight"] =
                    serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid max_tlc_number_in_flight".to_string());
                return;
            }
        }

        match client.call("open_channel", vec![params]).await {
            Ok(result) => {
                // Extract temporary_channel_id from response
                let channel_id = result
                    .get("temporary_channel_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                self.status_message = Some(format!("Channel opened, temp_id: {}", channel_id));
                self.view = ChannelView::List;
                self.form_editing = false;
                self.fetch_data(client).await;
            }
            Err(e) => {
                self.status_message = Some(format!("Open channel failed: {}", e));
            }
        }
    }

    async fn submit_update_channel(&mut self, client: &RpcClient) {
        let selected = self.table_state.selected().unwrap_or(0);
        let channel_id = match self.channels.get(selected) {
            Some(ch) => ch.channel_id,
            None => {
                self.status_message = Some("No channel selected".to_string());
                return;
            }
        };

        let enabled_str = self
            .form_fields
            .first()
            .map(|f| f.1.clone())
            .unwrap_or_default();
        let expiry_str = self
            .form_fields
            .get(1)
            .map(|f| f.1.clone())
            .unwrap_or_default();
        let fee_str = self
            .form_fields
            .get(2)
            .map(|f| f.1.clone())
            .unwrap_or_default();

        let mut params = serde_json::json!({
            "channel_id": format!("{}", channel_id),
        });

        if !enabled_str.is_empty() {
            params["enabled"] = serde_json::Value::Bool(enabled_str.to_lowercase() == "true");
        }
        if !expiry_str.is_empty() {
            if let Ok(v) = expiry_str.parse::<u64>() {
                params["tlc_expiry_delta"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid TLC expiry delta".to_string());
                return;
            }
        }
        if !fee_str.is_empty() {
            if let Ok(v) = fee_str.parse::<u128>() {
                params["tlc_fee_proportional_millionths"] =
                    serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid TLC fee rate".to_string());
                return;
            }
        }

        match client.call("update_channel", vec![params]).await {
            Ok(_) => {
                self.status_message = Some("Channel updated successfully".to_string());
                self.view = ChannelView::List;
                self.form_editing = false;
                self.fetch_data(client).await;
            }
            Err(e) => {
                self.status_message = Some(format!("Update channel failed: {}", e));
            }
        }
    }

    /// Execute a previously confirmed destructive action.
    pub async fn execute_confirmed(&mut self, client: &RpcClient) {
        let action = match self.pending_action.take() {
            Some(a) => a,
            None => return,
        };
        match action {
            ChannelPendingAction::Shutdown { channel_id } => {
                let params = ShutdownChannelParams {
                    channel_id,
                    close_script: None,
                    fee_rate: None,
                    force: None,
                };
                match client
                    .call_typed::<_, serde_json::Value>("shutdown_channel", &params)
                    .await
                {
                    Ok(_) => {
                        self.status_message = Some("Channel shutdown initiated".to_string());
                        self.fetch_data(client).await;
                    }
                    Err(e) => {
                        self.status_message = Some(format!("Shutdown failed: {}", e));
                    }
                }
            }
            ChannelPendingAction::Abandon { channel_id } => {
                let params = AbandonChannelParams { channel_id };
                match client
                    .call_typed::<_, serde_json::Value>("abandon_channel", &params)
                    .await
                {
                    Ok(_) => {
                        self.status_message = Some("Channel abandoned".to_string());
                        self.fetch_data(client).await;
                    }
                    Err(e) => {
                        self.status_message = Some(format!("Abandon failed: {}", e));
                    }
                }
            }
        }
    }

    /// Get a display-friendly state name for a channel.
    pub fn state_name(ch: &Channel) -> &'static str {
        match &ch.state {
            fiber_json_types::ChannelState::NegotiatingFunding(_) => "Negotiating",
            fiber_json_types::ChannelState::CollaboratingFundingTx(_) => "Collaborating",
            fiber_json_types::ChannelState::SigningCommitment(_) => "Signing",
            fiber_json_types::ChannelState::AwaitingTxSignatures(_) => "AwaitTxSig",
            fiber_json_types::ChannelState::AwaitingChannelReady(_) => "AwaitReady",
            fiber_json_types::ChannelState::ChannelReady => "Ready",
            fiber_json_types::ChannelState::ShuttingDown(_) => "ShuttingDown",
            fiber_json_types::ChannelState::Closed(_) => "Closed",
        }
    }
}
