//! Payments tab: list payments with status, send payment form.

use crossterm::event::{KeyCode, KeyEvent};
use fiber_json_types::{
    GetPaymentCommandResult, Hash256, ListPaymentsParams, ListPaymentsResult, PaymentStatus,
};
use ratatui::widgets::TableState;

use super::TabKind;
use crate::rpc_client::RpcClient;

/// View mode for the payments tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentView {
    /// List of payments
    List,
    /// Send payment form
    SendForm,
}

/// Payments tab state.
pub struct PaymentsTab {
    pub payments: Vec<GetPaymentCommandResult>,
    pub error: Option<String>,
    pub table_state: TableState,
    pub view: PaymentView,
    pub status_message: Option<String>,

    // Pagination
    pub last_cursor: Option<Hash256>,
    pub cursor_stack: Vec<Hash256>,
    pub current_page: usize,

    // Filter
    pub status_filter: Option<PaymentStatus>,

    // Send payment form fields
    pub form_fields: Vec<(String, String)>,
    pub form_selected: usize,
    pub form_editing: bool,
}

impl PaymentsTab {
    pub fn new() -> Self {
        Self {
            payments: Vec::new(),
            error: None,
            table_state: TableState::default(),
            view: PaymentView::List,
            status_message: None,
            last_cursor: None,
            cursor_stack: Vec::new(),
            current_page: 1,
            status_filter: None,
            form_fields: Vec::new(),
            form_selected: 0,
            form_editing: false,
        }
    }

    pub async fn fetch_data(&mut self, client: &RpcClient) {
        // Reset to first page
        self.cursor_stack.clear();
        self.current_page = 1;
        self.fetch_page(client, None).await;
    }

    async fn fetch_page(&mut self, client: &RpcClient, after: Option<Hash256>) {
        let params = ListPaymentsParams {
            status: self.status_filter,
            limit: None,
            after,
        };
        match client
            .call_typed::<_, ListPaymentsResult>("list_payments", &params)
            .await
        {
            Ok(result) => {
                self.payments = result.payments;
                self.last_cursor = result.last_cursor;
                self.error = None;
                // Reset selection to top of new page
                if !self.payments.is_empty() {
                    self.table_state.select(Some(0));
                } else {
                    self.table_state.select(None);
                }
            }
            Err(e) => {
                self.error = Some(e.to_string());
            }
        }
    }

    pub async fn fetch_next_page(&mut self, client: &RpcClient) {
        if let Some(cursor) = self.last_cursor {
            // Push the current cursor onto the stack for back navigation
            self.cursor_stack.push(cursor);
            self.current_page += 1;
            self.fetch_page(client, Some(cursor)).await;
        }
    }

    pub async fn fetch_prev_page(&mut self, client: &RpcClient) {
        if self.cursor_stack.len() <= 1 {
            // Go back to first page
            if self.current_page > 1 {
                self.fetch_data(client).await;
            }
        } else {
            // Pop current cursor, use the previous one
            self.cursor_stack.pop();
            let prev_cursor = self.cursor_stack.pop();
            self.current_page -= 1;
            self.fetch_page(client, prev_cursor).await;
        }
    }

    pub async fn handle_key(&mut self, key: KeyEvent, client: &RpcClient) -> Option<TabKind> {
        match self.view {
            PaymentView::List => self.handle_list_key(key, client).await,
            PaymentView::SendForm => Some(TabKind::EnterEditing),
        }
    }

