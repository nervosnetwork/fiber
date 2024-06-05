use ckb_jsonrpc_types::Status;
use ckb_types::core::TransactionView;
use ckb_types::packed::{OutPoint, Script, Transaction};
use ckb_types::prelude::{IntoTransactionView, Pack, Unpack};
use log::{debug, error, info, warn};

use ractor::{
    async_trait as rasync_trait, call_t, Actor, ActorCell, ActorProcessingErr, ActorRef,
    RpcReplyPort, SupervisionEvent,
};
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;
use tentacle::secio::SecioKeyPair;

use tentacle::{
    async_trait,
    builder::{MetaBuilder, ServiceBuilder},
    bytes::Bytes,
    context::SessionContext,
    context::{ProtocolContext, ProtocolContextMutRef, ServiceContext},
    multiaddr::Multiaddr,
    secio::PeerId,
    service::{
        ProtocolHandle, ProtocolMeta, ServiceAsyncControl, ServiceError, ServiceEvent,
        TargetProtocol,
    },
    traits::{ServiceHandle, ServiceProtocol},
    ProtocolId, SessionId,
};

use tokio::sync::{mpsc, oneshot};
use tokio_util::task::TaskTracker;

use super::channel::{
    ChannelActorMessage, ChannelActorStateStore, ChannelCommandWithId, ChannelEvent,
    ProcessingChannelError, ProcessingChannelResult,
};
use super::key::blake2b_hash_with_salt;
use super::types::{Hash256, OpenChannel};
use super::{
    channel::{ChannelActor, ChannelCommand, ChannelInitializationParameter},
    types::PCNMessage,
    CkbConfig,
};

use crate::ckb::channel::{TxCollaborationCommand, TxUpdateCommand};
use crate::ckb::types::TxSignatures;
use crate::ckb_chain::{CkbChainMessage, FundingRequest, FundingTx, TraceTxRequest};
use crate::{unwrap_or_return, Error};

pub const PCN_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

pub const DEFAULT_CHAIN_ACTOR_TIMEOUT: u64 = 60000;

#[derive(Debug)]
pub struct OpenChannelResponse {
    pub channel_id: Hash256,
}

#[derive(Debug)]
pub struct AcceptChannelResponse {
    pub old_channel_id: Hash256,
    pub new_channel_id: Hash256,
}

/// The struct here is used both internally and as an API to the outside world.
/// If we want to send a reply to the caller, we need to wrap the message with
/// a RpcReplyPort. Since outsider users have no knowledge of RpcReplyPort, we
/// need to hide it from the API. So in case a reply is needed, we need to put
/// an optional RpcReplyPort in the of the definition of this message.
#[derive(Debug)]
pub enum NetworkActorCommand {
    /// Network commands
    ConnectPeer(Multiaddr),
    DisconnectPeer(PeerId),
    // For internal use and debugging only. Most of the messages requires some
    // changes to local state. Even if we can send a message to a peer, some
    // part of the local state is not changed.
    SendPcnMessage(PCNMessageWithPeerId),
    // Open a channel to a peer.
    OpenChannel(
        OpenChannelCommand,
        RpcReplyPort<Result<OpenChannelResponse, String>>,
    ),
    // Accept a channel to a peer.
    AcceptChannel(
        AcceptChannelCommand,
        RpcReplyPort<Result<AcceptChannelResponse, String>>,
    ),
    // Send a command to a channel.
    ControlPcnChannel(ChannelCommandWithId),
    UpdateChannelFunding(Hash256, Transaction, FundingRequest),
    SignTx(PeerId, Hash256, Transaction, Option<Vec<Vec<u8>>>),
}

#[derive(Debug)]
pub struct OpenChannelCommand {
    pub peer_id: PeerId,
    pub funding_amount: u128,
}

#[derive(Debug)]
pub struct AcceptChannelCommand {
    pub temp_channel_id: Hash256,
    pub funding_amount: u128,
}

impl NetworkActorMessage {
    pub fn new_event(event: NetworkActorEvent) -> Self {
        Self::Event(event)
    }

    pub fn new_command(command: NetworkActorCommand) -> Self {
        Self::Command(command)
    }
}

#[derive(Debug)]
pub enum NetworkServiceEvent {
    ServiceError(ServiceError),
    ServiceEvent(ServiceEvent),
    NetworkStarted(PeerId, Multiaddr),
    PeerConnected(PeerId, Multiaddr),
    PeerDisConnected(PeerId, Multiaddr),
    ChannelCreated(PeerId, Hash256),
    ChannelPendingToBeAccepted(PeerId, Hash256),
    ChannelReady(PeerId, Hash256),
    ChannelShutDown(PeerId, Hash256),
    ChannelClosed(PeerId, Hash256, TransactionView),
    // We should sign a commitment transaction and send it to the other party.
    CommitmentSignaturePending(PeerId, Hash256, u64),
    // We have signed a commitment transaction and sent it to the other party.
    // The last element is the witnesses for this commitment transaction.
    // The TransactionView here is not a valid commitment transaction per se,
    // as we need the other party's signature.
    LocalCommitmentSigned(PeerId, Hash256, u64, TransactionView, Vec<u8>),
    // The other party has signed a valid commitment transaction,
    // and we successfully assemble the partial signature from other party
    // to create a complete commitment transaction.
    RemoteCommitmentSigned(PeerId, Hash256, u64, TransactionView),
}

