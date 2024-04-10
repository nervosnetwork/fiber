use std::collections::HashMap;

use super::{error::CchDbError, SendBTCOrder};

// TODO: persist orders
#[derive(Default)]
pub struct CchOrdersDb {
    /// SendBTCOrder map by payment hash
    send_btc_orders: HashMap<String, SendBTCOrder>,
}

impl CchOrdersDb {
    pub async fn insert_send_btc_order(&mut self, order: SendBTCOrder) -> Result<(), CchDbError> {
        self.send_btc_orders
            .insert(order.payment_hash.clone(), order);
        Ok(())
    }

    pub async fn get_send_btc_order(
        &mut self,
        payment_hash: &str,
    ) -> Result<Option<SendBTCOrder>, CchDbError> {
        Ok(self.send_btc_orders.get(payment_hash).cloned())
    }
}
