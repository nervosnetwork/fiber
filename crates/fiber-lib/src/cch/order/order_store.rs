use crate::cch::error::CchStoreError;
use fiber_types::{CchOrder, CchReceiveBtcOrderCreation, Hash256};

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
    fn update_cch_order(&self, order: CchOrder);

    /// Get an iterator to list keys of all orders.
    fn get_cch_order_keys_iter(&self) -> impl IntoIterator<Item = Hash256>;

    /// Deletes an order from the store.
    fn delete_cch_order(&self, payment_hash: &Hash256);

    /// Gets a durable `receive_btc` creation intent by payment hash.
    fn get_receive_btc_order_creation(
        &self,
        payment_hash: &Hash256,
    ) -> Result<CchReceiveBtcOrderCreation, CchStoreError>;

    /// Inserts a durable `receive_btc` creation intent.
    fn insert_receive_btc_order_creation(
        &self,
        creation: CchReceiveBtcOrderCreation,
    ) -> Result<(), CchStoreError>;

    /// Iterates over all durable `receive_btc` creation intent keys.
    fn get_receive_btc_order_creation_keys_iter(&self) -> impl IntoIterator<Item = Hash256>;

    /// Atomically replaces a durable creation intent with its completed order.
    fn complete_receive_btc_order_creation(&self, order: CchOrder) -> Result<(), CchStoreError>;

    /// Deletes a durable `receive_btc` creation intent.
    fn delete_receive_btc_order_creation(&self, payment_hash: &Hash256);
}
