/// Returns whether a failed hosted payment cannot succeed with a later route attempt.
#[cfg(not(target_arch = "wasm32"))]
pub fn is_permanent_hosted_payment_failure(error_code: crate::fiber_types::TlcErrorCode) -> bool {
    use crate::fiber_types::TlcErrorCode;
    matches!(
        error_code,
        TlcErrorCode::IncorrectOrUnknownPaymentDetails
            | TlcErrorCode::InvoiceExpired
            | TlcErrorCode::InvoiceCancelled
            | TlcErrorCode::FinalIncorrectExpiryDelta
            | TlcErrorCode::FinalIncorrectTlcAmount
            | TlcErrorCode::HoldTlcTimeout
    )
}

#[cfg(not(target_arch = "wasm32"))]
mod config;
#[cfg(not(target_arch = "wasm32"))]
mod delivery;
#[cfg(not(target_arch = "wasm32"))]
mod invoice;
#[cfg(not(target_arch = "wasm32"))]
mod port;
#[cfg(not(target_arch = "wasm32"))]
mod registry;
#[cfg(not(target_arch = "wasm32"))]
mod runtime;
#[cfg(not(target_arch = "wasm32"))]
mod service;
#[cfg(not(target_arch = "wasm32"))]
mod tenant;
#[cfg(not(target_arch = "wasm32"))]
mod token;
#[cfg(all(not(target_arch = "wasm32"), feature = "watchtower"))]
mod watch;

#[cfg(not(target_arch = "wasm32"))]
pub use config::{LspConfig, DEFAULT_MAX_ACTIVE_TENANTS};
#[cfg(not(target_arch = "wasm32"))]
pub use delivery::{
    LspPaymentDelivery, LspPaymentDeliveryKey, LspPaymentDeliveryLimits, LspPaymentDeliveryManager,
    LspPaymentDeliveryStatus, LspPaymentDeliveryStore, LSP_DELIVERY_SAFETY_MARGIN_MS,
};
#[cfg(not(target_arch = "wasm32"))]
pub use invoice::{
    LspInvoiceHint, LspInvoiceHintPayload, LspInvoiceRegistration, LspInvoiceRegistry,
    LspInvoiceStore, DEFAULT_LSP_BUFFER_DURATION_MS, MAX_LSP_BUFFER_DURATION_MS,
};
#[cfg(not(target_arch = "wasm32"))]
pub use port::LspDeliveryPort;
#[cfg(not(target_arch = "wasm32"))]
pub use registry::{TenantRegistry, TenantRegistryStore};
#[cfg(not(target_arch = "wasm32"))]
pub use runtime::{
    FiberTenantRuntimeFactory, HostedTenantRpcContext, HostedTenantRuntime, TenantRuntimeFactory,
    TenantSupervisor,
};
#[cfg(not(target_arch = "wasm32"))]
pub use service::{
    HostedTenantRegistration, LspService, LspServiceArgs, LspServiceMessage, LspServiceState,
    LspServiceStatus, TenantSubscriptionContext,
};
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use tenant::select_tenant_channel;
#[cfg(not(target_arch = "wasm32"))]
pub use tenant::{
    tenant_watchtower_node_id, HostedTenantChannelStatus, HostedTenantRecord, HostedTenantStatus,
    TenantChannelRecord, TenantId, TenantRuntimeStatus,
};
#[cfg(not(target_arch = "wasm32"))]
pub use token::BiscuitTokenIssuer;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
