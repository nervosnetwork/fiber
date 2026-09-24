//! Hosted LSP JSON-RPC request and response types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{
    schema_helpers::schema_as_uint_hex, serde_utils::U64Hex, GetPaymentCommandParams, Hash256,
    Pubkey,
};

/// Parameters that identify a hosted tenant.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct LspTenantParams {
    /// Stable operator-facing tenant identifier.
    pub tenant_id: String,
}

/// Parameters for issuing a one-time hosted tenant registration nonce.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct GetLspTenantRegistryNonceParams {
    /// RootSigner identity that will sign the registration payload.
    pub root_signer_pubkey: Pubkey,
}

/// One-time challenge and LSP identity used to build `TenantRegistryPayload`.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct GetLspTenantRegistryNonceResult {
    /// Public Fiber identity of the hosted LSP.
    pub lsp_node_id: Pubkey,
    /// RootSigner identity associated with this nonce.
    pub root_signer_pubkey: Pubkey,
    /// Cryptographically random, single-use 32-byte nonce.
    pub nonce: Hash256,
}

/// RootSigner-authenticated hosted tenant registration request.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct RegisterLspTenantParams {
    /// RootSigner identity that deterministically derives the tenant ID.
    pub root_signer_pubkey: Pubkey,
    /// Most recent nonce issued for this RootSigner by this LSP.
    pub nonce: Hash256,
    /// Compact ECDSA signature over the canonical `TenantRegistryPayload`, as hex.
    pub signature: String,
}

/// Parameters for retrieving a hosted tenant's outgoing payment session.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct GetLspPaymentParams {
    /// Hosted tenant that owns the outgoing payment session.
    pub tenant_id: String,
    /// Standard Fiber payment lookup parameters.
    pub payment: GetPaymentCommandParams,
}

/// Parameters for retrieving an invoice from a hosted tenant's store.
#[derive(Debug, Deserialize, JsonSchema, Serialize)]
pub struct GetLspInvoiceParams {
    /// Hosted tenant that owns the invoice.
    pub tenant_id: String,
    /// Payment hash of the invoice to retrieve.
    pub payment_hash: Hash256,
}

/// Parameters that identify a hosted invoice or payment delivery.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct LspPaymentHashParams {
    /// Payment hash of the hosted invoice.
    pub payment_hash: Hash256,
}

/// Current in-process state of a hosted tenant runtime.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LspTenantRuntimeStatus {
    /// Tenant metadata exists but its execution context is not running.
    Cold,
    /// The tenant execution context is running.
    Active,
}

/// Hosted tenant state boundary and liveness information.
#[serde_as]
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct LspTenantStatus {
    /// Stable operator-facing tenant identifier.
    pub tenant_id: String,
    /// RootSigner identity that owns this tenant, when registered through the
    /// authenticated tenant registry protocol.
    pub root_signer_pubkey: Option<Pubkey>,
    /// Tenant protocol key used for its private channel and invoice signatures;
    /// it is not a public, gossip-routable node identity.
    pub invoice_pubkey: Pubkey,
    /// Private channel currently bound to this tenant.
    pub private_channel_id: Option<Hash256>,
    /// Tenant creation timestamp in milliseconds since Unix epoch.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub created_at: u64,
    /// Whether the tenant execution context is currently resident in this process.
    pub runtime_status: LspTenantRuntimeStatus,
    /// Whether Public T currently has an online private channel to the tenant.
    pub channel_online: bool,
}

/// Result of registering a hosted tenant.
#[derive(Clone, Deserialize, JsonSchema, Serialize)]
pub struct RegisterLspTenantResult {
    /// Persistent and runtime status of the registered tenant.
    pub tenant: LspTenantStatus,
    /// Newly issued tenant access token.
    pub access_token: String,
}

/// Result of listing hosted tenants.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct ListLspTenantsResult {
    /// Registered hosted tenants.
    pub tenants: Vec<LspTenantStatus>,
}

/// Summary of the multi-tenant hosted LSP service.
#[serde_as]
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct LspServiceStatus {
    /// Public trampoline node identity advertised by the LSP.
    pub public_node_id: Pubkey,
    /// Root directory containing tenant-local runtime files such as signing keys.
    pub tenant_store_root: String,
    /// Number of persistently registered tenants.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub registered_tenants: u64,
    /// Number of tenant execution contexts currently resident in this process.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub active_tenants: u64,
}

/// Durable hosted-payment delivery state.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LspPaymentDeliveryStatus {
    /// Public T is waiting for the hosted tenant to become reachable.
    Deferred,
    /// Public T is starting downstream trampoline dispatch.
    Dispatching,
    /// A downstream payment session exists; the buffer deadline no longer applies.
    InFlight,
    /// The downstream outcome is durable and Public T is resolving the upstream TLC.
    SettlingUpstream,
    /// The downstream payment completed successfully.
    Succeeded,
    /// Delivery failed before or during downstream payment.
    Failed,
}

/// Operator-visible state of one hosted incoming payment.
#[serde_as]
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
pub struct LspPaymentDelivery {
    /// Payment hash of the hosted invoice.
    pub payment_hash: Hash256,
    /// Public T channel on which the incoming TLC was received.
    pub incoming_channel_id: Hash256,
    /// Incoming TLC identifier, unique within `incoming_channel_id`.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub incoming_tlc_id: u64,
    /// Tenant that owns the payment.
    pub tenant_id: String,
    /// Private channel selected internally for tenant delivery.
    pub private_channel_id: Hash256,
    /// Last instant at which an undispatched payment may remain buffered.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub buffer_deadline: u64,
    /// Current durable delivery state.
    pub status: LspPaymentDeliveryStatus,
    /// Number of downstream dispatch attempts started by Public T.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub attempt_count: u64,
    /// Most recent downstream dispatch or payment error, including retryable errors.
    pub last_error: Option<String>,
    /// Terminal detail when `status` is `failed`.
    pub failure_reason: Option<String>,
    /// Creation timestamp in milliseconds since Unix epoch.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub created_at: u64,
    /// Last update timestamp in milliseconds since Unix epoch.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub updated_at: u64,
}
