mod funding_tx;
mod limits;

pub(crate) use limits::validate_peer_funding_tx_complexity;

#[cfg(test)]
pub(crate) use limits::{
    MAX_PEER_ADDED_CELL_DEPS, MAX_PEER_ADDED_FUNDING_INPUTS, MAX_PEER_ADDED_FUNDING_OUTPUTS,
    MAX_PEER_FUNDING_TX_SERIALIZED_SIZE,
};

#[cfg(test)]
pub(crate) use funding_tx::map_tx_builder_error;
pub(crate) use funding_tx::{
    is_secp_sighash_placeholder_witness, FundingContext, LiveCellsExclusionMap,
};
#[cfg(test)]
pub(crate) use funding_tx::{
    secp_sighash_placeholder_witness, FundingTxBuilder, SECP_SIGHASH_PLACEHOLDER_SIGNATURE_BYTES,
};
pub use funding_tx::{FundingRequest, FundingTx};
