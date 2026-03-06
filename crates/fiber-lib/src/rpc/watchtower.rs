use jsonrpsee::proc_macros::rpc;

#[cfg(feature = "watchtower")]
use jsonrpsee::types::ErrorObjectOwned;

#[cfg(feature = "watchtower")]
use crate::rpc::context::RpcContext;
#[cfg(feature = "watchtower")]
use crate::rpc::utils::{rpc_error, rpc_error_no_data, RpcResultExt};
#[cfg(feature = "watchtower")]
use crate::watchtower::WatchtowerStore;
#[cfg(feature = "watchtower")]
use fiber_types::{Hash256, NodeId, Pubkey};
#[cfg(feature = "watchtower")]
use std::convert::TryFrom;

pub use fiber_json_types::{
    CreatePreimageParams, CreateWatchChannelParams, RemovePreimageParams, RemoveWatchChannelParams,
    UpdateLocalSettlementParams, UpdatePendingRemoteSettlementParams, UpdateRevocationParams,
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

    /// Update pending remote settlement
    #[method(name = "update_pending_remote_settlement")]
    async fn update_pending_remote_settlement(
        &self,
        ctx: RpcContext,
        params: UpdatePendingRemoteSettlementParams,
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

/// ignore rpc-doc-gen
/// RPC client
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

    /// Update pending remote settlement
    #[method(name = "update_pending_remote_settlement")]
    async fn update_pending_remote_settlement(
        &self,
        params: UpdatePendingRemoteSettlementParams,
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
    S: WatchtowerStore + Send + Sync + 'static,
{
    async fn create_watch_channel(
        &self,
        ctx: RpcContext,
        params: CreateWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = ctx.node_id.parse::<NodeId>().rpc_err_no_data()?;
        let channel_id = Hash256::from(&params.channel_id);
        let local_settlement_key_bytes =
            hex::decode(&params.local_settlement_key).rpc_err(&params)?;
        let local_settlement_key: fiber_types::Privkey =
            <[u8; 32]>::try_from(local_settlement_key_bytes.as_slice())
                .map_err(|_| rpc_error("invalid local_settlement_key length", &params))?
                .into();
        let remote_settlement_key =
            Pubkey::try_from(&params.remote_settlement_key).rpc_err(&params)?;
        let local_funding_pubkey =
            Pubkey::try_from(&params.local_funding_pubkey).rpc_err(&params)?;
        let remote_funding_pubkey =
            Pubkey::try_from(&params.remote_funding_pubkey).rpc_err(&params)?;
        let settlement_data =
            fiber_types::SettlementData::try_from(&params.settlement_data).rpc_err(&params)?;
        self.store.insert_watch_channel(
            node_id,
            channel_id,
            params.funding_udt_type_script.map(Into::into),
            local_settlement_key,
            remote_settlement_key,
            local_funding_pubkey,
            remote_funding_pubkey,
            settlement_data,
        );
        Ok(())
    }

    async fn remove_watch_channel(
        &self,
        ctx: RpcContext,
        params: RemoveWatchChannelParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = ctx.node_id.parse::<NodeId>().rpc_err_no_data()?;
        let channel_id = Hash256::from(&params.channel_id);
        self.store.remove_watch_channel(node_id, channel_id);
        Ok(())
    }

    async fn update_revocation(
        &self,
        ctx: RpcContext,
        params: UpdateRevocationParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = ctx.node_id.parse::<NodeId>().rpc_err_no_data()?;
        let channel_id = Hash256::from(&params.channel_id);
        let revocation_data =
            fiber_types::RevocationData::try_from(&params.revocation_data).rpc_err(&params)?;
        let settlement_data =
            fiber_types::SettlementData::try_from(&params.settlement_data).rpc_err(&params)?;
        self.store
            .update_revocation(node_id, channel_id, revocation_data, settlement_data);
        Ok(())
    }

    async fn update_pending_remote_settlement(
        &self,
        ctx: RpcContext,
        params: UpdatePendingRemoteSettlementParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = ctx.node_id.parse::<NodeId>().rpc_err_no_data()?;
        let channel_id = Hash256::from(&params.channel_id);
        let settlement_data =
            fiber_types::SettlementData::try_from(&params.settlement_data).rpc_err(&params)?;
        self.store
            .update_pending_remote_settlement(node_id, channel_id, settlement_data);
        Ok(())
    }

    async fn update_local_settlement(
        &self,
        ctx: RpcContext,
        params: UpdateLocalSettlementParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = ctx.node_id.parse::<NodeId>().rpc_err_no_data()?;
        let channel_id = Hash256::from(&params.channel_id);
        let settlement_data =
            fiber_types::SettlementData::try_from(&params.settlement_data).rpc_err(&params)?;
        self.store
            .update_local_settlement(node_id, channel_id, settlement_data);
        Ok(())
    }

    async fn create_preimage(
        &self,
        ctx: RpcContext,
        params: CreatePreimageParams,
    ) -> Result<(), ErrorObjectOwned> {
        use fiber_types::HashAlgorithm;

        let node_id = ctx.node_id.parse::<NodeId>().rpc_err_no_data()?;
        let payment_hash = Hash256::from(&params.payment_hash);
        let preimage = Hash256::from(&params.preimage);

        if HashAlgorithm::supported_algorithms()
            .iter()
            .all(|algorithm| payment_hash != algorithm.hash(preimage).into())
        {
            return Err(rpc_error_no_data("Wrong preimage"));
        }
        self.store
            .insert_watch_preimage(node_id, payment_hash, preimage);
        Ok(())
    }
    async fn remove_preimage(
        &self,
        ctx: RpcContext,
        params: RemovePreimageParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = ctx.node_id.parse::<NodeId>().rpc_err_no_data()?;
        let payment_hash = Hash256::from(&params.payment_hash);
        self.store.remove_watch_preimage(node_id, payment_hash);
        Ok(())
    }
}
