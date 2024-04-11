use ckb_types::packed::Byte32;
use log::{debug, error, info, warn};
use std::{
    collections::{HashMap, HashSet},
    str,
    sync::Arc,
};
use tentacle::{
    async_trait,
    builder::{MetaBuilder, ServiceBuilder},
    bytes::Bytes,
    context::{ProtocolContext, ProtocolContextMutRef, ServiceContext},
    secio::PeerId,
    service::{
        ProtocolHandle, ProtocolMeta, ServiceAsyncControl, ServiceError, ServiceEvent,
        TargetProtocol,
    },
    traits::{ServiceHandle, ServiceProtocol},
    ProtocolId, SessionId,
};
use tokio::select;
use tokio::sync::{mpsc, Mutex};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::ckb::{
    channel::{
        ChannelCommand, COUNTERPARTY_INITIAL_COMMITMENT_NUMBER, DEFAULT_COMMITMENT_FEE_RATE,
        DEFAULT_FEE_RATE, DEFAULT_MAX_ACCEPT_TLCS, DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
        DEFAULT_MIN_TLC_VALUE, DEFAULT_TO_SELF_DELAY, HOLDER_INITIAL_COMMITMENT_NUMBER,
    },
    types::AcceptChannel,
};

use crate::unwrap_or_return;

use super::{
    channel::{Channel, ChannelEvent, ProcessingChannelError, ProcessingChannelResult},
    command::PCNMessageWithPeerId,
    types::{Hash256, OpenChannel, PCNMessage},
    CkbConfig, Command, Event,
};

pub const PCN_PROTOCOL_ID: ProtocolId = ProtocolId::new(42);

#[derive(Clone, Debug)]
struct PHandle {
    state: SharedState,
}

impl PHandle {
    fn new(state: SharedState) -> Self {
        Self { state }
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

    async fn send_event(&self, event: Event) {
        let _ = self.state.event_sender.send(event).await;
    }

    pub fn handle_pcnmessage(
        &self,
        peer_id: PeerId,
        peer: &mut PeerInfo,
        msg: PCNMessage,
    ) -> ProcessingChannelResult {
        match msg {
            PCNMessage::TestMessage(test) => {
                debug!("Test message {:?}", test);
                Ok(())
            }
            PCNMessage::OpenChannel(open_channel) => {
                debug!("Openning channel {:?}", &open_channel);

                let counterpart_pubkeys = (&open_channel).into();
                let OpenChannel {
                    channel_id,
                    chain_hash,
                    funding_type_script,
                    funding_amount,
                    to_self_delay,
                    first_per_commitment_point,
                    second_per_commitment_point,
                    ..
                } = &open_channel;

                if peer.channels.contains_key(&open_channel.channel_id) {
                    return Err(ProcessingChannelError::InvalidParameter(format!(
                        "Trying to open channel {:?} that already exists",
                        open_channel.channel_id
                    )));
                }

                if *chain_hash != [0u8; 32].into() {
                    return Err(ProcessingChannelError::InvalidParameter(format!(
                        "Invalid chain hash {:?}",
                        chain_hash
                    )));
                }

                if funding_type_script.is_some() {
                    return Err(ProcessingChannelError::InvalidParameter(
                        "Funding type script is not none".to_string(),
                    ));
                }

                let channel_user_id = peer.channels.len();
                let seed = channel_user_id
                    .to_be_bytes()
                    .into_iter()
                    .chain(peer_id.as_bytes().iter().cloned())
                    .collect::<Vec<u8>>();

                let channel = Channel::new_inbound_channel(
                    *channel_id,
                    &seed,
                    peer_id.clone(),
                    *funding_amount,
                    *to_self_delay,
                    counterpart_pubkeys,
                    first_per_commitment_point.clone(),
                    second_per_commitment_point.clone(),
                );

                let commitment_number = COUNTERPARTY_INITIAL_COMMITMENT_NUMBER;

                let accept_channel = AcceptChannel {
                    channel_id: *channel_id,
                    funding_amount: 0,
                    max_tlc_value_in_flight: DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
                    max_accept_tlcs: DEFAULT_MAX_ACCEPT_TLCS,
                    to_self_delay: *to_self_delay,
                    funding_pubkey: channel.signer.funding_key.pubkey(),
                    revocation_basepoint: channel.signer.revocation_base_key.pubkey(),
                    payment_basepoint: channel.signer.payment_key.pubkey(),
                    min_tlc_value: DEFAULT_MIN_TLC_VALUE,
                    delayed_payment_basepoint: channel.signer.delayed_payment_base_key.pubkey(),
                    tlc_basepoint: channel.signer.tlc_base_key.pubkey(),
                    first_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number),
                    second_per_commitment_point: channel
                        .signer
                        .get_commitment_point(commitment_number - 1),
                    next_local_nonce: channel.signer.misig_nonce.public_nonce(),
                };

                if peer.channels.contains_key(&channel_id) {
                    error!("Channel {:?} already exists", &channel_id);
                }
                let _ = peer.channels.insert(channel_id.clone(), channel);
                let command = PCNMessageWithPeerId {
                    peer_id,
                    message: PCNMessage::AcceptChannel(accept_channel),
                };
                // TODO: maybe we should not use try_send here.
                self.state
                    .command_sender
                    .try_send(Command::SendPcnMessage(command))?;

                debug!("Peer {:?} openning channel", peer);
                Ok(())
            }

            PCNMessage::AcceptChannel(accpet_channel) => {
                debug!("Accepting channel {:?}", &accpet_channel);
                let channel = match peer.channels.get_mut(&accpet_channel.channel_id) {
                    Some(channel) => channel,
                    None => {
                        return Err(ProcessingChannelError::InvalidParameter(format!(
                            "Trying to accept channel {:?} that does not exist",
                            &accpet_channel.channel_id
                        )));
                    }
                };
                channel.step(ChannelEvent::AcceptChannel(accpet_channel))
            }

            _ => {
                error!("Message handling for {:?} unimplemented", msg);
                Ok(())
            }
        }
    }
}

