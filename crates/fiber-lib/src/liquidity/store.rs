//! Liquidity persistence traits and records.

use fiber_types::{
    EntityHex, Hash256, LiquidityAsset, LiquidityAssetError, LiquiditySwapState, Pubkey,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use thiserror::Error;

use crate::liquidity::types::LoopOutQuoteTerms;

pub use fiber_types::{LiquiditySwapKind, LiquiditySwapRecord, LiquiditySwapRole};

/// Persistence error returned by liquidity storage implementations.
#[derive(Debug, Error)]
pub enum LiquidityStoreError {
    /// The requested swap does not exist.
    #[error("liquidity swap not found: {0:?}")]
    SwapNotFound(Hash256),
    /// The requested asset does not exist.
    #[error("liquidity asset not found: {0}")]
    AssetNotFound(String),
    /// The requested asset is invalid.
    #[error("invalid liquidity asset: {0}")]
    InvalidAsset(#[from] LiquidityAssetError),
    /// The requested transition is not allowed by the liquidity state machine.
    #[error("invalid liquidity state transition from {from:?} to {to:?}")]
    InvalidStateTransition {
        /// Current persisted state.
        from: LiquiditySwapState,
        /// Requested next state.
        to: LiquiditySwapState,
    },
    /// Backend-specific persistence failure.
    #[error("liquidity store backend error: {0}")]
    Backend(String),
    /// Provider mode operation failed.
    #[error("provider mode operation failed: {0}")]
    ProviderModeError(String),
}

/// Filter for paginated swap history queries.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LiquiditySwapFilter {
    /// Optional state filter.
    pub state: Option<LiquiditySwapState>,
    /// Optional asset filter.
    pub asset_id: Option<String>,
    /// Maximum number of rows to return.
    pub limit: Option<u64>,
    /// Backend-defined pagination cursor.
    pub cursor: Option<String>,
}

/// Page returned by swap history queries.
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LiquiditySwapPage {
    /// Matching swap records.
    pub swaps: Vec<LiquiditySwapRecord>,
    /// Cursor for the next page, if more records are available.
    pub next_cursor: Option<String>,
}

pub(crate) fn loop_out_quote_record_from_terms(
    quote: LoopOutQuoteTerms,
    created_at: u64,
) -> fiber_types::LoopOutQuoteRecord {
    fiber_types::LoopOutQuoteRecord {
        quote_id: quote.quote_id,
        swap_kind: quote.swap_kind,
        provider: quote.provider,
        asset: quote.asset,
        amount: quote.amount,
        provider_fee: quote.provider_fee,
        routing_fee_limit: quote.routing_fee_limit,
        onchain_fee_estimate_ckb: quote.onchain_fee_estimate_ckb,
        capacity_requirement_ckb: quote.capacity_requirement_ckb,
        payment_hash: quote.payment_hash,
        expires_at: quote.expires_at,
        payout_deadline: quote.payout_deadline,
        refund_after_lock_time: quote.refund_after_lock_time,
        claimant_lock: quote.claimant_lock,
        refund_lock: quote.refund_lock,
        client_invoice: quote.client_invoice,
        created_at,
    }
}

pub(crate) fn loop_out_quote_terms_from_record(
    record: fiber_types::LoopOutQuoteRecord,
) -> LoopOutQuoteTerms {
    LoopOutQuoteTerms {
        quote_id: record.quote_id,
        swap_kind: record.swap_kind,
        provider: record.provider,
        asset: record.asset,
        amount: record.amount,
        provider_fee: record.provider_fee,
        routing_fee_limit: record.routing_fee_limit,
        onchain_fee_estimate_ckb: record.onchain_fee_estimate_ckb,
        capacity_requirement_ckb: record.capacity_requirement_ckb,
        payment_hash: record.payment_hash,
        expires_at: record.expires_at,
        payout_deadline: record.payout_deadline,
        refund_after_lock_time: record.refund_after_lock_time,
        claimant_lock: record.claimant_lock,
        refund_lock: record.refund_lock,
        client_invoice: record.client_invoice,
    }
}

#[serde_as]
#[derive(Deserialize, Serialize)]
struct QuoteRecordWithoutClientInvoice {
    quote_id: Hash256,
    #[serde(default = "default_loop_out_swap_kind_for_store")]
    swap_kind: fiber_types::LiquiditySwapKind,
    provider: Pubkey,
    asset: LiquidityAsset,
    amount: u128,
    provider_fee: u128,
    routing_fee_limit: u128,
    onchain_fee_estimate_ckb: u64,
    capacity_requirement_ckb: u64,
    payment_hash: Hash256,
    expires_at: u64,
    payout_deadline: u64,
    refund_after_lock_time: u64,
    #[serde_as(as = "EntityHex")]
    claimant_lock: ckb_types::packed::Script,
    #[serde_as(as = "EntityHex")]
    refund_lock: ckb_types::packed::Script,
    created_at: u64,
}

fn default_loop_out_swap_kind_for_store() -> fiber_types::LiquiditySwapKind {
    fiber_types::LiquiditySwapKind::LoopOut
}

impl From<QuoteRecordWithoutClientInvoice> for fiber_types::LoopOutQuoteRecord {
    fn from(record: QuoteRecordWithoutClientInvoice) -> Self {
        Self {
            quote_id: record.quote_id,
            swap_kind: record.swap_kind,
            provider: record.provider,
            asset: record.asset,
            amount: record.amount,
            provider_fee: record.provider_fee,
            routing_fee_limit: record.routing_fee_limit,
            onchain_fee_estimate_ckb: record.onchain_fee_estimate_ckb,
            capacity_requirement_ckb: record.capacity_requirement_ckb,
            payment_hash: record.payment_hash,
            expires_at: record.expires_at,
            payout_deadline: record.payout_deadline,
            refund_after_lock_time: record.refund_after_lock_time,
            claimant_lock: record.claimant_lock,
            refund_lock: record.refund_lock,
            client_invoice: None,
            created_at: record.created_at,
        }
    }
}

#[serde_as]
#[derive(Deserialize, Serialize)]
struct LegacyLoopOutQuoteRecord {
    quote_id: Hash256,
    provider: Pubkey,
    asset: LiquidityAsset,
    amount: u128,
    provider_fee: u128,
    routing_fee_limit: u128,
    onchain_fee_estimate_ckb: u64,
    capacity_requirement_ckb: u64,
    payment_hash: Hash256,
    expires_at: u64,
    payout_deadline: u64,
    refund_after_lock_time: u64,
    #[serde_as(as = "EntityHex")]
    claimant_lock: ckb_types::packed::Script,
    #[serde_as(as = "EntityHex")]
    refund_lock: ckb_types::packed::Script,
    created_at: u64,
}

impl From<LegacyLoopOutQuoteRecord> for fiber_types::LoopOutQuoteRecord {
    fn from(record: LegacyLoopOutQuoteRecord) -> Self {
        Self {
            quote_id: record.quote_id,
            swap_kind: fiber_types::LiquiditySwapKind::LoopOut,
            provider: record.provider,
            asset: record.asset,
            amount: record.amount,
            provider_fee: record.provider_fee,
            routing_fee_limit: record.routing_fee_limit,
            onchain_fee_estimate_ckb: record.onchain_fee_estimate_ckb,
            capacity_requirement_ckb: record.capacity_requirement_ckb,
            payment_hash: record.payment_hash,
            expires_at: record.expires_at,
            payout_deadline: record.payout_deadline,
            refund_after_lock_time: record.refund_after_lock_time,
            claimant_lock: record.claimant_lock,
            refund_lock: record.refund_lock,
            client_invoice: None,
            created_at: record.created_at,
        }
    }
}

pub(crate) fn loop_out_quote_record_from_bytes(
    value: &[u8],
) -> Result<fiber_types::LoopOutQuoteRecord, LiquidityStoreError> {
    bincode::deserialize::<fiber_types::LoopOutQuoteRecord>(value)
        .or_else(|_| bincode::deserialize::<QuoteRecordWithoutClientInvoice>(value).map(Into::into))
        .or_else(|_| bincode::deserialize::<LegacyLoopOutQuoteRecord>(value).map(Into::into))
        .map_err(|err| {
            LiquidityStoreError::Backend(format!(
                "deserialization of LoopOutQuoteRecord failed: {err}"
            ))
        })
}

/// State transition metadata persisted with each transition.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LiquidityStateTransition {
    /// Requested next state.
    pub state: LiquiditySwapState,
    /// Transition timestamp in milliseconds.
    pub updated_at: u64,
    /// Optional reason or recovery event description.
    pub reason: Option<String>,
}

