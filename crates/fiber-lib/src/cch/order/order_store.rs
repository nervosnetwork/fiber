use crate::cch::error::CchStoreError;
use fiber_types::{CchOrder, Hash256, SwapProposal};

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

    /// Gets a pending swap proposal from the store.
    ///
    /// Returns the proposal if found, a NotFound error otherwise. Pending
    /// proposals live in a table separate from orders: no [`CchOrder`] exists
    /// until the proposal is accepted.
    fn get_cch_pending_proposal(
        &self,
        payment_hash: &Hash256,
    ) -> Result<SwapProposal, CchStoreError>;

    /// Inserts a new pending proposal.
    ///
    /// Returns an error if a proposal with the same payment hash already exists.
    fn insert_cch_pending_proposal(&self, proposal: SwapProposal) -> Result<(), CchStoreError>;

    /// Get an iterator to list keys of all pending proposals. Used on startup
    /// to re-broadcast proposals and re-arm their timeouts.
    fn get_cch_pending_proposal_keys_iter(&self) -> impl IntoIterator<Item = Hash256>;

    /// Deletes a pending proposal from the store. Called once the proposal is
    /// accepted (and an order is created) or rejected/timed out.
    fn delete_cch_pending_proposal(&self, payment_hash: &Hash256);
}
