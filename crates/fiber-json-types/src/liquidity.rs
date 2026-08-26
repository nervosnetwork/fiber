//! Liquidity management JSON-RPC types.

use crate::schema_helpers::{
    schema_as_hex_bytes_optional, schema_as_uint_hex, schema_as_uint_hex_optional,
};
use crate::serde_utils::{EntityHex, Hash256, Pubkey, U128Hex, U64Hex};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// Asset family in the provider liquidity registry.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LiquidityAssetKind {
    /// Native CKB capacity denominated in shannons.
    Ckb,
    /// User-defined token identified by a type script.
    Udt,
}

/// JSON representation of a provider liquidity asset.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiquidityAssetInfo {
    /// Stable provider-local asset identifier.
    pub asset_id: String,
    /// Asset family.
    pub kind: LiquidityAssetKind,
    /// Required for UDT assets and absent for CKB.
    pub udt_type_script: Option<ckb_jsonrpc_types::Script>,
    /// Smallest raw swap amount accepted by the provider.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub min_amount: u128,
    /// Largest raw swap amount accepted by the provider.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_amount: u128,
    /// Provider-advertised capacity for this asset.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub available_capacity: u128,
    /// Fixed provider fee charged in the swapped asset.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub base_fee: u128,
    /// Proportional provider fee in parts per million.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub proportional_fee_ppm: u64,
    /// Whether the provider currently quotes this asset.
    pub enabled: bool,
}

/// Direction of a liquidity swap.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LiquiditySwapKind {
    /// Move Fiber channel balance to an on-chain receiver.
    LoopOut,
    /// Move on-chain funds into Fiber channel balance.
    LoopIn,
}

/// Complete liquidity quote terms transferred between independent nodes.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiquidityQuoteEnvelope {
    /// Provider-generated quote identifier.
    pub quote_id: Hash256,
    /// Swap direction.
    pub swap_kind: LiquiditySwapKind,
    /// Public key of the provider that issued the quote.
    pub provider_pubkey: Pubkey,
    /// Complete information for the quoted asset.
    pub asset: LiquidityAssetInfo,
    /// Raw swap amount in the asset's smallest unit.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount: u128,
    /// Fee charged by the provider in the swapped asset.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub provider_fee: u128,
    /// Maximum Fiber routing fee in the swapped asset.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub routing_fee_limit: u128,
    /// Estimated CKB transaction fee in shannons.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub onchain_fee_estimate_ckb: u64,
    /// CKB capacity required by the on-chain cells in shannons.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub capacity_requirement_ckb: u64,
    /// CKB hash of the 32-byte payment preimage.
    pub payment_hash: Hash256,
    /// Quote expiry as a Unix timestamp in milliseconds.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expires_at: u64,
    /// Deadline for confirming the provider payout as a Unix timestamp in milliseconds.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub payout_deadline: u64,
    /// Exact encoded CKB `since` value after which the funder can refund.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub refund_after_lock_time: u64,
    /// Claimant lock script encoded as Molecule script bytes in `0x` hex.
    pub claimant_lock: String,
    /// Refund lock script encoded as Molecule script bytes in `0x` hex.
    pub refund_lock: String,
    /// Client invoice required for Loop In and absent for Loop Out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_invoice: Option<String>,
}

/// Parameters for importing a provider's complete liquidity quote.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ImportLiquidityQuoteParams {
    /// Complete quote terms received from the provider.
    pub quote: LiquidityQuoteEnvelope,
    /// Maximum provider fee accepted by the client.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_provider_fee: u128,
    /// Maximum Fiber routing fee accepted by the client.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_routing_fee: u128,
}

/// Parameters for enabling or disabling liquidity provider mode.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SetLiquidityProviderModeParams {
    /// Whether liquidity provider mode should be enabled.
    pub enabled: bool,
}

/// Parameters for requesting a Loop Out quote.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuoteLoopOutParams {
    /// Provider node identifier or endpoint.
    pub provider: String,
    /// Provider asset registry identifier.
    pub asset_id: String,
    /// Raw on-chain destination amount before routing fees.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount: u128,
    /// Claimant lock script bytes encoded for the payout lock.
    pub claimant_lock: String,
    /// Maximum provider fee accepted by the client.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_provider_fee: u128,
    /// Maximum Fiber routing fee accepted by the client.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_routing_fee: u128,
    /// Relative quote expiry requested by the client.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expires_after_seconds: u64,
}