/// Events that can be sent to the network actor. Except for NetworkServiceEvent,
/// all events are processed by the network actor.
#[derive(Debug)]
pub enum NetworkActorEvent {
    /// Network eventss to be processed by this actor.
    PeerConnected(PeerId, SessionContext),
    PeerDisconnected(PeerId, SessionContext),
    PeerMessage(PeerId, SessionContext, PCNMessage),

    /// Channel related events.

    /// A new channel is created and the peer id and actor reference is given here.
    ChannelCreated(Hash256, PeerId, ActorRef<ChannelActorMessage>),
    /// A channel has been accepted.
    /// The two Hash256 are respectively newly agreed channel id and temp channel id,
    /// The two u128 are respectively local and remote funding amount,
    /// and the script is the lock script of the agreed funding cell.
    ChannelAccepted(PeerId, Hash256, Hash256, u128, u128, Script),
    /// A channel is ready to use.
    ChannelReady(Hash256, PeerId),
    /// A channel is being shutting down.
    ChannelShutdown(Hash256, PeerId),
    /// A channel is already closed.
    ChannelClosed(Hash256, PeerId, TransactionView),

    /// Both parties are now able to broadcast a valid funding transaction.
    FundingTransactionPending(Transaction, OutPoint, Hash256),

    /// A funding transaction has been confirmed.
    FundingTransactionConfirmed(OutPoint),

    /// A funding transaction has been confirmed.
    FundingTransactionFailed(OutPoint),

    /// A commitment transaction is signed by us and has sent to the other party.
    LocalCommitmentSigned(PeerId, Hash256, u64, TransactionView, Vec<u8>),

    /// Network service events to be sent to outside observers.
    /// These events may be both present at `NetworkActorEvent` and
    /// this branch of `NetworkActorEvent`. This is because some events
    /// (e.g. `ChannelClosed`)require some processing internally,
    /// and they are also interesting to outside observers.
    /// Once we processed these events, we will send them to outside observers.
    NetworkServiceEvent(NetworkServiceEvent),
}

#[derive(Debug)]
pub enum NetworkActorMessage {
    Command(NetworkActorCommand),
    Event(NetworkActorEvent),
}

#[derive(Debug)]
pub struct PCNMessageWithPeerId {
    pub peer_id: PeerId,
    pub message: PCNMessage,
}

impl PCNMessageWithPeerId {
    pub fn new(peer_id: PeerId, message: PCNMessage) -> Self {
        Self { peer_id, message }
    }
}

#[derive(Debug)]
pub struct PCNMessageWithSessionId {
    pub session_id: SessionId,
    pub message: PCNMessage,
}

#[derive(Debug)]
pub struct PCNMessageWithChannelId {
    pub channel_id: Hash256,
    pub message: PCNMessage,
}

pub struct NetworkActor<S> {
    // An event emitter to notify ourside observers.
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    chain_actor: ActorRef<CkbChainMessage>,
    store: S,
}