/// Recovery fields that may become known after swap creation.
#[serde_as]
#[derive(Debug, Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LiquiditySwapUpdate {
    /// Known 32-byte preimage after payment settlement.
    pub payment_preimage: Option<Hash256>,
    /// On-chain lock or payout outpoint once observed or broadcast.
    #[serde_as(as = "Option<EntityHex>")]
    pub onchain_outpoint: Option<ckb_types::packed::OutPoint>,
    /// Failure reason for terminal failed swaps.
    pub failure_reason: Option<String>,
    /// Last update timestamp in milliseconds.
    pub updated_at: u64,
}

/// Store interface required by liquidity swap execution and recovery.
pub trait LiquidityStore {
    /// Persist provider-generated Loop Out quote terms.
    fn insert_loop_out_quote(
        &self,
        quote: LoopOutQuoteTerms,
        created_at: u64,
    ) -> Result<(), LiquidityStoreError>;

    /// Get provider-generated Loop Out quote terms by quote id.
    fn get_loop_out_quote(
        &self,
        quote_id: &Hash256,
    ) -> Result<Option<LoopOutQuoteTerms>, LiquidityStoreError>;

    /// Insert a new swap record.
    fn insert_liquidity_swap(&self, swap: LiquiditySwapRecord) -> Result<(), LiquidityStoreError>;

