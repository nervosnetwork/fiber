use jsonrpsee::proc_macros::rpc;

#[cfg(feature = "watchtower")]
use jsonrpsee::types::ErrorObjectOwned;

#[cfg(feature = "watchtower")]
use crate::rpc::utils::{rpc_error, RpcResultExt};
#[cfg(feature = "watchtower")]
use crate::watchtower::WatchtowerStore;
#[cfg(feature = "watchtower")]
pub use fiber_json_types::RpcContext;
#[cfg(feature = "watchtower")]
use fiber_types::{NodeId, Pubkey};

pub use fiber_json_types::{
    CreatePreimageParams, CreateWatchChannelParams, GetWatchtowerSigningStatusParams,
    GetWatchtowerSigningStatusResult, RemovePreimageParams, RemoveWatchChannelParams,
    SubmitWatchtowerSignatureParams, SubmitWatchtowerSignatureResult, UpdateLocalSettlementParams,
    UpdatePendingRemoteSettlementParams, UpdateRevocationParams, WatchtowerSigningStatus,
};

/// RPC module for watchtower related operations
#[cfg(feature = "watchtower")]
#[rpc(server)]
trait WatchtowerRpc {
    /// Create a new watched channel.
    ///
    /// Supplying `local_settlement_key` leaves the settlement secret on the
    /// watchtower. Omit the private key and pass `local_settlement_key_pubkey`
    /// for an externally signed channel. Settlement then pauses until the
    /// owner submits a signature through `get_watchtower_signing_status` and
    /// `submit_watchtower_signature`.
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

    /// Read the current external watchtower signing status for a watched channel.
    #[method(name = "get_watchtower_signing_status")]
    async fn get_watchtower_signing_status(
        &self,
        ctx: RpcContext,
        params: GetWatchtowerSigningStatusParams,
    ) -> Result<GetWatchtowerSigningStatusResult, ErrorObjectOwned>;

    /// Submit an external watchtower settlement or TLC signature.
    #[method(name = "submit_watchtower_signature")]
    async fn submit_watchtower_signature(
        &self,
        ctx: RpcContext,
        params: SubmitWatchtowerSignatureParams,
    ) -> Result<SubmitWatchtowerSignatureResult, ErrorObjectOwned>;
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

    /// Read the current external watchtower signing status for a watched channel.
    #[method(name = "get_watchtower_signing_status")]
    async fn get_watchtower_signing_status(
        &self,
        params: GetWatchtowerSigningStatusParams,
    ) -> Result<GetWatchtowerSigningStatusResult, ErrorObjectOwned>;

    /// Submit an external watchtower settlement or TLC signature.
    #[method(name = "submit_watchtower_signature")]
    async fn submit_watchtower_signature(
        &self,
        params: SubmitWatchtowerSignatureParams,
    ) -> Result<SubmitWatchtowerSignatureResult, ErrorObjectOwned>;
}

#[cfg(feature = "watchtower")]
pub struct WatchtowerRpcServerImpl<S> {
    store: S,
    signer_actor: Option<ractor::ActorRef<crate::fiber::signer_actor::SignerActorMessage>>,
}

#[cfg(feature = "watchtower")]
impl<S> WatchtowerRpcServerImpl<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            signer_actor: None,
        }
    }

    pub fn with_signer_actor(
        mut self,
        signer_actor: Option<ractor::ActorRef<crate::fiber::signer_actor::SignerActorMessage>>,
    ) -> Self {
        self.signer_actor = signer_actor;
        self
    }
}