impl<S> NetworkActor<S>
where
    S: ChannelActorStateStore + Clone + Send + Sync + 'static,
{
    pub fn new(
        event_sender: mpsc::Sender<NetworkServiceEvent>,
        chain_actor: ActorRef<CkbChainMessage>,
        store: S,
    ) -> Self {
        Self {
            event_sender,
            chain_actor,
            store,
        }
    }

    pub async fn on_service_event(&self, event: NetworkServiceEvent) {
        let _ = self.event_sender.send(event).await;
    }

    pub async fn handle_peer_message(
        &self,
        state: &mut NetworkActorState,
        peer_id: PeerId,
        session: SessionContext,
        message: PCNMessage,
    ) -> crate::Result<()> {
        debug!(
            "Received message from peer {:?} on session {:?}: {:?}",
            &peer_id, &session.id, &message
        );

        match message {
            PCNMessage::TestMessage(test) => {
                debug!("Test message {:?}", test);
            }

            // We should process OpenChannel message here because there is no channel corresponding
            // to the channel id in the message yet.
            PCNMessage::OpenChannel(open_channel) => {
                state.on_open_channel_msg(peer_id, open_channel).await?;
            }

            _ => match state.channels.get(&message.get_channel_id()) {
                None => {
                    return Err(Error::ChannelNotFound(message.get_channel_id()));
                }
                Some(c) => {
                    c.send_message(ChannelActorMessage::PeerMessage(message))
                        .expect("channel actor alive");
                }
            },
        };
        Ok(())
    }

    pub async fn handle_command(
        &self,
        myself: ActorRef<NetworkActorMessage>,
        state: &mut NetworkActorState,
        command: NetworkActorCommand,
    ) -> crate::Result<()> {
        match command {
            NetworkActorCommand::SendPcnMessage(PCNMessageWithPeerId { peer_id, message }) => {
                debug!(
                    "SendPcnMessage command received: sending message to peer {:?}",
                    &peer_id
                );
                state.send_message_to_peer(&peer_id, message).await?;
            }

            NetworkActorCommand::ConnectPeer(addr) => {
                // TODO: It is more than just dialing a peer. We need to exchange capabilities of the peer,
                // e.g. whether the peer support some specific feature.
                // TODO: If we are already connected to the peer, skip connecting.
                debug!("ConnectPeer command received, dialing {}", &addr);
                state
                    .control
                    .dial(addr.clone(), TargetProtocol::All)
                    .await?
                // TODO: note that the dial function does not return error immediately even if dial fails.
                // Tentacle sends an event by calling handle_error function instead, which
                // may receive errors like DialerError.
            }

            NetworkActorCommand::DisconnectPeer(peer_id) => {
                debug!(
                    "DisconnectPeer command received, disconnecting peer {:?}",
                    &peer_id
                );
                if let Some(session) = state.get_peer_session(&peer_id) {
                    state.control.disconnect(session).await?;
                }
            }

            NetworkActorCommand::OpenChannel(open_channel, reply) => {
                match state
                    .create_outbound_channel(open_channel, self.store.clone())
                    .await
                {
                    Ok((_, channel_id)) => {
                        let _ = reply.send(Ok(OpenChannelResponse { channel_id }));
                    }
                    Err(err) => {
                        error!("Failed to create channel: {}", err);
                        let _ = reply.send(Err(err.to_string()));
                    }
                }
            }
            NetworkActorCommand::AcceptChannel(accept_channel, reply) => {
                match state
                    .create_inbound_channel(accept_channel, self.store.clone())
                    .await
                {
                    Ok((_, old_channel_id, new_channel_id)) => {
                        let _ = reply.send(Ok(AcceptChannelResponse {
                            old_channel_id,
                            new_channel_id,
                        }));
                    }
                    Err(err) => {
                        error!("Failed to accept channel: {}", err);
                        let _ = reply.send(Err(err.to_string()));
                    }
                }
            }

            NetworkActorCommand::ControlPcnChannel(c) => {
                state
                    .send_command_to_channel(c.channel_id, c.command)
                    .await?
            }
            NetworkActorCommand::UpdateChannelFunding(channel_id, transaction, request) => {
                let old_tx = transaction.into_view();
                debug!(
                    "Updating channel funding for channel {:?}, current tx: {:?}",
                    &channel_id, old_tx
                );
                let mut tx = FundingTx::new();
                tx.update_for_self(old_tx)?;
                let tx = match call_t!(
                    self.chain_actor.clone(),
                    CkbChainMessage::Fund,
                    DEFAULT_CHAIN_ACTOR_TIMEOUT,
                    tx,
                    request
                ) {
                    Ok(Ok(tx)) => match tx.into_inner() {
                        Some(tx) => tx,
                        _ => {
                            error!("Obtained empty funding tx");
                            return Ok(());
                        }
                    },
                    Ok(Err(err)) => {
                        error!("Failed to fund channel: {}", err);
                        return Ok(());
                    }
                    Err(err) => {
                        error!("Failed to call chain actor: {}", err);
                        return Ok(());
                    }
                };
                debug!("Funding transaction updated on our part: {:?}", tx);
                state
                    .send_command_to_channel(
                        channel_id,
                        ChannelCommand::TxCollaborationCommand(TxCollaborationCommand::TxUpdate(
                            TxUpdateCommand {
                                transaction: tx.data(),
                            },
                        )),
                    )
                    .await?
            }
            NetworkActorCommand::SignTx(
                ref peer_id,
                ref channel_id,
                funding_tx,
                partial_witnesses,
            ) => {
                let msg = match partial_witnesses {
                    Some(partial_witnesses) => {
                        debug!(
                            "Received SignTx request with for transaction {:?} and partial witnesses {:?}",
                            &funding_tx,
                            partial_witnesses
                                .iter()
                                .map(|x| hex::encode(x))
                                .collect::<Vec<_>>()
                        );
                        let funding_tx = funding_tx
                            .into_view()
                            .as_advanced_builder()
                            .set_witnesses(
                                partial_witnesses.into_iter().map(|x| x.pack()).collect(),
                            )
                            .build();

                        let mut funding_tx = call_t!(
                            self.chain_actor,
                            CkbChainMessage::Sign,
                            DEFAULT_CHAIN_ACTOR_TIMEOUT,
                            funding_tx.into()
                        )
                        .expect("chain alive")
                        .expect("Signing succeeded");
                        debug!("Funding transaction signed: {:?}", &funding_tx);

                        // Since we have received a valid tx_signatures message, we're now sure that
                        // we can broadcast a valid transaction to the network, i.e. we can wait for
                        // the funding transaction to be confirmed.
                        let funding_tx = funding_tx.take().expect("take tx");
                        let witnesses = funding_tx.witnesses();
                        let outpoint = funding_tx
                            .output_pts_iter()
                            .next()
                            .expect("funding tx output exists");

                        myself
                            .send_message(NetworkActorMessage::new_event(
                                NetworkActorEvent::FundingTransactionPending(
                                    funding_tx.data(),
                                    outpoint,
                                    *channel_id,
                                ),
                            ))
                            .expect("network actor alive");
                        debug!("Fully signed funding tx {:?}", &funding_tx);

                        PCNMessageWithPeerId {
                            peer_id: peer_id.clone(),
                            message: PCNMessage::TxSignatures(TxSignatures {
                                channel_id: *channel_id,
                                witnesses: witnesses.into_iter().map(|x| x.unpack()).collect(),
                                tx_hash: funding_tx.hash().into(),
                            }),
                        }
                    }
                    None => {
                        debug!(
                            "Received SignTx request with for transaction {:?} without partial witnesses, so start signing it now",
                            &funding_tx,
                        );
                        let mut funding_tx = call_t!(
                            self.chain_actor,
                            CkbChainMessage::Sign,
                            DEFAULT_CHAIN_ACTOR_TIMEOUT,
                            funding_tx.into()
                        )
                        .expect("chain alive")?;
                        debug!("Funding transaction signed: {:?}", &funding_tx);
                        let funding_tx = funding_tx.take().expect("take tx");
                        let witnesses = funding_tx.witnesses();

                        debug!("Partially signed funding tx {:?}", &funding_tx);
                        PCNMessageWithPeerId {
                            peer_id: peer_id.clone(),
                            message: PCNMessage::TxSignatures(TxSignatures {
                                channel_id: *channel_id,
                                witnesses: witnesses.into_iter().map(|x| x.unpack()).collect(),
                                tx_hash: funding_tx.hash().into(),
                            }),
                        }
                    }
                };
                debug!(
                    "Handled tx_signatures, peer: {:?}, messge to send: {:?}",
                    &peer_id, &msg
                );
                myself
                    .send_message(NetworkActorMessage::new_command(
                        NetworkActorCommand::SendPcnMessage(msg),
                    ))
                    .expect("network actor alive");
            }
        };
        Ok(())
    }
}

