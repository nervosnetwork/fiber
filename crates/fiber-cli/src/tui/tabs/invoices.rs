//! Invoices tab: create, list, and lookup invoices.

use crossterm::event::{KeyCode, KeyEvent};
use fiber_json_types::{
    CkbInvoiceStatus, GetInvoiceResult, Hash256, ListInvoicesParams, ListInvoicesResult,
};
use ratatui::widgets::TableState;

use super::TabKind;
use crate::rpc_client::RpcClient;

/// View mode for the invoices tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceView {
    /// Main view: list of invoices
    Main,
    /// Create invoice form
    CreateForm,
    /// Lookup result
    LookupResult,
    /// Cancel invoice form (enter payment hash)
    CancelForm,
    /// Parse invoice form (enter encoded invoice)
    ParseForm,
    /// Parse invoice result display
    ParseResult,
}

/// Invoices tab state.
pub struct InvoicesTab {
    pub view: InvoiceView,
    pub status_message: Option<String>,

    // Lookup
    pub lookup_hash: String,
    pub lookup_result: Option<GetInvoiceResult>,
    pub lookup_error: Option<String>,

    // Parse invoice result
    pub parse_result: Option<serde_json::Value>,
    pub parse_error: Option<String>,

    // Invoice list (fetched from backend via list_invoices RPC)
    pub invoices: Vec<GetInvoiceResult>,
    pub error: Option<String>,

    // Pagination
    pub last_cursor: Option<Hash256>,
    pub cursor_stack: Vec<Hash256>,
    pub current_page: usize,

    // Filter
    pub status_filter: Option<CkbInvoiceStatus>,

    // Create form
    pub form_fields: Vec<(String, String)>,
    pub form_selected: usize,
    pub form_editing: bool,
    pub table_state: TableState,
}

impl InvoicesTab {
    pub fn new() -> Self {
        Self {
            view: InvoiceView::Main,
            status_message: None,
            lookup_hash: String::new(),
            lookup_result: None,
            lookup_error: None,
            parse_result: None,
            parse_error: None,
            invoices: Vec::new(),
            error: None,
            last_cursor: None,
            cursor_stack: Vec::new(),
            current_page: 1,
            status_filter: None,
            form_fields: Vec::new(),
            form_selected: 0,
            form_editing: false,
            table_state: TableState::default(),
        }
    }

    pub async fn fetch_data(&mut self, client: &RpcClient) {
        // Reset to first page
        self.cursor_stack.clear();
        self.current_page = 1;
        self.fetch_page(client, None).await;
    }

