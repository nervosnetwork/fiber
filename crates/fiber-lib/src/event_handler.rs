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
            remote_settlement_key,
            local_funding_pubkey,
            remote_funding_pubkey,
            settlement_data,
        ) => {
            watchtower_client
                .create_watch_channel(CreateWatchChannelParams {
                    channel_id: channel_id.into(),
                    funding_udt_type_script: funding_udt_type_script.map(Into::into),
                    local_settlement_key: local_settlement_key.0.secret_bytes().into(),
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
