use std::collections::HashMap;

use crate::fiber::types::Hash256;

use super::{error::CchDbError, CchOrder};

// TODO: persist orders
#[derive(Default)]
pub struct CchOrdersDb(HashMap<Hash256, CchOrder>);

impl CchOrdersDb {
    pub async fn insert_order(&mut self, order: CchOrder) -> Result<(), CchDbError> {
        let key = order.payment_hash;
        match self.0.insert(key, order) {
            Some(_) => Err(CchDbError::Duplicated(key)),
            None => Ok(()),
        }
    }

    pub async fn get_order(&mut self, payment_hash: &Hash256) -> Result<CchOrder, CchDbError> {
        self.0
            .get(payment_hash)
            .ok_or(CchDbError::NotFound(*payment_hash))
            .cloned()
    }

    pub async fn update_order(&mut self, order: CchOrder) -> Result<(), CchDbError> {
        let key = order.payment_hash;
        match self.0.insert(key, order) {
            Some(_) => Ok(()),
            None => Err(CchDbError::NotFound(key)),
        }
    }
}
