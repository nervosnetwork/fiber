use ckb_types::packed::Script;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::fiber::payment::{SendPaymentData, SendPaymentDataBuilder};
use crate::fiber_types::{
    EntityHex, Hash256, HashAlgorithm, PrevTlcInfo, Pubkey, TrampolineContext,
};

/// A validated trampoline forwarding request that may be handed to the hosted LSP service.
#[serde_as]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TrampolineForwardingRequest {
    pub payment_hash: Hash256,
    pub next_node_id: Pubkey,
    pub amount_to_forward: u128,
    pub hash_algorithm: HashAlgorithm,
    pub build_max_fee_amount: u128,
    pub tlc_expiry_delta: u64,
    pub tlc_expiry_limit: u64,
    pub max_parts: Option<u64>,
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<Script>,
    pub remaining_trampoline_onion: Vec<u8>,
    pub previous_tlc: PrevTlcInfo,
    pub max_outgoing_tlc_expiry: u64,
}

impl TrampolineForwardingRequest {
    pub(crate) fn into_send_payment_data(self) -> Result<SendPaymentData, String> {
        SendPaymentDataBuilder::new(self.next_node_id, self.amount_to_forward, self.payment_hash)
            .final_tlc_expiry_delta(self.tlc_expiry_delta)
            .tlc_expiry_limit(self.tlc_expiry_limit)
            .max_fee_amount(Some(self.build_max_fee_amount))
            .max_parts(self.max_parts)
            .udt_type_script(self.udt_type_script)
            .trampoline_context(Some(TrampolineContext {
                remaining_trampoline_onion: self.remaining_trampoline_onion,
                // The current trampoline forwarding flow supports one upstream TLC.
                previous_tlcs: vec![self.previous_tlc],
                hash_algorithm: self.hash_algorithm,
                max_outgoing_tlc_expiry: Some(self.max_outgoing_tlc_expiry),
            }))
            .allow_mpp(self.max_parts.is_some_and(|value| value > 1))
            .build()
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod config;
#[cfg(not(target_arch = "wasm32"))]
mod delivery;
#[cfg(not(target_arch = "wasm32"))]
mod dispatcher;
#[cfg(not(target_arch = "wasm32"))]
mod invoice;
#[cfg(not(target_arch = "wasm32"))]
mod registry;
#[cfg(not(target_arch = "wasm32"))]
mod runtime;
#[cfg(not(target_arch = "wasm32"))]
mod service;
#[cfg(not(target_arch = "wasm32"))]
mod tenant;

#[cfg(not(target_arch = "wasm32"))]
pub use config::{LspConfig, DEFAULT_MAX_ACTIVE_TENANTS};
#[cfg(not(target_arch = "wasm32"))]
pub use delivery::{
    LspPaymentDelivery, LspPaymentDeliveryLimits, LspPaymentDeliveryManager,
    LspPaymentDeliveryStatus, LspPaymentDeliveryStore, LSP_DELIVERY_SAFETY_MARGIN_MS,
};
#[cfg(not(target_arch = "wasm32"))]
pub use invoice::{
    LspInvoiceHint, LspInvoiceHintPayload, LspInvoiceRegistration, LspInvoiceRegistry,
    LspInvoiceStore, DEFAULT_LSP_BUFFER_DURATION_MS, MAX_LSP_BUFFER_DURATION_MS,
};
#[cfg(not(target_arch = "wasm32"))]
pub use registry::{TenantRegistry, TenantRegistryStore};
#[cfg(not(target_arch = "wasm32"))]
pub use runtime::{
    FiberTenantRuntimeFactory, HostedTenantRpcContext, HostedTenantRuntime, TenantRuntimeFactory,
    TenantSupervisor,
};
#[cfg(not(target_arch = "wasm32"))]
pub use service::{
    HostedTenantRegistration, LspDeliveryDecision, LspService, LspServiceArgs, LspServiceMessage,
    LspServiceState, LspServiceStatus,
};
#[cfg(not(target_arch = "wasm32"))]
pub use tenant::{HostedTenantRecord, HostedTenantStatus, TenantId, TenantRuntimeStatus};

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