#[cfg(feature = "watchtower")]
fn authorized_watchtower_node_id(ctx: &RpcContext) -> Result<NodeId, ErrorObjectOwned> {
    let node_id = ctx.node_id.parse::<NodeId>().rpc_err()?;
    if ctx.tenant_scoped && node_id == NodeId::local() {
        return Err(rpc_error(
            "tenant token cannot access the host watchtower namespace",
        ));
    }
    Ok(node_id)
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
        let node_id = authorized_watchtower_node_id(&ctx)?;
        let channel_id = params.channel_id.into();
        let local_settlement_key = params
            .local_settlement_key
            .map(TryInto::try_into)
            .transpose()
            .map_err(rpc_error)?;
        let supplied_local_settlement_pubkey = params
            .local_settlement_key_pubkey
            .map(Pubkey::try_from)
            .transpose()
            .rpc_err()?;
        let local_settlement_key_pubkey = supplied_local_settlement_pubkey
            .or_else(|| {
                local_settlement_key
                    .as_ref()
                    .map(fiber_types::Privkey::pubkey)
            })
            .ok_or_else(|| {
                rpc_error(
                    "local_settlement_key_pubkey is required when local_settlement_key is omitted",
                )
            })?;
        if local_settlement_key
            .as_ref()
            .is_some_and(|key| key.pubkey() != local_settlement_key_pubkey)
        {
            return Err(rpc_error(
                "local_settlement_key_pubkey does not match local_settlement_key",
            ));
        }
        let remote_settlement_key = Pubkey::try_from(params.remote_settlement_key).rpc_err()?;
        let local_funding_pubkey = Pubkey::try_from(params.local_funding_pubkey).rpc_err()?;
        let remote_funding_pubkey = Pubkey::try_from(params.remote_funding_pubkey).rpc_err()?;
        // Move fields out of params last, after all borrows of params are done.
        let funding_udt_type_script = params.funding_udt_type_script;
        let settlement_data: fiber_types::SettlementData = params
            .settlement_data
            .try_into()
            .map_err(|e: String| rpc_error(e))?;
        self.store.insert_watch_channel(
            node_id,
            channel_id,
            funding_udt_type_script.map(Into::into),
            local_settlement_key,
            local_settlement_key_pubkey,
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
        let node_id = authorized_watchtower_node_id(&ctx)?;
        let channel_id = params.channel_id.into();
        self.store.remove_watch_channel(node_id, channel_id);
        Ok(())
    }

    async fn update_revocation(
        &self,
        ctx: RpcContext,
        params: UpdateRevocationParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = authorized_watchtower_node_id(&ctx)?;
        let channel_id = params.channel_id.into();
        let revocation_data: fiber_types::RevocationData = params
            .revocation_data
            .try_into()
            .map_err(|e: String| rpc_error(e))?;
        let settlement_data: fiber_types::SettlementData = params
            .settlement_data
            .try_into()
            .map_err(|e: String| rpc_error(e))?;
        self.store
            .update_revocation(node_id, channel_id, revocation_data, settlement_data);
        Ok(())
    }

    async fn update_pending_remote_settlement(
        &self,
        ctx: RpcContext,
        params: UpdatePendingRemoteSettlementParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = authorized_watchtower_node_id(&ctx)?;
        let channel_id = params.channel_id.into();
        let settlement_data: fiber_types::SettlementData = params
            .settlement_data
            .try_into()
            .map_err(|e: String| rpc_error(e))?;
        self.store
            .update_pending_remote_settlement(node_id, channel_id, settlement_data);
        Ok(())
    }

    async fn update_local_settlement(
        &self,
        ctx: RpcContext,
        params: UpdateLocalSettlementParams,
    ) -> Result<(), ErrorObjectOwned> {
        let node_id = authorized_watchtower_node_id(&ctx)?;
        let channel_id = params.channel_id.into();
        let settlement_data: fiber_types::SettlementData = params
            .settlement_data
            .try_into()
            .map_err(|e: String| rpc_error(e))?;
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

        let node_id = authorized_watchtower_node_id(&ctx)?;
        let payment_hash = params.payment_hash.into();
        let preimage = params.preimage.into();

        if HashAlgorithm::supported_algorithms()
            .iter()
            .all(|algorithm| payment_hash != algorithm.hash(preimage).into())
        {
            return Err(rpc_error("Wrong preimage"));
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
        let node_id = authorized_watchtower_node_id(&ctx)?;
        let payment_hash = params.payment_hash.into();
        self.store.remove_watch_preimage(node_id, payment_hash);
        Ok(())
    }

    async fn get_watchtower_signing_status(
        &self,
        ctx: RpcContext,
        params: GetWatchtowerSigningStatusParams,
    ) -> Result<GetWatchtowerSigningStatusResult, ErrorObjectOwned> {
        let node_id = authorized_watchtower_node_id(&ctx)?;
        let channel_id: fiber_types::Hash256 = params.channel_id.into();
        if self
            .store
            .get_watch_channel(&node_id, &channel_id)
            .is_none()
        {
            return Err(rpc_error("watched channel not found"));
        }
        let status = to_rpc_watchtower_signing_status(
            self.store.get_watchtower_signer(&node_id, &channel_id),
        );
        Ok(GetWatchtowerSigningStatusResult {
            channel_id: channel_id.into(),
            status,
        })
    }

    async fn submit_watchtower_signature(
        &self,
        ctx: RpcContext,
        params: SubmitWatchtowerSignatureParams,
    ) -> Result<SubmitWatchtowerSignatureResult, ErrorObjectOwned> {
        use fiber_types::{
            LastAppliedWatchtowerSignature, WatchtowerExternalState, WatchtowerSignerState,
        };

        let node_id = authorized_watchtower_node_id(&ctx)?;
        let channel_id: fiber_types::Hash256 = params.channel_id.into();
        let request_id: fiber_types::Hash256 = params.request_id.into();
        let signature: [u8; 65] = params
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| rpc_error("watchtower signature must be 65 bytes"))?;
        if self
            .store
            .get_watch_channel(&node_id, &channel_id)
            .is_none()
        {
            return Err(rpc_error("watched channel not found"));
        }
        if let Some(signer_actor) = self.signer_actor.as_ref() {
            return ractor::call!(signer_actor, |rpc_reply| {
                crate::fiber::signer_actor::SignerActorMessage::SubmitWatchtowerSignature {
                    node_id,
                    channel_id,
                    request_id,
                    signature,
                    rpc_reply: Some(rpc_reply),
                }
            })
            .map_err(|error| rpc_error(error.to_string()))?
            .map_err(rpc_error);
        }
        let current = self.store.get_watchtower_signer(&node_id, &channel_id);
        let WatchtowerSignerState::External(mut external) = current else {
            return Err(rpc_error("watched channel does not use an external signer"));
        };
        if external
            .last_applied
            .as_ref()
            .is_some_and(|applied| applied.request_id == request_id)
        {
            if external
                .last_applied
                .as_ref()
                .is_some_and(|applied| applied.signature == signature)
            {
                return Ok(SubmitWatchtowerSignatureResult::AlreadyApplied);
            }
            return Err(rpc_error(
                "submitted signature does not match the previously applied result",
            ));
        }
        let WatchtowerExternalState::AwaitingSignature {
            request_id: expected,
            content,
        } = external.state
        else {
            return Err(rpc_error(
                "watched channel is not waiting for an external signature",
            ));
        };
        if expected != request_id {
            return Err(rpc_error(
                "signature request id does not match the current request",
            ));
        }
        external.last_applied = Some(LastAppliedWatchtowerSignature {
            request_id,
            signature,
        });
        external.state = WatchtowerExternalState::Signed {
            request_id,
            content,
            signature,
        };
        self.store.put_watchtower_signer(
            &node_id,
            &channel_id,
            WatchtowerSignerState::External(external),
        );
        if let Some(signer_actor) = self.signer_actor.as_ref() {
            let _ = signer_actor.send_message(
                crate::fiber::signer_actor::SignerActorMessage::ClearWatchtowerPending {
                    node_id,
                    channel_id,
                    request_id,
                },
            );
        }
        Ok(SubmitWatchtowerSignatureResult::Applied)
    }
}

