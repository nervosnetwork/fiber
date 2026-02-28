use crate::fiber::types::Hash256;

/// Status of a TLC as reported by the watchtower
#[derive(Clone)]
pub struct TlcWatchtowerStatus {
    /// The preimage for the payment hash, if known by the watchtower
    pub preimage: Option<Hash256>,
    /// Whether the TLC has been settled on chain
    pub is_settled: bool,
}

/// Trait for querying watchtower status of TLCs.
/// Implemented by direct store access (built-in watchtower) and RPC client (standalone watchtower).
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait WatchtowerQuerier: Send + Sync {
    /// Query the watchtower for the status of a TLC identified by channel_id and payment_hash.
    /// Returns `None` if the watchtower is unreachable or encounters an error.
    async fn query_tlc_status(
        &self,
        channel_id: &Hash256,
        payment_hash: &Hash256,
    ) -> Option<TlcWatchtowerStatus>;
}