    /// Get a swap by local identifier.
    fn get_liquidity_swap(
        &self,
        swap_id: &Hash256,
    ) -> Result<Option<LiquiditySwapRecord>, LiquidityStoreError>;

    /// List swaps for history and recovery scans.
    fn list_liquidity_swaps(
        &self,
        filter: LiquiditySwapFilter,
    ) -> Result<LiquiditySwapPage, LiquidityStoreError>;

    /// List swaps matching any of the supplied states and swap kind for restart recovery.
    fn list_liquidity_swaps_by_states(
        &self,
        states: &[LiquiditySwapState],
        swap_kind: LiquiditySwapKind,
    ) -> Result<Vec<LiquiditySwapRecord>, LiquidityStoreError>;

    /// Transition a swap if allowed by the state machine.
    fn update_liquidity_swap_state(
        &self,
        swap_id: &Hash256,
        transition: LiquidityStateTransition,
    ) -> Result<(), LiquidityStoreError>;

    /// Persist recovery fields learned after swap creation.
    fn update_liquidity_swap(
        &self,
        swap_id: &Hash256,
        update: LiquiditySwapUpdate,
    ) -> Result<(), LiquidityStoreError>;

    /// Insert a liquidity CKB transaction identity record.
    fn insert_liquidity_chain_tx(
        &self,
        record: fiber_types::LiquidityChainTxRecord,
    ) -> Result<(), LiquidityStoreError>;

    /// Get a liquidity CKB transaction record by swap and role.
    fn get_liquidity_chain_tx(
        &self,
        swap_id: &Hash256,
        role: fiber_types::LiquidityChainTxRole,
    ) -> Result<Option<fiber_types::LiquidityChainTxRecord>, LiquidityStoreError>;

    /// Update a liquidity CKB transaction status and failure reason.
    fn update_liquidity_chain_tx_status(
        &self,
        swap_id: &Hash256,
        role: fiber_types::LiquidityChainTxRole,
        status: fiber_types::LiquidityChainTxStatus,
        failure_reason: Option<String>,
        updated_at: u64,
    ) -> Result<(), LiquidityStoreError>;

    /// List liquidity CKB transaction records matching any supplied status.
    fn list_liquidity_chain_txs_by_status(
        &self,
        statuses: &[fiber_types::LiquidityChainTxStatus],
    ) -> Result<Vec<fiber_types::LiquidityChainTxRecord>, LiquidityStoreError>;

    /// Persist the signed serialized transaction bytes for a liquidity CKB transaction.
    ///
    /// The bytes are stored under a dedicated key, keeping the
    /// [`fiber_types::LiquidityChainTxRecord`] bincode shape unchanged. Recovery reloads
    /// these bytes to rebroadcast a locally built transaction after restart.
    fn insert_liquidity_chain_tx_signed_tx(
        &self,
        swap_id: &Hash256,
        role: fiber_types::LiquidityChainTxRole,
        tx: ckb_types::packed::Transaction,
    ) -> Result<(), LiquidityStoreError>;

    /// Get the signed serialized transaction bytes for a liquidity CKB transaction.
    fn get_liquidity_chain_tx_signed_tx(
        &self,
        swap_id: &Hash256,
        role: fiber_types::LiquidityChainTxRole,
    ) -> Result<Option<ckb_types::packed::Transaction>, LiquidityStoreError>;

    /// Insert or update a provider asset registry entry.
    fn upsert_liquidity_asset(&self, asset: LiquidityAsset) -> Result<(), LiquidityStoreError>;

    /// Return a provider asset registry entry.
    fn get_liquidity_asset(
        &self,
        asset_id: &str,
    ) -> Result<Option<LiquidityAsset>, LiquidityStoreError>;