#[cfg(feature = "watchtower")]
fn to_rpc_watchtower_signing_status(
    state: fiber_types::WatchtowerSignerState,
) -> WatchtowerSigningStatus {
    use fiber_types::{OnchainKeyPurpose, WatchtowerExternalState, WatchtowerSignerState};

    match state {
        WatchtowerSignerState::Internal => WatchtowerSigningStatus::Internal,
        WatchtowerSignerState::External(external) => match external.state {
            WatchtowerExternalState::Ready | WatchtowerExternalState::Signed { .. } => {
                WatchtowerSigningStatus::NoSignatureRequired
            }
            WatchtowerExternalState::AwaitingSignature {
                request_id,
                content,
            } => WatchtowerSigningStatus::SignatureRequired {
                request_id: request_id.into(),
                content: fiber_json_types::OnchainSigningContent {
                    key_purpose: match content.key_purpose {
                        OnchainKeyPurpose::Settlement => {
                            fiber_json_types::OnchainKeyPurpose::Settlement
                        }
                        OnchainKeyPurpose::Tlc { commitment_number } => {
                            fiber_json_types::OnchainKeyPurpose::Tlc { commitment_number }
                        }
                    },
                    transaction: content.transaction.into(),
                },
            },
        },
    }
}

#[cfg(all(test, feature = "watchtower"))]
mod tests {
    use super::{authorized_watchtower_node_id, RpcContext};
    use fiber_types::NodeId;

    #[test]
    fn tenant_context_cannot_use_the_host_namespace() {
        let ctx = RpcContext {
            node_id: NodeId::local().to_string(),
            tenant_scoped: true,
        };
        assert!(authorized_watchtower_node_id(&ctx)
            .unwrap_err()
            .message()
            .contains("host watchtower namespace"));
    }

    #[test]
    fn tenant_context_keeps_its_own_node() {
        let node_id = NodeId::from_bytes(vec![1, 2, 3]);
        let ctx = RpcContext {
            node_id: node_id.to_string(),
            tenant_scoped: true,
        };
        assert_eq!(authorized_watchtower_node_id(&ctx).unwrap(), node_id);
    }
}