pub struct NetworkActorState {
    peer_id: PeerId,
    // This is the entropy used to generate various random values.
    // Must be kept secret.
    // TODO: Maybe we should abstract this into a separate trait.
    entropy: [u8; 32],
    network: ActorRef<NetworkActorMessage>,
    // This immutable attribute is placed here because we need to create it in
    // the pre_start function.
    control: ServiceAsyncControl,
    peer_session_map: HashMap<PeerId, SessionId>,
    session_channels_map: HashMap<SessionId, HashSet<Hash256>>,
    channels: HashMap<Hash256, ActorRef<ChannelActorMessage>>,
    // Channels in this hashmap are pending for acceptance. The user needs to
    // issue an AcceptChannelCommand with the amount of funding to accept the channel.
    to_be_accepted_channels: HashMap<Hash256, (PeerId, OpenChannel)>,
    // Channels in this hashmap are pending for funding transaction confirmation.
    pending_channels: HashMap<OutPoint, Hash256>,
    // Used to broadcast and query network info.
    chain_actor: ActorRef<CkbChainMessage>,
}

impl NetworkActorState {
    pub fn generate_channel_seed(&mut self) -> [u8; 32] {
        let channel_user_id = self.channels.len();
        let seed = channel_user_id
            .to_be_bytes()
            .into_iter()
            .chain(self.entropy.iter().cloned())
            .collect::<Vec<u8>>();
        let result = blake2b_hash_with_salt(&seed, b"PCN_CHANNEL_SEED");
        self.entropy = blake2b_hash_with_salt(&result, b"PCN_NETWORK_ENTROPY_UPDATE");
        result
    }

