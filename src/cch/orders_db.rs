use std::collections::HashMap;

use super::{error::CchDbError, CchOrderStatus, SendBTCOrder};

// TODO: persist orders
#[derive(Default)]
pub struct CchOrdersDb {
    /// SendBTCOrder map by payment hash
    send_btc_orders: HashMap<String, SendBTCOrder>,
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

    pub async fn update_send_btc_order(
        &mut self,
        payment_hash: &str,
        payment_preimage: Option<String>,
        status: CchOrderStatus,
    ) -> Result<(), CchDbError> {
        let payment_mut = self
            .send_btc_orders
            .get_mut(payment_hash)
            .ok_or_else(|| CchDbError::NotFound(payment_hash.to_string()))?;
        if payment_preimage.is_some() {
            payment_mut.payment_preimage = payment_preimage;
        }
        payment_mut.status = status;
        Ok(())
    }
}
