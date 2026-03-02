use std::sync::Arc;

use ractor::{Actor, ActorProcessingErr, ActorRef};
use strum::AsRefStr;
use tracing::debug;

use super::types::Hash256;
use super::watchtower_query::WatchtowerQuerier;
use super::{NetworkActorCommand, NetworkActorMessage, ASSUME_NETWORK_ACTOR_ALIVE};
use crate::utils::actor::ActorHandleLogGuard;

const ACTOR_HANDLE_WARN_THRESHOLD_MS: u64 = 15_000;

#[derive(Debug, Clone, AsRefStr)]
pub enum WatchtowerQueryActorMessage {
    /// Query the watchtower for a preimage for a received TLC.
    /// If found, the preimage is forwarded to the network actor to settle the TLC.
    QueryPreimage {
        channel_id: Hash256,
        tlc_id: u64,
        payment_hash: Hash256,
    },
    /// Query the watchtower for settlement status of an offered TLC.
    /// If settled on-chain, a command is sent to fail the forwarding TLC.
    QuerySettled {
        channel_id: Hash256,
        payment_hash: Hash256,
        forwarding_channel_id: Hash256,
        forwarding_tlc_id: u64,
        shared_secret: [u8; 32],
    },
}

pub struct WatchtowerQueryActor {
    querier: Arc<dyn WatchtowerQuerier>,
    network: ActorRef<NetworkActorMessage>,
}

impl WatchtowerQueryActor {
    pub fn new(
        querier: Arc<dyn WatchtowerQuerier>,
        network: ActorRef<NetworkActorMessage>,
    ) -> Self {
        Self { querier, network }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl Actor for WatchtowerQueryActor {
    type Msg = WatchtowerQueryActorMessage;
    type State = ();
    type Arguments = ();

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        _args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let _handle_log_guard = ActorHandleLogGuard::new(
            "WatchtowerQueryActor",
            message.as_ref().to_string(),
            "fiber.watchtower_query_actor",
            ACTOR_HANDLE_WARN_THRESHOLD_MS,
        );
        match message {
            WatchtowerQueryActorMessage::QueryPreimage {
                channel_id,
                tlc_id,
                payment_hash,
            } => {
                if let Some(preimage) = self
                    .querier
                    .query_tlc_status(&channel_id, &payment_hash)
                    .await
                    .and_then(|s| s.preimage)
                {
                    debug!(
                        "Watchtower returned preimage for channel {:?} tlc {:?}",
                        channel_id, tlc_id
                    );
                    self.network
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::WatchtowerPreimageResult {
                                channel_id,
                                tlc_id,
                                payment_hash,
                                preimage,
                            },
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                }
            }
            WatchtowerQueryActorMessage::QuerySettled {
                channel_id,
                payment_hash,
                forwarding_channel_id,
                forwarding_tlc_id,
                shared_secret,
            } => {
                let is_settled = self
                    .querier
                    .query_tlc_status(&channel_id, &payment_hash)
                    .await
                    .is_some_and(|s| s.is_settled);

                if is_settled {
                    debug!(
                        "Watchtower confirmed TLC settled for channel {:?} payment_hash {:?}",
                        channel_id, payment_hash
                    );
                    self.network
                        .send_message(NetworkActorMessage::new_command(
                            NetworkActorCommand::WatchtowerSettledResult {
                                forwarding_channel_id,
                                forwarding_tlc_id,
                                shared_secret,
                            },
                        ))
                        .expect(ASSUME_NETWORK_ACTOR_ALIVE);
                }
            }
        }
        Ok(())
    }
}