#[async_trait]
impl ServiceProtocol for PHandle {
    async fn init(&mut self, _context: &mut ProtocolContext) {}

    async fn connected(&mut self, context: ProtocolContextMutRef<'_>, version: &str) {
        let session = context.session;
        info!(
            "proto id [{}] open on session [{}], address: [{}], type: [{:?}], version: {}",
            context.proto_id, session.id, session.address, session.ty, version
        );
        self.send_event(Event::PeerConnected(context.session.address.clone()))
            .await;

        let peer_id = context.session.remote_pubkey.clone().map(Into::into);
        match peer_id {
            Some(peer_id) => {
                let mut peer_state = self.state.peers.lock().await;
                debug!("Trying to save session of peer {:?}", peer_id);
                let peer = peer_state.entry(peer_id).or_default();
                peer.sessions.insert(context.session.id);
            }
            _ => {
                warn!("Connected to a peer without public key");
                return;
            }
        }
    }

    async fn disconnected(&mut self, context: ProtocolContextMutRef<'_>) {
        info!(
            "proto id [{}] close on session [{}]",
            context.proto_id, context.session.id
        );
        self.send_event(Event::PeerDisConnected(context.session.address.clone()))
            .await;

        let peer_id = context.session.remote_pubkey.clone().map(Into::into);
        match peer_id.as_ref() {
            Some(peer_id) => {
                let mut peer_state = self.state.peers.lock().await;
                debug!("Trying to save session of peer {:?}", peer_id);
                let peer = peer_state.get_mut(peer_id);
                match peer {
                    Some(peer) => {
                        peer.sessions.remove(&context.session.id);
                        if peer.sessions.is_empty() {
                            debug!(
                                "Peer {:?} disconnected, the last session was {}",
                                peer_id, context.session.id
                            );
                            peer_state.remove(peer_id);
                        }
                    }
                    None => {
                        warn!(
                            "Trying to reomve a peer not recorded in peer state {:?}",
                            peer_id
                        )
                    }
                }
            }
            _ => {
                warn!("Disconnected from a peer without public key");
                return;
            }
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
        let peer_id = match context.session.remote_pubkey.clone().map(Into::into) {
            Some(peer_id) => peer_id,
            None => {
                warn!("Received message from a peer without public key");
                return;
            }
        };
        let mut peer_state = self.state.peers.lock().await;
        let peer = match peer_state.get_mut(&peer_id) {
            Some(peer) => peer,
            None => {
                warn!("Trying to send message to unknown peer {:?}", peer_id);
                return;
            }
        };
        if let Err(err) = self.handle_pcnmessage(peer_id, peer, msg) {
            error!("Error while processing message: {:?}", err);
        }
    }

    async fn notify(&mut self, _context: &mut ProtocolContext, _token: u64) {}
}

#[derive(Clone, Debug)]
struct SHandle {
    state: SharedState,
}

impl SHandle {
    fn new(state: SharedState) -> Self {
        Self { state }
    }

