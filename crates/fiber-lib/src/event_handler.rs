use std::fmt::Debug;

use crate::rpc::watchtower::{
    CreatePreimageParams, CreateWatchChannelParams, RemovePreimageParams, RemoveWatchChannelParams,
    UpdateLocalSettlementParams, UpdatePendingRemoteSettlementParams, UpdateRevocationParams,
    WatchtowerRpcClient,
};
use crate::NetworkServiceEvent;

/// A message indicating that the node should exit with an error.
///
/// Used by both native (`fiber-bin`) and WASM (`fiber-wasm`) entry points
/// as the error type for the main function.
pub struct ExitMessage(pub String);

impl Debug for ExitMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Exit because {}", self.0)
    }
}

impl ExitMessage {
    pub fn err(message: String) -> Result<(), ExitMessage> {
        Err(ExitMessage(message))
    }
}

/// Forward a [`NetworkServiceEvent`] to a remote watchtower via its RPC client.
///
/// This is the shared implementation used by both native and WASM entry points.
/// The concrete client type differs per platform (HTTP client vs WASM client),
/// but both implement the generated [`WatchtowerRpcClient`] trait.
///
/// Returns an error string if the RPC call fails, allowing the caller to log the
/// failure without panicking and killing the event-processing task.
pub async fn forward_event_to_client<T: WatchtowerRpcClient + Sync>(
    event: NetworkServiceEvent,
    watchtower_client: &T,
) -> Result<(), String> {
    match event {
        NetworkServiceEvent::RemoteTxComplete(
            _peer_id,
            channel_id,
            funding_udt_type_script,
            local_settlement_key,
            local_settlement_key_pubkey,
            remote_settlement_key,
            local_funding_pubkey,
            remote_funding_pubkey,
            settlement_data,
        ) => {
            watchtower_client
                .create_watch_channel(CreateWatchChannelParams {
                    channel_id: channel_id.into(),
                    funding_udt_type_script: funding_udt_type_script.map(Into::into),
                    local_settlement_key: local_settlement_key
                        .map(|key| key.0.secret_bytes().into()),
                    local_settlement_key_pubkey: Some(local_settlement_key_pubkey.into()),
                    remote_settlement_key: remote_settlement_key.into(),
                    local_funding_pubkey: local_funding_pubkey.into(),
                    remote_funding_pubkey: remote_funding_pubkey.into(),
                    settlement_data: settlement_data.into(),
                })
                .await
                .map_err(|e| format!("Failed to create watch channel: {e}"))?;
        }
        NetworkServiceEvent::ChannelClosed(_, channel_id, _)
        | NetworkServiceEvent::ChannelAbandon(channel_id)
        | NetworkServiceEvent::ChannelFundingAborted(channel_id) => {
            watchtower_client
                .remove_watch_channel(RemoveWatchChannelParams {
                    channel_id: channel_id.into(),
                })
                .await
                .map_err(|e| format!("Failed to remove watch channel: {e}"))?;
        }
        NetworkServiceEvent::RevokeAndAckReceived(
            _peer_id,
            channel_id,
            revocation_data,
            settlement_data,
        ) => {
            watchtower_client
                .update_revocation(UpdateRevocationParams {
                    channel_id: channel_id.into(),
                    revocation_data: revocation_data.into(),
                    settlement_data: settlement_data.into(),
                })
                .await
                .map_err(|e| format!("Failed to update revocation: {e}"))?;
        }
        NetworkServiceEvent::RemoteCommitmentSigned(
            _peer_id,
            channel_id,
            _commitment_tx,
            settlement_data,
        ) => {
            watchtower_client
                .update_local_settlement(UpdateLocalSettlementParams {
                    channel_id: channel_id.into(),
                    settlement_data: settlement_data.into(),
                })
                .await
                .map_err(|e| format!("Failed to update local settlement: {e}"))?;
        }
        NetworkServiceEvent::LocalCommitmentSigned(channel_id, settlement_data) => {
            watchtower_client
                .update_pending_remote_settlement(UpdatePendingRemoteSettlementParams {
                    channel_id: channel_id.into(),
                    settlement_data: settlement_data.into(),
                })
                .await
                .map_err(|e| format!("Failed to update pending remote settlement: {e}"))?;
        }
        NetworkServiceEvent::PreimageCreated(payment_hash, preimage) => {
            watchtower_client
                .create_preimage(CreatePreimageParams {
                    payment_hash: payment_hash.into(),
                    preimage: preimage.into(),
                })
                .await
                .map_err(|e| format!("Failed to create preimage: {e}"))?;
        }
        NetworkServiceEvent::PreimageRemoved(payment_hash) => {
            watchtower_client
                .remove_preimage(RemovePreimageParams {
                    payment_hash: payment_hash.into(),
                })
                .await
                .map_err(|e| format!("Failed to remove preimage: {e}"))?;
        }
        _ => {
            // ignore other non-watchtower related events
        }
    }
    Ok(())
}

