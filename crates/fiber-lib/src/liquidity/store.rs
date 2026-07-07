//! Liquidity persistence traits and records.

use fiber_types::{Hash256, LiquidityAsset, LiquiditySwapState};
use thiserror::Error;

/// Local role for a persisted liquidity swap.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum LiquiditySwapRole {
    /// Local node initiated the swap.
    Client,
    /// Local node provided liquidity for the swap.
    Provider,
}

/// Direction of a persisted liquidity swap.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum LiquiditySwapKind {
    /// Move Fiber balance out to chain.
    LoopOut,
    /// Move chain funds into Fiber balance.
    LoopIn,
}

/// Persistence error returned by liquidity storage implementations.
#[derive(Debug, Error)]
pub enum LiquidityStoreError {
    /// The requested swap does not exist.
    #[error("liquidity swap not found: {0:?}")]
    SwapNotFound(Hash256),
    /// The requested asset does not exist.
    #[error("liquidity asset not found: {0}")]
    AssetNotFound(String),
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
}

/// Persisted liquidity swap record needed for restart recovery.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LiquiditySwapRecord {
    /// Local swap identifier.
    pub swap_id: Hash256,
    /// Provider quote identifier.
    pub quote_id: Hash256,
    /// Local role in this swap.
    pub role: LiquiditySwapRole,
    /// Swap direction.
    pub swap_kind: LiquiditySwapKind,
    /// Provider asset registry identifier.
    pub asset_id: String,
    /// Current recovery state.
    pub state: LiquiditySwapState,
    /// CKB-hash of the 32-byte payment preimage.
    pub payment_hash: Hash256,
    /// Known 32-byte preimage after payment settlement.
    pub payment_preimage: Option<Hash256>,
    /// Raw swap amount.
    pub amount: u128,
    /// On-chain lock or payout outpoint once known.
    pub onchain_outpoint: Option<ckb_types::packed::OutPoint>,
    /// Loop Out payout confirmation deadline.
    pub payout_deadline: Option<u64>,
    /// Refund lock time encoded in the liquidity-lock args.
    pub refund_after_lock_time: u64,
    /// Quote expiry timestamp in milliseconds.
    pub expires_at: u64,
    /// Failure reason for terminal failed swaps.
    pub failure_reason: Option<String>,
    /// Creation timestamp in milliseconds.
    pub created_at: u64,
    /// Last update timestamp in milliseconds.
    pub updated_at: u64,
}

/// Filter for paginated swap history queries.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
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
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct LiquiditySwapPage {
    /// Matching swap records.
    pub swaps: Vec<LiquiditySwapRecord>,
    /// Cursor for the next page, if more records are available.
    pub next_cursor: Option<String>,
}

/// State transition metadata persisted with each transition.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LiquidityStateTransition {
    /// Requested next state.
    pub state: LiquiditySwapState,
    /// Transition timestamp in milliseconds.
    pub updated_at: u64,
    /// Optional reason or recovery event description.
    pub reason: Option<String>,
}

/// Recovery fields that may become known after swap creation.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct LiquiditySwapUpdate {
    /// Known 32-byte preimage after payment settlement.
    pub payment_preimage: Option<Hash256>,
    /// On-chain lock or payout outpoint once observed or broadcast.
    pub onchain_outpoint: Option<ckb_types::packed::OutPoint>,
    /// Failure reason for terminal failed swaps.
    pub failure_reason: Option<String>,
    /// Last update timestamp in milliseconds.
    pub updated_at: u64,
}

/// Store interface required by liquidity swap execution and recovery.
pub trait LiquidityStore {
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

    /// Insert or update a provider asset registry entry.
    fn upsert_liquidity_asset(&self, asset: LiquidityAsset) -> Result<(), LiquidityStoreError>;

    /// Return a provider asset registry entry.
    fn get_liquidity_asset(
        &self,
        asset_id: &str,
    ) -> Result<Option<LiquidityAsset>, LiquidityStoreError>;

    /// List configured provider assets.
    fn list_liquidity_assets(&self) -> Result<Vec<LiquidityAsset>, LiquidityStoreError>;
}
