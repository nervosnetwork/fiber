use std::collections::HashMap;

use ckb_types::{core::TransactionView, packed::Byte32};

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
}
