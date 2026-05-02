//! Tab implementations for the TUI.

pub mod channels;
pub mod dashboard;
pub mod graph;
pub mod invoices;
pub mod logs;
pub mod payments;
pub mod peers;

pub use channels::ChannelsTab;
pub use dashboard::DashboardTab;
pub use graph::GraphTab;
pub use invoices::InvoicesTab;
pub use logs::LogsTab;
pub use payments::PaymentsTab;
pub use peers::PeersTab;

/// Signals that can be returned from a tab's key handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    /// Enter editing mode (for forms).
    EnterEditing,
    /// Request a confirmation dialog before proceeding with a destructive action.
    RequestConfirm,
}