/// Provider-side parameters for quoting a Loop Out request.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderQuoteLoopOutParams {
    /// Provider asset registry identifier.
    pub asset_id: String,
    /// Raw on-chain destination amount before routing fees.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount: u128,
    /// Claimant lock script bytes encoded for the payout lock.
    pub claimant_lock: String,
    /// Maximum provider fee accepted by the client.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_provider_fee: u128,
    /// Maximum Fiber routing fee accepted by the client.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_routing_fee: u128,
    /// Relative quote expiry requested by the client.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expires_after_seconds: u64,
}

/// Provider-side parameters for accepting a Loop Out quote.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderAcceptLoopOutParams {
    /// Provider-generated quote identifier.
    pub quote_id: Hash256,
}

/// Quote response shared by Loop In and Loop Out.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiquidityQuoteResponse {
    /// Provider-generated quote identifier.
    pub quote_id: Hash256,
    /// Swap direction.
    pub swap_kind: LiquiditySwapKind,
    /// Provider asset registry identifier.
    pub asset_id: String,
    /// Raw destination amount before routing fees.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount: u128,
    /// Fee charged in the swapped asset.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub provider_fee: u128,
    /// Maximum Fiber routing fee in the swapped asset.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub routing_fee_limit: u128,
    /// Estimated CKB transaction fee.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub onchain_fee_estimate_ckb: u64,
    /// CKB capacity required by the on-chain cells.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub capacity_requirement_ckb: u64,
    /// CKB-hash of the 32-byte preimage.
    pub payment_hash: Hash256,
    /// Quote expiry timestamp in milliseconds.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expires_at: u64,
    /// Loop Out deadline for confirming provider payout lock.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub payout_deadline: Option<u64>,
    /// Chain lock time after which the on-chain funder can refund.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub refund_after_lock_time: u64,
    /// Claimant lock script bytes encoded for the liquidity-lock output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimant_lock: Option<String>,
    /// Refund lock script bytes encoded for the liquidity-lock output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refund_lock: Option<String>,
}

/// Parameters for requesting a Loop In quote.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QuoteLoopInParams {
    /// Provider node identifier or endpoint.
    pub provider: String,
    /// Provider asset registry identifier.
    pub asset_id: String,
    /// Raw Fiber destination amount before routing fees.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount: u128,
    /// Client invoice the provider should pay.
    pub client_invoice: String,
    /// Client refund lock script bytes encoded for the client lock.
    pub refund_lock: String,
    /// Maximum provider fee accepted by the client.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_provider_fee: u128,
    /// Maximum Fiber routing fee accepted by the client.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_routing_fee: u128,
    /// Relative quote expiry requested by the client.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub expires_after_seconds: u64,
}

/// Parameters for executing Loop Out after quote acceptance.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoopOutParams {
    /// Provider-generated quote identifier.
    pub quote_id: Hash256,
    /// Maximum provider fee accepted at execution time.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_provider_fee: u128,
    /// Maximum Fiber routing fee accepted at execution time.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub max_routing_fee: u128,
    /// Payout lock outpoint returned by `provider_accept_loop_out`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payout_outpoint: Option<ckb_jsonrpc_types::OutPoint>,
}

/// Parameters for executing Loop In after quote acceptance.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoopInParams {
    /// Provider-generated quote identifier.
    pub quote_id: Hash256,
    /// Funding transaction hash or wallet funding descriptor.
    pub funding_tx: String,
}

/// Provider-side parameters for accepting an observed Loop In client lock.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderAcceptLoopInParams {
    /// Provider-generated quote identifier.
    pub quote_id: Hash256,
    /// Confirmable client lock transaction hash.
    pub lock_tx_hash: Hash256,
    /// Output index of the liquidity-lock cell in `lock_tx_hash`.
    pub lock_output_index: u32,
}

