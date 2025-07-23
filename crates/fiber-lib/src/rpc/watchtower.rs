use ckb_jsonrpc_types::Script;
use jsonrpsee::{
    proc_macros::rpc,
    types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned},
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{
    fiber::{
        channel::{RevocationData, SettlementData},
        types::Hash256,
    },
    invoice::PreimageStore,
    rpc::context::RpcContext,
    watchtower::WatchtowerStore,
};

/// RPC module for watchtower related operations
#[cfg(feature = "watchtower")]
#[rpc(server)]
trait WatchtowerRpc {
    /// Create a new watched channel
    #[method(name = "create_watch_channel")]
    async fn create_watch_channel(
        &self,
        ctx: RpcContext,
        params: CreateWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Remove a watched channel
    #[method(name = "remove_watch_channel")]
    async fn remove_watch_channel(
        &self,
        ctx: RpcContext,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Update revocation
    #[method(name = "update_revocation")]
    async fn update_revocation(
        &self,
        ctx: RpcContext,
        params: UpdateRevocationParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Update settlement
    #[method(name = "update_local_settlement")]
    async fn update_local_settlement(
        &self,
        ctx: RpcContext,
        params: UpdateLocalSettlementParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Create preimage
    #[method(name = "create_preimage")]
    async fn create_preimage(
        &self,
        ctx: RpcContext,
        params: CreatePreimageParams,
    ) -> Result<(), ErrorObjectOwned>;

    /// Remove preimage
    #[method(name = "remove_preimage")]
    async fn remove_preimage(
        &self,
        ctx: RpcContext,
        params: RemovePreimageParams,
    ) -> Result<(), ErrorObjectOwned>;
}

#[rpc(client)]
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

fn requrie_node_id_err() -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        CALL_EXECUTION_FAILED_CODE,
        "Require node_id, please check biscuit token",
        Option::<()>::None,
    )
}

#[cfg(feature = "watchtower")]
#[async_trait::async_trait]
impl<S> WatchtowerRpcServer for WatchtowerRpcServerImpl<S>
where
    S: PreimageStore + WatchtowerStore + Send + Sync + 'static,
{
    async fn create_watch_channel(
        &self,
        ctx: RpcContext,
        params: CreateWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let Some(node_id) = ctx.node_id else {
            return Err(requrie_node_id_err());
        };
        self.store.insert_watch_channel(
            node_id,
            params.channel_id,
            params.funding_tx_lock.into(),
            params.remote_settlement_data,
        );
        Ok(())
    }

    async fn remove_watch_channel(
        &self,
        ctx: RpcContext,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let Some(node_id) = ctx.node_id else {
            return Err(requrie_node_id_err());
        };
        self.store.remove_watch_channel(node_id, params.channel_id);
        Ok(())
    }

    async fn update_revocation(
        &self,
        ctx: RpcContext,
        params: UpdateRevocationParams,
    ) -> Result<(), ErrorObjectOwned> {
        let Some(node_id) = ctx.node_id else {
            return Err(requrie_node_id_err());
        };
        self.store.update_revocation(
            node_id,
            params.channel_id,
            params.revocation_data,
            params.settlement_data,
        );
        Ok(())
    }

    async fn update_local_settlement(
        &self,
        ctx: RpcContext,
        params: UpdateLocalSettlementParams,
    ) -> Result<(), ErrorObjectOwned> {
        let Some(node_id) = ctx.node_id else {
            return Err(requrie_node_id_err());
        };
        self.store
            .update_local_settlement(node_id, params.channel_id, params.settlement_data);
        Ok(())
    }

    async fn create_preimage(
        &self,
        ctx: RpcContext,
        params: CreatePreimageParams,
    ) -> Result<(), ErrorObjectOwned> {
        let Some(node_id) = ctx.node_id else {
            return Err(requrie_node_id_err());
        };
        self.store
            .insert_watch_preimage(node_id, params.payment_hash, params.preimage);
        Ok(())
    }
    async fn remove_preimage(
        &self,
        ctx: RpcContext,
        params: RemovePreimageParams,
    ) -> Result<(), ErrorObjectOwned> {
        let Some(node_id) = ctx.node_id else {
            return Err(requrie_node_id_err());
        };
        self.store
            .remove_watch_preimage(node_id, params.payment_hash);
        Ok(())
    }
}
