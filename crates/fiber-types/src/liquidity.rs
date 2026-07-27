//! Liquidity management domain types.

use crate::serde_utils::EntityHex;
use crate::{Hash256, Pubkey};

use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use thiserror::Error;

/// Asset family supported by the liquidity protocol.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LiquidityAssetKind {
    /// Native CKB capacity denominated in shannons.
    Ckb,
    /// User-defined token identified by a provider whitelist entry.
    Udt,
}

/// Validation error for provider asset registry entries.
#[derive(Debug, Error, Copy, Clone, Eq, PartialEq)]
pub enum LiquidityAssetError {
    /// UDT assets must include a type script.
    #[error("UDT liquidity asset is missing type script")]
    MissingUdtTypeScript,
    /// CKB assets must not include a UDT type script.
    #[error("CKB liquidity asset must not include UDT type script")]
    UnexpectedUdtTypeScript,
    /// Minimum amount must not exceed maximum amount.
    #[error("liquidity asset minimum amount exceeds maximum amount")]
    InvalidAmountRange,
}

/// Local role for a persisted liquidity swap.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum LiquiditySwapRole {
    /// Local node initiated the swap.
    Client,
    /// Local node provided liquidity for the swap.
    Provider,
}

/// Direction of a persisted liquidity swap.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum LiquiditySwapKind {
    /// Move Fiber balance out to chain.
    LoopOut,
    /// Move chain funds into Fiber balance.
    LoopIn,
}

/// Provider asset registry entry.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct LiquidityAsset {
    /// Stable provider-local identifier used by quote and swap RPCs.
    pub asset_id: String,
    /// Asset family.
    pub kind: LiquidityAssetKind,
    /// Required when `kind` is `Udt`; absent when `kind` is `Ckb`.
    pub udt_type_script: Option<ckb_jsonrpc_types::Script>,
    /// Smallest raw swap amount accepted by the provider.
    pub min_amount: u128,
    /// Largest raw swap amount accepted by the provider.
    pub max_amount: u128,
    /// Provider-advertised capacity for this asset.
    pub available_capacity: u128,
    /// Fixed provider fee charged in the swapped asset.
    pub base_fee: u128,
    /// Proportional provider fee in parts per million.
    pub proportional_fee_ppm: u64,
    /// Whether the provider currently quotes this asset.
    pub enabled: bool,
}

/// Persisted provider Loop Out quote fields required to reconstruct accepted terms.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoopOutQuoteRecord {
    /// Unique provider quote identifier.
    pub quote_id: Hash256,
    /// Quoted swap direction.
    #[serde(default = "default_loop_out_swap_kind")]
    pub swap_kind: LiquiditySwapKind,
    /// Provider node public key.
    pub provider: Pubkey,
    /// Asset being swapped out.
    pub asset: LiquidityAsset,
    /// Net amount the client wants to receive on-chain.
    pub amount: u128,
    /// Provider fee charged in the quoted asset unit.
    pub provider_fee: u128,
    /// Maximum Fiber routing fee the client accepts.
    pub routing_fee_limit: u128,
    /// Estimated CKB transaction fee for the on-chain operation.
    pub onchain_fee_estimate_ckb: u64,
    /// CKB capacity required for the payout lock output.
    pub capacity_requirement_ckb: u64,
    /// Payment hash used for the Fiber payment and on-chain lock.
    pub payment_hash: Hash256,
    /// Quote expiration timestamp in milliseconds.
    pub expires_at: u64,
    /// Deadline timestamp by which the payout lock must be confirmed.
    pub payout_deadline: u64,
    /// Lock time after which the provider may refund the payout lock.
    pub refund_after_lock_time: u64,
    /// Client claimant lock used for the claim transaction.
    #[serde_as(as = "EntityHex")]
    pub claimant_lock: ckb_types::packed::Script,
    /// Provider refund lock used if the swap is not paid and claimed.
    #[serde_as(as = "EntityHex")]
    pub refund_lock: ckb_types::packed::Script,
    /// Creation timestamp in milliseconds.
    pub created_at: u64,
}

fn default_loop_out_swap_kind() -> LiquiditySwapKind {
    LiquiditySwapKind::LoopOut
}

/// Persisted liquidity swap record needed for restart recovery.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
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
    #[serde_as(as = "Option<EntityHex>")]
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

/// Liquidity CKB transaction role within a swap.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum LiquidityChainTxRole {
    /// Provider payout lock transaction.
    Payout,
    /// Client claim transaction.
    Claim,
    /// Provider refund transaction.
    Refund,
}

/// Persisted lifecycle status for a liquidity CKB transaction.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum LiquidityChainTxStatus {
    /// Transaction identity is planned and persisted before broadcast.
    Planned,
    /// Transaction was submitted to the CKB actor.
    Broadcast,
    /// Transaction reached the required confirmations.
    Confirmed,
    /// Transaction was rejected or failed to broadcast.
    Rejected,
}

/// Persisted CKB transaction identity for liquidity swap recovery.
#[serde_as]
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct LiquidityChainTxRecord {
    /// Swap that owns the transaction.
    pub swap_id: Hash256,
    /// Transaction role within the swap.
    pub role: LiquidityChainTxRole,
    /// CKB transaction hash.
    pub tx_hash: Hash256,
    /// Created output, if the transaction creates one tracked by recovery.
    #[serde_as(as = "Option<EntityHex>")]
    pub outpoint: Option<ckb_types::packed::OutPoint>,
    /// Current transaction status.
    pub status: LiquidityChainTxStatus,
    /// Optional failure reason for rejected/failed transactions.
    pub failure_reason: Option<String>,
    /// Creation timestamp in milliseconds.
    pub created_at: u64,
    /// Last update timestamp in milliseconds.
    pub updated_at: u64,
}

