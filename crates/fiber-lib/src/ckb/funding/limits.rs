//! Complexity budgets for peer-supplied funding transactions.
//!
//! During funding collaboration a remote peer sends a `TxUpdate` carrying a full
//! CKB transaction. Verifying that transaction requires one `get_live_cell` RPC
//! per peer-added input, so an unbounded `TxUpdate` lets a single P2P message
//! amplify into many serialized CKB RPC lookups while the channel actor blocks.
//! The constants below bound how much a peer may add relative to the local
//! funding transaction, and [`validate_peer_funding_tx_complexity`] enforces them
//! using only cheap structural checks, before any chain lookup is performed.
//!
//! # Future work
//!
//! 1. Make these limits user-configurable. Promote the constants to fields on a
//!    `PeerFundingLimits` struct with a `Default` matching today's values, surface
//!    them through the node config (mirroring the `gossip_policy` rate-limit
//!    configs), and thread the resolved config into `FundingContext` so the
//!    validator reads operator-tuned values. Operators with heavy UDT
//!    consolidation could then raise the input cap deliberately.
//! 2. Advertise/negotiate the limits with peers. Today they are unilateral and a
//!    violating peer only learns about them via `TxAbort` after the fact. Future
//!    protocol work could publish the accepted maxima in node announcements or as
//!    channel negotiation parameters exchanged in `OpenChannel`/`AcceptChannel`,
//!    so the funding initiator can build a compliant `TxUpdate` up front. This
//!    requires a spec change and backward-compatible defaults for peers that do
//!    not advertise the field.

use ckb_types::core::TransactionView;

use crate::ckb::error::FundingError;

/// Maximum serialized size (in block) accepted for a peer funding transaction.
///
/// This mirrors the P2P frame cap `MAX_SERVICE_PROTOCOAL_DATA_SIZE`
/// (`1024 * (128 + 2)`) in `fiber::network`; a transaction cannot exceed the
/// frame that carried it. It is duplicated here to keep the `ckb` layer
/// independent of `fiber::network`.
pub(crate) const MAX_PEER_FUNDING_TX_SERIALIZED_SIZE: usize = 1024 * (128 + 2);

/// Maximum number of inputs a peer may add beyond the local funding tx.
///
/// Each peer-added input triggers one `get_live_cell` RPC, so this directly
/// bounds the RPC fan-out per `TxUpdate`. The value comfortably covers UDT
/// consolidation and multi-cell peer funding while keeping the work bounded.
pub(crate) const MAX_PEER_ADDED_FUNDING_INPUTS: usize = 64;

/// Maximum number of outputs a peer may add beyond the local funding tx.
///
/// When the local node still has the default empty funding tx, a peer who funds
/// first may add up to three outputs:
///
/// 1. The funding cell.
/// 2. A UDT change output, when the peer spends a UDT cell that holds more
///    tokens than the channel requires.
/// 3. A CKB change output, when the peer adds CKB inputs to pay transaction
///    fees.
///
/// CKB-only channels use (1) and (3) only. UDT channels may use all three.
/// Peers are recommended to merge the UDT and CKB change into a single cell
/// when possible, but the current funding builder emits them separately.
pub(crate) const MAX_PEER_ADDED_FUNDING_OUTPUTS: usize = 3;

/// Maximum number of cell deps a peer may add beyond the local funding tx.
///
/// Covers UDT and external-funding lock cell deps, far above what legitimate
/// dual funding requires.
pub(crate) const MAX_PEER_ADDED_CELL_DEPS: usize = 8;

/// Validate that the peer's funding transaction does not exceed the allowed
/// complexity relative to the local funding transaction.
///
/// All checks here are cheap and structural; they must run before any
/// `get_live_cell` RPC so a malicious `TxUpdate` cannot amplify into many chain
/// lookups. Every violation returns [`FundingError::PeerFundingTxExceedsLimit`]
/// with a message naming the breached budget, which is relayed to the peer via
/// `TxAbort`.
///
/// Limits are expressed as peer-added deltas against the local funding tx. When
/// the local node still has the default empty funding tx, those deltas equal the
/// remote transaction totals.
pub(crate) fn validate_peer_funding_tx_complexity(
    local_tx: &TransactionView,
    remote_tx: &TransactionView,
) -> Result<(), FundingError> {
    let serialized_size = remote_tx.data().serialized_size_in_block();
    if serialized_size > MAX_PEER_FUNDING_TX_SERIALIZED_SIZE {
        return Err(FundingError::PeerFundingTxExceedsLimit(format!(
            "serialized size {} > {}",
            serialized_size, MAX_PEER_FUNDING_TX_SERIALIZED_SIZE
        )));
    }

    let added_inputs = remote_tx
        .inputs()
        .len()
        .saturating_sub(local_tx.inputs().len());
    if added_inputs > MAX_PEER_ADDED_FUNDING_INPUTS {
        return Err(FundingError::PeerFundingTxExceedsLimit(format!(
            "peer-added inputs {} > {}",
            added_inputs, MAX_PEER_ADDED_FUNDING_INPUTS
        )));
    }

    let added_outputs = remote_tx
        .outputs()
        .len()
        .saturating_sub(local_tx.outputs().len());
    if added_outputs > MAX_PEER_ADDED_FUNDING_OUTPUTS {
        return Err(FundingError::PeerFundingTxExceedsLimit(format!(
            "peer-added outputs {} > {}",
            added_outputs, MAX_PEER_ADDED_FUNDING_OUTPUTS
        )));
    }

    let added_cell_deps = remote_tx
        .cell_deps()
        .len()
        .saturating_sub(local_tx.cell_deps().len());
    if added_cell_deps > MAX_PEER_ADDED_CELL_DEPS {
        return Err(FundingError::PeerFundingTxExceedsLimit(format!(
            "peer-added cell deps {} > {}",
            added_cell_deps, MAX_PEER_ADDED_CELL_DEPS
        )));
    }

    // Witnesses are intentionally not capped here: their total cost is already
    // bounded by the serialized transaction size limit above.

    Ok(())
}