    async fn send_event(&self, event: Event) {
        let _ = self.state.event_sender.send(event).await;
    }
}

#[async_trait]
impl ServiceHandle for SHandle {
    async fn handle_error(&mut self, _context: &mut ServiceContext, error: ServiceError) {
        self.send_event(Event::ServiceError(error)).await;
    }
    async fn handle_event(&mut self, _context: &mut ServiceContext, event: ServiceEvent) {
        self.send_event(Event::ServiceEvent(event)).await;
    }
}

#[derive(Clone, Debug)]
pub struct SharedState {
    peers: Arc<Mutex<HashMap<PeerId, PeerInfo>>>,
    event_sender: mpsc::Sender<Event>,
    command_sender: mpsc::Sender<Command>,
}

impl SharedState {
    fn new(event_sender: mpsc::Sender<Event>, command_sender: mpsc::Sender<Command>) -> Self {
        Self {
            peers: Arc::new(Mutex::new(HashMap::new())),
            event_sender,
            command_sender,
        }
    }

    async fn save_channel(&self, peer: &PeerId, channel: Channel) -> bool {
        let mut peer_state = self.peers.lock().await;
        let peer = peer_state.get_mut(peer).expect("Peer must exist");

        let channel_id = channel.id();
        if peer.channels.contains_key(&channel_id) {
            return false;
        }
        let _ = peer.channels.insert(channel_id, channel);
        true
    }
}

struct NetworkState {
    control: ServiceAsyncControl,
    shared_state: SharedState,
    token: CancellationToken,
    command_receiver: mpsc::Receiver<Command>,
}

impl NetworkState {
    fn new(
        control: ServiceAsyncControl,
        shared_state: SharedState,
        token: CancellationToken,
        command_receiver: mpsc::Receiver<Command>,
    ) -> Self {
        Self {
            control,
            shared_state,
            token,
            command_receiver,
        }
    }

    async fn get_peer_session(&self, peer_id: &PeerId) -> Option<SessionId> {
        let peer_state = self.shared_state.peers.lock().await;
        peer_state
            .get(&peer_id)
            .and_then(|p| p.sessions.iter().next())
            .cloned()
    }