    async fn fetch_page(&mut self, client: &RpcClient, after: Option<Hash256>) {
        let params = ListInvoicesParams {
            status: self.status_filter,
            limit: None,
            after,
        };
        match client
            .call_typed::<_, ListInvoicesResult>("list_invoices", &params)
            .await
        {
            Ok(result) => {
                self.invoices = result.invoices;
                self.last_cursor = result.last_cursor;
                self.error = None;
                // Reset selection to top of new page
                if !self.invoices.is_empty() {
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
            InvoiceView::Main => self.handle_main_key(key, client).await,
            InvoiceView::CreateForm | InvoiceView::CancelForm | InvoiceView::ParseForm => {
                Some(TabKind::EnterEditing)
            }
            InvoiceView::LookupResult => {
                if key.code == KeyCode::Esc || key.code == KeyCode::Backspace {
                    self.view = InvoiceView::Main;
                }
                None
            }
            InvoiceView::ParseResult => {
                if key.code == KeyCode::Esc || key.code == KeyCode::Backspace {
                    self.view = InvoiceView::Main;
                }
                None
            }
        }
    }

    async fn handle_main_key(&mut self, key: KeyEvent, client: &RpcClient) -> Option<TabKind> {
        match key.code {
            KeyCode::Char('n') => {
                // Create new invoice
                self.init_create_form();
                self.view = InvoiceView::CreateForm;
                return Some(TabKind::EnterEditing);
            }
            KeyCode::Char('l') => {
                // Lookup invoice -- enter editing for the hash input
                self.lookup_hash.clear();
                self.lookup_result = None;
                self.lookup_error = None;
                self.form_fields = vec![("Payment Hash".to_string(), String::new())];
                self.form_selected = 0;
                self.form_editing = true;
                self.view = InvoiceView::LookupResult;
                return Some(TabKind::EnterEditing);
            }
            KeyCode::Char('c') => {
                // Cancel invoice -- enter editing for the payment hash
                self.form_fields = vec![("Payment Hash".to_string(), String::new())];
                self.form_selected = 0;
                self.form_editing = true;
                self.view = InvoiceView::CancelForm;
                return Some(TabKind::EnterEditing);
            }
            KeyCode::Char('p') => {
                // Parse invoice -- enter editing for the encoded invoice
                self.parse_result = None;
                self.parse_error = None;
                self.form_fields = vec![("Encoded Invoice".to_string(), String::new())];
                self.form_selected = 0;
                self.form_editing = true;
                self.view = InvoiceView::ParseForm;
                return Some(TabKind::EnterEditing);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.table_state.select_previous();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.invoices.is_empty() {
                    self.table_state.select_next();
                    if let Some(sel) = self.table_state.selected() {
                        if sel >= self.invoices.len() {
                            self.table_state.select(Some(self.invoices.len() - 1));
                        }
                    }
                }
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
                // Cycle status filter: All -> Open -> Cancelled -> Expired -> Received -> Paid -> All
                self.status_filter = match self.status_filter {
                    None => Some(CkbInvoiceStatus::Open),
                    Some(CkbInvoiceStatus::Open) => Some(CkbInvoiceStatus::Cancelled),
                    Some(CkbInvoiceStatus::Cancelled) => Some(CkbInvoiceStatus::Expired),
                    Some(CkbInvoiceStatus::Expired) => Some(CkbInvoiceStatus::Received),
                    Some(CkbInvoiceStatus::Received) => Some(CkbInvoiceStatus::Paid),
                    Some(CkbInvoiceStatus::Paid) => None,
                };
                self.fetch_data(client).await;
            }
            _ => {}
        }
        None
    }

    fn init_create_form(&mut self) {
        self.form_fields = vec![
            ("Amount (shannons)".to_string(), String::new()),
            ("Description (optional)".to_string(), String::new()),
            ("Expiry (seconds, optional)".to_string(), String::new()),
            ("Currency (Fibb/Fibt/Fibd)".to_string(), "Fibd".to_string()),
            (
                "Payment Preimage (hex, optional)".to_string(),
                String::new(),
            ),
            ("Payment Hash (hex, optional)".to_string(), String::new()),
            ("Fallback Address (optional)".to_string(), String::new()),
            (
                "Final Expiry Delta (ms, optional)".to_string(),
                String::new(),
            ),
            (
                "UDT Type Script (JSON, optional)".to_string(),
                String::new(),
            ),
            (
                "Hash Algorithm (ckb_hash/sha256)".to_string(),
                "ckb_hash".to_string(),
            ),
            (
                "Allow MPP (true/false, optional)".to_string(),
                String::new(),
            ),
            (
                "Allow Trampoline (true/false, optional)".to_string(),
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
            KeyCode::Enter => match self.view {
                InvoiceView::CreateForm => self.submit_create_invoice(client).await,
                InvoiceView::LookupResult => self.submit_lookup(client).await,
                InvoiceView::CancelForm => self.submit_cancel_invoice(client).await,
                InvoiceView::ParseForm => self.submit_parse_invoice(client).await,
                _ => {}
            },
            KeyCode::Esc => {
                self.view = InvoiceView::Main;
                self.form_editing = false;
            }
            _ => {}
        }
    }

    pub fn should_exit_editing(&self) -> bool {
        !self.form_editing
    }

    async fn submit_create_invoice(&mut self, client: &RpcClient) {
        let amount_str = self
            .form_fields
            .first()
            .map(|f| f.1.clone())
            .unwrap_or_default();
        let description = self
            .form_fields
            .get(1)
            .map(|f| f.1.clone())
            .unwrap_or_default();
        let expiry_str = self
            .form_fields
            .get(2)
            .map(|f| f.1.clone())
            .unwrap_or_default();

        if amount_str.is_empty() {
            self.status_message = Some("Amount is required".to_string());
            return;
        }

        let amount: u128 = match amount_str.parse() {
            Ok(a) => a,
            Err(_) => {
                self.status_message = Some("Invalid amount".to_string());
                return;
            }
        };

        // Currency (index 3), default to "Fibd"
        let currency = self
            .form_fields
            .get(3)
            .map(|f| f.1.clone())
            .unwrap_or_default();
        let currency = if currency.is_empty() {
            "Fibd".to_string()
        } else {
            currency
        };

        let mut params = serde_json::json!({
            "amount": format!("0x{:x}", amount),
            "currency": currency,
        });

        if !description.is_empty() {
            params["description"] = serde_json::Value::String(description);
        }
        if !expiry_str.is_empty() {
            if let Ok(expiry) = expiry_str.parse::<u64>() {
                params["expiry"] = serde_json::Value::String(format!("0x{:x}", expiry));
            } else {
                self.status_message = Some("Invalid expiry".to_string());
                return;
            }
        }

        // payment_preimage (hex string, index 4)
        let preimage = self.form_fields.get(4).map(|f| f.1.as_str()).unwrap_or("");
        if !preimage.is_empty() {
            params["payment_preimage"] = serde_json::Value::String(preimage.to_string());
        }

        // payment_hash (hex string, index 5)
        let hash = self.form_fields.get(5).map(|f| f.1.as_str()).unwrap_or("");
        if !hash.is_empty() {
            params["payment_hash"] = serde_json::Value::String(hash.to_string());
        }

        // fallback_address (string, index 6)
        let fallback = self.form_fields.get(6).map(|f| f.1.as_str()).unwrap_or("");
        if !fallback.is_empty() {
            params["fallback_address"] = serde_json::Value::String(fallback.to_string());
        }

        // final_expiry_delta (u64 hex, index 7)
        let fed = self.form_fields.get(7).map(|f| f.1.as_str()).unwrap_or("");
        if !fed.is_empty() {
            if let Ok(v) = fed.parse::<u64>() {
                params["final_expiry_delta"] = serde_json::Value::String(format!("0x{:x}", v));
            } else {
                self.status_message = Some("Invalid final_expiry_delta".to_string());
                return;
            }
        }

        // udt_type_script (JSON, index 8)
        let udt = self.form_fields.get(8).map(|f| f.1.as_str()).unwrap_or("");
        if !udt.is_empty() {
            match serde_json::from_str::<serde_json::Value>(udt) {
                Ok(v) => params["udt_type_script"] = v,
                Err(_) => {
                    self.status_message = Some("Invalid JSON for udt_type_script".to_string());
                    return;
                }
            }
        }

        // hash_algorithm (string, index 9)
        let algo = self.form_fields.get(9).map(|f| f.1.as_str()).unwrap_or("");
        if !algo.is_empty() {
            params["hash_algorithm"] = serde_json::Value::String(algo.to_string());
        }

        // allow_mpp (bool, index 10)
        let mpp = self.form_fields.get(10).map(|f| f.1.as_str()).unwrap_or("");
        if !mpp.is_empty() {
            params["allow_mpp"] = serde_json::Value::Bool(mpp.to_lowercase() == "true");
        }

        // allow_trampoline_routing (bool, index 11)
        let trampoline = self.form_fields.get(11).map(|f| f.1.as_str()).unwrap_or("");
        if !trampoline.is_empty() {
            params["allow_trampoline_routing"] =
                serde_json::Value::Bool(trampoline.to_lowercase() == "true");
        }

        match client.call("new_invoice", vec![params]).await {
            Ok(result) => {
                // Extract invoice_address from response
                let addr = result
                    .get("invoice_address")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if addr.is_empty() {
                    self.status_message = Some("Invoice created successfully".to_string());
                } else {
                    self.status_message = Some(format!("Invoice created: {}", addr));
                }
                self.view = InvoiceView::Main;
                self.form_editing = false;
                // Refresh invoice list from backend
                self.fetch_data(client).await;
            }
            Err(e) => {
                self.status_message = Some(format!("Create invoice failed: {}", e));
            }
        }
    }

    async fn submit_lookup(&mut self, client: &RpcClient) {
        let hash = self
            .form_fields
            .first()
            .map(|f| f.1.clone())
            .unwrap_or_default();
        if hash.is_empty() {
            self.status_message = Some("Payment hash is required".to_string());
            return;
        }

        let params = serde_json::json!({ "payment_hash": hash });

        match client.call("get_invoice", vec![params]).await {
            Ok(result) => match serde_json::from_value::<GetInvoiceResult>(result) {
                Ok(invoice) => {
                    self.lookup_result = Some(invoice);
                    self.lookup_error = None;
                    self.form_editing = false;
                }
                Err(e) => {
                    self.lookup_error = Some(format!("Failed to parse: {}", e));
                    self.form_editing = false;
                }
            },
            Err(e) => {
                self.lookup_error = Some(e.to_string());
                self.form_editing = false;
            }
        }
    }

    async fn submit_cancel_invoice(&mut self, client: &RpcClient) {
        let hash = self
            .form_fields
            .first()
            .map(|f| f.1.clone())
            .unwrap_or_default();
        if hash.is_empty() {
            self.status_message = Some("Payment hash is required".to_string());
            return;
        }

        let params = serde_json::json!({ "payment_hash": hash });

        match client.call("cancel_invoice", vec![params]).await {
            Ok(_) => {
                self.status_message = Some("Invoice cancelled successfully".to_string());
                self.view = InvoiceView::Main;
                self.form_editing = false;
                // Refresh invoice list from backend
                self.fetch_data(client).await;
            }
            Err(e) => {
                self.status_message = Some(format!("Cancel invoice failed: {}", e));
            }
        }
    }

    async fn submit_parse_invoice(&mut self, client: &RpcClient) {
        let invoice = self
            .form_fields
            .first()
            .map(|f| f.1.clone())
            .unwrap_or_default();
        if invoice.is_empty() {
            self.status_message = Some("Encoded invoice string is required".to_string());
            return;
        }

        let params = serde_json::json!({ "invoice": invoice });

        match client.call("parse_invoice", vec![params]).await {
            Ok(result) => {
                self.parse_result = Some(result);
                self.parse_error = None;
                self.view = InvoiceView::ParseResult;
                self.form_editing = false;
            }
            Err(e) => {
                self.parse_error = Some(e.to_string());
                self.view = InvoiceView::ParseResult;
                self.form_editing = false;
            }
        }
    }

    /// Get display-friendly status name.
    pub fn status_name(status: &CkbInvoiceStatus) -> &'static str {
        match status {
            CkbInvoiceStatus::Open => "Open",
            CkbInvoiceStatus::Cancelled => "Cancelled",
            CkbInvoiceStatus::Expired => "Expired",
            CkbInvoiceStatus::Received => "Received",
            CkbInvoiceStatus::Paid => "Paid",
        }
    }

    /// Get the current filter label for display.
    pub fn filter_label(&self) -> &'static str {
        match self.status_filter {
            None => "All",
            Some(ref s) => Self::status_name(s),
        }
    }
}