    /// List configured provider assets.
    fn list_liquidity_assets(&self) -> Result<Vec<LiquidityAsset>, LiquidityStoreError>;

    /// Set the global provider mode flag.
    fn set_provider_mode(&self, enabled: bool) -> Result<(), LiquidityStoreError>;

    /// Read the global provider mode flag (defaults to false if not set).
    fn get_provider_mode(&self) -> Result<bool, LiquidityStoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    use ckb_types::{packed::Byte32, packed::OutPoint, prelude::*};

    #[test]
    fn liquidity_swap_record_round_trips_through_bincode() {
        let outpoint = OutPoint::new(Byte32::from_slice(&[9u8; 32]).unwrap(), 1);
        let record = LiquiditySwapRecord {
            swap_id: [1u8; 32].into(),
            quote_id: [2u8; 32].into(),
            role: LiquiditySwapRole::Client,
            swap_kind: LiquiditySwapKind::LoopOut,
            asset_id: "ckb".to_string(),
            state: LiquiditySwapState::Created,
            payment_hash: [3u8; 32].into(),
            payment_preimage: Some([4u8; 32].into()),
            amount: 1000,
            onchain_outpoint: Some(outpoint),
            payout_deadline: Some(2000),
            refund_after_lock_time: 3000,
            expires_at: 4000,
            failure_reason: Some("failed".to_string()),
            created_at: 5000,
            updated_at: 6000,
        };

        let bytes = bincode::serialize(&record).expect("serialize record");
        let decoded: LiquiditySwapRecord =
            bincode::deserialize(&bytes).expect("deserialize record");

        assert_eq!(decoded, record);
    }

    #[test]
    fn legacy_loop_out_quote_record_defaults_to_loop_out_kind() {
        let private_key = secp256k1::SecretKey::from_slice(&[42u8; 32]).unwrap();
        let record = LegacyLoopOutQuoteRecord {
            quote_id: [1u8; 32].into(),
            provider: Pubkey::from(private_key.public_key(secp256k1::SECP256K1)),
            asset: LiquidityAsset {
                asset_id: "ckb".to_string(),
                kind: fiber_types::LiquidityAssetKind::Ckb,
                udt_type_script: None,
                min_amount: 1,
                max_amount: 1_000,
                available_capacity: 1_000,
                base_fee: 1,
                proportional_fee_ppm: 0,
                enabled: true,
            },
            amount: 100,
            provider_fee: 1,
            routing_fee_limit: 1,
            onchain_fee_estimate_ckb: 1_000,
            capacity_requirement_ckb: 10_000,
            payment_hash: [2u8; 32].into(),
            expires_at: 20_000,
            payout_deadline: 30_000,
            refund_after_lock_time: 40_000,
            claimant_lock: Default::default(),
            refund_lock: Default::default(),
            created_at: 1_000,
        };
        let bytes = bincode::serialize(&record).unwrap();

        let decoded = loop_out_quote_record_from_bytes(&bytes).unwrap();

        assert_eq!(decoded.swap_kind, LiquiditySwapKind::LoopOut);
        assert_eq!(decoded.quote_id, record.quote_id);
    }

    #[test]
    fn loop_in_quote_record_round_trips_client_invoice() {
        let private_key = secp256k1::SecretKey::from_slice(&[42u8; 32]).unwrap();
        let terms = LoopOutQuoteTerms {
            quote_id: [1u8; 32].into(),
            swap_kind: LiquiditySwapKind::LoopIn,
            provider: Pubkey::from(private_key.public_key(secp256k1::SECP256K1)),
            asset: LiquidityAsset {
                asset_id: "ckb".to_string(),
                kind: fiber_types::LiquidityAssetKind::Ckb,
                udt_type_script: None,
                min_amount: 1,
                max_amount: 1_000,
                available_capacity: 1_000,
                base_fee: 1,
                proportional_fee_ppm: 0,
                enabled: true,
            },
            amount: 100,
            provider_fee: 1,
            routing_fee_limit: 0,
            onchain_fee_estimate_ckb: 1_000,
            capacity_requirement_ckb: 10_000,
            payment_hash: [2u8; 32].into(),
            expires_at: 20_000,
            payout_deadline: 20_000,
            refund_after_lock_time: 40_000,
            claimant_lock: Default::default(),
            refund_lock: Default::default(),
            client_invoice: Some("lnbc-client-invoice".to_string()),
        };

        let record = loop_out_quote_record_from_terms(terms.clone(), 1_000);
        let decoded = loop_out_quote_terms_from_record(record);

        assert_eq!(decoded.client_invoice, terms.client_invoice);
    }
}
