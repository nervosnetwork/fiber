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
            keys,
            settlement_data,
            channel_features,
        ) => {
            let (local_settlement_key, local_settlement_key_pubkey) =
                keys.local_settlement.to_parts();
            watchtower_client
                .create_watch_channel(CreateWatchChannelParams {
                    channel_id: channel_id.into(),
                    funding_udt_type_script: funding_udt_type_script.map(Into::into),
                    local_settlement_key: local_settlement_key
                        .map(|key| key.0.secret_bytes().into()),
                    local_settlement_key_pubkey: local_settlement_key_pubkey.map(Into::into),
                    remote_settlement_key: keys.remote_settlement.into(),
                    local_funding_pubkey: keys.local_funding.into(),
                    remote_funding_pubkey: keys.remote_funding.into(),
                    settlement_data: settlement_data.into(),
                    channel_features: channel_features.into(),
                    payout_lock_script: None,
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
    events: &crate::signing::SigningEvents,
    event: NetworkServiceEvent,
    store: &S,
    node_id: fiber_types::NodeId,
    quotas: &crate::lsp_config::WatchtowerQuotas,
) {
    use fiber_types::HashAlgorithm;

    match event {
        // create_watch_channel: register funding identity and the initial
        // settlement snapshot. External signers omit the settlement secret.
        NetworkServiceEvent::RemoteTxComplete(
            _peer_id,
            channel_id,
            funding_udt_type_script,
            keys,
            settlement_data,
            commitment_contract_features,
        ) => {
            let signing_scope = events.watchtower(&node_id);
            let signing_update = signing_scope.prepare_created(channel_id);
            if node_id == fiber_types::NodeId::local() {
                store.insert_watch_channel(
                    node_id,
                    channel_id,
                    funding_udt_type_script,
                    keys,
                    settlement_data,
                    commitment_contract_features,
                    None,
                );
                signing_update.publish();
            } else if let Err(error) = store.insert_watch_channel_checked(
                node_id.clone(),
                channel_id,
                funding_udt_type_script,
                keys,
                settlement_data,
                commitment_contract_features,
                None,
                quotas,
            ) {
                tracing::warn!(
                    node_id = %node_id,
                    %channel_id,
                    %error,
                    "skipping hosted watchtower channel registration: quota exceeded"
                );
            } else {
                signing_update.publish();
            }
        }
        // remove_watch_channel: stop scanning a closed or aborted channel.
        NetworkServiceEvent::ChannelClosed(_, channel_id, _)
        | NetworkServiceEvent::ChannelAbandon(channel_id)
        | NetworkServiceEvent::ChannelFundingAborted(channel_id) => {
            let signing_scope = events.watchtower(&node_id);
            let signing_update = signing_scope.prepare_removed(channel_id);
            if store.remove_watch_channel(node_id, channel_id) {
                signing_update.publish();
            }
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
            if node_id == fiber_types::NodeId::local() {
                store.insert_watch_preimage(node_id, payment_hash, preimage);
            } else if let Err(error) =
                store.insert_watch_preimage_checked(node_id.clone(), payment_hash, preimage, quotas)
            {
                tracing::warn!(
                    node_id = %node_id,
                    %payment_hash,
                    %error,
                    "skipping hosted watchtower preimage registration: quota exceeded"
                );
            }
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
            &Default::default(),
            NetworkServiceEvent::RemoteTxComplete(
                Privkey::from(&[9; 32]).pubkey(),
                channel_id,
                None,
                fiber_types::WatchChannelKeys {
                    local_settlement: fiber_types::SettlementKey::external(local_settlement_pubkey),
                    remote_settlement: Privkey::from(&[2; 32]).pubkey(),
                    local_funding: Privkey::from(&[3; 32]).pubkey(),
                    remote_funding: Privkey::from(&[4; 32]).pubkey(),
                },
                SettlementData {
                    local_amount: 1,
                    remote_amount: 1,
                    tlcs: Vec::new(),
                },
                fiber_types::ChannelFeatures::V2,
            ),
            &store,
            node_id.clone(),
            &Default::default(),
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
                fiber_types::WatchtowerExternalSignerState::default()
            )
        );
        assert!(store
            .get_watch_channel(&NodeId::local(), &channel_id)
            .is_none());
    }

    #[test]
    fn hosted_preimage_created_enforces_quotas_while_host_is_exempt() {
        use crate::fiber_types::HashAlgorithm;
        use crate::lsp_config::WatchtowerQuotas;

        let (store, _dir) = crate::generate_store();
        let tenant_a = tenant_node_id();
        let tenant_b = NodeId::from_bytes(Privkey::from(&[8; 32]).pubkey().serialize().to_vec());
        let quotas = WatchtowerQuotas {
            max_channels_per_node: 2,
            max_channels_total: 3,
            max_preimages_per_node: 2,
            max_preimages_total: 3,
        };
        let events = Default::default();

        let make_pair = |seed: u8| -> (Hash256, Hash256) {
            let preimage = Hash256::from([seed; 32]);
            let hash: Hash256 = HashAlgorithm::Sha256.hash(preimage.as_ref()).into();
            (hash, preimage)
        };

        let (h1, p1) = make_pair(1);
        let (h2, p2) = make_pair(2);
        let (h3, p3) = make_pair(3);
        let (h4, p4) = make_pair(4);
        let (h5, p5) = make_pair(5);
        let (h6, p6) = make_pair(6);

        // Tenant A registers 2 preimages (fills per-node cap = 2).
        for (h, p) in [(h1, p1), (h2, p2), (h3, p3)] {
            forward_event_to_watchtower_store(
                &events,
                NetworkServiceEvent::PreimageCreated(h, p),
                &store,
                tenant_a.clone(),
                &quotas,
            );
        }
        assert_eq!(store.get_watch_preimage(&tenant_a, &h1), Some(p1));
        assert_eq!(store.get_watch_preimage(&tenant_a, &h2), Some(p2));
        assert_eq!(
            store.get_watch_preimage(&tenant_a, &h3),
            None,
            "3rd preimage for tenant A must be skipped when per-node quota is exceeded"
        );

        // Tenant B registers 1 preimage (fills global cap = 3); 2nd is skipped.
        for (h, p) in [(h4, p4), (h5, p5)] {
            forward_event_to_watchtower_store(
                &events,
                NetworkServiceEvent::PreimageCreated(h, p),
                &store,
                tenant_b.clone(),
                &quotas,
            );
        }
        assert_eq!(store.get_watch_preimage(&tenant_b, &h4), Some(p4));
        assert_eq!(
            store.get_watch_preimage(&tenant_b, &h5),
            None,
            "2nd preimage for tenant B must be skipped when global quota is exceeded"
        );

        // Removing a preimage from Tenant A frees global capacity for Tenant B.
        forward_event_to_watchtower_store(
            &events,
            NetworkServiceEvent::PreimageRemoved(h2),
            &store,
            tenant_a.clone(),
            &quotas,
        );
        forward_event_to_watchtower_store(
            &events,
            NetworkServiceEvent::PreimageCreated(h5, p5),
            &store,
            tenant_b.clone(),
            &quotas,
        );
        assert_eq!(store.get_watch_preimage(&tenant_b, &h5), Some(p5));

        // Host (NodeId::local()) bypasses tenant preimage quotas.
        forward_event_to_watchtower_store(
            &events,
            NetworkServiceEvent::PreimageCreated(h6, p6),
            &store,
            NodeId::local(),
            &quotas,
        );
        assert_eq!(store.get_watch_preimage(&NodeId::local(), &h6), Some(p6));
    }
}
