use std::collections::HashMap;
use thiserror::Error;

use crate::fiber::types::Hash256;

use super::CchOrder;

#[derive(Error, Debug)]
pub enum CchDbError {
    #[error("Inserting duplicated key: {0}")]
    Duplicated(String),

    #[error("Key not found: {0}")]
    NotFound(String),
}

// TODO: persist orders
#[derive(Default)]
pub struct CchOrdersDb {
    /// Orders by payment hash
    orders: HashMap<Hash256, CchOrder>,
}

impl CchOrdersDb {
    pub fn insert_cch_order(&mut self, order: CchOrder) -> Result<(), CchDbError> {
        let key = order.payment_hash;
        match self.orders.insert(key, order) {
            Some(_) => Err(CchDbError::Duplicated(key.to_string())),
            None => Ok(()),
        }
    }

    pub fn get_cch_order(&mut self, payment_hash: &Hash256) -> Result<CchOrder, CchDbError> {
        self.orders
            .get(payment_hash)
            .ok_or_else(|| CchDbError::NotFound(payment_hash.to_string()))
            .cloned()
    }

    pub fn update_cch_order(&mut self, order: CchOrder) -> Result<(), CchDbError> {
        let key = order.payment_hash;
        match self.orders.insert(key, order) {
            Some(_) => Ok(()),
            None => Err(CchDbError::NotFound(key.to_string())),
        }
    }
}
