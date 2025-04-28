use std::collections::HashMap;

use super::{error::CchDbError, ReceiveBTCOrder, SendBTCOrder};

// TODO: persist orders
#[derive(Default)]
pub struct CchOrdersDb {
    /// SendBTCOrder map by payment hash
    send_btc_orders: HashMap<String, SendBTCOrder>,
    receive_btc_orders: HashMap<String, ReceiveBTCOrder>,
}

impl CchOrdersDb {
    pub async fn insert_send_btc_order(&mut self, order: SendBTCOrder) -> Result<(), CchDbError> {
        let key = order.payment_hash.clone();
        match self.send_btc_orders.insert(key.clone(), order) {
            Some(_) => Err(CchDbError::Duplicated(key)),
            None => Ok(()),
        }
    }

    pub async fn get_send_btc_order(
        &mut self,
        payment_hash: &str,
    ) -> Result<SendBTCOrder, CchDbError> {
        self.send_btc_orders
            .get(payment_hash)
            .ok_or_else(|| CchDbError::NotFound(payment_hash.to_string()))
            .cloned()
    }

    pub async fn update_send_btc_order(&mut self, order: SendBTCOrder) -> Result<(), CchDbError> {
        let key = order.payment_hash.clone();
        match self.send_btc_orders.insert(key.clone(), order) {
            Some(_) => Ok(()),
            None => Err(CchDbError::NotFound(key)),
        }
    }

    pub async fn insert_receive_btc_order(
        &mut self,
        order: ReceiveBTCOrder,
    ) -> Result<(), CchDbError> {
        let key = order.payment_hash.clone();
        match self.receive_btc_orders.insert(key.clone(), order) {
            Some(_) => Err(CchDbError::Duplicated(key)),
            None => Ok(()),
        }
    }

    pub async fn get_receive_btc_order(
        &mut self,
        payment_hash: &str,
    ) -> Result<ReceiveBTCOrder, CchDbError> {
        self.receive_btc_orders
            .get(payment_hash)
            .ok_or_else(|| CchDbError::NotFound(payment_hash.to_string()))
            .cloned()
    }

    pub async fn update_receive_btc_order(
        &mut self,
        order: ReceiveBTCOrder,
    ) -> Result<(), CchDbError> {
        let key = order.payment_hash.clone();
        match self.receive_btc_orders.insert(key.clone(), order) {
            Some(_) => Ok(()),
            None => Err(CchDbError::NotFound(key)),
        }
    }
}