/// Apply one Fiber event to the host watchtower store as if this node had
/// called the watchtower RPC methods (`create_watch_channel`,
/// `update_revocation`, …).
///
/// PeriodicCheck settles from this `(node_id, channel_id)` row only. It does
/// not rebuild balances from the chain, and Public T's updates go to
/// [`fiber_types::NodeId::local`] — a different row, opposite local/remote
/// orientation. Hosted tenants must therefore keep their own row current.
///
/// `node_id` is the watched node's identifier, the same value RPC injects
/// from a biscuit `node(...)` fact.
#[cfg(feature = "watchtower")]
pub fn forward_event_to_watchtower_store<S: crate::watchtower::WatchtowerStore>(
    event: NetworkServiceEvent,
    store: &S,
    node_id: fiber_types::NodeId,
) {
    use fiber_types::HashAlgorithm;

    match event {
        // create_watch_channel: register funding identity and the initial
        // settlement snapshot. External signers omit the settlement secret.
        NetworkServiceEvent::RemoteTxComplete(
            _peer_id,
            channel_id,
            funding_udt_type_script,
            local_settlement_key,
            local_settlement_key_pubkey,
            remote_settlement_key,
            local_funding_pubkey,
            remote_funding_pubkey,
            settlement_data,
        ) => store.insert_watch_channel(
            node_id,
            channel_id,
            funding_udt_type_script,
            local_settlement_key,
            local_settlement_key_pubkey,
            remote_settlement_key,
            local_funding_pubkey,
            remote_funding_pubkey,
            settlement_data,
        ),
        // remove_watch_channel: stop scanning a closed or aborted channel.
        NetworkServiceEvent::ChannelClosed(_, channel_id, _)
        | NetworkServiceEvent::ChannelAbandon(channel_id)
        | NetworkServiceEvent::ChannelFundingAborted(channel_id) => {
            store.remove_watch_channel(node_id, channel_id);
        }
        // update_revocation: justice path for an outdated remote commitment.
        NetworkServiceEvent::RevokeAndAckReceived(
            _peer_id,
            channel_id,
            revocation_data,
            settlement_data,
        ) => store.update_revocation(node_id, channel_id, revocation_data, settlement_data),
        // update_local_settlement: snapshot that must hash into our
        // commitment lock args when we settle after a force-close.
        NetworkServiceEvent::RemoteCommitmentSigned(
            _peer_id,
            channel_id,
            _commitment_tx,
            settlement_data,
        ) => store.update_local_settlement(node_id, channel_id, settlement_data),
        // update_pending_remote_settlement: used until the matching
        // revocation arrives for a remote-initiated close.
        NetworkServiceEvent::LocalCommitmentSigned(channel_id, settlement_data) => {
            store.update_pending_remote_settlement(node_id, channel_id, settlement_data);
        }
        // create_preimage: unlock inbound TLCs observed on-chain.
        NetworkServiceEvent::PreimageCreated(payment_hash, preimage)
            if HashAlgorithm::supported_algorithms()
                .iter()
                .any(|algorithm| payment_hash == algorithm.hash(preimage).into()) =>
        {
            store.insert_watch_preimage(node_id, payment_hash, preimage);
        }
        // remove_preimage: drop a preimage that is no longer live.
        NetworkServiceEvent::PreimageRemoved(payment_hash) => {
            store.remove_watch_preimage(node_id, payment_hash);
        }
        _ => {}
    }
}

#[cfg(all(test, feature = "watchtower", not(target_arch = "wasm32")))]
mod tests {
    use super::forward_event_to_watchtower_store;
    use crate::fiber_types::{Hash256, NodeId, Privkey, SettlementData};
    use crate::watchtower::WatchtowerStore;
    use crate::NetworkServiceEvent;

    fn tenant_node_id() -> NodeId {
        NodeId::from_bytes(Privkey::from(&[7; 32]).pubkey().serialize().to_vec())
    }

    #[test]
    fn remote_tx_complete_without_a_secret_is_an_external_watch_channel() {
        let (store, _dir) = crate::generate_store();
        let node_id = tenant_node_id();
        let channel_id = Hash256::from([0x11; 32]);
        let local_settlement_pubkey = Privkey::from(&[1; 32]).pubkey();
        forward_event_to_watchtower_store(
            NetworkServiceEvent::RemoteTxComplete(
                Privkey::from(&[9; 32]).pubkey(),
                channel_id,
                None,
                None,
                local_settlement_pubkey,
                Privkey::from(&[2; 32]).pubkey(),
                Privkey::from(&[3; 32]).pubkey(),
                Privkey::from(&[4; 32]).pubkey(),
                SettlementData {
                    local_amount: 1,
                    remote_amount: 1,
                    tlcs: Vec::new(),
                },
            ),
            &store,
            node_id.clone(),
        );

        let channel = store
            .get_watch_channel(&node_id, &channel_id)
            .expect("watched channel");
        assert!(channel.local_settlement_key.is_none());
        assert_eq!(
            channel.local_settlement_key_pubkey,
            Some(local_settlement_pubkey)
        );
        assert_eq!(
            store.get_watchtower_signer(&node_id, &channel_id),
            fiber_types::WatchtowerSignerState::External(
                fiber_types::WatchtowerExternalSignerState {
                    state: fiber_types::WatchtowerExternalState::Ready,
                    last_applied: None,
                }
            )
        );
        assert!(store
            .get_watch_channel(&NodeId::local(), &channel_id)
            .is_none());
    }
}