impl LiquidityAsset {
    /// Validate this registry entry against the M0 asset rules.
    pub fn validate(&self) -> Result<(), LiquidityAssetError> {
        match (self.kind, self.udt_type_script.is_some()) {
            (LiquidityAssetKind::Udt, false) => Err(LiquidityAssetError::MissingUdtTypeScript),
            (LiquidityAssetKind::Ckb, true) => Err(LiquidityAssetError::UnexpectedUdtTypeScript),
            _ if self.min_amount > self.max_amount => Err(LiquidityAssetError::InvalidAmountRange),
            _ => Ok(()),
        }
    }
}

/// Shared liquidity swap lifecycle state.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub enum LiquiditySwapState {
    /// Local order record exists before external side effects.
    Created,
    /// Provider quote is accepted and capacity is reserved.
    Quoted,
    /// Loop In client on-chain lock transaction is broadcast but not confirmed.
    OnchainLockPending,
    /// Loop In client on-chain lock is confirmed.
    OnchainLocked,
    /// Loop Out provider payout lock transaction is broadcast but not confirmed.
    PayoutPending,
    /// Loop Out provider payout lock is confirmed.
    PayoutLocked,
    /// Fiber payment has been sent and is waiting for result.
    PaymentInFlight,
    /// Fiber payment settled and the 32-byte preimage is known.
    PaymentSettled,
    /// Claim transaction is broadcast but not confirmed.
    ClaimPending,
    /// Refund transaction is broadcast but not confirmed.
    RefundPending,
    /// Swap completed successfully.
    Success,
    /// Swap failed before funds were locked in a way that requires refund.
    Failed,
    /// Swap failed and locked funds were returned through refund.
    Refunded,
}

impl LiquiditySwapState {
    /// Return whether a state transition is allowed by the M0 state machine.
    pub fn can_transition_to(self, to: Self) -> bool {
        use LiquiditySwapState::*;

        matches!(
            (self, to),
            (Created, Quoted)
                | (Quoted, OnchainLockPending)
                | (Quoted, PayoutPending)
                | (OnchainLockPending, OnchainLocked)
                | (OnchainLocked, PaymentInFlight)
                | (PayoutPending, PayoutLocked)
                | (PayoutLocked, PaymentInFlight)
                | (PaymentInFlight, PaymentSettled)
                | (PaymentInFlight, Failed)
                | (OnchainLockPending, Failed)
                | (PaymentSettled, ClaimPending)
                | (ClaimPending, Success)
                | (OnchainLockPending, RefundPending)
                | (OnchainLocked, RefundPending)
                | (PayoutPending, RefundPending)
                | (PayoutLocked, RefundPending)
                | (PaymentInFlight, RefundPending)
                | (RefundPending, Refunded)
        )
    }

    /// Return whether this state is terminal.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Success | Self::Failed | Self::Refunded)
    }
}

#[cfg(test)]
mod tests {
    use ckb_jsonrpc_types::Script;

    use super::*;

    fn sample_script() -> Script {
        serde_json::from_value(serde_json::json!({
            "code_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "hash_type": "type",
            "args": "0x01"
        }))
        .expect("script")
    }

    #[test]
    fn liquidity_asset_validation_requires_udt_type_script() {
        let asset = LiquidityAsset {
            asset_id: "udt-btc".to_string(),
            kind: LiquidityAssetKind::Udt,
            udt_type_script: None,
            min_amount: 1,
            max_amount: 100,
            available_capacity: 1000,
            base_fee: 1,
            proportional_fee_ppm: 100,
            enabled: true,
        };

        assert_eq!(
            asset.validate(),
            Err(LiquidityAssetError::MissingUdtTypeScript)
        );
    }

    #[test]
    fn liquidity_asset_validation_rejects_ckb_type_script() {
        let asset = LiquidityAsset {
            asset_id: "ckb".to_string(),
            kind: LiquidityAssetKind::Ckb,
            udt_type_script: Some(sample_script()),
            min_amount: 1,
            max_amount: 100,
            available_capacity: 1000,
            base_fee: 1,
            proportional_fee_ppm: 100,
            enabled: true,
        };

        assert_eq!(
            asset.validate(),
            Err(LiquidityAssetError::UnexpectedUdtTypeScript)
        );
    }

    #[test]
    fn liquidity_state_machine_requires_claim_before_success() {
        assert!(
            LiquiditySwapState::PaymentSettled.can_transition_to(LiquiditySwapState::ClaimPending)
        );
        assert!(!LiquiditySwapState::PaymentSettled.can_transition_to(LiquiditySwapState::Success));
        assert!(!LiquiditySwapState::PaymentSettled
            .can_transition_to(LiquiditySwapState::PaymentSettled));
        assert!(LiquiditySwapState::ClaimPending.can_transition_to(LiquiditySwapState::Success));
    }

    #[test]
    fn liquidity_asset_kind_serializes_as_snake_case() {
        let value = serde_json::to_value(LiquidityAssetKind::Ckb).expect("json");
        assert_eq!(value, "ckb");
    }
}