/// Response returned when a liquidity swap is created.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiquiditySwapResponse {
    /// Local swap identifier.
    pub swap_id: Hash256,
    /// Initial persisted state name.
    pub state: String,
    /// CKB-hash of the 32-byte preimage.
    pub payment_hash: Hash256,
    /// Payout lock outpoint produced by the provider accept (Loop Out provider only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payout_outpoint: Option<ckb_jsonrpc_types::OutPoint>,
    /// Creation timestamp in milliseconds.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub created_at: u64,
}

/// Parameters for querying a liquidity swap.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GetSwapParams {
    /// Local swap identifier.
    pub swap_id: Hash256,
}

/// Persisted swap record returned by `get_swap` and `list_swaps`.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiquiditySwapRecord {
    /// Local swap identifier.
    pub swap_id: Hash256,
    /// Swap direction.
    pub swap_kind: LiquiditySwapKind,
    /// Current persisted state name.
    pub state: String,
    /// Provider asset registry identifier.
    pub asset_id: String,
    /// Raw swap amount.
    #[serde_as(as = "U128Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub amount: u128,
    /// CKB-hash of the 32-byte preimage.
    pub payment_hash: Hash256,
    /// Creation timestamp in milliseconds.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub created_at: u64,
    /// Last update timestamp in milliseconds.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub updated_at: u64,
}

/// Parameters for listing liquidity swaps.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListSwapsParams {
    /// Optional state filter.
    pub state: Option<String>,
    /// Optional asset filter.
    pub asset_id: Option<String>,
    /// Maximum number of rows to return.
    #[serde_as(as = "Option<U64Hex>")]
    #[schemars(schema_with = "schema_as_uint_hex_optional")]
    pub limit: Option<u64>,
    /// Pagination cursor returned by the previous call.
    pub cursor: Option<String>,
}

/// Response returned by `list_swaps`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListSwapsResponse {
    /// Swap records in cursor order.
    pub swaps: Vec<LiquiditySwapRecord>,
    /// Cursor for the next page, if more records are available.
    pub next_cursor: Option<String>,
}

/// Parameters for provider asset administration calls.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiquidityAssetParams {
    /// Stable provider-local asset identifier.
    pub asset_id: String,
}

/// Parameters for adding a provider asset registry entry.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddLiquidityAssetParams {
    /// Asset entry to add.
    pub asset: LiquidityAssetInfo,
}

/// Parameters for updating a provider asset registry entry.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UpdateLiquidityAssetParams {
    /// Replacement asset entry.
    pub asset: LiquidityAssetInfo,
}

/// Response returned by `list_liquidity_assets`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListLiquidityAssetsResponse {
    /// Configured provider assets.
    pub assets: Vec<LiquidityAssetInfo>,
}

/// Provider status returned by `get_liquidity_provider_status`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiquidityProviderStatus {
    /// Whether provider mode is enabled.
    pub enabled: bool,
    /// Number of currently enabled assets.
    pub enabled_asset_count: u64,
    /// Number of non-terminal provider swaps.
    pub active_swaps: u64,
}

/// Semantic role of a liquidity CKB transaction within a swap's lifecycle.
///
/// This is the externally visible role label. It diverges from the persisted
/// `LiquidityChainTxRole` for Loop In swaps, where the stored `Payout` role
/// represents the client's on-chain lock transaction and is surfaced as
/// `loop_in_lock`.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LiquidityChainTransactionRole {
    /// Provider payout lock transaction (Loop Out).
    Payout,
    /// Client on-chain lock transaction (Loop In, persisted as the payout role).
    LoopInLock,
    /// Client claim transaction.
    Claim,
    /// Provider refund transaction.
    Refund,
}

/// Persisted chain transaction record returned by `list_liquidity_chain_transactions`.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
pub struct LiquidityChainTransaction {
    /// Semantic transaction role within the swap.
    pub role: LiquidityChainTransactionRole,
    /// CKB transaction hash.
    pub tx_hash: Hash256,
    /// Created output outpoint, if tracked by recovery.
    #[serde_as(as = "Option<EntityHex>")]
    #[schemars(schema_with = "schema_as_hex_bytes_optional")]
    pub outpoint: Option<ckb_types::packed::OutPoint>,
    /// Current transaction status name.
    pub status: String,
    /// Optional failure reason for rejected or failed transactions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    /// Creation timestamp in milliseconds.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub created_at: u64,
    /// Last update timestamp in milliseconds.
    #[serde_as(as = "U64Hex")]
    #[schemars(schema_with = "schema_as_uint_hex")]
    pub updated_at: u64,
}

