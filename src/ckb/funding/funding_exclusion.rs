use std::collections::HashMap;

use ckb_sdk::traits::CellCollector;
use ckb_types::{core::TransactionView, packed::Byte32};
use tracing::error;

use crate::ckb::FundingError;

use super::FundingTx;

#[derive(Default, Clone, Debug)]
pub struct FundingExclusion {
    pending_funding_txs: HashMap<Byte32, TransactionView>,
}

impl FundingExclusion {
    pub fn insert(&mut self, tx: FundingTx) {
        if let Some(tx) = tx.into_inner() {
            let tx_hash = tx.hash();
            self.pending_funding_txs.insert(tx_hash, tx);
        }
    }

    pub fn remove(&mut self, tx_hash: &Byte32) {
        self.pending_funding_txs.remove(tx_hash);
    }

    pub fn apply_to_cell_collector(
        &self,
        collector: &mut dyn CellCollector,
    ) -> Result<(), FundingError> {
        for tx in self.pending_funding_txs.values() {
            collector.apply_tx(tx.data(), u64::MAX).map_err(|err| {
                error!("Failed to apply exclusion list to cell collector: {}", err);
                FundingError::FundingExclusionError
            })?;
        }
        Ok(())
    }
}