    async fn handle_list_key(&mut self, key: KeyEvent, client: &RpcClient) -> Option<TabKind> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.table_state.select_previous();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.payments.is_empty() {
                    self.table_state.select_next();
                    if let Some(sel) = self.table_state.selected() {
                        if sel >= self.payments.len() {
                            self.table_state.select(Some(self.payments.len() - 1));
                        }
                    }
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.table_state.select_first();
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.payments.is_empty() {
                    self.table_state.select_last();
                }
            }
            KeyCode::Char('n') => {
                // Send payment form
                self.init_send_form();
                self.view = PaymentView::SendForm;
                return Some(TabKind::EnterEditing);
            }
            KeyCode::Char('N') => {
                // Next page (Shift+N)
                self.fetch_next_page(client).await;
            }
            KeyCode::Char('P') => {
                // Previous page (Shift+P)
                if self.current_page > 1 {
                    self.fetch_prev_page(client).await;
                }
            }
            KeyCode::Char('f') => {
                // Cycle status filter: All -> Created -> Inflight -> Success -> Failed -> All
                self.status_filter = match self.status_filter {
                    None => Some(PaymentStatus::Created),
                    Some(PaymentStatus::Created) => Some(PaymentStatus::Inflight),
                    Some(PaymentStatus::Inflight) => Some(PaymentStatus::Success),
                    Some(PaymentStatus::Success) => Some(PaymentStatus::Failed),
                    Some(PaymentStatus::Failed) => None,
                };
                self.fetch_data(client).await;
            }
            _ => {}
        }
        None
    }

    fn init_send_form(&mut self) {
        self.form_fields = vec![
            ("Invoice (encoded string)".to_string(), String::new()),
            (
                "Target Pubkey (optional if invoice set)".to_string(),
                String::new(),
            ),
            (
                "Amount (shannons, optional if invoice set)".to_string(),
                String::new(),
            ),
            ("Keysend (true/false)".to_string(), "false".to_string()),
            ("Payment Hash (hex, optional)".to_string(), String::new()),
            (
                "Final TLC Expiry Delta (ms, optional)".to_string(),
                String::new(),
            ),
            ("TLC Expiry Limit (ms, optional)".to_string(), String::new()),
            ("Timeout (seconds, optional)".to_string(), String::new()),
            (
                "Max Fee Amount (shannons, optional)".to_string(),
                String::new(),
            ),
            (
                "Max Fee Rate (per 1000, optional)".to_string(),
                String::new(),
            ),
            ("Max Parts (optional)".to_string(), String::new()),
            (
                "Trampoline Hops (comma-sep pubkeys, optional)".to_string(),
                String::new(),
            ),
            (
                "UDT Type Script (JSON, optional)".to_string(),
                String::new(),
            ),
            (
                "Allow Self Payment (true/false, optional)".to_string(),
                String::new(),
            ),
            ("Custom Records (JSON, optional)".to_string(), String::new()),
            (
                "Hop Hints (JSON array, optional)".to_string(),
                String::new(),
            ),
            ("Dry Run (true/false, optional)".to_string(), String::new()),
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
                self.submit_send_payment(client).await;
            }
            KeyCode::Esc => {
                self.view = PaymentView::List;
                self.form_editing = false;
            }
            _ => {}
        }
    }

    pub fn should_exit_editing(&self) -> bool {
        !self.form_editing
    }

    async fn submit_send_payment(&mut self, client: &RpcClient) {
        let invoice = self
            .form_fields
            .first()
            .map(|f| f.1.clone())
            .unwrap_or_default();
        let target_pubkey = self
            .form_fields
            .get(1)
            .map(|f| f.1.clone())
            .unwrap_or_default();
        let amount_str = self
            .form_fields
            .get(2)
            .map(|f| f.1.clone())
            .unwrap_or_default();
        let keysend_str = self
            .form_fields
            .get(3)
            .map(|f| f.1.clone())
            .unwrap_or_default();

        let mut params = serde_json::json!({});

        if !invoice.is_empty() {
            params["invoice"] = serde_json::Value::String(invoice);
        }
        if !target_pubkey.is_empty() {
            params["target_pubkey"] = serde_json::Value::String(target_pubkey);
        }
        if !amount_str.is_empty() {
            if let Ok(amount) = amount_str.parse::<u128>() {
                params["amount"] = serde_json::Value::String(format!("0x{:x}", amount));
            } else {
                self.status_message = Some("Invalid amount".to_string());
                return;
            }
        }
        if keysend_str.to_lowercase() == "true" {
            params["keysend"] = serde_json::Value::Bool(true);
        }

        // payment_hash (hex string, index 4)
        let ph = self.form_fields.get(4).map(|f| f.1.as_str()).unwrap_or("");
        if !ph.is_empty() {
            params["payment_hash"] = serde_json::Value::String(ph.to_string());
        }

        // final_tlc_expiry_delta (u64 hex, index 5)
        let fted = self.form_fields.get(5).map(|f| f.1.as_str()).unwrap_or("");
        if !fted.is_empty() {
            if let Ok(v) = fted.parse::<u64>() {
                params["final_tlc_expiry_delta"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid final_tlc_expiry_delta".to_string());
                return;
            }
        }

        // tlc_expiry_limit (u64 hex, index 6)
        let tel = self.form_fields.get(6).map(|f| f.1.as_str()).unwrap_or("");
        if !tel.is_empty() {
            if let Ok(v) = tel.parse::<u64>() {
                params["tlc_expiry_limit"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid tlc_expiry_limit".to_string());
                return;
            }
        }

        // timeout (u64 hex, index 7)
        let timeout = self.form_fields.get(7).map(|f| f.1.as_str()).unwrap_or("");
        if !timeout.is_empty() {
            if let Ok(v) = timeout.parse::<u64>() {
                params["timeout"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid timeout".to_string());
                return;
            }
        }

        // max_fee_amount (u128 hex, index 8)
        let mfa = self.form_fields.get(8).map(|f| f.1.as_str()).unwrap_or("");
        if !mfa.is_empty() {
            if let Ok(v) = mfa.parse::<u128>() {
                params["max_fee_amount"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid max_fee_amount".to_string());
                return;
            }
        }

        // max_fee_rate (u64 hex, index 9)
        let mfr = self.form_fields.get(9).map(|f| f.1.as_str()).unwrap_or("");
        if !mfr.is_empty() {
            if let Ok(v) = mfr.parse::<u64>() {
                params["max_fee_rate"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid max_fee_rate".to_string());
                return;
            }
        }

        // max_parts (u64 hex, index 10)
        let mp = self.form_fields.get(10).map(|f| f.1.as_str()).unwrap_or("");
        if !mp.is_empty() {
            if let Ok(v) = mp.parse::<u64>() {
                params["max_parts"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid max_parts".to_string());
                return;
            }
        }

        // trampoline_hops (comma-separated pubkeys, index 11)
        let th = self.form_fields.get(11).map(|f| f.1.as_str()).unwrap_or("");
        if !th.is_empty() {
            let hops: Vec<serde_json::Value> = th
                .split(',')
                .map(|s| serde_json::Value::String(s.trim().to_string()))
                .collect();
            params["trampoline_hops"] = serde_json::Value::Array(hops);
        }

        // udt_type_script (JSON, index 12)
        let udt = self.form_fields.get(12).map(|f| f.1.as_str()).unwrap_or("");
        if !udt.is_empty() {
            match serde_json::from_str::<serde_json::Value>(udt) {
                Ok(v) => params["udt_type_script"] = v,
                Err(_) => {
                    self.status_message = Some("Invalid JSON for udt_type_script".to_string());
                    return;
                }
            }
        }

        // allow_self_payment (bool, index 13)
        let asp = self.form_fields.get(13).map(|f| f.1.as_str()).unwrap_or("");
        if !asp.is_empty() {
            params["allow_self_payment"] = serde_json::Value::Bool(asp.to_lowercase() == "true");
        }

        // custom_records (JSON, index 14)
        let cr = self.form_fields.get(14).map(|f| f.1.as_str()).unwrap_or("");
        if !cr.is_empty() {
            match serde_json::from_str::<serde_json::Value>(cr) {
                Ok(v) => params["custom_records"] = v,
                Err(_) => {
                    self.status_message = Some("Invalid JSON for custom_records".to_string());
                    return;
                }
            }
        }

        // hop_hints (JSON array, index 15)
        let hh = self.form_fields.get(15).map(|f| f.1.as_str()).unwrap_or("");
        if !hh.is_empty() {
            match serde_json::from_str::<serde_json::Value>(hh) {
                Ok(v) => params["hop_hints"] = v,
                Err(_) => {
                    self.status_message = Some("Invalid JSON for hop_hints".to_string());
                    return;
                }
            }
        }

        // dry_run (bool, index 16)
        let dr = self.form_fields.get(16).map(|f| f.1.as_str()).unwrap_or("");
        if !dr.is_empty() {
            params["dry_run"] = serde_json::Value::Bool(dr.to_lowercase() == "true");
        }

        match client.call("send_payment", vec![params]).await {
            Ok(result) => {
                // Extract payment_hash and status from response
                let hash = result
                    .get("payment_hash")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let status = result
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                self.status_message = Some(format!("Payment sent ({}), hash: {}", status, hash));
                self.view = PaymentView::List;
                self.form_editing = false;
                self.fetch_data(client).await;
            }
            Err(e) => {
                self.status_message = Some(format!("Send payment failed: {}", e));
            }
        }
    }

    /// Get display-friendly status name.
    pub fn status_name(payment: &GetPaymentCommandResult) -> &'static str {
        match payment.status {
            PaymentStatus::Created => "Created",
            PaymentStatus::Inflight => "Inflight",
            PaymentStatus::Success => "Success",
            PaymentStatus::Failed => "Failed",
        }
    }

    /// Get the current filter label for display.
    pub fn filter_label(&self) -> &'static str {
        match self.status_filter {
            None => "All",
            Some(PaymentStatus::Created) => "Created",
            Some(PaymentStatus::Inflight) => "Inflight",
            Some(PaymentStatus::Success) => "Success",
            Some(PaymentStatus::Failed) => "Failed",
        }
    }
}