    pub async fn create_outbound_channel<S: ChannelActorStateStore + Sync + Send + 'static>(
        &mut self,
        open_channel: OpenChannelCommand,
        store: S,
    ) -> Result<(ActorRef<ChannelActorMessage>, Hash256), ProcessingChannelError> {
        let network = self.network.clone();
        let OpenChannelCommand {
            peer_id,
            funding_amount,
        } = open_channel;
        let seed = self.generate_channel_seed();
        let (tx, rx) = oneshot::channel::<Hash256>();
        let channel = Actor::spawn_linked(
            None,
            ChannelActor::new(peer_id.clone(), network.clone(), store),
            ChannelInitializationParameter::OpenChannel(funding_amount, seed, tx),
            network.clone().get_cell(),
        )
        .await?
        .0;
        let temp_channel_id = rx.await.expect("msg received");
        Ok((channel, temp_channel_id))
    }

    pub async fn create_inbound_channel<S: ChannelActorStateStore + Sync + Send + 'static>(
        &mut self,
        accept_channel: AcceptChannelCommand,
        store: S,
    ) -> Result<(ActorRef<ChannelActorMessage>, Hash256, Hash256), ProcessingChannelError> {
        let AcceptChannelCommand {
            temp_channel_id,
            funding_amount,
        } = accept_channel;
        let (peer_id, open_channel) = self
            .to_be_accepted_channels
            .remove(&temp_channel_id)
            .ok_or(ProcessingChannelError::InvalidParameter(format!(
                "No channel with temp id {:?} found",
                &temp_channel_id
            )))?;
        let network = self.network.clone();
        let id = open_channel.channel_id;
        if let Some(channel) = self.channels.get(&id) {
            warn!("A channel of id {:?} is already created, returning it", &id);
            return Ok((channel.clone(), temp_channel_id, id));
        }

        let seed = self.generate_channel_seed();
        let (tx, rx) = oneshot::channel::<Hash256>();
        let channel = Actor::spawn_linked(
            None,
            ChannelActor::new(peer_id.clone(), network.clone(), store),
            ChannelInitializationParameter::AcceptChannel(
                funding_amount,
                seed,
                open_channel,
                Some(tx),
            ),
            network.clone().get_cell(),
        )
        .await?
        .0;
        let new_id = rx.await.expect("msg received");
        Ok((channel, temp_channel_id, new_id))
    }

    fn get_peer_session(&self, peer_id: &PeerId) -> Option<SessionId> {
        self.peer_session_map.get(peer_id).cloned()
    }

    async fn send_message_to_session(
        &self,
        session_id: SessionId,
        message: PCNMessage,
    ) -> crate::Result<()> {
        self.control
            .send_message_to(session_id, PCN_PROTOCOL_ID, message.to_molecule_bytes())
            .await?;
        Ok(())
    }

    async fn send_message_to_peer(
        &self,
        peer_id: &PeerId,
        message: PCNMessage,
    ) -> crate::Result<()> {
        match self.get_peer_session(peer_id) {
            Some(session) => self.send_message_to_session(session, message).await,
            None => Err(Error::PeerNotFound(peer_id.clone())),
        }
    }

    async fn send_command_to_channel(
        &self,
        channel_id: Hash256,
        command: ChannelCommand,
    ) -> crate::Result<()> {
        match self.channels.get(&channel_id) {
            Some(actor) => {
                actor.send_message(ChannelActorMessage::Command(command))?;
                Ok(())
            }
            None => Err(Error::ChannelNotFound(channel_id)),
        }
    }

    async fn on_peer_connected<S: ChannelActorStateStore + Clone + Send + Sync + 'static>(
        &mut self,
        peer_id: &PeerId,
        session: &SessionContext,
        store: S,
    ) {
        debug!("Peer connected: {:?}, session id: {}", &peer_id, session.id);
        self.peer_session_map.insert(peer_id.clone(), session.id);

        for channel_id in store.get_channels(&peer_id) {
            debug!("Reestablishing channel {:?}", &channel_id);
            if let Ok((channel, _)) = Actor::spawn_linked(
                None,
                ChannelActor::new(peer_id.clone(), self.network.clone(), store.clone()),
                ChannelInitializationParameter::ReestablishChannel(channel_id),
                self.network.get_cell(),
            )
            .await
            {
                self.on_channel_created(channel_id, peer_id, channel);
            }
        }
    }

    fn on_peer_disconnected(&mut self, id: &PeerId, session: &SessionContext) {
        debug!("Peer disconnected: {:?}, session id: {}", &id, session.id);
        if let Some(session) = self.peer_session_map.remove(id) {
            if let Some(channel_ids) = self.session_channels_map.remove(&session) {
                for channel_id in channel_ids {
                    if let Some(channel) = self.channels.remove(&channel_id) {
                        let _ = channel.send_message(ChannelActorMessage::Event(
                            ChannelEvent::PeerDisconnected,
                        ));
                    }
                }
            }
        }
    }

    fn on_channel_created(
        &mut self,
        id: Hash256,
        peer_id: &PeerId,
        actor: ActorRef<ChannelActorMessage>,
    ) {
        debug!("Channel to peer {:?} created: {:?}", &peer_id, &id);
        if let Some(session) = self.get_peer_session(peer_id) {
            self.channels.insert(id, actor);
            self.session_channels_map
                .entry(session)
                .or_insert_with(HashSet::new)
                .insert(id);
        }
    }

    fn on_channel_closed(&mut self, id: &Hash256, peer_id: &PeerId) {
        self.channels.remove(&id);
        if let Some(session) = self.get_peer_session(peer_id) {
            self.session_channels_map.get_mut(&session).map(|set| {
                set.remove(&id);
            });
        }
    }

    pub async fn on_open_channel_msg(
        &mut self,
        peer_id: PeerId,
        open_channel: OpenChannel,
    ) -> ProcessingChannelResult {
        let id = open_channel.channel_id;
        if let Some(channel) = self.to_be_accepted_channels.get(&id) {
            warn!(
                "A channel from {:?} of id {:?} is already awaiting to be accepted: {:?}",
                &peer_id, &id, channel
            );
            return Err(ProcessingChannelError::InvalidParameter(format!(
                "A channel from {:?} of id {:?} is already awaiting to be accepted",
                &peer_id, &id,
            )));
        }
        debug!(
            "Channel from {:?} of id {:?} is now awaiting to be accepted: {:?}",
            &peer_id, &id, &open_channel
        );
        self.to_be_accepted_channels
            .insert(id, (peer_id.clone(), open_channel));
        // Notify outside observers.
        self.network
            .clone()
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::NetworkServiceEvent(
                    NetworkServiceEvent::ChannelPendingToBeAccepted(peer_id, id),
                ),
            ))
            .expect("myself alive");
        Ok(())
    }

    async fn on_funding_transaction_pending(
        &mut self,
        transaction: Transaction,
        outpoint: OutPoint,
        channel_id: Hash256,
    ) -> Result<(), ActorProcessingErr> {
        if let Some(old) = self.pending_channels.remove(&outpoint) {
            if old != channel_id {
                panic!("Trying to associate a new channel id {:?} with the same outpoint {:?} when old channel id is {:?}. Rejecting.", channel_id, outpoint, old);
            }
        }
        self.pending_channels.insert(outpoint.clone(), channel_id);
        // TODO: try to broadcast the transaction to the network.
        debug!(
            "Funding transaction (outpoint {:?}) for channel {:?} is now ready. We can broadcast transaction {:?} now.",
            &outpoint, &channel_id, &transaction
        );
        let transaction = transaction.into_view();
        debug!("Trying to broadcast funding transaction {:?}", &transaction);
        self.chain_actor
            .send_message(CkbChainMessage::SendTx(transaction.clone()))
            .expect("chain actor alive");

        let hash = transaction.hash().into();

        info!("Funding transactoin sent to the network: {}", hash);

        // Trace the transaction status.

        // TODO: make number of confirmation to transaction configurable.
        const NUM_CONFIRMATIONS: u64 = 4;
        let request = TraceTxRequest {
            tx_hash: hash,
            confirmations: NUM_CONFIRMATIONS,
        };
        let chain = self.chain_actor.clone();
        let network = self.network.clone();
        // Spawn a new task to avoid blocking current actor message processing.
        ractor::concurrency::tokio_primatives::spawn(async move {
            debug!("Tracing transaction status {:?}", &request.tx_hash);
            let message = match call_t!(
                chain,
                CkbChainMessage::TraceTx,
                DEFAULT_CHAIN_ACTOR_TIMEOUT,
                request.clone()
            ) {
                Ok(status) if status == Status::Committed => {
                    info!("Funding transaction {:?} confirmed", &request.tx_hash,);
                    NetworkActorEvent::FundingTransactionConfirmed(outpoint)
                }
                Ok(status) => {
                    error!(
                        "Funding transaction {:?} failed to be confirmed with final status {:?}",
                        &request.tx_hash, &status
                    );
                    NetworkActorEvent::FundingTransactionFailed(outpoint)
                }
                Err(err) => {
                    error!(
                        "Failed to trace transaction {:?}: {:?}",
                        &request.tx_hash, &err
                    );
                    NetworkActorEvent::FundingTransactionFailed(outpoint)
                }
            };

            // Notify outside observers.
            network
                .send_message(NetworkActorMessage::new_event(message))
                .expect("myself alive");
        });

        Ok(())
    }

    async fn on_funding_transaction_confirmed(
        &mut self,
        outpoint: OutPoint,
    ) -> Result<(), ActorProcessingErr> {
        let channel_id = match self.pending_channels.remove(&outpoint) {
            Some(channel_id) => channel_id,
            None => {
                warn!(
                    "Funding transaction confirmed for outpoint {:?} but no channel found",
                    &outpoint
                );
                return Ok(());
            }
        };
        if let Some(channel) = self.channels.get(&channel_id) {
            channel
                .send_message(ChannelActorMessage::Event(
                    ChannelEvent::FundingTransactionConfirmed,
                ))
                .expect("channel actor alive");
        }
        Ok(())
    }
}