/// Parameters for listing a swap's persisted chain transactions.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
pub struct ListLiquidityChainTransactionsParams {
    /// Local swap identifier.
    pub swap_id: Hash256,
}

/// Response returned by `list_liquidity_chain_transactions`.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, JsonSchema)]
pub struct ListLiquidityChainTransactionsResponse {
    /// Chain transaction records in stable role order.
    pub transactions: Vec<LiquidityChainTransaction>,
}

#[cfg(test)]
mod tests {
    use molecule::prelude::Entity;

    use super::*;

    #[test]
    fn liquidity_quote_envelope_serializes_complete_ckb_terms() {
        let provider_pubkey = crate::Pubkey([2u8; 33]);
        let envelope = LiquidityQuoteEnvelope {
            quote_id: Hash256([1u8; 32]),
            swap_kind: LiquiditySwapKind::LoopOut,
            provider_pubkey,
            asset: LiquidityAssetInfo {
                asset_id: "ckb".to_string(),
                kind: LiquidityAssetKind::Ckb,
                udt_type_script: None,
                min_amount: 1,
                max_amount: 1_000,
                available_capacity: 10_000,
                base_fee: 2,
                proportional_fee_ppm: 30,
                enabled: true,
            },
            amount: 100,
            provider_fee: 2,
            routing_fee_limit: 3,
            onchain_fee_estimate_ckb: 4,
            capacity_requirement_ckb: 5,
            payment_hash: Hash256([3u8; 32]),
            expires_at: 6,
            payout_deadline: 7,
            refund_after_lock_time: 8,
            claimant_lock: "0x0102".to_string(),
            refund_lock: "0x0304".to_string(),
            client_invoice: None,
        };

        let value = serde_json::to_value(&envelope).expect("json");
        let decoded: LiquidityQuoteEnvelope = serde_json::from_value(value.clone()).expect("json");

        assert_eq!(value["amount"], "0x64");
        assert_eq!(decoded.provider_pubkey, provider_pubkey);
        assert!(value.get("client_invoice").is_none());
        assert!(decoded.client_invoice.is_none());
        assert_eq!(value["claimant_lock"], "0x0102");
        assert_eq!(value["refund_lock"], "0x0304");
    }

    #[test]
    fn liquidity_quote_envelope_round_trips_udt_terms_and_invoice() {
        let script_json = serde_json::json!({
            "code_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
            "hash_type": "type",
            "args": "0x1234"
        });
        let envelope = LiquidityQuoteEnvelope {
            quote_id: Hash256([4u8; 32]),
            swap_kind: LiquiditySwapKind::LoopIn,
            provider_pubkey: crate::Pubkey([3u8; 33]),
            asset: LiquidityAssetInfo {
                asset_id: "udt".to_string(),
                kind: LiquidityAssetKind::Udt,
                udt_type_script: Some(
                    serde_json::from_value(script_json.clone()).expect("script json"),
                ),
                min_amount: 1,
                max_amount: u128::MAX,
                available_capacity: u128::MAX,
                base_fee: 2,
                proportional_fee_ppm: 30,
                enabled: true,
            },
            amount: u128::MAX,
            provider_fee: 2,
            routing_fee_limit: 3,
            onchain_fee_estimate_ckb: 4,
            capacity_requirement_ckb: 5,
            payment_hash: Hash256([5u8; 32]),
            expires_at: 6,
            payout_deadline: 7,
            refund_after_lock_time: 8,
            claimant_lock: "0x0506".to_string(),
            refund_lock: "0x0708".to_string(),
            client_invoice: Some("fiber-invoice".to_string()),
        };

        let value = serde_json::to_value(&envelope).expect("json");
        let decoded: LiquidityQuoteEnvelope = serde_json::from_value(value.clone()).expect("json");

        assert_eq!(value["asset"]["udt_type_script"], script_json);
        assert_eq!(value["amount"], "0xffffffffffffffffffffffffffffffff");
        assert_eq!(
            value["asset"]["available_capacity"],
            "0xffffffffffffffffffffffffffffffff"
        );
        assert_eq!(decoded.client_invoice.as_deref(), Some("fiber-invoice"));
    }

