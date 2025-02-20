use crate::fiber::types::Hash256;

use super::{error::CchDbError, CchOrder};

pub trait CchOrderStore {
    fn create_cch_order(&self, order: CchOrder) -> Result<(), CchDbError>;

    fn get_cch_order(&self, payment_hash: &Hash256) -> Result<CchOrder, CchDbError>;

    fn update_cch_order(&self, order: CchOrder) -> Result<(), CchDbError>;

    fn get_cch_orders(&self) -> Result<impl IntoIterator<Item = CchOrder>, CchDbError>;

    fn get_active_cch_orders(&self) -> Result<impl IntoIterator<Item = CchOrder>, CchDbError> {
        self.get_cch_orders()
            .map(|orders| orders.into_iter().filter(|order| !order.is_finalized()))
    }
}