    async fn process_command(&self, command: Command) {
        debug!("Processing command {:?}", command);

        macro_rules! get_session_or_return {
            ($expr:expr) => {
                match self.get_peer_session($expr).await {
                    Some(s) => s,
                    None => {
                        error!("Session for peer {:?} not found", $expr);
                        return;
                    }
                }
            };
        }

        match command {
            Command::ConnectPeer(addr) => {
                // TODO: It is more than just dialing a peer. We need to exchange capabilities of the peer,
                // e.g. whether the peer support some specific feature.
                // TODO: If we are already connected to the peer, skip connecting.
                debug!("Dialing {}", &addr);
                let result = self.control.dial(addr.clone(), TargetProtocol::All).await;
                if let Err(err) = result {
                    error!("Dialing {} failed: {}", &addr, err);
                }
            }

            Command::SendPcnMessage(PCNMessageWithPeerId { peer_id, message }) => {
                debug!(
                    "received command to send message {:?} to peer {:?}",
                    &peer_id, &message
                );

                let session_id = get_session_or_return!(&peer_id);

                let result = self
                    .control
                    .send_message_to(session_id, PCN_PROTOCOL_ID, message.to_molecule_bytes())
                    .await;
                if let Err(err) = result {
                    error!(
                        "Sending message to session {:?} failed: {}",
                        &session_id, err
                    );
                    return;
                }
                info!("Message send to peer {:?}", &peer_id);
            }

            Command::ControlPcnChannel(c) => match c {
                ChannelCommand::OpenChannel(open_channel) => {
                    info!("Trying to open a channel to {:?}", &open_channel.peer_id);
                    let session_id = get_session_or_return!(&open_channel.peer_id);

                    let channel: Result<Channel, ProcessingChannelError> =
                        open_channel.create_channel();
                    let channel = unwrap_or_return!(channel, "failed to create a channel");
                    if !self
                        .shared_state
                        .save_channel(&open_channel.peer_id, channel.clone())
                        .await
                    {
                        error!(
                            "Failed to save a already existed channel {:?}",
                            channel.id()
                        );
                    }

                    let commitment_number = HOLDER_INITIAL_COMMITMENT_NUMBER;
                    let message = PCNMessage::OpenChannel(OpenChannel {
                        chain_hash: Byte32::zero().into(),
                        funding_type_script: None,
                        funding_amount: channel.to_self_value,
                        funding_fee_rate: DEFAULT_FEE_RATE,
                        commitment_fee_rate: DEFAULT_COMMITMENT_FEE_RATE,
                        max_tlc_value_in_flight: DEFAULT_MAX_TLC_VALUE_IN_FLIGHT,
                        max_accept_tlcs: DEFAULT_MAX_ACCEPT_TLCS,
                        min_tlc_value: DEFAULT_MIN_TLC_VALUE,
                        to_self_delay: DEFAULT_TO_SELF_DELAY,
                        channel_flags: 0,
                        first_per_commitment_point: channel
                            .signer
                            .get_commitment_point(commitment_number),
                        second_per_commitment_point: channel
                            .signer
                            .get_commitment_point(commitment_number + 1),
                        channel_id: channel.temp_id,
                        funding_pubkey: channel.holder_channel_parameters.pubkeys.funding_pubkey,
                        revocation_basepoint: channel
                            .holder_channel_parameters
                            .pubkeys
                            .revocation_basepoint,
                        payment_basepoint: channel.holder_channel_parameters.pubkeys.payment_point,
                        delayed_payment_basepoint: channel
                            .holder_channel_parameters
                            .pubkeys
                            .delayed_payment_basepoint,
                        tlc_basepoint: channel.holder_channel_parameters.pubkeys.tlc_basepoint,
                        next_local_nonce: channel.signer.misig_nonce.public_nonce(),
                    });

                    debug!(
                        "OpenChannel message to {:?} created: {:?}",
                        &open_channel.peer_id, &message
                    );
                    let message_bytes = message.to_molecule_bytes();

                    let result = self
                        .control
                        .send_message_to(session_id, PCN_PROTOCOL_ID, message_bytes)
                        .await;
                    if let Err(err) = result {
                        error!("Sending message to session {} failed: {}", session_id, err);
                    }
                    debug!(
                        "Sent OpenChannel message to peer {:?}",
                        &open_channel.peer_id
                    );
                }
            },
        }
    }

    async fn run(mut self) {
        loop {
            select! {
                _ = self.token.cancelled() => {
                    debug!("Cancellation received, shutting down tentacle service");
                    let _ = self.control.shutdown().await;
                    break;
                }
                command = self.command_receiver.recv() => {
                    match command {
                        None => {
                            debug!("Command receiver completed, shutting down tentacle service");
                            let _ = self.control.shutdown().await;
                            break;
                        }
                        Some(command) => {
                            self.process_command(command).await;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct PeerInfo {
    sessions: HashSet<SessionId>,
    channels: HashMap<Hash256, Channel>,
}

pub async fn start_ckb(
    config: CkbConfig,
    command_receiver: mpsc::Receiver<Command>,
    command_sender: mpsc::Sender<Command>,
    event_sender: mpsc::Sender<Event>,
    token: CancellationToken,
    tracker: TaskTracker,
) {
    let kp = config
        .read_or_generate_secret_key()
        .expect("read or generate secret key");
    let pk = kp.public_key();
    let shared_state = SharedState::new(event_sender, command_sender);
    let mut service = ServiceBuilder::default()
        .insert_protocol(PHandle::new(shared_state.clone()).create_meta(PCN_PROTOCOL_ID))
        .key_pair(kp)
        .build(SHandle::new(shared_state.clone()));
    let listen_addr = service
        .listen(
            format!("/ip4/127.0.0.1/tcp/{}", config.listening_port)
                .parse()
                .expect("valid tentacle address"),
        )
        .await
        .expect("listen tentacle");

    info!(
        "Started listening tentacle on {}/p2p/{}",
        listen_addr,
        PeerId::from(pk).to_base58()
    );

    let control = service.control().to_owned();

    tracker.spawn(async move {
        service.run().await;
        debug!("Tentacle service shutdown");
    });

    tracker.spawn(async move {
        NetworkState::new(control, shared_state, token, command_receiver)
            .run()
            .await;
    });
}
