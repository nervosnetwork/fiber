use crate::{cch::error::CchStoreError, fiber::types::Hash256};

use super::CchOrder;

pub trait CchOrderStore {
    /// Gets an order from the store.
    ///
    /// Returns the order if found, a NotFound error otherwise.
    fn get_cch_order(&self, payment_hash: &Hash256) -> Result<CchOrder, CchStoreError>;

    /// Inserts a new order.
    ///
    /// Returns an error if an order with the same payment hash already exist in the database.
    fn insert_cch_order(&self, order: CchOrder) -> Result<(), CchStoreError>;

    /// Inserts or updates an order.
    ///
    /// Inserts
    fn update_cch_order(&self, order: CchOrder);

    /// Get an iterator to list keys of all orders.
    fn get_cch_order_keys_iter(&self) -> impl IntoIterator<Item = Hash256>;
}

/// Used for delegating the store trait
pub trait CchOrderStoreDeref {
    type Target: CchOrderStore;
    fn cch_order_store_deref(&self) -> &Self::Target;
}

impl<T: CchOrderStoreDeref> CchOrderStore for T {
    fn get_cch_order(&self, payment_hash: &Hash256) -> Result<CchOrder, CchStoreError> {
        self.cch_order_store_deref().get_cch_order(payment_hash)
    }

    fn insert_cch_order(&self, order: CchOrder) -> Result<(), CchStoreError> {
        self.cch_order_store_deref().insert_cch_order(order)
    }

    fn update_cch_order(&self, order: CchOrder) {
        self.cch_order_store_deref().update_cch_order(order);
    }

    /// Get an iterator to list keys of all orders.
    fn get_cch_order_keys_iter(&self) -> impl IntoIterator<Item = Hash256> {
        self.cch_order_store_deref().get_cch_order_keys_iter()
    }
}