    #[test]
    fn import_liquidity_quote_params_round_trip_fee_caps() {
        let params = ImportLiquidityQuoteParams {
            quote: LiquidityQuoteEnvelope {
                quote_id: Hash256([6u8; 32]),
                swap_kind: LiquiditySwapKind::LoopOut,
                provider_pubkey: crate::Pubkey([2u8; 33]),
                asset: LiquidityAssetInfo {
                    asset_id: "ckb".to_string(),
                    kind: LiquidityAssetKind::Ckb,
                    udt_type_script: None,
                    min_amount: 1,
                    max_amount: 100,
                    available_capacity: 1_000,
                    base_fee: 2,
                    proportional_fee_ppm: 30,
                    enabled: true,
                },
                amount: 100,
                provider_fee: 2,
                routing_fee_limit: 3,
                onchain_fee_estimate_ckb: 4,
                capacity_requirement_ckb: 5,
                payment_hash: Hash256([7u8; 32]),
                expires_at: 6,
                payout_deadline: 7,
                refund_after_lock_time: 8,
                claimant_lock: "0x0102".to_string(),
                refund_lock: "0x0304".to_string(),
                client_invoice: None,
            },
            max_provider_fee: u128::MAX,
            max_routing_fee: 0x1_0000_0000_0000_0000,
        };

        let value = serde_json::to_value(&params).expect("json");
        let decoded: ImportLiquidityQuoteParams =
            serde_json::from_value(value.clone()).expect("json");

        assert_eq!(
            value["max_provider_fee"],
            "0xffffffffffffffffffffffffffffffff"
        );
        assert_eq!(value["max_routing_fee"], "0x10000000000000000");
        assert_eq!(decoded.max_provider_fee, u128::MAX);
        assert_eq!(decoded.max_routing_fee, 0x1_0000_0000_0000_0000);
    }

    #[test]
    fn quote_loop_out_params_serialize_amount_as_hex() {
        let params = QuoteLoopOutParams {
            provider: "02ab".to_string(),
            asset_id: "ckb".to_string(),
            amount: 100,
            claimant_lock: "0x0102".to_string(),
            max_provider_fee: 2,
            max_routing_fee: 3,
            expires_after_seconds: 60,
        };

        let value = serde_json::to_value(params).expect("json");
        assert_eq!(value["amount"], "0x64");
        assert_eq!(value["claimant_lock"], "0x0102");
        assert!(value.get("refund_lock").is_none());
        assert!(value.get("receiver").is_none());
        assert_eq!(value["max_provider_fee"], "0x2");
        assert_eq!(value["max_routing_fee"], "0x3");
    }

    #[test]
    fn quote_response_keeps_direction_specific_fields_optional() {
        let response = LiquidityQuoteResponse {
            quote_id: Hash256([0u8; 32]),
            swap_kind: LiquiditySwapKind::LoopOut,
            asset_id: "ckb".to_string(),
            amount: 100,
            provider_fee: 2,
            routing_fee_limit: 3,
            onchain_fee_estimate_ckb: 4,
            capacity_requirement_ckb: 5,
            payment_hash: Hash256([0x11u8; 32]),
            expires_at: 6,
            payout_deadline: Some(7),
            refund_after_lock_time: 8,
            claimant_lock: None,
            refund_lock: None,
        };

        let value = serde_json::to_value(response).expect("json");
        assert_eq!(value["payout_deadline"], "0x7");
        assert_eq!(value["refund_after_lock_time"], "0x8");
        assert!(value.get("claimant_lock").is_none());
        assert!(value.get("refund_lock").is_none());
    }

    #[test]
    fn swap_kind_serializes_as_snake_case() {
        let value = serde_json::to_value(LiquiditySwapKind::LoopOut).expect("json");
        assert_eq!(value, "loop_out");
    }

