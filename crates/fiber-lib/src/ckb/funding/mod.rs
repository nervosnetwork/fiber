mod funding_tx;

#[cfg(test)]
pub(crate) use funding_tx::map_tx_builder_error;
pub(crate) use funding_tx::{
    is_secp_sighash_placeholder_witness, FundingContext, LiveCellsExclusionMap,
};
pub use funding_tx::{FundingRequest, FundingTx};
