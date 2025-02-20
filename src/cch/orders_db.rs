use std::{collections::HashMap, sync::Arc};

use tokio::sync::RwLock;

use crate::fiber::types::Hash256;

use super::{error::CchDbError, CchOrder};

// TODO: persist orders
#[derive(Default, Clone)]
pub struct CchOrdersDb {
    cch_orders: Arc<RwLock<HashMap<Hash256, CchOrder>>>,
}

impl CchOrdersDb {
    pub async fn insert_cch_order(&self, order: CchOrder) -> Result<(), CchDbError> {
        let key = order.payment_hash;
        let orders_db = self.cch_orders.clone();
        let mut cch_orders = orders_db.write().await;
        match cch_orders.insert(key, order) {
            Some(_) => Err(CchDbError::Duplicated(key)),
            None => Ok(()),
        }
    }

    pub async fn get_cch_order(&self, payment_hash: &Hash256) -> Result<CchOrder, CchDbError> {
        let cch_orders = self.cch_orders.read().await;
        cch_orders
            .get(payment_hash)
            .ok_or(CchDbError::NotFound(*payment_hash))
            .cloned()
    }

    pub async fn update_cch_order(&self, order: CchOrder) -> Result<(), CchDbError> {
        let key = order.payment_hash;
        let orders_db = self.cch_orders.clone();
        let mut cch_orders = orders_db.write().await;
        match cch_orders.insert(key, order) {
            Some(_) => Ok(()),
            None => Err(CchDbError::NotFound(key)),
        }
    }
}
