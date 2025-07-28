use jsonrpsee::proc_macros::rpc;
#[cfg(feature = "watchtower")]
use jsonrpsee::types::ErrorObjectOwned;

use ckb_jsonrpc_types::Script;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::fiber::{
    channel::{RevocationData, SettlementData},
    types::Hash256,
};

#[cfg(feature = "watchtower")]
use crate::{invoice::PreimageStore, watchtower::WatchtowerStore};

/// RPC module for watchtower related operations
#[cfg_attr(feature = "watchtower", rpc(client, server))]
#[cfg_attr(not(feature = "watchtower"), rpc(client))]
trait WatchtowerRpc {
    /// Create a new watched channel
    #[method(name = "create_watch_channel")]
    async fn create_watch_channel(
        &self,
        params: CreateWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Remove a watched channel
    #[method(name = "remove_watch_channel")]
    async fn remove_watch_channel(
        &self,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Update revocation
    #[method(name = "update_revocation")]
    async fn update_revocation(
        &self,
        params: UpdateRevocationParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Update settlement
    #[method(name = "update_local_settlement")]
    async fn update_local_settlement(
        &self,
        params: UpdateLocalSettlementParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Create preimage
    #[method(name = "create_preimage")]
    async fn create_preimage(&self, params: CreatePreimageParams) -> Result<(), ErrorObjectOwned>;

    /// Remove preimage
    #[method(name = "remove_preimage")]
    async fn remove_preimage(&self, params: RemovePreimageParams) -> Result<(), ErrorObjectOwned>;
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreateWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Channel funding transaction lock script
    pub funding_tx_lock: Script,
    /// Remote settlement data
    pub remote_settlement_data: SettlementData,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoveWatchChannelParams {
    /// Channel ID
    pub channel_id: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UpdateRevocationParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Revocation data
    pub revocation_data: RevocationData,
    /// Settlement data
    pub settlement_data: SettlementData,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UpdateLocalSettlementParams {
    /// Channel ID
    pub channel_id: Hash256,
    /// Settlement data
    pub settlement_data: SettlementData,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CreatePreimageParams {
    /// Payment hash
    pub payment_hash: Hash256,
    /// Preimage
    pub preimage: Hash256,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemovePreimageParams {
    /// Payment hash
    pub payment_hash: Hash256,
}

#[cfg(feature = "watchtower")]
pub struct WatchtowerRpcServerImpl<S> {
    store: S,
}

#[cfg(feature = "watchtower")]
impl<S> WatchtowerRpcServerImpl<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

#[cfg(feature = "watchtower")]
#[async_trait::async_trait]
impl<S> WatchtowerRpcServer for WatchtowerRpcServerImpl<S>
where
    S: PreimageStore + WatchtowerStore + Send + Sync + 'static,
{
    async fn create_watch_channel(
        &self,
        params: CreateWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        self.store.insert_watch_channel(
            params.channel_id,
            params.funding_tx_lock.into(),
            params.remote_settlement_data,
        );
        Ok(())
    }

    async fn remove_watch_channel(
        &self,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        self.store.remove_watch_channel(params.channel_id);
        Ok(())
    }

    async fn update_revocation(
        &self,
        params: UpdateRevocationParams,
    ) -> Result<(), ErrorObjectOwned> {
        self.store.update_revocation(
            params.channel_id,
            params.revocation_data,
            params.settlement_data,
        );
        Ok(())
    }

    async fn update_local_settlement(
        &self,
        params: UpdateLocalSettlementParams,
    ) -> Result<(), ErrorObjectOwned> {
        self.store
            .update_local_settlement(params.channel_id, params.settlement_data);
        Ok(())
    }

    async fn create_preimage(&self, params: CreatePreimageParams) -> Result<(), ErrorObjectOwned> {
        self.store
            .insert_preimage(params.payment_hash, params.preimage);
        Ok(())
    }
    async fn remove_preimage(&self, params: RemovePreimageParams) -> Result<(), ErrorObjectOwned> {
        self.store.remove_preimage(&params.payment_hash);
        Ok(())
    }
}