#[rasync_trait]
impl<S> Actor for NetworkActor<S>
where
    S: ChannelActorStateStore + Clone + Send + Sync + 'static,
{
    type Msg = NetworkActorMessage;
    type State = NetworkActorState;
    type Arguments = (CkbConfig, TaskTracker);

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        (config, tracker): Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("SystemTime::now() should after UNIX_EPOCH");
        let kp = config
            .read_or_generate_secret_key()
            .expect("read or generate secret key");
        let entropy = blake2b_hash_with_salt(
            [kp.as_ref(), now.as_nanos().to_le_bytes().as_ref()]
                .concat()
                .as_slice(),
            b"PCN_NETWORK_ENTROPY",
        );
        let secio_kp = SecioKeyPair::from(kp.into());
        let secio_pk = secio_kp.public_key();
        let handle = Handle::new(myself.clone());
        let mut service = ServiceBuilder::default()
            .insert_protocol(handle.clone().create_meta(PCN_PROTOCOL_ID))
            .handshake_type(secio_kp.into())
            .build(handle);
        let listen_addr = service
            .listen(
                format!("/ip4/127.0.0.1/tcp/{}", config.listening_port)
                    .parse()
                    .expect("valid tentacle address"),
            )
            .await
            .expect("listen tentacle");

        let my_peer_id: PeerId = PeerId::from(secio_pk);
        info!(
            "Started listening tentacle on {}/p2p/{}",
            listen_addr,
            my_peer_id.to_base58()
        );

        let control = service.control().to_owned();

        myself
            .send_message(NetworkActorMessage::new_event(
                NetworkActorEvent::NetworkServiceEvent(NetworkServiceEvent::NetworkStarted(
                    my_peer_id.clone(),
                    listen_addr,
                )),
            ))
            .expect("network actor myself alive");

        tracker.spawn(async move {
            service.run().await;
            debug!("Tentacle service shutdown");
        });

        Ok(NetworkActorState {
            peer_id: my_peer_id,
            entropy,
            network: myself,
            control,
            peer_session_map: Default::default(),
            session_channels_map: Default::default(),
            channels: Default::default(),
            to_be_accepted_channels: Default::default(),
            pending_channels: Default::default(),
            chain_actor: self.chain_actor.clone(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        debug!("Network actor processing message {:?}", message);

        match message {
            NetworkActorMessage::Event(event) => match event {
                NetworkActorEvent::NetworkServiceEvent(e) => {
                    self.on_service_event(e).await;
                }
                NetworkActorEvent::PeerConnected(id, session) => {
                    state
                        .on_peer_connected(&id, &session, self.store.clone())
                        .await;
                    // Notify outside observers.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::PeerConnected(id, session.address),
                            ),
                        ))
                        .expect("myself alive");
                }
                NetworkActorEvent::PeerDisconnected(id, session) => {
                    state.on_peer_disconnected(&id, &session);
                    // Notify outside observers.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::PeerDisConnected(id, session.address),
                            ),
                        ))
                        .expect("myself alive");
                }
                NetworkActorEvent::ChannelCreated(channel_id, peer_id, actor) => {
                    state.on_channel_created(channel_id, &peer_id, actor);
                    // Notify outside observers.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::ChannelCreated(peer_id, channel_id),
                            ),
                        ))
                        .expect("myself alive");
                }
                NetworkActorEvent::ChannelAccepted(peer_id, new, old, local, remote, script) => {
                    assert_ne!(new, old, "new and old channel id must be different");
                    if let Some(session) = state.get_peer_session(&peer_id) {
                        if let Some(channel) = state.channels.remove(&old) {
                            debug!("Channel accepted: {:?} -> {:?}", old, new);
                            state.channels.insert(new, channel);
                            state.session_channels_map.get_mut(&session).map(|set| {
                                set.remove(&old);
                                set.insert(new);
                            });

                            debug!("Starting funding channel");
                            // TODO: Here we imply that the one who receives AcceptChannel message
                            // (i.e. the channel initiator) will send TxUpdate message first.
                            myself
                                .send_message(NetworkActorMessage::new_command(
                                    NetworkActorCommand::UpdateChannelFunding(
                                        new,
                                        Default::default(),
                                        FundingRequest {
                                            udt_info: None,
                                            script,
                                            local_amount: local as u64,
                                            local_fee_rate: 0,
                                            remote_amount: remote as u64,
                                        },
                                    ),
                                ))
                                .expect("myself alive");
                        }
                    }
                }
                NetworkActorEvent::ChannelReady(channel_id, peer_id) => {
                    info!(
                        "Channel ({:?}) to peer {:?} is now ready",
                        channel_id, peer_id
                    );
                    // Notify outside observers.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::ChannelReady(peer_id, channel_id),
                            ),
                        ))
                        .expect("myself alive");
                }
                NetworkActorEvent::ChannelShutdown(channel_id, peer_id) => {
                    info!(
                        "Channel ({:?}) to peer {:?} is being shutdown.",
                        channel_id, peer_id
                    );
                    // Notify outside observers.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::ChannelShutDown(peer_id, channel_id),
                            ),
                        ))
                        .expect("myself alive");
                }
                NetworkActorEvent::ChannelClosed(channel_id, peer_id, tx) => {
                    state.on_channel_closed(&channel_id, &peer_id);
                    info!(
                        "Channel ({:?}) to peer {:?} is already closed. Closing transaction {:?} can be broacasted now.",
                        channel_id, peer_id, tx
                    );
                    self.chain_actor
                        .send_message(CkbChainMessage::SendTx(tx.clone()))
                        .expect("chain actor alive");
                    // Notify outside observers.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::ChannelClosed(peer_id, channel_id, tx),
                            ),
                        ))
                        .expect("myself alive");
                }
                NetworkActorEvent::PeerMessage(peer_id, session, message) => {
                    self.handle_peer_message(state, peer_id, session, message)
                        .await?
                }
                NetworkActorEvent::FundingTransactionPending(transaction, outpoint, channel_id) => {
                    debug!(
                        "Funding transaction pending for channel {:?}: {:?}",
                        channel_id, outpoint
                    );
                    state
                        .on_funding_transaction_pending(transaction, outpoint.clone(), channel_id)
                        .await?;
                }
                NetworkActorEvent::FundingTransactionConfirmed(outpoint) => {
                    state.on_funding_transaction_confirmed(outpoint).await?
                }
                NetworkActorEvent::FundingTransactionFailed(_outpoint) => {
                    unimplemented!("handling funding transaction failed");
                }
                NetworkActorEvent::LocalCommitmentSigned(
                    peer_id,
                    channel_id,
                    version,
                    tx,
                    witnesses,
                ) => {
                    // TODO: We should save the witnesses to the channel state.
                    // Notify outside observers.
                    myself
                        .send_message(NetworkActorMessage::new_event(
                            NetworkActorEvent::NetworkServiceEvent(
                                NetworkServiceEvent::LocalCommitmentSigned(
                                    peer_id, channel_id, version, tx, witnesses,
                                ),
                            ),
                        ))
                        .expect("myself alive");
                }
            },
            NetworkActorMessage::Command(command) => {
                let result = self.handle_command(myself, state, command).await;
                if let Err(err) = result {
                    error!("Failed to handle ckb network command: {}", err);
                }
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        if let Err(err) = state.control.close().await {
            error!("Failed to close tentacle service: {}", err);
        }
        debug!("Network service for {:?} shutdown", state.peer_id);
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: SupervisionEvent,
        _state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisionEvent::ActorTerminated(who, _, _) => {
                debug!("Actor {:?} terminated", who);
            }
            SupervisionEvent::ActorPanicked(who, _) => {
                error!("Actor {:?} panicked", who);
            }
            _ => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct Handle {
    actor: ActorRef<NetworkActorMessage>,
}

impl Handle {
    pub fn new(actor: ActorRef<NetworkActorMessage>) -> Self {
        Self { actor }
    }

    fn send_actor_message(&self, message: NetworkActorMessage) {
        // If we are closing the whole network service, we may have already stopped the network actor.
        // In that case the send_message will fail.
        // Ideally, we should close tentacle network service first, then stop the network actor.
        // But ractor provides only api for `post_stop` instead of `pre_stop`.
        let _ = self.actor.send_message(message);
    }

    fn emit_event(&self, event: NetworkServiceEvent) {
        self.send_actor_message(NetworkActorMessage::Event(
            NetworkActorEvent::NetworkServiceEvent(event),
        ));
    }

    fn create_meta(self, id: ProtocolId) -> ProtocolMeta {
        MetaBuilder::new()
            .id(id)
            .service_handle(move || {
                let handle = Box::new(self);
                ProtocolHandle::Callback(handle)
            })
            .build()
    }
}

#[async_trait]
impl ServiceProtocol for Handle {
    async fn init(&mut self, _context: &mut ProtocolContext) {}

    async fn connected(&mut self, context: ProtocolContextMutRef<'_>, version: &str) {
        let session = context.session;
        info!(
            "proto id [{}] open on session [{}], address: [{}], type: [{:?}], version: {}",
            context.proto_id, session.id, session.address, session.ty, version
        );

        if let Some(peer_id) = context.session.remote_pubkey.clone().map(PeerId::from) {
            self.send_actor_message(NetworkActorMessage::new_event(
                NetworkActorEvent::PeerConnected(peer_id, context.session.clone()),
            ));
        } else {
            warn!("Peer connected without remote pubkey {:?}", context.session);
        }
    }

    async fn disconnected(&mut self, context: ProtocolContextMutRef<'_>) {
        info!(
            "proto id [{}] close on session [{}]",
            context.proto_id, context.session.id
        );

        if let Some(peer_id) = context.session.remote_pubkey.clone().map(PeerId::from) {
            self.send_actor_message(NetworkActorMessage::new_event(
                NetworkActorEvent::PeerDisconnected(peer_id, context.session.clone()),
            ));
        } else {
            warn!(
                "Peer disconnected without remote pubkey {:?}",
                context.session
            );
        }
    }

    async fn received(&mut self, context: ProtocolContextMutRef<'_>, data: Bytes) {
        info!(
            "received from [{}]: proto [{}] data {:?}",
            context.session.id,
            context.proto_id,
            hex::encode(data.as_ref()),
        );

        let msg = unwrap_or_return!(PCNMessage::from_molecule_slice(&data), "parse message");
        if let Some(peer_id) = context.session.remote_pubkey.clone().map(PeerId::from) {
            self.send_actor_message(NetworkActorMessage::new_event(
                NetworkActorEvent::PeerMessage(peer_id, context.session.clone(), msg),
            ));
        } else {
            warn!(
                "Received message from a peer without remote pubkey {:?}",
                context.session
            );
        }
    }

    async fn notify(&mut self, _context: &mut ProtocolContext, _token: u64) {}
}

#[async_trait]
impl ServiceHandle for Handle {
    async fn handle_error(&mut self, _context: &mut ServiceContext, error: ServiceError) {
        self.emit_event(NetworkServiceEvent::ServiceError(error));
    }
    async fn handle_event(&mut self, _context: &mut ServiceContext, event: ServiceEvent) {
        self.emit_event(NetworkServiceEvent::ServiceEvent(event));
    }
}

pub async fn start_ckb<S: ChannelActorStateStore + Clone + Send + Sync + 'static>(
    config: CkbConfig,
    chain_actor: ActorRef<CkbChainMessage>,
    event_sender: mpsc::Sender<NetworkServiceEvent>,
    tracker: TaskTracker,
    root_actor: ActorCell,
    store: S,
) -> ActorRef<NetworkActorMessage> {
    let (actor, _handle) = Actor::spawn_linked(
        Some("network actor".to_string()),
        NetworkActor::new(event_sender, chain_actor, store),
        (config, tracker),
        root_actor,
    )
    .await
    .expect("Failed to start network actor");

    actor
}
