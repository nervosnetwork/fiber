#[cfg(not(target_arch = "wasm32"))]
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;

use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{fiber::types::Hash256, watchtower::WatchtowerStore};

/// RPC module for watchtower related operations
#[cfg(not(target_arch = "wasm32"))]
#[rpc(server)]
trait WatchtowerRpc {
    /// Remove a watched channel from the watchtower store
    #[method(name = "remove_watch_channel")]
    async fn remove_watch_channel(
        &self,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned>;
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoveWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
}

pub struct WatchtowerRpcServerImpl<S> {
    store: S,
}

impl<S> WatchtowerRpcServerImpl<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait::async_trait]
impl<S> WatchtowerRpcServer for WatchtowerRpcServerImpl<S>
where
    S: WatchtowerStore + Send + Sync + 'static,
{
    async fn remove_watch_channel(
        &self,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        self.remove_watch_channel(params).await
    }
}

impl<S> WatchtowerRpcServerImpl<S>
where
    S: WatchtowerStore + Send + Sync + 'static,
{
    async fn remove_watch_channel(
        &self,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        self.store.remove_watch_channel(params.channel_id);
        Ok(())
    }
}
