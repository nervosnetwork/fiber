mod funding_tx;

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