    #[test]
    fn loop_in_response_and_list_types_match_rpc_contract() {
        let response = LiquiditySwapResponse {
            swap_id: Hash256([1u8; 32]),
            state: "created".to_string(),
            payment_hash: Hash256([2u8; 32]),
            payout_outpoint: None,
            created_at: 42,
        };
        let list_params = ListSwapsParams {
            state: Some("created".to_string()),
            asset_id: Some("ckb".to_string()),
            limit: Some(10),
            cursor: Some("cursor".to_string()),
        };

        let response_value = serde_json::to_value(response).expect("json");
        let list_value = serde_json::to_value(list_params).expect("json");

        assert_eq!(response_value["state"], "created");
        assert_eq!(response_value["created_at"], "0x2a");
        assert_eq!(list_value["limit"], "0xa");
    }

    #[test]
    fn provider_asset_info_serializes_kind_and_amounts() {
        let asset = LiquidityAssetInfo {
            asset_id: "ckb".to_string(),
            kind: LiquidityAssetKind::Ckb,
            udt_type_script: None,
            min_amount: 1,
            max_amount: 100,
            available_capacity: 1000,
            base_fee: 2,
            proportional_fee_ppm: 30,
            enabled: true,
        };

        let value = serde_json::to_value(asset).expect("json");

        assert_eq!(value["kind"], "ckb");
        assert_eq!(value["min_amount"], "0x1");
        assert_eq!(value["proportional_fee_ppm"], "0x1e");
    }

    #[test]
    fn provider_accept_loop_out_params_serialize_quote_id() {
        let params = ProviderAcceptLoopOutParams {
            quote_id: Hash256([1u8; 32]),
        };

        let value = serde_json::to_value(params).expect("json");

        assert_eq!(
            value["quote_id"],
            "0x0101010101010101010101010101010101010101010101010101010101010101"
        );
        assert_eq!(value.as_object().unwrap().len(), 1);
    }

    #[test]
    fn list_liquidity_chain_transactions_dto_serializes_records_without_signed_tx() {
        let outpoint = ckb_types::packed::OutPoint::new(
            ckb_types::packed::Byte32::from_slice(&[9u8; 32]).unwrap(),
            1,
        );
        let transaction = LiquidityChainTransaction {
            role: LiquidityChainTransactionRole::LoopInLock,
            tx_hash: Hash256([2u8; 32]),
            outpoint: Some(outpoint),
            status: "broadcast".to_string(),
            failure_reason: None,
            created_at: 42,
            updated_at: 43,
        };
        let params = ListLiquidityChainTransactionsParams {
            swap_id: Hash256([1u8; 32]),
        };
        let response = ListLiquidityChainTransactionsResponse {
            transactions: vec![transaction],
        };

        let params_value = serde_json::to_value(&params).expect("params json");
        let value = serde_json::to_value(&response).expect("response json");
        let decoded: ListLiquidityChainTransactionsResponse =
            serde_json::from_value(value.clone()).expect("decode response");

        assert_eq!(
            params_value["swap_id"],
            "0x0101010101010101010101010101010101010101010101010101010101010101"
        );
        let tx = &value["transactions"][0];
        assert_eq!(tx["role"], "loop_in_lock");
        assert_eq!(tx["status"], "broadcast");
        assert_eq!(
            tx["tx_hash"],
            "0x0202020202020202020202020202020202020202020202020202020202020202"
        );
        assert_eq!(tx["created_at"], "0x2a");
        assert_eq!(tx["updated_at"], "0x2b");
        assert!(tx.get("failure_reason").is_none());
        assert!(tx.get("signed_tx").is_none());
        assert!(tx.get("signed_tx_bytes").is_none());
        assert_eq!(decoded.transactions, response.transactions);
    }

    #[test]
    fn liquidity_chain_transaction_role_serializes_semantic_labels() {
        let roles = [
            (LiquidityChainTransactionRole::Payout, "payout"),
            (LiquidityChainTransactionRole::LoopInLock, "loop_in_lock"),
            (LiquidityChainTransactionRole::Claim, "claim"),
            (LiquidityChainTransactionRole::Refund, "refund"),
        ];

        for (role, expected) in roles {
            assert_eq!(serde_json::to_value(role).expect("role json"), expected);
        }
    }
}
